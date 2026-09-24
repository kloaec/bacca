//! HTTP requests to the Ledger Manager API, used by Ledger Live to get information about the
//! firmwares and the apps.
//!
//! Endpoints and parameters are taken from
//! https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/repositories/HttpManagerApiRepository.ts
//! and https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/api.ts
//! The entities are defined in
//! https://github.com/LedgerHQ/ledger-live/tree/develop/libs/device-core/src/managerApi/entities
//! Only the fields we use are parsed.

use crate::{
    device::{coerce_version, DeviceInfo},
    error::Error,
};

use serde::{de::DeserializeOwned, Deserializer};
use serde_derive::Deserialize;

/// The Ledger Live API requires requests to set their claimed version of Ledger Live. This is the
/// version of ledger-live-common at the time of writing
/// (https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/package.json).
pub(crate) const LIVE_COMMON_VERSION: &str = "38.0.0";

/// The Ledger Live API has multiple channels ("providers") to download binaries. 1 is the
/// default, see `DeviceInfo::provider` for the others.
pub(crate) const DEFAULT_PROVIDER: u32 = 1;

pub(crate) const BASE_API_V1_URL: &str = "https://manager.api.live.ledger.com/api";
const BASE_API_V2_URL: &str = "https://manager.api.live.ledger.com/api/v2";

/// The salt used by Ledger Live for the incremental deployment of firmware updates:
/// `sha256(USER_ID + "|firmwareSalt")`, hex encoded, truncated to 6 characters (see
/// libs/device-core/src/managerApi/use-cases/getUserHashes.ts). We use the default (empty)
/// `USER_ID` of Ledger Live, which gives sha256("|firmwareSalt") = 544d897c...
const FIRMWARE_SALT: &str = "544d89";

/// A URL with these query parameters, escaped. `livecommonversion` is added first, as Ledger Live
/// does.
pub(crate) fn url_with_params(base: &str, params: &[(&str, &str)]) -> String {
    let mut ser = form_urlencoded::Serializer::new(String::new());
    ser.append_pair("livecommonversion", LIVE_COMMON_VERSION);
    for (k, v) in params {
        ser.append_pair(k, v);
    }
    format!("{}?{}", base, ser.finish())
}

fn check_status(resp: minreq::Response, url: &str) -> Result<minreq::Response, Error> {
    if (200..300).contains(&resp.status_code) {
        return Ok(resp);
    }
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

pub(crate) fn get_json<T: DeserializeOwned>(url: &str) -> Result<T, Error> {
    log::debug!("GET {}", url);
    let resp = check_status(minreq::get(url).send()?, url)?;
    Ok(serde_json::from_slice(resp.as_bytes())?)
}

/// GET a text document (e.g. the APDUs of a language pack).
pub(crate) fn get_text(url: &str) -> Result<String, Error> {
    log::debug!("GET {}", url);
    let resp = check_status(minreq::get(url).send()?, url)?;
    Ok(resp.as_str()?.to_string())
}

/// Like `get_json`, but a 404 means the API doesn't know the firmware, as in Ledger Live.
fn get_firmware_json<T: DeserializeOwned>(url: &str) -> Result<T, Error> {
    match get_json(url) {
        Err(Error::Api { status: 404, .. }) => Err(Error::Other(
            "The Ledger API did not recognize this device's firmware.".into(),
        )),
        r => r,
    }
}

/// Deserialize a field which may be missing or `null` as its default value. The Ledger API is
/// not consistent about it and Ledger Live treats most of these fields as nullable (e.g.
/// `mcuVersion.from_bootloader_version ?? ""` in hw/flash.ts). Use with `#[serde(default)]`.
pub(crate) fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    let value: Option<T> = serde::Deserialize::deserialize(deserializer)?;
    Ok(value.unwrap_or_default())
}

/// Parse the entries of a list one by one: an entry which can't be parsed is skipped (with a
/// warning) rather than failing the whole list, which could prevent updating any device.
fn parse_list<T: DeserializeOwned>(entries: Vec<serde_json::Value>) -> Vec<T> {
    entries
        .into_iter()
        .filter_map(|e| {
            serde_json::from_value(e.clone())
                .map_err(|err| log::warn!("Skipping entry from the Ledger API ({}): {}", err, e))
                .ok()
        })
        .collect()
}

/// A "device version", as the Ledger API calls a hardware version of a model.
#[derive(Debug, Clone, Deserialize)]
struct DeviceVersion {
    id: i64,
}

/// An OS Updater: the firmware installed on the device to perform a firmware update.
#[derive(Debug, Clone, Deserialize)]
pub struct OsuFirmware {
    pub id: i64,
    pub name: String,
    pub perso: String,
    pub firmware: String,
    pub firmware_key: String,
    /// The identifier the device displays for the user to confirm the update.
    #[serde(default)]
    pub hash: Option<String>,
    pub next_se_firmware_final_version: i64,
}

/// A final firmware: what the device runs after an update.
#[derive(Debug, Clone, Deserialize)]
pub struct FinalFirmware {
    pub id: i64,
    /// The version, e.g. "2.2.3".
    pub name: String,
    #[serde(default)]
    pub notes: Option<String>,
    pub perso: String,
    /// The firmware to install after the OSU, on legacy devices. Empty for most firmwares: the OSU
    /// installs the final firmware itself.
    #[serde(default)]
    pub firmware: Option<String>,
    #[serde(default)]
    pub firmware_key: Option<String>,
    /// The ids of the MCU versions this firmware runs with.
    #[serde(default, deserialize_with = "null_as_default")]
    pub mcu_versions: Vec<i64>,
}

impl FinalFirmware {
    /// Whether the final firmware must be installed separately after the OSU (legacy flow).
    /// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/hasFinalFirmware.ts
    pub(crate) fn has_final_firmware(&self) -> bool {
        self.firmware.as_deref().is_some_and(|f| !f.is_empty())
    }
}

/// A MCU firmware version (libs/types-live/src/manager.ts).
#[derive(Debug, Clone, Deserialize)]
pub struct McuVersion {
    pub id: i64,
    pub name: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub providers: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub from_bootloader_version: String,
}

fn get_device_version(target_id: u32, provider: u32) -> Result<DeviceVersion, Error> {
    let url = url_with_params(
        &format!("{}/get_device_version", BASE_API_V1_URL),
        &[
            ("provider", &provider.to_string()),
            ("target_id", &target_id.to_string()),
        ],
    );
    get_firmware_json(&url)
}

/// The id of the device version of this target id.
pub(crate) fn device_version_id(target_id: u32, provider: u32) -> Result<i64, Error> {
    Ok(get_device_version(target_id, provider)?.id)
}

/// The final firmware of this version name ("2.2.3") for this device version.
pub(crate) fn get_final_firmware(
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
    get_firmware_json(&url)
}

/// The OSU of this version name ("2.2.3", without the "-osu" suffix) for this device version.
pub(crate) fn get_osu(
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

pub(crate) fn get_final_firmware_by_id(id: i64) -> Result<FinalFirmware, Error> {
    let url = url_with_params(
        &format!("{}/firmware_final_versions/{}", BASE_API_V1_URL, id),
        &[],
    );
    get_json(&url)
}

/// Get all the MCU versions.
pub fn fetch_mcus() -> Result<Vec<McuVersion>, Error> {
    let url = url_with_params(&format!("{}/mcu_versions", BASE_API_V1_URL), &[]);
    Ok(parse_list(get_json(&url)?))
}

/// The MCU with the highest version (`findBestMCU` in manager/api.ts).
pub(crate) fn find_best_mcu<'a>(
    mcus: impl IntoIterator<Item = &'a McuVersion>,
) -> Option<&'a McuVersion> {
    let mut mcus = mcus.into_iter();
    let mut best = mcus.next()?;
    for mcu in mcus {
        if let (Some(a), Some(b)) = (coerce_version(&mcu.name), coerce_version(&best.name)) {
            if a > b {
                best = mcu;
            }
        }
    }
    Some(best)
}

/// The best MCU to flash for this final firmware (hw/flash.ts).
pub(crate) fn best_mcu_for_final_firmware<'a>(
    mcus: &'a [McuVersion],
    final_firmware: &FinalFirmware,
    provider: u32,
) -> Option<&'a McuVersion> {
    find_best_mcu(mcus.iter().filter(|m| {
        m.providers.contains(&(provider as i64))
            && m.from_bootloader_version != "none"
            && final_firmware.mcu_versions.contains(&m.id)
    }))
}

/// A firmware update available for a device.
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
}

/// Get the firmware update available for this device, if any.
/// Ported from https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/use-cases/getLatestFirmwareForDevice.ts
pub fn latest_firmware(device_info: &DeviceInfo) -> Result<Option<FirmwareUpdateInfo>, Error> {
    if device_info.is_bootloader {
        return Err(Error::DeviceInBootloader);
    }
    let provider = device_info.provider;
    let device_version = device_version_id(device_info.target_id, provider)?;
    let osu = if device_info.is_osu {
        get_osu(&device_info.version, device_version, provider)?
    } else {
        let current = get_final_firmware(&device_info.version, device_version, provider)?;
        #[derive(Deserialize)]
        struct LatestFirmwareResponse {
            result: String,
            #[serde(default)]
            se_firmware_osu_version: Option<OsuFirmware>,
        }
        let url = url_with_params(
            &format!("{}/get_latest_firmware", BASE_API_V1_URL),
            &[
                ("salt", FIRMWARE_SALT),
                ("current_se_firmware_final_version", &current.id.to_string()),
                ("device_version", &device_version.to_string()),
                ("provider", &provider.to_string()),
            ],
        );
        let resp: LatestFirmwareResponse = get_json(&url)?;
        match resp.se_firmware_osu_version {
            Some(osu) if resp.result != "null" => osu,
            _ => return Ok(None),
        }
    };

    let mcus = fetch_mcus()?;
    let current_mcu = mcus
        .iter()
        .find(|m| Some(m.name.as_str()) == device_info.mcu_version.as_deref())
        .ok_or_else(|| {
            Error::Other("The device's MCU version is unknown to the Ledger API.".into())
        })?;
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
    let provider = device_info.provider;
    let device_version = device_version_id(device_info.target_id, provider)?;
    get_final_firmware(&device_info.version, device_version, provider)
}

/// An app, as known by the Ledger API (`ApplicationV2Entity`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppInfo {
    /// The name of the app, e.g. "Bitcoin" or "Bitcoin Test".
    pub version_name: String,
    /// The version of the app, e.g. "2.1.3".
    pub version: String,
    pub perso: String,
    /// The firmware used to uninstall the app.
    #[serde(default, deserialize_with = "null_as_default")]
    pub delete: String,
    pub delete_key: String,
    pub firmware: String,
    pub firmware_key: String,
    pub hash: String,
}

/// Get the apps with these hashes, in the same order. `None` for the ones the Ledger API doesn't
/// know (e.g. sideloaded apps).
pub fn apps_by_hashes(hashes: Vec<Vec<u8>>) -> Result<Vec<Option<AppInfo>>, Error> {
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    let hashes: Vec<String> = hashes.into_iter().map(hex::encode).collect();
    let url = url_with_params(&format!("{}/apps/hash", BASE_API_V2_URL), &[]);
    log::debug!("POST {}", url);
    let resp = check_status(minreq::post(&url).with_json(&hashes)?.send()?, &url)?;
    let apps: Vec<serde_json::Value> = serde_json::from_slice(resp.as_bytes())?;
    Ok(apps
        .into_iter()
        .map(|a| {
            // Some elements may be `null`.
            serde_json::from_value(a)
                .map_err(|e| log::debug!("Could not parse app from Ledger API: {}", e))
                .ok()
        })
        .collect())
}

/// The apps with these names available for this device (running its current firmware), from the
/// catalog of the v2 API. The other apps are not parsed.
/// https://github.com/LedgerHQ/ledger-live/blob/5a0a1aa5dc183116839851b79bceb6704f1de4b9/libs/ledger-live-common/src/apps/listApps/v2.ts
pub(crate) fn catalog_apps(
    device_info: &DeviceInfo,
    names: &[&str],
) -> Result<Vec<AppInfo>, Error> {
    let url = url_with_params(
        &format!("{}/apps/by-target", BASE_API_V2_URL),
        &[
            ("provider", &device_info.provider.to_string()),
            ("target_id", &device_info.target_id.to_string()),
            ("firmware_version_name", &device_info.version),
        ],
    );
    let entries: Vec<serde_json::Value> = get_json(&url)?;
    Ok(parse_list(
        entries
            .into_iter()
            .filter(|a| {
                a.get("versionName")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| names.contains(&n))
            })
            .collect(),
    ))
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
        let mcus: Vec<McuVersion> = parse_list(vec![
            serde_json::json!({"id": 1, "name": "1.7", "from_bootloader_version": "0.11", "providers": [1]}),
            serde_json::json!({"id": 2, "name": "1.12", "from_bootloader_version": "0.11", "providers": [1]}),
            serde_json::json!({"id": 3, "name": "1.9", "from_bootloader_version": "0.11", "providers": [1]}),
            serde_json::json!({"id": 4, "name": "2.30", "from_bootloader_version": "none", "providers": [1]}),
            serde_json::json!({"id": 5, "name": "3.0", "from_bootloader_version": "0.11", "providers": [2]}),
        ]);
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
        let best = best_mcu_for_final_firmware(&mcus, &final_fw, 1);
        assert_eq!(best.map(|m| m.id), Some(3));
    }

    #[test]
    fn nullable_fields() {
        // Shapes seen on the live API: most string fields of the final firmwares may be null.
        let final_fw: FinalFirmware = serde_json::from_value(serde_json::json!({
            "id": 521,
            "name": "2.6.0",
            "version": "2.6.0",
            "description": null,
            "notes": null,
            "perso": "perso_11",
            "firmware": null,
            "firmware_key": null,
            "hash": null,
            "se_firmware": 5,
            "device_versions": null,
            "providers": [1],
            "mcu_versions": null,
            "osu_versions": [{"id": 1, "description": null, "hash": null}],
        }))
        .unwrap();
        assert!(!final_fw.has_final_firmware());
        assert!(final_fw.mcu_versions.is_empty());

        let osu: OsuFirmware = serde_json::from_value(serde_json::json!({
            "id": 1,
            "name": "2.2.3-to-2.6.0",
            "perso": "perso_11",
            "firmware": "nanox/2.6.0/upgrade_osu",
            "firmware_key": "nanox/2.6.0/upgrade_osu_key",
            "hash": null,
            "providers": null,
            "next_se_firmware_final_version": 521,
        }))
        .unwrap();
        assert!(osu.hash.is_none());

        // Invalid MCU entries are skipped.
        let mcus: Vec<McuVersion> = parse_list(vec![
            serde_json::json!({
                "id": 2, "mcu": null, "name": "2.30", "providers": null,
                "from_bootloader_version": null, "se_firmware_final_versions": null,
            }),
            serde_json::json!({"id": "three", "name": "1.1"}),
            serde_json::json!({"id": 4}),
            serde_json::Value::Null,
            serde_json::json!({"id": 5, "name": "2.12", "from_bootloader_version": "1.12"}),
        ]);
        assert_eq!(mcus.iter().map(|m| m.id).collect::<Vec<_>>(), vec![2, 5]);
        assert_eq!(mcus[0].from_bootloader_version, "");
        assert!(mcus[0].providers.is_empty());
    }

    #[test]
    fn apps() {
        let app: AppInfo = serde_json::from_value(serde_json::json!({
            "versionId": 1234,
            "versionName": "Bitcoin",
            "versionDisplayName": "Bitcoin",
            "version": "2.1.3",
            "currencyId": "bitcoin",
            "hash": "00ff",
            "perso": "perso_11",
            "parentName": null,
            "firmware": "nanox/2.2.3/bitcoin/app_2.1.3",
            "firmwareKey": "nanox/2.2.3/bitcoin/app_2.1.3_key",
            "delete": "nanox/2.2.3/bitcoin/app_2.1.3_del",
            "deleteKey": "nanox/2.2.3/bitcoin/app_2.1.3_del_key",
            "bytes": 90000,
        }))
        .unwrap();
        assert_eq!(app.delete, "nanox/2.2.3/bitcoin/app_2.1.3_del");
        assert_eq!(app.firmware_key, "nanox/2.2.3/bitcoin/app_2.1.3_key");

        let app: AppInfo = serde_json::from_value(serde_json::json!({
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
        assert!(app.delete.is_empty());
    }
}
