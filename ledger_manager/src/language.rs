//! The language of the device: listing the language packs available for a firmware, and
//! installing one.
//!
//! Ported from:
//! - https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/installLanguage.ts
//! - https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/uninstallLanguage.ts
//! - `getLanguagePackagesForDevice` in
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/api.ts

use crate::{
    api::{
        device_version_id, get_final_firmware, get_json, get_text, null_as_default,
        url_with_params, BASE_API_V1_URL,
    },
    device::{apdu, ApduExchange, DeviceInfo},
    error::*,
    socket::deser_apdu_command,
};

use serde_derive::Deserialize;

pub(crate) const ENGLISH_LANGUAGE_ID: u8 = 0x00;

/// The name of a language as the Ledger API calls it, from its id as returned by the device.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/types-live/src/languages.ts
pub(crate) fn language_name(id: u8) -> Option<&'static str> {
    let names = [
        "english",
        "french",
        "spanish",
        "brazilian",
        "german",
        "russian",
        "turkish",
        "thai",
    ];
    names.get(id as usize).copied()
}

/// The name of a language for humans, e.g. "French".
pub(crate) fn language_display_name(id: u8) -> String {
    match (id, language_name(id)) {
        (0x03, _) => "Portuguese (Brazil)".to_string(),
        (_, Some(name)) => name[..1].to_uppercase() + &name[1..],
        (_, None) => format!("unknown language ({:#04x})", id),
    }
}

/// A language pack, as listed by the Ledger API
/// (libs/device-core/src/managerApi/entities/LanguagePackageEntity.ts).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LanguagePackage {
    /// The language, e.g. "french". Set from the enclosing entry of the API response.
    #[serde(default)]
    pub language: String,
    pub id: i64,
    #[serde(default, deserialize_with = "null_as_default")]
    pub version: String,
    /// The URL of the APDUs to send to the device to install the pack.
    pub apdu_install_url: String,
    #[serde(default, deserialize_with = "null_as_default")]
    pub device_versions: Vec<i64>,
    #[serde(default, deserialize_with = "null_as_default")]
    pub se_firmware_final_versions: Vec<i64>,
}

/// The language packs of the response of the `/language-package` endpoint which can be installed
/// on this device version running this final firmware. Entries which can't be parsed are skipped.
fn packages_for_firmware(
    entries: Vec<serde_json::Value>,
    device_version_id: i64,
    final_firmware_id: i64,
) -> Vec<LanguagePackage> {
    let mut packages = Vec::new();
    for entry in entries {
        let (Some(language), Some(serde_json::Value::Array(versions))) = (
            entry.get("language").and_then(|l| l.as_str()),
            entry.get("language_package_version"),
        ) else {
            continue;
        };
        for version in versions {
            match serde_json::from_value::<LanguagePackage>(version.clone()) {
                Ok(pack)
                    if pack.device_versions.contains(&device_version_id)
                        && pack.se_firmware_final_versions.contains(&final_firmware_id) =>
                {
                    packages.push(LanguagePackage {
                        language: language.to_string(),
                        ..pack
                    })
                }
                Ok(_) => {}
                Err(e) => log::debug!("Skipping language pack ({}): {}", e, version),
            }
        }
    }
    packages
}

/// Get the language packs available for the firmware the device is running.
pub fn language_packages_for_device(
    device_info: &DeviceInfo,
) -> Result<Vec<LanguagePackage>, Error> {
    let provider = device_info.provider;
    let device_version = device_version_id(device_info.target_id, provider)?;
    let firmware = get_final_firmware(&device_info.version, device_version, provider)?;
    let url = url_with_params(&format!("{}/language-package", BASE_API_V1_URL), &[]);
    Ok(packages_for_firmware(
        get_json(&url)?,
        device_version,
        firmware.id,
    ))
}

/// Parse the APDUs of a language pack: one hex-encoded APDU per line.
fn parse_language_apdus(text: &str) -> Result<Vec<Vec<u8>>, Error> {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| {
            hex::decode(l).map_err(|e| {
                Error::InvalidDeviceData(format!("invalid APDU in language pack: {}", e))
            })
        })
        .collect()
}

/// Uninstall all the language packs of the device (it then uses English).
fn uninstall_all_languages(transport: &impl ApduExchange) -> Result<(), Error> {
    match transport
        .exchange_apdu(&apdu(0xe0, 0x33, 0xff, vec![]))?
        .retcode()
    {
        // Expected responses when uninstalling.
        SW_OK | SW_USER_REFUSED => Ok(()),
        s => Err(Error::DeviceStatus(s)),
    }
}

/// A step of a language pack installation.
#[derive(Debug, Clone, PartialEq)]
pub enum LanguageInstallStep {
    Downloading,
    /// The user must allow the installation of the language pack on the device.
    PermissionRequested,
    /// Sending the language pack to the device. `progress` is between 0 and 1.
    Installing {
        progress: f32,
    },
}

/// Send the APDUs of a language pack to the device, after uninstalling the language packs
/// already installed.
fn install_language_apdus(
    transport: &impl ApduExchange,
    apdus: &[Vec<u8>],
    mut progress: impl FnMut(LanguageInstallStep),
) -> Result<(), Error> {
    // Check all the APDUs before sending anything.
    let commands = apdus
        .iter()
        .map(|raw| deser_apdu_command(raw).map(|c| (raw.starts_with(&[0xe0, 0x30]), c)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::InvalidDeviceData(format!("invalid language pack: {}", e)))?;

    uninstall_all_languages(transport)?;
    for (i, (asks_permission, command)) in commands.iter().enumerate() {
        if *asks_permission {
            progress(LanguageInstallStep::PermissionRequested);
        }
        match transport.exchange_apdu(command)?.retcode() {
            SW_OK => {}
            SW_USER_REFUSED => return Err(Error::RefusedOnDevice("The language installation")),
            SW_NOT_ENOUGH_SPACE => return Err(Error::NotEnoughSpace),
            SW_LOCKED => return Err(Error::DeviceLocked),
            s => return Err(Error::DeviceStatus(s)),
        }
        progress(LanguageInstallStep::Installing {
            progress: (i + 1) as f32 / commands.len() as f32,
        });
    }
    Ok(())
}

/// Install the pack of this language (e.g. "french") for the firmware the device is running, and
/// switch the device to it. The user has to allow it on the device. Installing "english"
/// uninstalls all the language packs. The device must be on its dashboard.
pub(crate) fn install_language(
    transport: &impl ApduExchange,
    device_info: &DeviceInfo,
    language: &str,
    mut progress: impl FnMut(LanguageInstallStep),
) -> Result<(), Error> {
    device_info.check_normal_mode()?;
    if language == "english" {
        return uninstall_all_languages(transport);
    }
    progress(LanguageInstallStep::Downloading);
    let packages = language_packages_for_device(device_info)?;
    // Like Ledger Live, the first pack listed for the language.
    let pack = packages
        .iter()
        .find(|p| p.language == language)
        .ok_or_else(|| {
            Error::Other(format!(
                "No '{}' language pack is available for the firmware of the device.",
                language
            ))
        })?;
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
    use crate::device::MockDevice;

    #[test]
    fn language_names() {
        assert_eq!(language_name(0), Some("english"));
        assert_eq!(language_name(7), Some("thai"));
        assert_eq!(language_name(8), None);
        assert_eq!(language_display_name(1), "French");
        assert_eq!(language_display_name(3), "Portuguese (Brazil)");
        assert_eq!(language_display_name(0x42), "unknown language (0x42)");
    }

    // A trimmed response of https://manager.api.live.ledger.com/api/language-package
    const FIXTURE: &str = include_str!("../tests/data/language_packages.json");

    #[test]
    fn language_package_selection() {
        let entries: Vec<serde_json::Value> = serde_json::from_str(FIXTURE).unwrap();
        // Stax (device version 17) running 1.10.0 (final firmware 559).
        let stax = packages_for_firmware(entries.clone(), 17, 559);
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
        let fr = stax.iter().find(|p| p.language == "french").unwrap();
        assert_eq!(fr.id, 983);
        assert_eq!(
            fr.apdu_install_url,
            "https://download.languages.ledger.com/stax/french/bolos_1.10.0_pack_0.0.4_fr.apdu"
        );
        // Another firmware of the same device.
        let other = packages_for_firmware(entries.clone(), 17, 535);
        assert!(!other.is_empty() && other.iter().all(|p| p.id != 983));
        // Unknown firmware.
        assert!(packages_for_firmware(entries, 17, 1).is_empty());
    }

    #[test]
    fn language_install() {
        let apdus =
            parse_language_apdus("e03001000400007840\r\ne0310100020102\n\ne032010000\n").unwrap();
        assert_eq!(apdus.len(), 3);
        assert!(parse_language_apdus("e0zz").is_err());

        let device = MockDevice::new(|c| match (c.ins, c.p1) {
            (0x33, 0xff) => (vec![], 0x5501),
            _ => (vec![], 0x9000),
        });
        let mut steps = Vec::new();
        install_language_apdus(&device, &apdus, |s| steps.push(s)).unwrap();
        let sent = device.sent.borrow();
        assert_eq!(sent.len(), 4);
        assert_eq!(sent[0], vec![0xe0, 0x33, 0xff, 0x00, 0x00]);
        assert_eq!(sent[1], hex::decode("e03001000400007840").unwrap());
        assert_eq!(sent[3], hex::decode("e032010000").unwrap());
        assert_eq!(steps[0], LanguageInstallStep::PermissionRequested);
        assert_eq!(
            steps.last(),
            Some(&LanguageInstallStep::Installing { progress: 1.0 })
        );

        // Refused on the device, not enough space, uninstalling failed.
        for (ins, status, error) in [
            (
                0x30,
                0x5501,
                "The language installation was refused on the device.",
            ),
            (
                0x31,
                0x5102,
                "Not enough space on the device. Uninstall some applications and retry.",
            ),
            (0x33, 0x6d00, "Device returned status 0x6d00"),
        ] {
            let device = MockDevice::new(move |c| match c.ins {
                i if i == ins => (vec![], status),
                _ => (vec![], 0x9000),
            });
            let res = install_language_apdus(&device, &apdus, |_| {});
            assert_eq!(res.unwrap_err().to_string(), error);
        }
        // An invalid APDU is detected before sending anything.
        let device = MockDevice::new(|_| (vec![], 0x9000));
        assert!(install_language_apdus(&device, &[vec![0xe0]], |_| {}).is_err());
        assert!(device.sent.borrow().is_empty());
    }
}
