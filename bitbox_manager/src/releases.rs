//! Discovery and download of the official signed firmware releases.
//!
//! Releases are published on <https://github.com/BitBoxSwiss/bitbox02-firmware/releases>:
//! - since v9.25.0, tag `firmware/vX.Y.Z` with assets `firmware-bitbox02-multi.vX.Y.Z.signed.bin`,
//!   `firmware-bitbox02-btconly.vX.Y.Z.signed.bin`, `firmware-bitbox02nova-multi.vX.Y.Z.signed.bin`
//!   and `firmware-bitbox02nova-btconly.vX.Y.Z.signed.bin` (see
//!   `bitbox02-firmware/scripts/create_release.py`),
//! - until v9.24.0, tags `firmware/vX.Y.Z` (asset `firmware.vX.Y.Z.signed.bin`, Multi) and
//!   `firmware-btc-only/vX.Y.Z` (asset `firmware-btc.vX.Y.Z.signed.bin`, Bitcoin-only), see
//!   `bitbox02-firmware/releases/README.md`.
//!
//! Upgrading from old firmwares requires installing and booting intermediate firmwares first
//! (see `bitbox-wallet-app/backend/devices/bitbox02bootloader/firmware.go` and the release notes
//! template in `create_release.py`). Their unsigned binary hashes are pinned here.

use std::fmt;

use crate::{
    product::{Product, Version},
    signed_firmware::{FirmwareFormatError, SignedFirmware},
};

pub const RELEASES_API_URL: &str =
    "https://api.github.com/repos/BitBoxSwiss/bitbox02-firmware/releases";
const USER_AGENT: &str = concat!("bacca-bitbox_manager/", env!("CARGO_PKG_VERSION"));
const HTTP_TIMEOUT_SECS: u64 = 120;
/// Max size of a downloaded asset. A signed firmware is at most 4 + 584 + 884736 bytes.
const MAX_DOWNLOAD_SIZE: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub enum ReleaseError {
    Http(minreq::Error),
    HttpStatus(u16, String),
    Json(String),
    /// No release found for this product.
    NotFound(Product),
    InvalidFirmware(FirmwareFormatError),
    /// The downloaded firmware is not what we expected.
    Mismatch(String),
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(e) => write!(f, "HTTP error: {}", e),
            Self::HttpStatus(c, u) => write!(f, "HTTP error {} when fetching {}", c, u),
            Self::Json(e) => write!(f, "could not parse the GitHub releases: {}", e),
            Self::NotFound(p) => write!(f, "no firmware release found for {}", p),
            Self::InvalidFirmware(e) => write!(f, "invalid downloaded firmware: {}", e),
            Self::Mismatch(s) => write!(f, "downloaded firmware mismatch: {}", s),
        }
    }
}

impl std::error::Error for ReleaseError {}

impl From<minreq::Error> for ReleaseError {
    fn from(e: minreq::Error) -> Self {
        ReleaseError::Http(e)
    }
}

/// A release, as returned by the GitHub releases API (only the fields we use).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct GithubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assets: Vec<GithubAsset>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GithubAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// A firmware release asset for a given product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareRelease {
    pub product: Product,
    /// Marketing version, e.g. 9.27.1.
    pub version: Version,
    pub tag: String,
    pub asset_name: String,
    pub url: String,
    /// The firmware hash as published in the release notes, if found.
    pub published_sighash: Option<[u8; 32]>,
}

/// Parse the version from an asset name `<prefix>.vX.Y.Z.signed.bin` if the prefix is one of the
/// names used for `product`.
pub fn asset_version(product: Product, asset_name: &str) -> Option<Version> {
    let rest = asset_name.strip_suffix(".signed.bin")?;
    product.release_asset_prefixes().iter().find_map(|prefix| {
        let v = rest.strip_prefix(prefix)?.strip_prefix(".v")?;
        Version::parse(v)
    })
}

/// Find the firmware hash published for `product` in the release notes, in the format of
/// `render_release_notes()` of `bitbox02-firmware/scripts/create_release.py`:
/// ``- BitBox02 Bitcoin-only: `<hex>` ``.
pub fn parse_published_sighash(product: Product, body: &str) -> Option<[u8; 32]> {
    let marker = format!("- {}: `", product.release_label());
    body.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(&marker)?;
        let hex_str = rest.split('`').next()?;
        let bytes = hex::decode(hex_str.trim()).ok()?;
        bytes.try_into().ok()
    })
}

/// Find the firmware assets for `product` in a list of releases (drafts and pre-releases are
/// ignored), sorted from the newest to the oldest version.
pub fn firmware_releases(product: Product, releases: &[GithubRelease]) -> Vec<FirmwareRelease> {
    let mut out: Vec<FirmwareRelease> = releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        .flat_map(|r| {
            r.assets.iter().filter_map(move |a| {
                let version = asset_version(product, &a.name)?;
                Some(FirmwareRelease {
                    product,
                    version,
                    tag: r.tag_name.clone(),
                    asset_name: a.name.clone(),
                    url: a.browser_download_url.clone(),
                    published_sighash: r
                        .body
                        .as_deref()
                        .and_then(|b| parse_published_sighash(product, b)),
                })
            })
        })
        .collect();
    out.sort_by(|a, b| b.version.cmp(&a.version));
    out.dedup_by(|a, b| a.version == b.version);
    out
}

/// Parse the JSON returned by the GitHub releases API.
pub fn parse_releases(json: &str) -> Result<Vec<GithubRelease>, ReleaseError> {
    serde_json::from_str(json).map_err(|e| ReleaseError::Json(e.to_string()))
}

fn get(url: &str) -> Result<minreq::Response, ReleaseError> {
    log::debug!("GET {}", url);
    let response = minreq::get(url)
        .with_header("User-Agent", USER_AGENT)
        .with_timeout(HTTP_TIMEOUT_SECS)
        .with_max_redirects(10)
        .send()?;
    if response.status_code != 200 {
        return Err(ReleaseError::HttpStatus(
            response.status_code,
            url.to_string(),
        ));
    }
    Ok(response)
}

/// Fetch the latest 100 releases from GitHub.
pub fn fetch_releases() -> Result<Vec<GithubRelease>, ReleaseError> {
    let url = format!("{}?per_page=100", RELEASES_API_URL);
    let response = get(&url)?;
    let body = response
        .as_str()
        .map_err(|e| ReleaseError::Json(e.to_string()))?;
    parse_releases(body)
}

/// Fetch a release by tag name.
pub fn fetch_release_by_tag(tag: &str) -> Result<GithubRelease, ReleaseError> {
    let url = format!("{}/tags/{}", RELEASES_API_URL, tag.replace('/', "%2F"));
    let response = get(&url)?;
    let body = response
        .as_str()
        .map_err(|e| ReleaseError::Json(e.to_string()))?;
    serde_json::from_str(body).map_err(|e| ReleaseError::Json(e.to_string()))
}

/// Find the latest firmware release for this product.
pub fn latest_release(product: Product) -> Result<FirmwareRelease, ReleaseError> {
    let releases = fetch_releases()?;
    firmware_releases(product, &releases)
        .into_iter()
        .next()
        .ok_or(ReleaseError::NotFound(product))
}

/// Download a release asset and check it is a valid signed firmware for the expected product
/// (and matches the published hash, if any).
pub fn download(release: &FirmwareRelease) -> Result<SignedFirmware, ReleaseError> {
    let response = get(&release.url)?;
    let bytes = response.as_bytes();
    if bytes.len() > MAX_DOWNLOAD_SIZE {
        return Err(ReleaseError::Mismatch(format!(
            "asset too big ({} bytes)",
            bytes.len()
        )));
    }
    let firmware = SignedFirmware::parse(bytes).map_err(ReleaseError::InvalidFirmware)?;
    if firmware.product() != release.product {
        return Err(ReleaseError::Mismatch(format!(
            "{} is a firmware for {}, expected {}",
            release.asset_name,
            firmware.product(),
            release.product
        )));
    }
    if let Some(expected) = release.published_sighash {
        if firmware.published_sighash() != expected {
            return Err(ReleaseError::Mismatch(format!(
                "firmware hash {} differs from the one published in the release notes ({})",
                hex::encode(firmware.published_sighash()),
                hex::encode(expected)
            )));
        }
    }
    Ok(firmware)
}

/// How a device signals that an intermediate firmware was booted and the upgrade can continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntermediateCompletion {
    /// The firmware bumps its monotonic version by one when it first boots.
    MonotonicVersionBump,
    /// The firmware upgrades the bootloader to at least this version when it first boots.
    BootloaderVersion(Version),
}

/// An intermediate firmware that must be installed and booted before upgrading further.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Intermediate {
    pub version: Version,
    pub monotonic_version: u32,
    pub completion: IntermediateCompletion,
    pub tag: &'static str,
    pub asset_name: &'static str,
    /// sha256 of the unsigned firmware binary, from the reproducible build assertions in
    /// `bitbox02-firmware/releases/firmware[-btc]-v<version>/assertion*.txt` (also in the
    /// `.sha256` files next to the binaries bundled by the BitBoxApp).
    pub unsigned_sha256: &'static str,
}

impl Intermediate {
    /// Whether this intermediate is installed and must be booted before continuing the upgrade.
    /// See `bootRequired()` in `bitbox-wallet-app/backend/devices/bitbox02bootloader/firmware.go`.
    pub fn boot_required(
        &self,
        current_firmware_version: u32,
        bootloader_version: Version,
    ) -> bool {
        if current_firmware_version != self.monotonic_version {
            return false;
        }
        match self.completion {
            IntermediateCompletion::MonotonicVersionBump => true,
            IntermediateCompletion::BootloaderVersion(v) => bootloader_version < v,
        }
    }

    /// Download the intermediate firmware from GitHub, verifying the pinned hash.
    pub fn download(&self, product: Product) -> Result<SignedFirmware, ReleaseError> {
        let gh = fetch_release_by_tag(self.tag)?;
        let asset = gh
            .assets
            .iter()
            .find(|a| a.name == self.asset_name)
            .ok_or_else(|| {
                ReleaseError::Mismatch(format!(
                    "asset {} not found in release {}",
                    self.asset_name, self.tag
                ))
            })?;
        let release = FirmwareRelease {
            product,
            version: self.version,
            tag: self.tag.to_string(),
            asset_name: asset.name.clone(),
            url: asset.browser_download_url.clone(),
            published_sighash: None,
        };
        let firmware = download(&release)?;
        self.check(&firmware)?;
        Ok(firmware)
    }

    /// Check a firmware is this intermediate.
    pub fn check(&self, firmware: &SignedFirmware) -> Result<(), ReleaseError> {
        if hex::encode(firmware.unsigned_hash()) != self.unsigned_sha256
            || firmware.firmware_version() != self.monotonic_version
        {
            return Err(ReleaseError::Mismatch(format!(
                "{} does not match the pinned v{} intermediate firmware",
                self.asset_name, self.version
            )));
        }
        Ok(())
    }
}

const V9_17_1: Version = Version::new(9, 17, 1);
const V9_26_2: Version = Version::new(9, 26, 2);
const BOOTLOADER_1_2_2: Version = Version::new(1, 2, 2);

/// Required intermediate upgrades for a product, ordered by version. Mirrors `bundledFirmwares`
/// in `bitbox-wallet-app/backend/devices/bitbox02bootloader/firmware.go`.
pub fn intermediates(product: Product) -> &'static [Intermediate] {
    const BB02_MULTI: &[Intermediate] = &[
        Intermediate {
            version: V9_17_1,
            monotonic_version: 36,
            completion: IntermediateCompletion::MonotonicVersionBump,
            tag: "firmware/v9.17.1",
            asset_name: "firmware.v9.17.1.signed.bin",
            unsigned_sha256: "73bcd846f03691ecb6924ca4a47aaf2ca10be3a6201a6cc53ee3e24984aa8e29",
        },
        Intermediate {
            version: V9_26_2,
            monotonic_version: 50,
            completion: IntermediateCompletion::BootloaderVersion(BOOTLOADER_1_2_2),
            tag: "firmware/v9.26.2",
            asset_name: "firmware-bitbox02-multi.v9.26.2.signed.bin",
            unsigned_sha256: "ab311b65ff68053420c4459980a1076bb21318a7b556e96704e8206fa411d30c",
        },
    ];
    const BB02_BTCONLY: &[Intermediate] = &[
        Intermediate {
            version: V9_17_1,
            monotonic_version: 36,
            completion: IntermediateCompletion::MonotonicVersionBump,
            tag: "firmware-btc-only/v9.17.1",
            asset_name: "firmware-btc.v9.17.1.signed.bin",
            unsigned_sha256: "685ee47afaae0e9ad01bb22c40555040b0ab308cd0b0623a84fc2d8eb95e26a6",
        },
        Intermediate {
            version: V9_26_2,
            monotonic_version: 50,
            completion: IntermediateCompletion::BootloaderVersion(BOOTLOADER_1_2_2),
            tag: "firmware/v9.26.2",
            asset_name: "firmware-bitbox02-btconly.v9.26.2.signed.bin",
            unsigned_sha256: "25dee90b71e95fa38d9eda304d65b35a2eb8d75d6362ccbfcc718441b1a6a55f",
        },
    ];
    const NOVA_MULTI: &[Intermediate] = &[Intermediate {
        version: V9_26_2,
        monotonic_version: 50,
        completion: IntermediateCompletion::BootloaderVersion(BOOTLOADER_1_2_2),
        tag: "firmware/v9.26.2",
        asset_name: "firmware-bitbox02nova-multi.v9.26.2.signed.bin",
        unsigned_sha256: "a3d4539bd3ef341e725fb3e25496328737decbfedc060f189b2d4242911b0dbd",
    }];
    const NOVA_BTCONLY: &[Intermediate] = &[Intermediate {
        version: V9_26_2,
        monotonic_version: 50,
        completion: IntermediateCompletion::BootloaderVersion(BOOTLOADER_1_2_2),
        tag: "firmware/v9.26.2",
        asset_name: "firmware-bitbox02nova-btconly.v9.26.2.signed.bin",
        unsigned_sha256: "3f38bf0fc6f4766044a6f86ad6fae7cf52fbf825d59a9e7afbe17e795348e68c",
    }];
    match product {
        Product::BitBox02Multi => BB02_MULTI,
        Product::BitBox02BtcOnly => BB02_BTCONLY,
        Product::BitBox02NovaMulti => NOVA_MULTI,
        Product::BitBox02NovaBtcOnly => NOVA_BTCONLY,
    }
}

/// What to do next on a device in bootloader mode, given its monotonic firmware version and its
/// bootloader version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextStep {
    /// An intermediate firmware is installed and must be booted first.
    BootIntermediate(Intermediate),
    /// Install this intermediate firmware first.
    InstallIntermediate(Intermediate),
    /// Install the target firmware.
    InstallTarget,
}

/// Mirrors `firmwareBootRequired()` and `nextFirmware()` in
/// `bitbox-wallet-app/backend/devices/bitbox02bootloader/device.go` and `firmware.go`.
/// `target_firmware_version` is the monotonic version of the firmware we want to install.
pub fn next_step(
    product: Product,
    current_firmware_version: u32,
    bootloader_version: Version,
    target_firmware_version: u32,
    skip: &[u32],
) -> NextStep {
    let intermediates = intermediates(product);
    // Only consider intermediates older than the target, and not skipped (identified by their
    // monotonic version).
    let relevant = intermediates
        .iter()
        .filter(|i| i.monotonic_version < target_firmware_version)
        .filter(|i| !skip.contains(&i.monotonic_version));
    for i in relevant.clone() {
        if i.boot_required(current_firmware_version, bootloader_version) {
            return NextStep::BootIntermediate(*i);
        }
    }
    for i in relevant {
        if i.monotonic_version > current_firmware_version {
            return NextStep::InstallIntermediate(*i);
        }
    }
    NextStep::InstallTarget
}

/// Adjust the step returned by [`next_step`] for a device whose firmware area is erased (for
/// instance because a previous flash was interrupted). There is no intermediate firmware to boot
/// then, even though the bootloader still reports its monotonic version: booting would only bring
/// the device back to the bootloader, so install the intermediate firmware again instead.
pub fn adjust_for_erased(step: NextStep, erased: bool) -> NextStep {
    match step {
        NextStep::BootIntermediate(i) if erased => NextStep::InstallIntermediate(i),
        step => step,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASES_FIXTURE: &str = include_str!("../tests/data/releases.json");

    #[test]
    fn asset_names() {
        use Product::*;
        assert_eq!(
            asset_version(
                BitBox02BtcOnly,
                "firmware-bitbox02-btconly.v9.27.1.signed.bin"
            ),
            Some(Version::new(9, 27, 1))
        );
        assert_eq!(
            asset_version(BitBox02BtcOnly, "firmware-btc.v9.17.1.signed.bin"),
            Some(Version::new(9, 17, 1))
        );
        assert_eq!(
            asset_version(BitBox02Multi, "firmware.v9.17.1.signed.bin"),
            Some(Version::new(9, 17, 1))
        );
        assert_eq!(
            asset_version(BitBox02Multi, "firmware-btc.v9.17.1.signed.bin"),
            None
        );
        assert_eq!(
            asset_version(
                BitBox02Multi,
                "firmware-bitbox02-btconly.v9.27.1.signed.bin"
            ),
            None
        );
        assert_eq!(
            asset_version(
                BitBox02NovaMulti,
                "firmware-bitbox02nova-multi.v9.27.1.signed.bin"
            ),
            Some(Version::new(9, 27, 1))
        );
        assert_eq!(
            asset_version(
                BitBox02NovaMulti,
                "firmware-bitbox02-multi.v9.27.1.signed.bin"
            ),
            None
        );
        assert_eq!(
            asset_version(
                BitBox02BtcOnly,
                "firmware-bitbox02-btconly.v9.27.1.signed.bin.asc"
            ),
            None
        );
        assert_eq!(
            asset_version(
                BitBox02BtcOnly,
                "bitbox02-multi-v9.27.1-simulator1.0.0-linux-amd64"
            ),
            None
        );
    }

    #[test]
    fn published_hash() {
        let body = "**Verify the hash shown by the BitBox02:**\r\n\r\nThe hash of the firmware as verified/shown by the BitBox02 at startup is:\r\n\r\n- BitBox02 Bitcoin-only: `2522156c870b430c9ffcbc768895bbca24148b922945582f749f1a73438abc73`\r\n- BitBox02 Nova Bitcoin-only: `f48bb49f4af9ae7b845f666332bafbdc0e1a80d65ccb25e44d9419fb02a2acb4`\r\n";
        assert_eq!(
            parse_published_sighash(Product::BitBox02BtcOnly, body).map(hex::encode),
            Some("2522156c870b430c9ffcbc768895bbca24148b922945582f749f1a73438abc73".to_string())
        );
        assert_eq!(
            parse_published_sighash(Product::BitBox02NovaBtcOnly, body).map(hex::encode),
            Some("f48bb49f4af9ae7b845f666332bafbdc0e1a80d65ccb25e44d9419fb02a2acb4".to_string())
        );
        assert_eq!(parse_published_sighash(Product::BitBox02Multi, body), None);
        assert_eq!(
            parse_published_sighash(Product::BitBox02Multi, "- BitBox02 Multi: `abcd`"),
            None
        );
    }

    #[test]
    fn parse_github_releases() {
        let releases = parse_releases(RELEASES_FIXTURE).unwrap();
        assert_eq!(releases.len(), 6);

        let btc = firmware_releases(Product::BitBox02BtcOnly, &releases);
        let versions: Vec<String> = btc.iter().map(|r| r.version.to_string()).collect();
        // 9.28.0 is a draft and 9.28.0-rc a pre-release: both ignored.
        assert_eq!(versions, vec!["9.27.1", "9.26.2", "9.24.0"]);
        assert_eq!(btc[0].tag, "firmware/v9.27.1");
        assert_eq!(
            btc[0].url,
            "https://github.com/BitBoxSwiss/bitbox02-firmware/releases/download/firmware%2Fv9.27.1/firmware-bitbox02-btconly.v9.27.1.signed.bin"
        );
        assert_eq!(
            btc[0].published_sighash.map(hex::encode).as_deref(),
            Some("2522156c870b430c9ffcbc768895bbca24148b922945582f749f1a73438abc73")
        );
        assert_eq!(btc[2].tag, "firmware-btc-only/v9.24.0");
        assert_eq!(btc[2].published_sighash, None);

        let multi = firmware_releases(Product::BitBox02Multi, &releases);
        let versions: Vec<String> = multi.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(versions, vec!["9.27.1", "9.26.2", "9.24.0"]);
        assert_eq!(multi[2].asset_name, "firmware.v9.24.0.signed.bin");

        let nova = firmware_releases(Product::BitBox02NovaBtcOnly, &releases);
        let versions: Vec<String> = nova.iter().map(|r| r.version.to_string()).collect();
        assert_eq!(versions, vec!["9.27.1", "9.26.2"]);

        assert!(parse_releases("{\"not\": \"a list\"}").is_err());
        assert!(parse_releases("[]").unwrap().is_empty());
    }

    #[test]
    fn intermediates_table() {
        for p in Product::ALL {
            let list = intermediates(p);
            assert!(list
                .windows(2)
                .all(|w| w[0].monotonic_version < w[1].monotonic_version));
            for i in list {
                assert_eq!(
                    asset_version(p, i.asset_name),
                    Some(i.version),
                    "{}",
                    i.asset_name
                );
                assert_eq!(i.unsigned_sha256.len(), 64);
            }
        }
    }

    #[test]
    fn intermediate_check_with_fixture() {
        use std::io::Read;
        let gz: &[u8] =
            include_bytes!("../tests/data/firmware-bitbox02-btconly.v9.26.2.signed.bin.gz");
        let mut bin = Vec::new();
        flate2::read::GzDecoder::new(gz)
            .read_to_end(&mut bin)
            .unwrap();
        let fw = SignedFirmware::parse(&bin).unwrap();
        let i = intermediates(Product::BitBox02BtcOnly)[1];
        assert!(i.check(&fw).is_ok());
        assert!(intermediates(Product::BitBox02BtcOnly)[0]
            .check(&fw)
            .is_err());
        assert!(intermediates(Product::BitBox02Multi)[1].check(&fw).is_err());
    }

    #[test]
    fn upgrade_path() {
        use Product::*;
        let bl_old = Version::new(1, 0, 6);
        let bl_new = Version::new(1, 2, 2);
        let latest = 55;
        // Tests mirroring TestNextFirmware / TestIntermediate* in the BitBoxApp firmware_test.go.
        let step = |p, cur, bl| next_step(p, cur, bl, latest, &[]);
        for p in [BitBox02Multi, BitBox02BtcOnly] {
            assert!(
                matches!(step(p, 1, bl_old), NextStep::InstallIntermediate(i) if i.monotonic_version == 36)
            );
            assert!(
                matches!(step(p, 36, bl_old), NextStep::BootIntermediate(i) if i.monotonic_version == 36)
            );
            assert!(
                matches!(step(p, 37, bl_old), NextStep::InstallIntermediate(i) if i.monotonic_version == 50)
            );
            assert!(
                matches!(step(p, 50, bl_old), NextStep::BootIntermediate(i) if i.monotonic_version == 50)
            );
            assert_eq!(step(p, 50, bl_new), NextStep::InstallTarget);
            assert_eq!(step(p, 55, bl_new), NextStep::InstallTarget);
            // Target is itself older than the intermediates: no intermediate.
            assert_eq!(next_step(p, 1, bl_old, 30, &[]), NextStep::InstallTarget);
            // Target is the 9.17.1 intermediate itself.
            assert_eq!(next_step(p, 1, bl_old, 36, &[]), NextStep::InstallTarget);
        }
        for p in [BitBox02NovaMulti, BitBox02NovaBtcOnly] {
            assert!(
                matches!(step(p, 1, bl_old), NextStep::InstallIntermediate(i) if i.monotonic_version == 50)
            );
            assert!(matches!(
                step(p, 50, Version::new(1, 1, 9)),
                NextStep::BootIntermediate(_)
            ));
            assert_eq!(step(p, 50, bl_new), NextStep::InstallTarget);
        }
        // An erased device can't boot the intermediate firmware, it must be installed again.
        for p in [BitBox02Multi, BitBox02BtcOnly] {
            assert!(matches!(
                adjust_for_erased(step(p, 36, bl_old), true),
                NextStep::InstallIntermediate(i) if i.monotonic_version == 36
            ));
            assert!(matches!(
                adjust_for_erased(step(p, 50, bl_old), true),
                NextStep::InstallIntermediate(i) if i.monotonic_version == 50
            ));
            assert!(matches!(
                adjust_for_erased(step(p, 36, bl_old), false),
                NextStep::BootIntermediate(i) if i.monotonic_version == 36
            ));
            assert!(matches!(
                adjust_for_erased(step(p, 1, bl_old), true),
                NextStep::InstallIntermediate(i) if i.monotonic_version == 36
            ));
            assert_eq!(
                adjust_for_erased(step(p, 55, bl_new), true),
                NextStep::InstallTarget
            );
        }
        let i = intermediates(BitBox02BtcOnly)[0];
        assert!(i.boot_required(36, Version::new(9, 9, 9)));
        assert!(!i.boot_required(37, Version::new(0, 0, 0)));
        let i = intermediates(BitBox02BtcOnly)[1];
        assert!(i.boot_required(50, Version::new(1, 1, 9)));
        assert!(!i.boot_required(50, Version::new(1, 2, 2)));
        assert!(!i.boot_required(51, Version::new(1, 1, 9)));
    }

    // A development bootloader refuses the bootloader upgrade of v9.26.2: the device stays on its
    // old bootloader with v9.26.2 (monotonic version 50) installed, which is only an installer.
    // Once that intermediate is skipped, the target is installed directly over it.
    #[test]
    fn skip_refused_bootloader_upgrade() {
        let bl_dev = Version::new(1, 0, 5);
        for p in [Product::BitBox02Multi, Product::BitBox02BtcOnly] {
            assert!(matches!(
                next_step(p, 50, bl_dev, 55, &[]),
                NextStep::BootIntermediate(i) if i.monotonic_version == 50
            ));
            assert_eq!(next_step(p, 50, bl_dev, 55, &[50]), NextStep::InstallTarget);
            // An older device still needs v9.17.1 first, and v9.26.2 isn't installed again once
            // skipped.
            assert!(matches!(
                next_step(p, 30, bl_dev, 55, &[50]),
                NextStep::InstallIntermediate(i) if i.monotonic_version == 36
            ));
            assert_eq!(next_step(p, 37, bl_dev, 55, &[50]), NextStep::InstallTarget);
        }
    }
}
