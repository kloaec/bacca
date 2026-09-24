//! The firmware server: <https://jadefw.blockstream.com/bin/<hw_target>/index.json> lists the
//! firmwares of each hardware target, see `download_file()` and `get_fw_metadata()` in
//! `update_jade_fw.py`.
//!
//! Only the full firmwares of the "stable" channel are used (no beta, no delta patches), with the
//! same config (Bluetooth or not) as the one installed.

use sha2::{Digest, Sha256};

use crate::{Error, Version};

const FWSERVER_URL_ROOT: &str = "https://jadefw.blockstream.com/bin";
pub(crate) const USER_AGENT: &str = concat!("bacca-jade_manager/", env!("CARGO_PKG_VERSION"));
const HTTP_TIMEOUT_SECS: u64 = 120;
/// Compressed firmwares are about 1MB, the index files about 100kB.
const MAX_DOWNLOAD_SIZE: usize = 8 * 1024 * 1024;

/// A firmware from the index (only the fields we use).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
struct IndexEntry {
    filename: String,
    version: String,
    config: String,
    fwsize: u32,
    cmphash: String,
    fwhash: String,
}

#[derive(Debug, serde::Deserialize)]
struct Channel {
    #[serde(default)]
    full: Vec<IndexEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct Index {
    stable: Option<Channel>,
}

/// A full firmware available on the firmware server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FirmwareRelease {
    pub hw_target: String,
    pub version: Version,
    /// "ble" or "noradio".
    pub config: String,
    pub url: String,
    /// Size of the uncompressed firmware.
    pub fwsize: u32,
    /// sha256 of the downloaded (compressed) file.
    pub cmphash: [u8; 32],
    /// sha256 of the uncompressed firmware: shown by the device when asking to confirm the
    /// update, and checked by the device before booting it.
    pub fwhash: [u8; 32],
}

fn hash_from_hex(s: &str) -> Option<[u8; 32]> {
    hex::decode(s).ok()?.try_into().ok()
}

/// The newest stable full firmware with this config in an index file.
fn latest_in(hw_target: &str, config: &str, index_json: &str) -> Result<FirmwareRelease, Error> {
    let index: Index = serde_json::from_str(index_json)
        .map_err(|e| Error::Other(format!("could not parse the firmware index: {}", e)))?;
    let entries = index.stable.map(|c| c.full).unwrap_or_default();
    let mut found: Vec<FirmwareRelease> = entries
        .into_iter()
        // Full firmwares are at the root of the hw target directory (deltas are in "deltas/").
        .filter(|e| e.config.eq_ignore_ascii_case(config) && !e.filename.contains(['/', '\\']))
        .filter_map(|e| {
            Some(FirmwareRelease {
                hw_target: hw_target.to_string(),
                version: Version::parse(&e.version)?,
                config: e.config.to_lowercase(),
                url: format!("{}/{}/{}", FWSERVER_URL_ROOT, hw_target, e.filename),
                fwsize: e.fwsize,
                cmphash: hash_from_hex(&e.cmphash)?,
                fwhash: hash_from_hex(&e.fwhash)?,
            })
        })
        .collect();
    found.sort_by(|a, b| b.version.cmp(&a.version));
    found.into_iter().next().ok_or_else(|| {
        Error::Other(format!(
            "no stable firmware with config '{}' found for {}",
            config, hw_target
        ))
    })
}

fn get(url: &str) -> Result<Vec<u8>, Error> {
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
    let bytes = response.into_bytes();
    if bytes.len() > MAX_DOWNLOAD_SIZE {
        return Err(Error::Other(format!("{} is too big", url)));
    }
    Ok(bytes)
}

/// Fetch the index of this hardware target and return its latest stable full firmware with this
/// config ("ble" or "noradio").
pub fn latest_release(hw_target: &str, config: &str) -> Result<FirmwareRelease, Error> {
    let index = get(&format!("{}/{}/index.json", FWSERVER_URL_ROOT, hw_target))?;
    let index = String::from_utf8(index)
        .map_err(|_| Error::Other("the firmware index is not UTF-8".into()))?;
    latest_in(hw_target, config, &index)
}

/// Download a compressed firmware and check its hash against the index (as `download_file()` in
/// `update_jade_fw.py`).
pub fn download(release: &FirmwareRelease) -> Result<Vec<u8>, Error> {
    let firmware = get(&release.url)?;
    check_download(release, &firmware)?;
    Ok(firmware)
}

fn check_download(release: &FirmwareRelease, firmware: &[u8]) -> Result<(), Error> {
    let hash: [u8; 32] = Sha256::digest(firmware).into();
    if hash != release.cmphash {
        return Err(Error::Other(format!(
            "the downloaded firmware's hash {} doesn't match the index ({})",
            hex::encode(hash),
            hex::encode(release.cmphash)
        )));
    }
    // As checked by the device (`ota_init()` in `main/process/ota_util.c`).
    if firmware.is_empty() || release.fwsize as usize <= firmware.len() {
        return Err(Error::Other("invalid firmware sizes".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// https://jadefw.blockstream.com/bin/jade2.0/index.json, fetched on 2026-09-24.
    const INDEX: &str = include_str!("../tests/data/index_jade2.0.json");

    #[test]
    fn latest() {
        let ble = latest_in("jade2.0", "BLE", INDEX).unwrap();
        assert_eq!(
            ble,
            FirmwareRelease {
                hw_target: "jade2.0".into(),
                version: Version::new(1, 0, 41),
                config: "ble".into(),
                url: "https://jadefw.blockstream.com/bin/jade2.0/1.0.41_ble_1445888_fw.bin".into(),
                fwsize: 1445888,
                cmphash: hash_from_hex(
                    "323cb6b17671d153ece20af8f6a3ddce7142e4bcc6cf2577b9ab7c2a14d55660"
                )
                .unwrap(),
                fwhash: hash_from_hex(
                    "1dac2d6f605870147dfd40898be0feb61dc0e4374e81d92d5c274a76642f5ce3"
                )
                .unwrap(),
            }
        );
        let noradio = latest_in("jade2.0", "NORADIO", INDEX).unwrap();
        assert_eq!(noradio.version, Version::new(1, 0, 41));
        assert!(noradio.url.ends_with("/1.0.41_noradio_1183744_fw.bin"));
        assert!(latest_in("jade2.0", "other", INDEX).is_err());
    }

    #[test]
    fn index_filtering() {
        let entry = |version: &str, filename: &str| {
            format!(
                r#"{{"filename": "{}", "version": "{}", "config": "ble", "fwsize": 10,
                    "cmphash": "{}", "fwhash": "{}"}}"#,
                filename,
                version,
                "00".repeat(32),
                "11".repeat(32)
            )
        };
        let index = |full: &[String]| {
            format!(
                r#"{{"beta": {{"full": [{}]}}, "stable": {{"full": [{}], "delta": []}}}}"#,
                entry("9.9.9", "9.9.9_ble_10_fw.bin"),
                full.join(",")
            )
        };
        // The newest stable version, not the beta.
        let i = index(&[
            entry("1.0.9", "a.bin"),
            entry("1.0.10", "b.bin"),
            entry("1.0.2", "c.bin"),
        ]);
        assert_eq!(
            latest_in("jade", "ble", &i).unwrap().version,
            Version::new(1, 0, 10)
        );
        // Files in subdirectories, unparseable versions are ignored.
        let i = index(&[
            entry("1.0.11", "deltas/x.bin"),
            entry("1.0.12", "../x.bin"),
            entry("latest", "y.bin"),
            entry("1.0.1", "z.bin"),
        ]);
        assert_eq!(
            latest_in("jade", "ble", &i).unwrap().version,
            Version::new(1, 0, 1)
        );
        assert!(latest_in("jade", "ble", r#"{"stable": {}}"#).is_err());
        assert!(latest_in("jade", "ble", "{}").is_err());
        assert!(latest_in("jade", "ble", "not json").is_err());
    }

    #[test]
    fn download_checks() {
        let firmware = b"compressed firmware";
        let mut release = latest_in("jade2.0", "ble", INDEX).unwrap();
        assert!(check_download(&release, firmware).is_err());
        release.cmphash = Sha256::digest(firmware).into();
        assert!(check_download(&release, firmware).is_ok());
        release.fwsize = firmware.len() as u32;
        assert!(check_download(&release, firmware).is_err());
    }
}
