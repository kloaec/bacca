//! BitBox02 products (hardware platform + firmware edition), their identifiers, and versions.
//!
//! References:
//! - HID product strings: `bitbox02-firmware/py/bitbox02/bitbox02/communication/devices.py`.
//! - Signed firmware magics and product ids: `bitbox02-firmware/scripts/signed_firmware.py` and
//!   `bitbox02-firmware/src/bootloader/bootloader_product.h`.
//! - Release asset names and labels: `bitbox02-firmware/scripts/create_release.py` (since v9.25.0)
//!   and `bitbox02-firmware/releases/README.md` (until v9.24.0).

use std::fmt;

/// USB vendor id (Microchip) of all BitBox02 devices.
pub(crate) const VENDOR_ID: u16 = 0x03eb;
/// USB product id of all BitBox02 devices, in both firmware and bootloader mode.
pub(crate) const PRODUCT_ID: u16 = 0x2403;
/// USB product id of some development bootloaders, accepted like the Python library does.
pub(crate) const PRODUCT_ID_DEV_BOOTLOADER: u16 = 0x2402;

/// The hardware platform of a BitBox02 device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    BitBox02,
    /// Called "BitBox02 Plus" or "bb02p" in the sources.
    BitBox02Nova,
}

/// The firmware edition. On the BitBox02 the edition *is* the "app": there is no separate Bitcoin
/// app to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    Multi,
    BitcoinOnly,
}

/// A BitBox02 product: a platform running a firmware edition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    BitBox02Multi,
    BitBox02BtcOnly,
    BitBox02NovaMulti,
    BitBox02NovaBtcOnly,
}

/// Whether a device is running its firmware or sitting in its bootloader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Firmware,
    Bootloader,
}

use Product::*;

impl Product {
    pub fn platform(self) -> Platform {
        match self {
            BitBox02Multi | BitBox02BtcOnly => Platform::BitBox02,
            BitBox02NovaMulti | BitBox02NovaBtcOnly => Platform::BitBox02Nova,
        }
    }

    pub fn edition(self) -> Edition {
        match self {
            BitBox02Multi | BitBox02NovaMulti => Edition::Multi,
            BitBox02BtcOnly | BitBox02NovaBtcOnly => Edition::BitcoinOnly,
        }
    }

    pub(crate) fn from_hid_product_string(s: &str) -> Option<(Product, Mode)> {
        Some(match s {
            "BitBox02" => (BitBox02Multi, Mode::Firmware),
            "BitBox02BTC" => (BitBox02BtcOnly, Mode::Firmware),
            "BitBox02 Nova Multi" => (BitBox02NovaMulti, Mode::Firmware),
            "BitBox02 Nova BTC-only" => (BitBox02NovaBtcOnly, Mode::Firmware),
            "bb02-bootloader" => (BitBox02Multi, Mode::Bootloader),
            "bb02btc-bootloader" => (BitBox02BtcOnly, Mode::Bootloader),
            "BitBox02 Nova Multi bl" => (BitBox02NovaMulti, Mode::Bootloader),
            "BitBox02 Nova BTC-only bl" => (BitBox02NovaBtcOnly, Mode::Bootloader),
            _ => return None,
        })
    }

    /// From the 4 bytes (big endian) magic prefix of a signed firmware.
    pub(crate) fn from_sigdata_magic(magic: u32) -> Option<Self> {
        match magic {
            0x653f362b => Some(BitBox02Multi),
            0x11233b0b => Some(BitBox02BtcOnly),
            0x5b648ceb => Some(BitBox02NovaMulti),
            0x48714774 => Some(BitBox02NovaBtcOnly),
            _ => None,
        }
    }

    /// The product id mixed into the firmware hash by bootloaders >= 1.2.0.
    pub(crate) fn bootloader_product_id(self) -> u16 {
        match self {
            BitBox02Multi => 1,
            BitBox02BtcOnly => 2,
            BitBox02NovaMulti => 3,
            BitBox02NovaBtcOnly => 4,
        }
    }

    /// Prefixes of the release assets `<prefix>.vX.Y.Z.signed.bin`: the current naming (since
    /// v9.25.0), then the legacy one (until v9.24.0, only for the BitBox02).
    pub(crate) fn release_asset_prefixes(self) -> &'static [&'static str] {
        match self {
            BitBox02Multi => &["firmware-bitbox02-multi", "firmware"],
            BitBox02BtcOnly => &["firmware-bitbox02-btconly", "firmware-btc"],
            BitBox02NovaMulti => &["firmware-bitbox02nova-multi"],
            BitBox02NovaBtcOnly => &["firmware-bitbox02nova-btconly"],
        }
    }
}

/// The label used in the release notes (`PRODUCTS` in `create_release.py`).
impl fmt::Display for Product {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            BitBox02Multi => "BitBox02 Multi",
            BitBox02BtcOnly => "BitBox02 Bitcoin-only",
            BitBox02NovaMulti => "BitBox02 Nova Multi",
            BitBox02NovaBtcOnly => "BitBox02 Nova Bitcoin-only",
        })
    }
}

impl fmt::Display for Edition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Edition::Multi => "Multi",
            Edition::BitcoinOnly => "Bitcoin-only",
        })
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Platform::BitBox02 => "BitBox02",
            Platform::BitBox02Nova => "BitBox02 Nova",
        })
    }
}

/// A `major.minor.patch` version. Pre-release and build metadata are ignored, like
/// `Bootloader.__init__` does in `bitbox02-firmware/py/bitbox02/bitbox02/bitbox02/bootloader.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Version {
            major,
            minor,
            patch,
        }
    }

    /// Parse `X.Y.Z`, optionally prefixed with `v` and followed by `-pre`/`+build` metadata.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let mut parts = s.split(['-', '+']).next()?.split('.');
        let mut next = || parts.next()?.parse().ok();
        let v = Version::new(next()?, next()?, next()?);
        parts.next().is_none().then_some(v)
    }

    /// Find a `vX.Y.Z` version anywhere in a string (the HID serial number string, which contains
    /// the firmware or bootloader version), like `parse_device_version()` in
    /// `bitbox02-firmware/py/bitbox02/bitbox02/communication/devices.py`.
    pub(crate) fn find_in(s: &str) -> Option<Self> {
        s.match_indices('v').find_map(|(i, _)| {
            let rest = &s[i + 1..];
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+'))
                .unwrap_or(rest.len());
            Version::parse(&rest[..end])
        })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn product_strings() {
        assert_eq!(
            Product::from_hid_product_string("BitBox02BTC"),
            Some((BitBox02BtcOnly, Mode::Firmware))
        );
        assert_eq!(
            Product::from_hid_product_string("BitBox02 Nova BTC-only bl"),
            Some((BitBox02NovaBtcOnly, Mode::Bootloader))
        );
        assert_eq!(Product::from_hid_product_string("BitBox02 "), None);
    }

    #[test]
    fn versions() {
        assert_eq!(Version::parse("v9.27.1"), Some(Version::new(9, 27, 1)));
        assert_eq!(Version::parse("3.0.0-pre+dev"), Some(Version::new(3, 0, 0)));
        assert_eq!(Version::parse("1.2"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("vx.2.3"), None);
        assert_eq!(
            Version::find_in("abcdef v9.27.1-dev"),
            Some(Version::new(9, 27, 1))
        );
        assert_eq!(Version::find_in("nothing"), None);
        assert!(Version::new(9, 27, 1) > Version::new(9, 9, 0));
    }
}
