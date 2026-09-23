//! HTTP requests to the Ledger Manager API (the API used by Ledger Live to get information about
//! firmwares and applications).
//!
//! Endpoints and parameters are taken from
//! https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/repositories/HttpManagerApiRepository.ts
//! and https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/api.ts

use crate::{
    error::Error, version::SemVer, DeviceInfo, BASE_API_V1_URL, BASE_API_V2_URL,
    LIVE_COMMON_VERSION,
};

use form_urlencoded::Serializer as UrlSerializer;
use serde::{de::DeserializeOwned, Deserialize, Deserializer};
use serde_derive::Deserialize;

/// The salt used by Ledger Live for the incremental deployment of firmware updates. It is derived
/// from the `USER_ID` of the Ledger Live instance as `sha256(USER_ID + "|firmwareSalt")`, hex
/// encoded, truncated to 6 characters. See
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/use-cases/getUserHashes.ts
/// We use the default (empty) `USER_ID` of Ledger Live
/// (https://github.com/LedgerHQ/ledger-live/blob/develop/shared/env/src/definitions/team-live-devices/index.ts),
/// which gives sha256("|firmwareSalt") = 544d897c...
pub const FIRMWARE_SALT: &str = "544d89";

/// Build a URL with the given query parameters, properly escaped. `livecommonversion` is always
/// added first, as Ledger Live does.
pub(crate) fn url_with_params(base: &str, params: &[(&str, &str)]) -> String {
    let mut ser = UrlSerializer::new(String::new());
    ser.append_pair("livecommonversion", LIVE_COMMON_VERSION);
    for (k, v) in params {
        ser.append_pair(k, v);
    }
    format!("{}?{}", base, ser.finish())
}

fn check_status(resp: &minreq::Response, url: &str) -> Result<(), Error> {
    if (200..300).contains(&resp.status_code) {
        Ok(())
    } else {
        log::debug!(
            "Ledger API error {} for {}: {}",
            resp.status_code,
            url,
            resp.as_str().unwrap_or("<non-utf8 body>")
        );
        Err(Error::Api {
            status: resp.status_code,
            url: url.to_string(),
        })
    }
}

fn parse_json<T: DeserializeOwned>(resp: &minreq::Response) -> Result<T, Error> {
    Ok(serde_json::from_slice(resp.as_bytes())?)
}

pub(crate) fn get_json<T: DeserializeOwned>(url: &str) -> Result<T, Error> {
    log::debug!("GET {}", url);
    let resp = minreq::get(url).send()?;
    check_status(&resp, url)?;
    parse_json(&resp)
}

/// Same as `get_json` but a 404 is turned into `Error::FirmwareNotRecognized`, as Ledger Live does
/// for `get_device_version` and `get_firmware_version`.
fn get_json_firmware<T: DeserializeOwned>(url: &str) -> Result<T, Error> {
    match get_json(url) {
        Err(Error::Api { status: 404, .. }) => Err(Error::FirmwareNotRecognized),
        r => r,
    }
}

pub(crate) fn post_json<T: DeserializeOwned>(
    url: &str,
    body: &serde_json::Value,
) -> Result<T, Error> {
    log::debug!("POST {}", url);
    let resp = minreq::post(url).with_json(body)?.send()?;
    check_status(&resp, url)?;
    parse_json(&resp)
}

/// Deserialize a field which may be missing or explicitly `null` as its default value. The Ledger
/// API is not consistent about it and Ledger Live treats most of these fields as nullable (e.g.
/// `mcuVersion.from_bootloader_version ?? ""` in hw/flash.ts). Use along with `#[serde(default)]`.
fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A "device version", as the Ledger API calls a hardware version of a device model.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/entities/DeviceVersionEntity.ts
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceVersion {
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub target_id: Option<serde_json::Value>,
    #[serde(default)]
    pub device: Option<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub providers: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub mcu_versions: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub se_firmware_final_versions: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub osu_versions: Vec<i64>,
}

/// An OS Updater firmware: the firmware installed on the device to perform a firmware update.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/entities/FirmwareUpdateContextEntity.ts
#[derive(Debug, Clone, Deserialize)]
pub struct OsuFirmware {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    pub perso: String,
    pub firmware: String,
    pub firmware_key: String,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub device_versions: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub providers: Vec<i64>,
    pub next_se_firmware_final_version: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub previous_se_firmware_final_version: Vec<i64>,
}

/// A final firmware, as known by the Ledger API.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/entities/FirmwareUpdateContextEntity.ts
#[derive(Debug, Clone, Deserialize)]
pub struct FinalFirmware {
    pub id: i64,
    /// The firmware version name, e.g. "2.2.3".
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    pub perso: String,
    /// The firmware to install. Empty for most firmwares: the OSU installs the final firmware
    /// itself. See `has_final_firmware`.
    #[serde(default)]
    pub firmware: Option<String>,
    #[serde(default)]
    pub firmware_key: Option<String>,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub se_firmware: Option<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub device_versions: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub providers: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub mcu_versions: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub application_versions: Vec<i64>,
    #[serde(default)]
    pub bytes: Option<u64>,
}

impl FinalFirmware {
    /// Whether the final firmware must be installed separately after the OSU (legacy flow).
    /// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/hasFinalFirmware.ts
    pub fn has_final_firmware(&self) -> bool {
        self.firmware
            .as_deref()
            .map(|f| !f.is_empty())
            .unwrap_or(false)
    }
}

/// Information about a firmware version, as queried from the Ledger API. Kept for backward
/// compatibility, this is the current final firmware of the device.
pub type FirmwareInfo = FinalFirmware;

/// A MCU firmware version.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/types-live/src/manager.ts
#[derive(Debug, Clone, Deserialize)]
pub struct McuVersion {
    pub id: i64,
    #[serde(default)]
    pub mcu: Option<i64>,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub providers: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub from_bootloader_version: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub device_versions: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub se_firmware_final_versions: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize)]
struct LatestFirmwareResponse {
    result: String,
    #[serde(default)]
    se_firmware_osu_version: Option<OsuFirmware>,
}

/// Get the device version for this target id.
pub fn get_device_version(target_id: u32, provider: u32) -> Result<DeviceVersion, Error> {
    let url = url_with_params(
        &format!("{}/get_device_version", BASE_API_V1_URL),
        &[
            ("provider", &provider.to_string()),
            ("target_id", &target_id.to_string()),
        ],
    );
    get_json_firmware(&url)
}

/// Get the final firmware information for this version name ("2.2.3") and device version id.
pub fn get_current_firmware(
    version: &str,
    device_version_id: i64,
    provider: u32,
) -> Result<FinalFirmware, Error> {
    let url = url_with_params(
        &format!("{}/get_firmware_version", BASE_API_V1_URL),
        &[
            ("device_version", &device_version_id.to_string()),
            ("version_name", version),
            ("provider", &provider.to_string()),
        ],
    );
    get_json_firmware(&url)
}

/// Get the OSU firmware for this version name ("2.2.3", without the "-osu" suffix) and device
/// version id.
pub fn get_current_osu(
    version: &str,
    device_version_id: i64,
    provider: u32,
) -> Result<OsuFirmware, Error> {
    let url = url_with_params(
        &format!("{}/get_osu_version", BASE_API_V1_URL),
        &[
            ("device_version", &device_version_id.to_string()),
            ("version_name", &format!("{}-osu", version)),
            ("provider", &provider.to_string()),
        ],
    );
    get_json(&url)
}

/// Get the OSU firmware of the next available firmware, if any.
pub fn get_latest_firmware(
    current_se_firmware_final_version: i64,
    device_version_id: i64,
    provider: u32,
) -> Result<Option<OsuFirmware>, Error> {
    let url = url_with_params(
        &format!("{}/get_latest_firmware", BASE_API_V1_URL),
        &[
            ("salt", FIRMWARE_SALT),
            (
                "current_se_firmware_final_version",
                &current_se_firmware_final_version.to_string(),
            ),
            ("device_version", &device_version_id.to_string()),
            ("provider", &provider.to_string()),
        ],
    );
    let resp: LatestFirmwareResponse = get_json(&url)?;
    if resp.result == "null" {
        return Ok(None);
    }
    Ok(resp.se_firmware_osu_version)
}

/// Get a final firmware by its id.
pub fn get_final_firmware_by_id(id: i64) -> Result<FinalFirmware, Error> {
    let url = url_with_params(
        &format!("{}/firmware_final_versions/{}", BASE_API_V1_URL, id),
        &[],
    );
    get_json(&url)
}

/// Parse the list of MCU versions element by element: an entry which can't be parsed is skipped
/// (with a warning) rather than failing the whole list, as it would otherwise prevent firmware
/// updates for every device.
pub(crate) fn parse_mcus(entries: Vec<serde_json::Value>) -> Vec<McuVersion> {
    entries
        .into_iter()
        .filter_map(|e| match serde_json::from_value::<McuVersion>(e.clone()) {
            Ok(mcu) => Some(mcu),
            Err(err) => {
                log::warn!("Skipping MCU version from the Ledger API ({}): {}", err, e);
                None
            }
        })
        .collect()
}

/// Get all the MCU versions.
pub fn fetch_mcus() -> Result<Vec<McuVersion>, Error> {
    let url = url_with_params(&format!("{}/mcu_versions", BASE_API_V1_URL), &[]);
    Ok(parse_mcus(get_json(&url)?))
}

/// Find the MCU with the highest version.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/api.ts (`findBestMCU`)
pub fn find_best_mcu(mcus: &[McuVersion]) -> Option<&McuVersion> {
    let mut best = mcus.first()?;
    for mcu in &mcus[1..] {
        let (a, b) = (SemVer::coerce(&mcu.name), SemVer::coerce(&best.name));
        if let (Some(a), Some(b)) = (a, b) {
            if a > b {
                best = mcu;
            }
        }
    }
    Some(best)
}

/// Among all MCU versions, find the best one to flash for this final firmware.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/flash.ts
pub(crate) fn mcus_for_final_firmware(
    mcus: &[McuVersion],
    final_firmware: &FinalFirmware,
    provider: u32,
) -> Vec<McuVersion> {
    mcus.iter()
        .filter(|m| {
            m.providers.contains(&(provider as i64))
                && m.from_bootloader_version != "none"
                && final_firmware.mcu_versions.contains(&m.id)
        })
        .cloned()
        .collect()
}

/// A firmware update available for a device.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/entities/FirmwareUpdateContextEntity.ts
#[derive(Debug, Clone)]
pub struct FirmwareUpdateInfo {
    /// The OS updater to install on the device.
    pub osu: OsuFirmware,
    /// The firmware the device will be running after the update.
    pub final_firmware: FinalFirmware,
    /// Whether the MCU must be flashed as part of the update.
    pub should_flash_mcu: bool,
}

impl FirmwareUpdateInfo {
    /// The version the device will be running after the update, e.g. "2.4.1".
    pub fn version(&self) -> &str {
        &self.final_firmware.name
    }

    /// The version of the OSU, without the "-osu" suffix.
    /// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/index.ts (`getFirmwareVersion`)
    pub fn osu_version(&self) -> String {
        self.osu.name.replace("-osu", "")
    }

    /// Whether a final firmware must be installed separately after the OSU (legacy devices).
    pub fn has_final_firmware(&self) -> bool {
        self.final_firmware.has_final_firmware()
    }
}

/// Get the firmware update available for this device, if any.
///
/// Ported from https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/use-cases/getLatestFirmwareForDevice.ts
pub fn latest_firmware(device_info: &DeviceInfo) -> Result<Option<FirmwareUpdateInfo>, Error> {
    if device_info.is_bootloader {
        return Err(Error::DeviceOnDashboardExpected);
    }
    let provider = device_info.provider_id();
    let device_version = get_device_version(device_info.target_id, provider)?;

    let osu = if device_info.is_osu {
        Some(get_current_osu(
            &device_info.version,
            device_version.id,
            provider,
        )?)
    } else {
        let current = get_current_firmware(&device_info.version, device_version.id, provider)?;
        get_latest_firmware(current.id, device_version.id, provider)?
    };
    let osu = match osu {
        Some(osu) => osu,
        None => return Ok(None),
    };

    let mcus = fetch_mcus()?;
    let current_mcu = mcus
        .iter()
        .find(|m| Some(m.name.as_str()) == device_info.mcu_version.as_deref())
        .ok_or(Error::UnknownMcu)?;
    let final_firmware = get_final_firmware_by_id(osu.next_se_firmware_final_version)?;
    let should_flash_mcu = !final_firmware.mcu_versions.contains(&current_mcu.id);

    Ok(Some(FirmwareUpdateInfo {
        osu,
        final_firmware,
        should_flash_mcu,
    }))
}

/// Get the current final firmware of this device.
pub fn current_firmware(device_info: &DeviceInfo) -> Result<FinalFirmware, Error> {
    let provider = device_info.provider_id();
    let device_version = get_device_version(device_info.target_id, provider)?;
    get_current_firmware(&device_info.version, device_version.id, provider)
}

impl FinalFirmware {
    /// Get the current firmware of this device from the Ledger API.
    pub fn from_device(device_info: &DeviceInfo) -> Result<Self, Error> {
        current_firmware(device_info)
    }
}

/// Information about an application as queried from the Ledger API (not the Ledger device).
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/entities/AppEntity.ts (`ApplicationV2Entity`)
#[derive(Debug, Clone, Deserialize)]
pub struct BitcoinAppInfo {
    /// The name of the application, e.g. "Bitcoin" or "Bitcoin Test".
    #[serde(rename = "versionName")]
    pub version_name: String,
    #[serde(rename = "versionId")]
    pub version_id: u32,
    #[serde(rename = "versionDisplayName", default)]
    pub version_display_name: Option<String>,
    /// The version of the application, e.g. "2.1.3".
    pub version: String,
    pub perso: String,
    /// The firmware used to uninstall the application.
    #[serde(default, deserialize_with = "null_as_default")]
    pub delete: String,
    #[serde(rename = "deleteKey")]
    pub delete_key: String,
    pub firmware: String,
    #[serde(rename = "firmwareKey")]
    pub firmware_key: String,
    pub hash: String,
    /// The application this one depends on, if any.
    #[serde(rename = "parentName", default)]
    pub parent_name: Option<String>,
    /// The size of the application.
    #[serde(default)]
    pub bytes: Option<u64>,
    #[serde(default)]
    pub warning: Option<String>,
}

// Returns a Vec of Options as some elements in the response's JSON array may be `null`.
/// Get metadata about a list of Bitcoin apps identified by their hash. Elements returned seem to
/// be in the same order as the hashes, with `None` for not found.
pub fn bitcoin_apps_by_hashes(hashes: Vec<Vec<u8>>) -> Result<Vec<Option<BitcoinAppInfo>>, Error> {
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    let hashes_hex: Vec<_> = hashes
        .into_iter()
        .map(|h| serde_json::Value::String(hex::encode(h)))
        .collect();
    let url = url_with_params(&format!("{}/apps/hash", BASE_API_V2_URL), &[]);
    let apps: Vec<serde_json::Value> = post_json(&url, &serde_json::Value::Array(hashes_hex))?;
    Ok(apps
        .into_iter()
        .map(|a| {
            if a.is_null() {
                return None;
            }
            serde_json::from_value(a)
                .map_err(|e| log::debug!("Could not parse app from Ledger API: {}", e))
                .ok()
        })
        .collect())
}

/// Get the catalog of applications available for this device. Only Bitcoin applications are
/// kept, others are discarded (and not even parsed).
// This uses the v2 API. See for reference:
// - https://github.com/LedgerHQ/ledger-live/blob/5a0a1aa5dc183116839851b79bceb6704f1de4b9/libs/ledger-live-common/src/apps/listApps/v2.ts
// - https://github.com/LedgerHQ/ledger-live/blob/5a0a1aa5dc183116839851b79bceb6704f1de4b9/libs/device-core/src/managerApi/repositories/HttpManagerApiRepository.ts#L211
pub(crate) fn bitcoin_apps_catalog(device_info: &DeviceInfo) -> Result<Vec<BitcoinAppInfo>, Error> {
    let url = url_with_params(
        &format!("{}/apps/by-target", BASE_API_V2_URL),
        &[
            ("provider", &device_info.provider_id().to_string()),
            ("target_id", &device_info.target_id.to_string()),
            ("firmware_version_name", &device_info.version),
        ],
    );
    let apps: Vec<serde_json::Value> = get_json(&url)?;
    Ok(apps
        .into_iter()
        .filter(|a| {
            a.get("versionName")
                .and_then(|n| n.as_str())
                .map(crate::apps::is_bitcoin_app_name)
                .unwrap_or(false)
        })
        .filter_map(|a| match serde_json::from_value::<BitcoinAppInfo>(a) {
            Ok(app) => Some(app),
            Err(e) => {
                log::warn!("Could not parse Bitcoin app from catalog: {}", e);
                None
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let url = url_with_params(
            "https://example.com/api/get_osu_version",
            &[
                ("version_name", "2.1.0-osu"),
                ("firmware", "nanox/2.1.0/fw_1 2"),
            ],
        );
        assert_eq!(
            url,
            format!(
                "https://example.com/api/get_osu_version?livecommonversion={}&version_name=2.1.0-osu&firmware=nanox%2F2.1.0%2Ffw_1+2",
                LIVE_COMMON_VERSION
            )
        );
    }

    #[test]
    fn best_mcu() {
        let mcu = |id, name: &str, from: &str, providers: Vec<i64>| McuVersion {
            id,
            mcu: None,
            name: name.to_string(),
            description: None,
            providers,
            from_bootloader_version: from.to_string(),
            device_versions: vec![],
            se_firmware_final_versions: vec![],
        };
        let mcus = vec![
            mcu(1, "1.7", "0.11", vec![1]),
            mcu(2, "1.12", "0.11", vec![1]),
            mcu(3, "1.9", "0.11", vec![1]),
            mcu(4, "2.30", "none", vec![1]),
            mcu(5, "3.0", "0.11", vec![2]),
        ];
        assert_eq!(find_best_mcu(&mcus[..3]).map(|m| m.id), Some(2));
        assert!(find_best_mcu(&[]).is_none());

        let final_fw: FinalFirmware = serde_json::from_value(serde_json::json!({
            "id": 42,
            "name": "2.1.0",
            "perso": "perso_11",
            "firmware": "",
            "firmware_key": "",
            "mcu_versions": [1, 3, 4, 5],
        }))
        .unwrap();
        assert!(!final_fw.has_final_firmware());
        let filtered = mcus_for_final_firmware(&mcus, &final_fw, 1);
        assert_eq!(
            filtered.iter().map(|m| m.id).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(find_best_mcu(&filtered).map(|m| m.id), Some(3));
    }

    #[test]
    fn nullable_fields() {
        // Shapes seen on the live API: most string fields of the final firmwares may be null.
        let final_fw: FinalFirmware = serde_json::from_value(serde_json::json!({
            "id": 521,
            "name": "2.6.0",
            "version": "2.6.0",
            "description": null,
            "display_name": null,
            "notes": null,
            "perso": "perso_11",
            "firmware": null,
            "firmware_key": null,
            "hash": null,
            "distribution_ratio": null,
            "bytes": null,
            "se_firmware": 5,
            "device_versions": null,
            "providers": [1],
            "mcu_versions": null,
            "application_versions": null,
            "osu_versions": [{"id": 1, "description": null, "hash": null}],
        }))
        .unwrap();
        assert!(!final_fw.has_final_firmware());
        assert!(final_fw.mcu_versions.is_empty());
        assert!(final_fw.device_versions.is_empty());
        assert_eq!(final_fw.providers, vec![1]);

        let osu: OsuFirmware = serde_json::from_value(serde_json::json!({
            "id": 1,
            "name": "2.2.3-to-2.6.0",
            "description": null,
            "display_name": null,
            "notes": null,
            "perso": "perso_11",
            "firmware": "nanox/2.6.0/upgrade_osu",
            "firmware_key": "nanox/2.6.0/upgrade_osu_key",
            "hash": null,
            "device_versions": null,
            "providers": null,
            "next_se_firmware_final_version": 521,
            "previous_se_firmware_final_version": null,
        }))
        .unwrap();
        assert!(osu.hash.is_none());
        assert!(osu.providers.is_empty());

        let dv: DeviceVersion = serde_json::from_value(serde_json::json!({
            "id": 17,
            "name": null,
            "providers": null,
            "mcu_versions": null,
            "se_firmware_final_versions": null,
            "osu_versions": null,
        }))
        .unwrap();
        assert!(dv.providers.is_empty() && dv.osu_versions.is_empty());
    }

    #[test]
    fn mcus_parsing() {
        let mcus = parse_mcus(vec![
            serde_json::json!({
                "id": 1,
                "mcu": 1,
                "name": "1.0",
                "description": "",
                "providers": [12],
                "device_versions": [1, 2],
                "from_bootloader_version": "",
                "from_bootloader_version_id": 2,
                "se_firmware_final_versions": [7, 12],
                "date_creation": "2018-09-20T13:30:50.156394Z",
                "date_last_modified": "2025-12-16T17:15:22.486525Z"
            }),
            // Nullable fields.
            serde_json::json!({
                "id": 2,
                "mcu": null,
                "name": "2.30",
                "description": null,
                "providers": null,
                "device_versions": null,
                "from_bootloader_version": null,
                "from_bootloader_version_id": null,
                "se_firmware_final_versions": null,
            }),
            // Invalid entries are skipped.
            serde_json::json!({"id": "three", "name": "1.1"}),
            serde_json::json!({"id": 4}),
            serde_json::Value::Null,
            serde_json::json!({"id": 5, "name": "2.12", "from_bootloader_version": "1.12"}),
        ]);
        assert_eq!(mcus.iter().map(|m| m.id).collect::<Vec<_>>(), vec![1, 2, 5]);
        assert_eq!(mcus[1].from_bootloader_version, "");
        assert!(mcus[1].providers.is_empty());
        assert_eq!(mcus[2].from_bootloader_version, "1.12");
    }

    #[test]
    fn deserialize_api_entities() {
        // Shapes from the ledger-live entity definitions and mocks (libs/device-core/src/managerApi/entities).
        let dv: DeviceVersion = serde_json::from_value(serde_json::json!({
            "name": "Ledger Nano S",
            "device": 3,
            "providers": [],
            "id": 5,
            "display_name": "Ledger Nano S",
            "target_id": "0x31100004",
            "description": "Ledger Nano S",
            "mcu_versions": [1],
            "se_firmware_final_versions": [2],
            "osu_versions": [],
            "application_versions": [],
            "date_creation": "2020-04-30T13:50:00.000Z",
            "date_last_modified": "2020-04-30T13:50:00.000Z"
        }))
        .unwrap();
        assert_eq!(dv.id, 5);

        let osu: OsuFirmware = serde_json::from_value(serde_json::json!({
            "id": 0,
            "name": "2.2.0-osu",
            "display_name": "",
            "notes": null,
            "perso": "perso_11",
            "firmware": "nanox/2.2.0/upgrade_osu_2.2.0",
            "firmware_key": "nanox/2.2.0/upgrade_osu_2.2.0_key",
            "hash": "abcd",
            "device_versions": [],
            "next_se_firmware_final_version": 123,
            "providers": [1],
            "date_creation": "",
            "date_last_modified": "",
            "description": "",
            "previous_se_firmware_final_version": [100]
        }))
        .unwrap();
        assert_eq!(osu.next_se_firmware_final_version, 123);

        let resp: LatestFirmwareResponse =
            serde_json::from_value(serde_json::json!({"result": "null"})).unwrap();
        assert_eq!(resp.result, "null");
        assert!(resp.se_firmware_osu_version.is_none());

        let app: BitcoinAppInfo = serde_json::from_value(serde_json::json!({
            "versionId": 1234,
            "versionName": "Bitcoin",
            "versionDisplayName": "Bitcoin",
            "version": "2.1.3",
            "currencyId": "bitcoin",
            "description": null,
            "applicationType": "currency",
            "dateModified": "",
            "icon": "bitcoin",
            "authorName": "Ledger",
            "supportURL": null,
            "contactURL": null,
            "sourceURL": null,
            "hash": "00ff",
            "perso": "perso_11",
            "parentName": null,
            "firmware": "nanox/2.2.3/bitcoin/app_2.1.3",
            "firmwareKey": "nanox/2.2.3/bitcoin/app_2.1.3_key",
            "delete": "nanox/2.2.3/bitcoin/app_2.1.3_del",
            "deleteKey": "nanox/2.2.3/bitcoin/app_2.1.3_del_key",
            "bytes": 90000,
            "warning": null,
            "isDevTools": false
        }))
        .unwrap();
        assert_eq!(app.delete, "nanox/2.2.3/bitcoin/app_2.1.3_del");

        let app2: BitcoinAppInfo = serde_json::from_value(serde_json::json!({
            "versionId": 1234,
            "versionName": "Bitcoin",
            "version": "2.1.3",
            "perso": "perso_11",
            "firmware": "f",
            "firmwareKey": "fk",
            "delete": null,
            "deleteKey": "dk",
            "hash": "00ff",
        }))
        .unwrap();
        assert!(app2.delete.is_empty());
        assert_eq!(app.parent_name, None);
        assert_eq!(app.bytes, Some(90000));
    }
}
