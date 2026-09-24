//! Discovery and download of the official signed firmware releases, and the intermediate upgrades.
//!
//! Releases are published on <https://github.com/BitBoxSwiss/bitbox02-firmware/releases>, see
//! [`Product::release_asset_prefixes`] for the asset names.
//!
//! Upgrading from old firmwares requires installing and booting intermediate firmwares first (see
//! `bitbox-wallet-app/backend/devices/bitbox02bootloader/firmware.go` and the release notes
//! template in `bitbox02-firmware/scripts/create_release.py`). Their unsigned binary hashes are
//! pinned here.

use crate::{signed_firmware::SignedFirmware, Error, Product, Version};

const RELEASES_API_URL: &str =
    "https://api.github.com/repos/BitBoxSwiss/bitbox02-firmware/releases";
const USER_AGENT: &str = concat!("bacca-bitbox_manager/", env!("CARGO_PKG_VERSION"));
const HTTP_TIMEOUT_SECS: u64 = 120;
/// A signed firmware is at most 4 + 584 + 884736 bytes.
const MAX_DOWNLOAD_SIZE: usize = 2 * 1024 * 1024;

/// A release, as returned by the GitHub releases API (only the fields we use).
#[derive(serde::Deserialize)]
struct GithubRelease {
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(serde::Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

/// A firmware release asset for a given product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareRelease {
    pub product: Product,
    pub version: Version,
    pub asset_name: String,
    pub url: String,
    /// The firmware hash as published in the release notes, if found.
    pub published_sighash: Option<[u8; 32]>,
}

/// The version of an asset named `<prefix>.vX.Y.Z.signed.bin`, if the prefix is one of the names
/// used for `product`.
fn asset_version(product: Product, asset_name: &str) -> Option<Version> {
    let rest = asset_name.strip_suffix(".signed.bin")?;
    product
        .release_asset_prefixes()
        .iter()
        .find_map(|prefix| Version::parse(rest.strip_prefix(prefix)?.strip_prefix(".v")?))
}

/// The firmware hash published for `product` in the release notes, in the format of
/// `render_release_notes()` of `create_release.py`: ``- BitBox02 Bitcoin-only: `<hex>` ``.
fn parse_published_sighash(product: Product, body: &str) -> Option<[u8; 32]> {
    let marker = format!("- {}: `", product);
    body.lines().find_map(|line| {
        let hex_str = line.trim().strip_prefix(&marker)?.split('`').next()?;
        hex::decode(hex_str.trim()).ok()?.try_into().ok()
    })
}

/// The newest firmware for `product` in a list of releases (drafts and pre-releases are ignored).
fn latest_in(product: Product, releases: &[GithubRelease]) -> Option<FirmwareRelease> {
    let mut found: Vec<FirmwareRelease> = releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        .flat_map(|r| {
            r.assets.iter().filter_map(move |a| {
                Some(FirmwareRelease {
                    product,
                    version: asset_version(product, &a.name)?,
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
    found.sort_by(|a, b| b.version.cmp(&a.version));
    found.into_iter().next()
}

fn get(url: &str) -> Result<minreq::Response, Error> {
    log::debug!("GET {}", url);
    let response = minreq::get(url)
        .with_header("User-Agent", USER_AGENT)
        .with_timeout(HTTP_TIMEOUT_SECS)
        .with_max_redirects(10)
        .send()?;
    if response.status_code != 200 {
        return Err(Error::Other(format!(
            "HTTP error {} when fetching {}",
            response.status_code, url
        )));
    }
    Ok(response)
}

fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T, Error> {
    let response = get(url)?;
    let parse_err = |e: &dyn std::fmt::Display| {
        Error::Other(format!("could not parse the GitHub releases: {}", e))
    };
    let body = response.as_str().map_err(|e| parse_err(&e))?;
    serde_json::from_str(body).map_err(|e| parse_err(&e))
}

/// Find the latest firmware release for this product, among the last 100 releases.
pub(crate) fn latest_release(product: Product) -> Result<FirmwareRelease, Error> {
    let releases: Vec<GithubRelease> = get_json(&format!("{}?per_page=100", RELEASES_API_URL))?;
    latest_in(product, &releases)
        .ok_or_else(|| Error::Other(format!("no firmware release found for {}", product)))
}

/// Download a release asset and check it is a valid signed firmware for the expected product,
/// matching the published hash if any.
pub fn download(release: &FirmwareRelease) -> Result<SignedFirmware, Error> {
    let response = get(&release.url)?;
    let mismatch = |s: String| Error::Other(format!("downloaded firmware mismatch: {}", s));
    let bytes = response.as_bytes();
    if bytes.len() > MAX_DOWNLOAD_SIZE {
        return Err(mismatch(format!("asset too big ({} bytes)", bytes.len())));
    }
    let firmware = SignedFirmware::parse(bytes)
        .map_err(|e| Error::Other(format!("invalid downloaded firmware: {}", e)))?;
    if firmware.product() != release.product {
        return Err(mismatch(format!(
            "{} is a firmware for {}, expected {}",
            release.asset_name,
            firmware.product(),
            release.product
        )));
    }
    if let Some(expected) = release.published_sighash {
        if firmware.published_sighash() != expected {
            return Err(mismatch(format!(
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
pub(crate) enum IntermediateCompletion {
    /// The firmware bumps its monotonic version by one when it first boots.
    MonotonicVersionBump,
    /// The firmware upgrades the bootloader to at least this version when it first boots.
    BootloaderVersion(Version),
}

/// An intermediate firmware that must be installed and booted before upgrading further.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Intermediate {
    pub version: Version,
    pub monotonic_version: u32,
    pub completion: IntermediateCompletion,
    tag: &'static str,
    asset_name: &'static str,
    /// sha256 of the unsigned firmware binary, from the reproducible build assertions in
    /// `bitbox02-firmware/releases/firmware[-btc]-v<version>/assertion*.txt` (also in the
    /// `.sha256` files next to the binaries bundled by the BitBoxApp).
    unsigned_sha256: &'static str,
}

impl Intermediate {
    /// Whether this intermediate is installed and must be booted before continuing the upgrade.
    /// See `bootRequired()` in `bitbox-wallet-app/backend/devices/bitbox02bootloader/firmware.go`.
    fn boot_required(&self, current_firmware_version: u32, bootloader_version: Version) -> bool {
        current_firmware_version == self.monotonic_version
            && match self.completion {
                IntermediateCompletion::MonotonicVersionBump => true,
                IntermediateCompletion::BootloaderVersion(v) => bootloader_version < v,
            }
    }

    /// Download the intermediate firmware from GitHub, verifying the pinned hash.
    pub(crate) fn download(&self, product: Product) -> Result<SignedFirmware, Error> {
        let url = format!("{}/tags/{}", RELEASES_API_URL, self.tag.replace('/', "%2F"));
        let release: GithubRelease = get_json(&url)?;
        let asset = release
            .assets
            .iter()
            .find(|a| a.name == self.asset_name)
            .ok_or_else(|| {
                Error::Other(format!(
                    "asset {} not found in release {}",
                    self.asset_name, self.tag
                ))
            })?;
        let firmware = download(&FirmwareRelease {
            product,
            version: self.version,
            asset_name: asset.name.clone(),
            url: asset.browser_download_url.clone(),
            published_sighash: None,
        })?;
        self.check(&firmware)?;
        Ok(firmware)
    }

    fn check(&self, firmware: &SignedFirmware) -> Result<(), Error> {
        if hex::encode(firmware.unsigned_hash()) != self.unsigned_sha256
            || firmware.firmware_version() != self.monotonic_version
        {
            return Err(Error::Other(format!(
                "{} does not match the pinned v{} intermediate firmware",
                self.asset_name, self.version
            )));
        }
        Ok(())
    }
}

/// v9.17.1 bumps the monotonic version when first booted.
const fn v9_17_1(tag: &'static str, asset_name: &'static str, sha: &'static str) -> Intermediate {
    Intermediate {
        version: Version::new(9, 17, 1),
        monotonic_version: 36,
        completion: IntermediateCompletion::MonotonicVersionBump,
        tag,
        asset_name,
        unsigned_sha256: sha,
    }
}

/// v9.26.2 upgrades the bootloader to v1.2.2 when first booted.
const fn v9_26_2(asset_name: &'static str, sha: &'static str) -> Intermediate {
    Intermediate {
        version: Version::new(9, 26, 2),
        monotonic_version: 50,
        completion: IntermediateCompletion::BootloaderVersion(Version::new(1, 2, 2)),
        tag: "firmware/v9.26.2",
        asset_name,
        unsigned_sha256: sha,
    }
}

/// Required intermediate upgrades for a product, ordered by version. Mirrors `bundledFirmwares`
/// in `bitbox-wallet-app/backend/devices/bitbox02bootloader/firmware.go`.
fn intermediates(product: Product) -> Vec<Intermediate> {
    match product {
        Product::BitBox02Multi => vec![
            v9_17_1(
                "firmware/v9.17.1",
                "firmware.v9.17.1.signed.bin",
                "73bcd846f03691ecb6924ca4a47aaf2ca10be3a6201a6cc53ee3e24984aa8e29",
            ),
            v9_26_2(
                "firmware-bitbox02-multi.v9.26.2.signed.bin",
                "ab311b65ff68053420c4459980a1076bb21318a7b556e96704e8206fa411d30c",
            ),
        ],
        Product::BitBox02BtcOnly => vec![
            v9_17_1(
                "firmware-btc-only/v9.17.1",
                "firmware-btc.v9.17.1.signed.bin",
                "685ee47afaae0e9ad01bb22c40555040b0ab308cd0b0623a84fc2d8eb95e26a6",
            ),
            v9_26_2(
                "firmware-bitbox02-btconly.v9.26.2.signed.bin",
                "25dee90b71e95fa38d9eda304d65b35a2eb8d75d6362ccbfcc718441b1a6a55f",
            ),
        ],
        Product::BitBox02NovaMulti => vec![v9_26_2(
            "firmware-bitbox02nova-multi.v9.26.2.signed.bin",
            "a3d4539bd3ef341e725fb3e25496328737decbfedc060f189b2d4242911b0dbd",
        )],
        Product::BitBox02NovaBtcOnly => vec![v9_26_2(
            "firmware-bitbox02nova-btconly.v9.26.2.signed.bin",
            "3f38bf0fc6f4766044a6f86ad6fae7cf52fbf825d59a9e7afbe17e795348e68c",
        )],
    }
}

/// What to do next on a device in bootloader mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NextStep {
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
///
/// If the firmware area is `erased` (e.g. a previous flash was interrupted), there is no
/// intermediate firmware to boot even though the bootloader still reports its monotonic version:
/// booting would only bring the device back to the bootloader, so it is installed again instead.
pub(crate) fn next_step(
    product: Product,
    current_firmware_version: u32,
    bootloader_version: Version,
    target_firmware_version: u32,
    erased: bool,
) -> NextStep {
    let relevant: Vec<Intermediate> = intermediates(product)
        .into_iter()
        .filter(|i| i.monotonic_version < target_firmware_version)
        .collect();
    if let Some(i) = relevant
        .iter()
        .find(|i| i.boot_required(current_firmware_version, bootloader_version))
    {
        return if erased {
            NextStep::InstallIntermediate(*i)
        } else {
            NextStep::BootIntermediate(*i)
        };
    }
    match relevant
        .iter()
        .find(|i| i.monotonic_version > current_firmware_version)
    {
        Some(i) => NextStep::InstallIntermediate(*i),
        None => NextStep::InstallTarget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Product::*;

    #[test]
    fn asset_names() {
        let v = |p, name| asset_version(p, name).map(|v| v.to_string());
        let v9_27_1 = Some("9.27.1".to_string());
        assert_eq!(
            v(
                BitBox02BtcOnly,
                "firmware-bitbox02-btconly.v9.27.1.signed.bin"
            ),
            v9_27_1
        );
        assert_eq!(
            v(
                BitBox02NovaMulti,
                "firmware-bitbox02nova-multi.v9.27.1.signed.bin"
            ),
            v9_27_1
        );
        assert_eq!(
            v(BitBox02BtcOnly, "firmware-btc.v9.17.1.signed.bin"),
            Some("9.17.1".into())
        );
        assert_eq!(
            v(BitBox02Multi, "firmware.v9.17.1.signed.bin"),
            Some("9.17.1".into())
        );
        assert_eq!(v(BitBox02Multi, "firmware-btc.v9.17.1.signed.bin"), None);
        assert_eq!(
            v(
                BitBox02Multi,
                "firmware-bitbox02-btconly.v9.27.1.signed.bin"
            ),
            None
        );
        assert_eq!(
            v(
                BitBox02NovaMulti,
                "firmware-bitbox02-multi.v9.27.1.signed.bin"
            ),
            None
        );
        assert_eq!(
            v(
                BitBox02BtcOnly,
                "firmware-bitbox02-btconly.v9.27.1.signed.bin.asc"
            ),
            None
        );
    }

    #[test]
    fn published_hash() {
        let body = "**Verify the hash shown by the BitBox02:**\r\n\r\nThe hash of the firmware as verified/shown by the BitBox02 at startup is:\r\n\r\n- BitBox02 Bitcoin-only: `2522156c870b430c9ffcbc768895bbca24148b922945582f749f1a73438abc73`\r\n- BitBox02 Nova Bitcoin-only: `f48bb49f4af9ae7b845f666332bafbdc0e1a80d65ccb25e44d9419fb02a2acb4`\r\n";
        assert_eq!(
            parse_published_sighash(BitBox02NovaBtcOnly, body).map(hex::encode),
            Some("f48bb49f4af9ae7b845f666332bafbdc0e1a80d65ccb25e44d9419fb02a2acb4".to_string())
        );
        assert_eq!(parse_published_sighash(BitBox02Multi, body), None);
        assert_eq!(
            parse_published_sighash(BitBox02Multi, "- BitBox02 Multi: `abcd`"),
            None
        );
    }

    #[test]
    fn parse_github_releases() {
        let releases: Vec<GithubRelease> =
            serde_json::from_str(include_str!("../tests/data/releases.json")).unwrap();
        // 9.28.0 is a draft and 9.28.0-rc a pre-release: both ignored.
        let btc = latest_in(BitBox02BtcOnly, &releases).unwrap();
        assert_eq!(btc.version, Version::new(9, 27, 1));
        assert_eq!(
            btc.url,
            "https://github.com/BitBoxSwiss/bitbox02-firmware/releases/download/firmware%2Fv9.27.1/firmware-bitbox02-btconly.v9.27.1.signed.bin"
        );
        assert_eq!(
            btc.published_sighash.map(hex::encode).as_deref(),
            Some("2522156c870b430c9ffcbc768895bbca24148b922945582f749f1a73438abc73")
        );
        for p in [BitBox02Multi, BitBox02NovaMulti, BitBox02NovaBtcOnly] {
            assert_eq!(
                latest_in(p, &releases).unwrap().version,
                Version::new(9, 27, 1)
            );
        }
        // Only a legacy (until v9.24.0) release.
        let legacy: Vec<GithubRelease> = serde_json::from_str(
            r#"[{"tag_name": "firmware-btc-only/v9.24.0", "assets": [{"name": "firmware-btc.v9.24.0.signed.bin", "browser_download_url": "u"}]}]"#,
        )
        .unwrap();
        assert_eq!(
            latest_in(BitBox02BtcOnly, &legacy).unwrap().version,
            Version::new(9, 24, 0)
        );
        assert_eq!(latest_in(BitBox02Multi, &legacy), None);
    }

    #[test]
    fn intermediates_table() {
        for p in [
            BitBox02Multi,
            BitBox02BtcOnly,
            BitBox02NovaMulti,
            BitBox02NovaBtcOnly,
        ] {
            for i in intermediates(p) {
                assert_eq!(
                    asset_version(p, i.asset_name),
                    Some(i.version),
                    "{}",
                    i.asset_name
                );
            }
        }
        // The pinned hash against the official firmware.
        let fw = SignedFirmware::parse(&crate::signed_firmware::tests::fixture()).unwrap();
        assert!(intermediates(BitBox02BtcOnly)[1].check(&fw).is_ok());
        assert!(intermediates(BitBox02BtcOnly)[0].check(&fw).is_err());
        assert!(intermediates(BitBox02Multi)[1].check(&fw).is_err());
    }

    #[test]
    fn upgrade_path() {
        let bl_old = Version::new(1, 0, 6);
        let bl_new = Version::new(1, 2, 2);
        // Mirroring TestNextFirmware / TestIntermediate* in the BitBoxApp firmware_test.go.
        let step = |p, cur, bl| next_step(p, cur, bl, 55, false);
        let install =
            |v| move |s| matches!(s, NextStep::InstallIntermediate(i) if i.monotonic_version == v);
        let boot =
            |v| move |s| matches!(s, NextStep::BootIntermediate(i) if i.monotonic_version == v);
        for p in [BitBox02Multi, BitBox02BtcOnly] {
            assert!(install(36)(step(p, 1, bl_old)));
            assert!(boot(36)(step(p, 36, bl_old)));
            assert!(install(50)(step(p, 37, bl_old)));
            assert!(boot(50)(step(p, 50, bl_old)));
            assert_eq!(step(p, 50, bl_new), NextStep::InstallTarget);
            assert_eq!(step(p, 55, bl_new), NextStep::InstallTarget);
            // The target is older than the intermediates, or is the 9.17.1 intermediate itself.
            assert_eq!(next_step(p, 1, bl_old, 30, false), NextStep::InstallTarget);
            assert_eq!(next_step(p, 1, bl_old, 36, false), NextStep::InstallTarget);
            // An erased device can't boot the intermediate firmware, it must be installed again.
            assert!(install(36)(next_step(p, 36, bl_old, 55, true)));
            assert!(install(50)(next_step(p, 50, bl_old, 55, true)));
            assert!(install(36)(next_step(p, 1, bl_old, 55, true)));
            assert_eq!(next_step(p, 55, bl_new, 55, true), NextStep::InstallTarget);
        }
        for p in [BitBox02NovaMulti, BitBox02NovaBtcOnly] {
            assert!(install(50)(step(p, 1, bl_old)));
            assert!(boot(50)(step(p, 50, Version::new(1, 1, 9))));
            assert_eq!(step(p, 50, bl_new), NextStep::InstallTarget);
        }
    }
}
