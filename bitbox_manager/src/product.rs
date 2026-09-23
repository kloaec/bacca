//! BitBox02 products (hardware platform + firmware edition) and their identifiers.
//!
//! References:
//! - HID product strings: `bitbox02-firmware/py/bitbox02/bitbox02/communication/devices.py` and
//!   `bitbox-wallet-app/vendor/github.com/BitBoxSwiss/bitbox02-api-go/api/common/common.go`.
//! - Signed firmware magics and product ids: `bitbox02-firmware/scripts/signed_firmware.py` and
//!   `bitbox02-firmware/src/bootloader/bootloader_product.h`.
//! - Release asset names: `bitbox02-firmware/scripts/create_release.py` (current naming, from
//!   v9.25.0) and `bitbox02-firmware/releases/README.md` (legacy naming, until v9.24.0).

use std::fmt;

/// USB vendor id of all BitBox02 devices (Microchip).
pub const VENDOR_ID: u16 = 0x03eb;
/// USB product id of all BitBox02 devices, in both firmware and bootloader mode.
pub const PRODUCT_ID: u16 = 0x2403;
/// USB product id of some development bootloaders. Accepted like the Python library does.
/// See `get_devices()` in `bitbox02-firmware/py/bitbox02/bitbox02/communication/devices.py`.
pub const PRODUCT_ID_DEV_BOOTLOADER: u16 = 0x2402;

/// The hardware platform of a BitBox02 device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Platform {
    /// The original BitBox02.
    BitBox02,
    /// The BitBox02 Nova (called "BitBox02 Plus" or "bb02p" in the sources).
    BitBox02Nova,
}

/// The firmware edition. On the BitBox02 the edition *is* the "app": there is no separate Bitcoin
/// app to install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edition {
    Multi,
    BitcoinOnly,
}

/// A BitBox02 product: a platform running a firmware edition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Product {
    BitBox02Multi,
    BitBox02BtcOnly,
    BitBox02NovaMulti,
    BitBox02NovaBtcOnly,
}

/// Whether a device is running its firmware or sitting in its bootloader.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    Firmware,
    Bootloader,
}

impl Product {
    pub const ALL: [Product; 4] = [
        Product::BitBox02Multi,
        Product::BitBox02BtcOnly,
        Product::BitBox02NovaMulti,
        Product::BitBox02NovaBtcOnly,
    ];

    pub fn new(platform: Platform, edition: Edition) -> Self {
        match (platform, edition) {
            (Platform::BitBox02, Edition::Multi) => Product::BitBox02Multi,
            (Platform::BitBox02, Edition::BitcoinOnly) => Product::BitBox02BtcOnly,
            (Platform::BitBox02Nova, Edition::Multi) => Product::BitBox02NovaMulti,
            (Platform::BitBox02Nova, Edition::BitcoinOnly) => Product::BitBox02NovaBtcOnly,
        }
    }

    pub fn platform(&self) -> Platform {
        match self {
            Product::BitBox02Multi | Product::BitBox02BtcOnly => Platform::BitBox02,
            Product::BitBox02NovaMulti | Product::BitBox02NovaBtcOnly => Platform::BitBox02Nova,
        }
    }

    pub fn edition(&self) -> Edition {
        match self {
            Product::BitBox02Multi | Product::BitBox02NovaMulti => Edition::Multi,
            Product::BitBox02BtcOnly | Product::BitBox02NovaBtcOnly => Edition::BitcoinOnly,
        }
    }

    /// The same platform, with the Bitcoin-only edition.
    pub fn bitcoin_only(&self) -> Self {
        Product::new(self.platform(), Edition::BitcoinOnly)
    }

    /// HID product string when running the firmware.
    pub fn firmware_product_string(&self) -> &'static str {
        match self {
            Product::BitBox02Multi => "BitBox02",
            Product::BitBox02BtcOnly => "BitBox02BTC",
            Product::BitBox02NovaMulti => "BitBox02 Nova Multi",
            Product::BitBox02NovaBtcOnly => "BitBox02 Nova BTC-only",
        }
    }

    /// HID product string when in bootloader mode.
    pub fn bootloader_product_string(&self) -> &'static str {
        match self {
            Product::BitBox02Multi => "bb02-bootloader",
            Product::BitBox02BtcOnly => "bb02btc-bootloader",
            Product::BitBox02NovaMulti => "BitBox02 Nova Multi bl",
            Product::BitBox02NovaBtcOnly => "BitBox02 Nova BTC-only bl",
        }
    }

    /// Parse a HID product string, returning the product and the mode it denotes.
    pub fn from_hid_product_string(s: &str) -> Option<(Product, Mode)> {
        Product::ALL.iter().find_map(|p| {
            if s == p.firmware_product_string() {
                Some((*p, Mode::Firmware))
            } else if s == p.bootloader_product_string() {
                Some((*p, Mode::Bootloader))
            } else {
                None
            }
        })
    }

    /// The 4 bytes (big endian) magic prefix of a signed firmware for this product.
    pub fn sigdata_magic(&self) -> u32 {
        match self {
            Product::BitBox02Multi => 0x653f362b,
            Product::BitBox02BtcOnly => 0x11233b0b,
            Product::BitBox02NovaMulti => 0x5b648ceb,
            Product::BitBox02NovaBtcOnly => 0x48714774,
        }
    }

    pub fn from_sigdata_magic(magic: u32) -> Option<Self> {
        Product::ALL
            .iter()
            .copied()
            .find(|p| p.sigdata_magic() == magic)
    }

    /// The product id mixed into the firmware hash by bootloaders >= 1.2.0.
    pub fn bootloader_product_id(&self) -> u16 {
        match self {
            Product::BitBox02Multi => 1,
            Product::BitBox02BtcOnly => 2,
            Product::BitBox02NovaMulti => 3,
            Product::BitBox02NovaBtcOnly => 4,
        }
    }

    /// Prefixes of the signed firmware release assets for this product, as in
    /// `<prefix>.vX.Y.Z.signed.bin`. The first one is the current naming (since v9.25.0), the
    /// others are legacy names (until v9.24.0, only for the BitBox02).
    pub fn release_asset_prefixes(&self) -> &'static [&'static str] {
        match self {
            Product::BitBox02Multi => &["firmware-bitbox02-multi", "firmware"],
            Product::BitBox02BtcOnly => &["firmware-bitbox02-btconly", "firmware-btc"],
            Product::BitBox02NovaMulti => &["firmware-bitbox02nova-multi"],
            Product::BitBox02NovaBtcOnly => &["firmware-bitbox02nova-btconly"],
        }
    }

    /// The label used for this product in the release notes of the official releases, see
    /// `PRODUCTS` in `bitbox02-firmware/scripts/create_release.py`.
    pub fn release_label(&self) -> &'static str {
        match self {
            Product::BitBox02Multi => "BitBox02 Multi",
            Product::BitBox02BtcOnly => "BitBox02 Bitcoin-only",
            Product::BitBox02NovaMulti => "BitBox02 Nova Multi",
            Product::BitBox02NovaBtcOnly => "BitBox02 Nova Bitcoin-only",
        }
    }
}

impl fmt::Display for Product {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.release_label())
    }
}

impl fmt::Display for Edition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Edition::Multi => write!(f, "Multi"),
            Edition::BitcoinOnly => write!(f, "Bitcoin-only"),
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Platform::BitBox02 => write!(f, "BitBox02"),
            Platform::BitBox02Nova => write!(f, "BitBox02 Nova"),
        }
    }
}

/// A simple `major.minor.patch` version. Pre-release and build metadata are parsed but ignored
/// for comparisons, like `Bootloader.__init__` does in
/// `bitbox02-firmware/py/bitbox02/bitbox02/bitbox02/bootloader.py`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let core = s.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version::new(major, minor, patch))
    }

    /// Find a `vX.Y.Z` version anywhere in a string, as `parse_device_version()` does in
    /// `bitbox02-firmware/py/bitbox02/bitbox02/communication/devices.py`. Used on the HID serial
    /// number string, which contains the firmware or bootloader version.
    pub fn find_in(s: &str) -> Option<Self> {
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
            Some((Product::BitBox02BtcOnly, Mode::Firmware))
        );
        assert_eq!(
            Product::from_hid_product_string("bb02-bootloader"),
            Some((Product::BitBox02Multi, Mode::Bootloader))
        );
        assert_eq!(
            Product::from_hid_product_string("BitBox02 Nova BTC-only bl"),
            Some((Product::BitBox02NovaBtcOnly, Mode::Bootloader))
        );
        assert_eq!(
            Product::from_hid_product_string("BitBox02 Nova Multi"),
            Some((Product::BitBox02NovaMulti, Mode::Firmware))
        );
        assert_eq!(Product::from_hid_product_string("bitbox"), None);
        assert_eq!(Product::from_hid_product_string("BitBox02 "), None);
        for p in Product::ALL {
            assert_eq!(Product::from_sigdata_magic(p.sigdata_magic()), Some(p));
            assert_eq!(Product::new(p.platform(), p.edition()), p);
            assert_eq!(p.bitcoin_only().edition(), Edition::BitcoinOnly);
            assert_eq!(p.bitcoin_only().platform(), p.platform());
        }
    }

    #[test]
    fn versions() {
        assert_eq!(Version::parse("v9.27.1"), Some(Version::new(9, 27, 1)));
        assert_eq!(Version::parse("1.2.2"), Some(Version::new(1, 2, 2)));
        assert_eq!(Version::parse("3.0.0-pre+dev"), Some(Version::new(3, 0, 0)));
        assert_eq!(Version::parse("1.2"), None);
        assert_eq!(Version::parse("1.2.3.4"), None);
        assert_eq!(Version::parse("vx.2.3"), None);
        assert_eq!(Version::find_in("v1.2.2"), Some(Version::new(1, 2, 2)));
        assert_eq!(
            Version::find_in("abcdef v9.27.1-dev"),
            Some(Version::new(9, 27, 1))
        );
        assert_eq!(Version::find_in("nothing"), None);
        assert!(Version::new(9, 27, 1) > Version::new(9, 9, 0));
        assert!(Version::new(1, 2, 0) <= Version::new(1, 2, 2));
    }
}
