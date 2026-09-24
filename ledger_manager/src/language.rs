//! Device languages: listing the language packs available for a firmware, and installing one.
//!
//! Ported from:
//! - https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/installLanguage.ts
//! - https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/uninstallLanguage.ts
//! - `getLanguagePackagesForDevice` in
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/api.ts
//!   (and libs/device-core/src/managerApi/repositories/HttpManagerApiRepository.ts)
//! - the language ids of https://github.com/LedgerHQ/ledger-live/blob/develop/libs/types-live/src/languages.ts

use crate::{
    api::url_with_params,
    api::{get_current_firmware, get_device_version, get_json, get_text, null_as_default},
    device::{ApduExchange, DeviceInfo},
    error::{Error, StatusCode},
    socket::deser_apdu_command,
    BASE_API_V1_URL,
};

use ledger_apdu::APDUCommand;
use ledger_transport_hidapi::TransportNativeHID;
use serde_derive::Deserialize;

/// The id of the English language, the default language of the devices.
pub const ENGLISH_LANGUAGE_ID: u8 = 0x00;

/// The languages of the devices, by id, with the name the Ledger API uses.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/types-live/src/languages.ts
pub const LANGUAGES: &[(u8, &str)] = &[
    (0x00, "english"),
    (0x01, "french"),
    (0x02, "spanish"),
    (0x03, "brazilian"),
    (0x04, "german"),
    (0x05, "russian"),
    (0x06, "turkish"),
    (0x07, "thai"),
];

/// The name of a language (as the Ledger API calls it, e.g. "french") from its id, as returned by
/// the device.
pub fn language_name(id: u8) -> Option<&'static str> {
    LANGUAGES.iter().find(|(i, _)| *i == id).map(|(_, n)| *n)
}

/// The id of a language from its name.
pub fn language_id(name: &str) -> Option<u8> {
    LANGUAGES.iter().find(|(_, n)| *n == name).map(|(i, _)| *i)
}

/// A human-readable name for a language id, e.g. "French". Unknown ids are shown as such.
pub fn language_display_name(id: u8) -> String {
    match id {
        0x03 => "Portuguese (Brazil)".to_string(),
        _ => match language_name(id) {
            Some(name) => {
                let mut chars = name.chars();
                chars
                    .next()
                    .map(|c| c.to_uppercase().chain(chars).collect())
                    .unwrap_or_default()
            }
            None => format!("unknown language ({:#04x})", id),
        },
    }
}

/// A language pack, as listed by the Ledger API.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/entities/LanguagePackageEntity.ts
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LanguagePackage {
    /// The language, e.g. "french". Set from the enclosing entry of the API response.
    #[serde(default)]
    pub language: String,
    /// The id of this version of the language pack.
    pub id: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub version: String,
    #[serde(default)]
    pub language_package_id: Option<i64>,
    /// The URL of the APDUs to send to the device to install the pack.
    pub apdu_install_url: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub device_versions: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub se_firmware_final_versions: Vec<i64>,
    #[serde(default)]
    pub bytes: Option<u64>,
}

/// Flatten the response of the `/language-package` endpoint into the list of all the language
/// packs, each with its language set. Entries which can't be parsed are skipped.
pub(crate) fn parse_language_packages(entries: Vec<serde_json::Value>) -> Vec<LanguagePackage> {
    let mut packages = Vec::new();
    for entry in entries {
        let language = match entry.get("language").and_then(|l| l.as_str()) {
            Some(l) => l.to_string(),
            None => continue,
        };
        let versions = match entry.get("language_package_version") {
            Some(serde_json::Value::Array(v)) => v,
            _ => continue,
        };
        for version in versions {
            match serde_json::from_value::<LanguagePackage>(version.clone()) {
                Ok(mut pack) => {
                    pack.language = language.clone();
                    packages.push(pack);
                }
                Err(e) => log::debug!("Skipping language pack ({}): {}", e, version),
            }
        }
    }
    packages
}

/// The language packs which can be installed on this device version running this final
/// firmware.
pub(crate) fn packages_for_firmware(
    packages: Vec<LanguagePackage>,
    device_version_id: i64,
    se_firmware_final_version_id: i64,
) -> Vec<LanguagePackage> {
    packages
        .into_iter()
        .filter(|p| {
            p.device_versions.contains(&device_version_id)
                && p.se_firmware_final_versions
                    .contains(&se_firmware_final_version_id)
        })
        .collect()
}

/// The pack to install for this language: the first one listed, as Ledger Live does.
pub(crate) fn select_package<'a>(
    packages: &'a [LanguagePackage],
    language: &str,
) -> Option<&'a LanguagePackage> {
    packages.iter().find(|p| p.language == language)
}

/// Get the language packs available for the firmware this device is running.
pub fn language_packages_for_device(
    device_info: &DeviceInfo,
) -> Result<Vec<LanguagePackage>, Error> {
    let provider = device_info.provider_id();
    let device_version = get_device_version(device_info.target_id, provider)?;
    let firmware = get_current_firmware(&device_info.version, device_version.id, provider)?;
    let url = url_with_params(&format!("{}/language-package", BASE_API_V1_URL), &[]);
    let entries: Vec<serde_json::Value> = get_json(&url)?;
    Ok(packages_for_firmware(
        parse_language_packages(entries),
        device_version.id,
        firmware.id,
    ))
}

/// Parse the APDUs of a language pack: one hex-encoded APDU per line.
pub(crate) fn parse_language_apdus(text: &str) -> Result<Vec<Vec<u8>>, Error> {
    text.split('\n')
        .map(|l| l.trim_end_matches('\r').trim())
        .filter(|l| !l.is_empty())
        .map(|l| {
            hex::decode(l).map_err(|e| {
                Error::InvalidDeviceData(format!("invalid APDU in language pack: {}", e))
            })
        })
        .collect()
}

/// The APDU uninstalling all the language packs (`p1` 0xff) or the pack of a given language id.
pub(crate) fn uninstall_language_command(id: u8) -> APDUCommand<Vec<u8>> {
    APDUCommand {
        cla: 0xe0,
        ins: 0x33,
        p1: id,
        p2: 0x00,
        data: Vec::new(),
    }
}

/// Uninstall all the language packs of the device (it then uses English).
pub(crate) fn uninstall_all_languages<T: ApduExchange>(transport: &T) -> Result<(), Error> {
    let resp = transport.exchange_apdu(&uninstall_language_command(0xff))?;
    // Expected responses when uninstalling.
    match resp.retcode() {
        0x9000 | 0x5501 => Ok(()),
        s => Err(Error::DeviceStatus(s)),
    }
}

/// A step of a language pack installation.
#[derive(Debug, Clone, PartialEq)]
pub enum LanguageInstallStep {
    /// Downloading the language pack.
    Downloading,
    /// The user must allow the installation of the language pack on the device.
    PermissionRequested,
    /// Sending the language pack to the device. `progress` is between 0 and 1.
    Installing { progress: f32 },
}

/// Send the APDUs of a language pack to the device, after uninstalling the language packs
/// already installed.
pub(crate) fn install_language_apdus<T: ApduExchange, P: FnMut(LanguageInstallStep)>(
    transport: &T,
    apdus: &[Vec<u8>],
    mut progress: P,
) -> Result<(), Error> {
    // Check all the APDUs before sending anything.
    let commands = apdus
        .iter()
        .map(|raw| deser_apdu_command(raw).map(|c| (raw.starts_with(&[0xe0, 0x30]), c)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::InvalidDeviceData(format!("invalid language pack: {}", e)))?;

    uninstall_all_languages(transport)?;

    let total = commands.len();
    for (i, (asks_permission, command)) in commands.iter().enumerate() {
        if *asks_permission {
            progress(LanguageInstallStep::PermissionRequested);
        }
        let resp = transport.exchange_apdu(command)?;
        match resp.retcode() {
            s if s == StatusCode::OK as u16 => {}
            s if s == StatusCode::UserRefusedOnDevice as u16 => {
                return Err(Error::LanguageInstallRefusedOnDevice)
            }
            s if s == StatusCode::NotEnoughSpace as u16 => return Err(Error::NotEnoughSpace),
            s if s == StatusCode::LockedDevice as u16 => return Err(Error::DeviceLocked),
            s => return Err(Error::DeviceStatus(s)),
        }
        progress(LanguageInstallStep::Installing {
            progress: (i + 1) as f32 / total as f32,
        });
    }
    Ok(())
}

/// Install the pack of this language (e.g. "french", see `LANGUAGES`) for the firmware the device
/// is running, and switch the device to it. The user has to allow it on the device. Installing
/// "english" uninstalls all the language packs.
///
/// The device must be on its dashboard.
pub fn install_language<P: FnMut(LanguageInstallStep)>(
    transport: &TransportNativeHID,
    device_info: &DeviceInfo,
    language: &str,
    mut progress: P,
) -> Result<(), Error> {
    device_info.check_normal_mode()?;
    if language == "english" {
        return uninstall_all_languages(transport);
    }
    progress(LanguageInstallStep::Downloading);
    let packages = language_packages_for_device(device_info)?;
    let pack = select_package(&packages, language)
        .ok_or_else(|| Error::LanguageNotFound(language.to_string()))?;
    log::info!(
        "Installing language pack {} {} ({}).",
        pack.language,
        pack.version,
        pack.apdu_install_url
    );
    let apdus = parse_language_apdus(&get_text(&pack.apdu_install_url)?)?;
    install_language_apdus(transport, &apdus, progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::tests_support::MockDevice;

    #[test]
    fn language_ids() {
        assert_eq!(language_name(0), Some("english"));
        assert_eq!(language_name(1), Some("french"));
        assert_eq!(language_name(7), Some("thai"));
        assert_eq!(language_name(8), None);
        assert_eq!(language_id("german"), Some(4));
        assert_eq!(language_id("qa-lingua"), None);
        assert_eq!(language_display_name(1), "French");
        assert_eq!(language_display_name(3), "Portuguese (Brazil)");
        assert_eq!(language_display_name(0x42), "unknown language (0x42)");
    }

    // A trimmed response of https://manager.api.live.ledger.com/api/language-package
    const FIXTURE: &str = include_str!("../tests/data/language_packages.json");

    #[test]
    fn language_package_selection() {
        let entries: Vec<serde_json::Value> = serde_json::from_str(FIXTURE).unwrap();
        let all = parse_language_packages(entries);
        assert!(all.len() > 7);
        assert!(all.iter().all(|p| !p.language.is_empty()));

        // Stax (device version 17) running 1.10.0 (final firmware 559).
        let stax = packages_for_firmware(all.clone(), 17, 559);
        let mut languages: Vec<&str> = stax.iter().map(|p| p.language.as_str()).collect();
        languages.sort_unstable();
        assert_eq!(
            languages,
            vec![
                "brazilian",
                "english",
                "french",
                "german",
                "russian",
                "spanish",
                "turkish"
            ]
        );
        let fr = select_package(&stax, "french").unwrap();
        assert_eq!(fr.id, 983);
        assert_eq!(
            fr.apdu_install_url,
            "https://download.languages.ledger.com/stax/french/bolos_1.10.0_pack_0.0.4_fr.apdu"
        );
        assert!(select_package(&stax, "thai").is_none());

        // Another firmware of the same device.
        let other = packages_for_firmware(all.clone(), 17, 535);
        assert!(other.iter().all(|p| p.id != 983));
        // Unknown firmware.
        assert!(packages_for_firmware(all, 17, 1).is_empty());
    }

    #[test]
    fn language_apdus() {
        let apdus =
            parse_language_apdus("e03001000400007840\r\ne0310100020102\n\ne032010000\n").unwrap();
        assert_eq!(apdus.len(), 3);
        assert_eq!(apdus[0], hex::decode("e03001000400007840").unwrap());
        assert!(parse_language_apdus("e0zz").is_err());
        assert!(parse_language_apdus("").unwrap().is_empty());
    }

    #[test]
    fn language_install() {
        let apdus = parse_language_apdus("e03001000400007840\ne0310100020102\ne032010000").unwrap();
        let device = MockDevice::new(|c| match (c.ins, c.p1) {
            (0x33, 0xff) => (vec![], 0x5501),
            _ => (vec![], 0x9000),
        });
        let mut steps = Vec::new();
        install_language_apdus(&device, &apdus, |s| steps.push(s)).unwrap();
        let sent = device.sent();
        assert_eq!(sent.len(), 4);
        assert_eq!(sent[0], vec![0xe0, 0x33, 0xff, 0x00, 0x00]);
        assert_eq!(sent[1], hex::decode("e03001000400007840").unwrap());
        assert_eq!(sent[3], hex::decode("e032010000").unwrap());
        assert_eq!(steps[0], LanguageInstallStep::PermissionRequested);
        assert_eq!(
            steps.last(),
            Some(&LanguageInstallStep::Installing { progress: 1.0 })
        );

        // Refused on the device.
        let device = MockDevice::new(|c| match c.ins {
            0x30 => (vec![], 0x5501),
            _ => (vec![], 0x9000),
        });
        assert!(matches!(
            install_language_apdus(&device, &apdus, |_| {}),
            Err(Error::LanguageInstallRefusedOnDevice)
        ));
        // Not enough space.
        let device = MockDevice::new(|c| match c.ins {
            0x31 => (vec![], 0x5102),
            _ => (vec![], 0x9000),
        });
        assert!(matches!(
            install_language_apdus(&device, &apdus, |_| {}),
            Err(Error::NotEnoughSpace)
        ));
        // Uninstalling failed.
        let device = MockDevice::new(|_| (vec![], 0x6d00));
        assert!(matches!(
            install_language_apdus(&device, &apdus, |_| {}),
            Err(Error::DeviceStatus(0x6d00))
        ));
        // An invalid APDU is detected before sending anything.
        let device = MockDevice::new(|_| (vec![], 0x9000));
        assert!(install_language_apdus(&device, &[vec![0xe0]], |_| {}).is_err());
        assert!(device.sent().is_empty());
    }
}
