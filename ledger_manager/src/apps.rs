//! The Bitcoin apps: listing the installed apps, installing, updating and opening the Bitcoin
//! apps. Also the genuine check, which works the same way as an app installation.

use crate::{
    api::{apps_by_hashes, catalog_apps, current_firmware, AppInfo},
    device::{apdu, quit_app, DeviceInfo},
    error::*,
    socket::{run_socket, socket_url, Context, SocketEvent, Transport},
};

use ledger_transport_hidapi::TransportNativeHID;

use std::{thread, time::Duration};

pub const BITCOIN_APP_NAME: &str = "Bitcoin";
pub const BITCOIN_TEST_APP_NAME: &str = "Bitcoin Test";
pub(crate) const BITCOIN_APPS: &[&str] = &[BITCOIN_APP_NAME, BITCOIN_TEST_APP_NAME];

fn bitcoin_app_name(is_testnet: bool) -> &'static str {
    if is_testnet {
        BITCOIN_TEST_APP_NAME
    } else {
        BITCOIN_APP_NAME
    }
}

// Retry parameters of the app installation.
// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/installApp.ts
const APP_INSTALL_RETRY_DELAY: Duration = Duration::from_millis(500);
const APP_INSTALL_RETRY_LIMIT: usize = 5;

/// Ledger Live waits a bit between two app operations, as older firmwares misbehave when actions
/// are performed too closely (`MANAGER_INSTALL_DELAY` in apps/runner.ts).
pub(crate) const MANAGER_INSTALL_DELAY: Duration = Duration::from_millis(1000);

/// An app installed on the device, as listed by the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    pub name: String,
    pub hash: Vec<u8>,
    /// All zeros for what isn't really an app (language packs, sideloaded apps).
    pub hash_code_data: Vec<u8>,
}

/// Parse a page of the answer to the ListApps APDU (without the status word).
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/listApps.ts
fn parse_list_apps_response(data: &[u8]) -> Result<Vec<InstalledApp>, Error> {
    let invalid = |s: &str| Error::InvalidDeviceData(format!("listApps: {}", s));
    let mut apps = Vec::new();
    if data.is_empty() {
        return Ok(apps);
    }
    if data[0] != 0x01 {
        return Err(invalid("unknown format"));
    }
    let mut i = 1;
    while i < data.len() {
        // Length, blocks (u16), flags (u16), code data hash, full hash, name length.
        if data.len() < i + 1 + 2 + 2 + 32 + 32 + 1 {
            return Err(invalid("not enough data"));
        }
        let len = data[i] as usize;
        let hash_code_data = data[i + 5..i + 37].to_vec();
        let hash = data[i + 37..i + 69].to_vec();
        let name_len = data[i + 69] as usize;
        i += 70;
        if data.len() < i + name_len {
            return Err(invalid("not enough data"));
        }
        if len != name_len + 70 {
            return Err(invalid("invalid length data"));
        }
        let name = String::from_utf8_lossy(&data[i..i + name_len]).into_owned();
        i += name_len;
        apps.push(InstalledApp {
            name,
            hash,
            hash_code_data,
        });
    }
    Ok(apps)
}

/// List the apps installed on the device. The user may have to allow the Ledger manager.
pub fn list_installed_apps_raw(transport: &TransportNativeHID) -> Result<Vec<InstalledApp>, Error> {
    let check = |status| match status {
        SW_OK => Ok(()),
        SW_LOCKED => Err(Error::DeviceLocked),
        SW_USER_REFUSED | SW_CONDITIONS_NOT_SATISFIED => {
            Err(Error::RefusedOnDevice("The Ledger manager"))
        }
        // ListApps is handled by the dashboard, an app answers it with these.
        SW_INS_NOT_SUPPORTED | SW_CLA_NOT_SUPPORTED => Err(Error::DeviceOnDashboardExpected),
        s => Err(Error::DeviceStatus(s)),
    };
    // https://github.com/LedgerHQ/ledger-live/blob/99879eb5bada1ecaea7a02d8886e16b44657af6d/libs/ledger-live-common/src/hw/listApps.ts
    let mut answer = transport.exchange(&apdu(0xe0, 0xde, 0x00, vec![]))?;
    check(answer.retcode())?;
    let mut apps = Vec::new();
    while !answer.data().is_empty() {
        apps.extend(parse_list_apps_response(answer.data())?);
        answer = transport.exchange(&apdu(0xe0, 0xdf, 0x00, vec![]))?;
        check(answer.retcode())?;
    }
    Ok(apps)
}

/// The installed apps, as known by the Ledger API (`None` for the ones it doesn't know).
pub fn list_installed_apps(transport: &TransportNativeHID) -> Result<Vec<Option<AppInfo>>, Error> {
    // Like Ledger Live (apps/listApps.ts), ignore what isn't really an app.
    let hashes = list_installed_apps_raw(transport)?
        .into_iter()
        .filter(|a| a.hash_code_data.iter().any(|b| *b != 0))
        .map(|a| a.hash)
        .collect();
    apps_by_hashes(hashes)
}

/// Find this app among the installed ones. The device names are compared case-insensitively.
pub(crate) fn find_app<'a>(apps: &'a [InstalledApp], name: &str) -> Option<&'a InstalledApp> {
    apps.iter().find(|a| a.name.eq_ignore_ascii_case(name))
}

/// The latest (mainnet, testnet) Bitcoin apps available for this device.
pub fn get_latest_apps(
    device_info: &DeviceInfo,
) -> Result<(Option<AppInfo>, Option<AppInfo>), Error> {
    device_info.check_normal_mode()?;
    let apps = catalog_apps(device_info, BITCOIN_APPS)?;
    let find = |name| apps.iter().find(|a| a.version_name == name).cloned();
    Ok((find(BITCOIN_APP_NAME), find(BITCOIN_TEST_APP_NAME)))
}

fn latest_app(device_info: &DeviceInfo, is_testnet: bool) -> Result<AppInfo, Error> {
    let (main, test) = get_latest_apps(device_info)?;
    if is_testnet { test } else { main }.ok_or_else(|| {
        Error::Other(format!(
            "The {} app is not available for this device in the Ledger catalog.",
            bitcoin_app_name(is_testnet)
        ))
    })
}

/// Open the Bitcoin app on the device.
/// https://github.com/LedgerHQ/ledger-live/blob/5a0a1aa5dc183116839851b79bceb6704f1de4b9/libs/ledger-live-common/src/hw/openApp.ts
pub fn open_bitcoin_app(transport: &TransportNativeHID, is_testnet: bool) -> Result<(), Error> {
    let name = bitcoin_app_name(is_testnet).as_bytes().to_vec();
    match transport.exchange(&apdu(0xe0, 0xd8, 0x00, name))?.retcode() {
        SW_OK => Ok(()),
        SW_LOCKED => Err(Error::DeviceLocked),
        SW_USER_REFUSED => Err(Error::RefusedOnDevice("Opening the app")),
        s => Err(Error::DeviceStatus(s)),
    }
}

/// Check the device is genuine. Returns `Error::NotGenuine` if it is not. The user may have to
/// allow the Ledger manager (`SocketEvent::DevicePermissionRequested`).
///
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/genuineCheck.ts
/// and https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/hooks/useGenuineCheck.ts
pub fn genuine_check(
    transport: &TransportNativeHID,
    on_event: impl FnMut(SocketEvent),
) -> Result<(), Error> {
    let device_info = DeviceInfo::new(transport)?;
    device_info.check_normal_mode()?;
    let firmware = current_firmware(&device_info)?;
    let url = socket_url(
        "genuine",
        &[
            ("targetId", &device_info.target_id.to_string()),
            ("perso", &firmware.perso),
        ],
    );
    let payload = match run_socket(
        Transport::Native(transport),
        &url,
        Context::GenuineCheck,
        on_event,
    )? {
        Some(serde_json::Value::String(s)) => s,
        Some(v) => v.to_string(),
        None => String::new(),
    };
    if payload == "0000" {
        Ok(())
    } else {
        Err(Error::NotGenuine(payload))
    }
}

/// A step of an app installation or update.
#[derive(Debug, Clone, PartialEq)]
pub enum AppInstallStep {
    /// Listing the installed apps (the user may have to allow the Ledger manager).
    ListingApps,
    QueryingApi,
    /// The user must allow the Ledger manager on the device.
    AllowManagerRequested,
    AllowManagerGranted,
    /// Uninstalling the previous version of the app. `progress` is between 0 and 1.
    Uninstalling {
        progress: f32,
    },
    /// `progress` is between 0 and 1.
    Installing {
        progress: f32,
    },
    /// Retrying after an error.
    Retrying {
        attempt: usize,
        error: String,
    },
    Done,
}

/// Run an "install" socket session to install the app, or to uninstall it if `uninstall` (then
/// its `delete` firmware is installed). See hw/installApp.ts and hw/uninstallApp.ts.
fn run_install_socket(
    transport: &TransportNativeHID,
    target_id: u32,
    app: &AppInfo,
    uninstall: bool,
    progress: &mut impl FnMut(AppInstallStep),
) -> Result<(), Error> {
    let (firmware, firmware_key) = if uninstall {
        (&app.delete, &app.delete_key)
    } else {
        (&app.firmware, &app.firmware_key)
    };
    let url = socket_url(
        "install",
        &[
            ("targetId", &target_id.to_string()),
            ("perso", &app.perso),
            ("deleteKey", &app.delete_key),
            ("firmware", firmware),
            ("firmwareKey", firmware_key),
            ("hash", &app.hash),
        ],
    );
    run_socket(Transport::Native(transport), &url, Context::App, |e| {
        progress(match e {
            SocketEvent::DevicePermissionRequested => AppInstallStep::AllowManagerRequested,
            SocketEvent::DevicePermissionGranted => AppInstallStep::AllowManagerGranted,
            SocketEvent::BulkProgress { .. } if uninstall => AppInstallStep::Uninstalling {
                progress: e.bulk_progress(),
            },
            SocketEvent::BulkProgress { .. } => AppInstallStep::Installing {
                progress: e.bulk_progress(),
            },
        })
    })?;
    Ok(())
}

/// Install this app on the device, retrying on errors as Ledger Live does.
pub(crate) fn install_app(
    transport: &TransportNativeHID,
    target_id: u32,
    app: &AppInfo,
    mut progress: impl FnMut(AppInstallStep),
) -> Result<(), Error> {
    quit_app(transport)?;
    let mut attempt = 0;
    loop {
        match run_install_socket(transport, target_id, app, false, &mut progress) {
            Ok(()) => return Ok(()),
            // Ledger Live does not retry on a locked device. We don't retry either on the errors
            // which won't go away by retrying (in Ledger Live the user would retry manually).
            Err(
                e @ (Error::DeviceLocked
                | Error::RefusedOnDevice(_)
                | Error::NotEnoughSpace
                | Error::AppAlreadyInstalled
                | Error::DeviceOnDashboardExpected),
            ) => return Err(e),
            Err(e) if attempt >= APP_INSTALL_RETRY_LIMIT => return Err(e),
            Err(e) => {
                attempt += 1;
                log::warn!(
                    "Retrying ({}/{}) app install on error: {}",
                    attempt,
                    APP_INSTALL_RETRY_LIMIT,
                    e
                );
                progress(AppInstallStep::Retrying {
                    attempt,
                    error: e.to_string(),
                });
                thread::sleep(APP_INSTALL_RETRY_DELAY);
            }
        }
    }
}

/// Quit the open app, if any, and get the information of the device, checking it runs its
/// firmware normally.
fn dashboard_device_info(transport: &TransportNativeHID) -> Result<DeviceInfo, Error> {
    if let Err(e) = quit_app(transport) {
        // Give a clearer error if this is because the device is in bootloader or updater mode.
        if let Ok(info) = DeviceInfo::new(transport) {
            info.check_normal_mode()?;
        }
        return Err(e);
    }
    let device_info = DeviceInfo::new(transport)?;
    device_info.check_normal_mode()?;
    Ok(device_info)
}

/// Install the Bitcoin app (or the Bitcoin Test app if `is_testnet`) on the device.
pub fn install_bitcoin_app(
    transport: &TransportNativeHID,
    is_testnet: bool,
    mut progress: impl FnMut(AppInstallStep),
) -> Result<(), Error> {
    let device_info = dashboard_device_info(transport)?;
    progress(AppInstallStep::ListingApps);
    if find_app(
        &list_installed_apps_raw(transport)?,
        bitcoin_app_name(is_testnet),
    )
    .is_some()
    {
        return Err(Error::AppAlreadyInstalled);
    }
    progress(AppInstallStep::QueryingApi);
    let app = latest_app(&device_info, is_testnet)?;
    install_app(transport, device_info.target_id, &app, &mut progress)?;
    progress(AppInstallStep::Done);
    Ok(())
}

type SemverKey = (u64, u64, u64, bool, Vec<Result<u64, String>>);

/// Parse a version for comparison, strictly like `semver.valid`: "1.2.3", "1.2.3-rc.1" or
/// "1.2.3+build" (a leading "v" or "=" is tolerated). The key orders versions like semver: a
/// version without pre-release is higher, pre-release identifiers compare numerically if
/// numeric, lexically otherwise, and numeric ones are lower.
fn semver_key(s: &str) -> Option<SemverKey> {
    let s = s.trim();
    let s = s.strip_prefix(['v', '=']).unwrap_or(s);
    let s = s.split('+').next()?;
    let (core, pre) = match s.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (s, None),
    };
    let num = |s: &str| {
        let valid = !s.is_empty()
            && s.chars().all(|c| c.is_ascii_digit())
            && !(s.len() > 1 && s.starts_with('0'));
        valid.then(|| s.parse::<u64>().ok()).flatten()
    };
    let mut parts = core.split('.');
    let (major, minor, patch) = (
        num(parts.next()?)?,
        num(parts.next()?)?,
        num(parts.next()?)?,
    );
    if parts.next().is_some() {
        return None;
    }
    let mut ids = Vec::new();
    for id in pre.map(|p| p.split('.')).into_iter().flatten() {
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return None;
        }
        ids.push(id.parse::<u64>().map_err(|_| id.to_string()));
    }
    Some((major, minor, patch, ids.is_empty(), ids))
}

/// Whether the available version of an app is an update of the installed one. If a version
/// can't be parsed, whether the hashes differ.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/apps/listApps.ts (`isUpdateAvailable`)
fn is_app_update_available(
    installed_version: Option<&str>,
    available_version: &str,
    installed_hash: &str,
    available_hash: &str,
) -> bool {
    match (
        installed_version.and_then(semver_key),
        semver_key(available_version),
    ) {
        (Some(installed), Some(available)) => available > installed,
        _ => !available_hash.eq_ignore_ascii_case(installed_hash),
    }
}

/// Update the Bitcoin app (or the Bitcoin Test app if `is_testnet`).
///
/// Like Ledger Live, the app is uninstalled (using the `delete` firmware of the latest version
/// from the catalog) and then the latest version is installed. See the "updateAll" action in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/apps/logic.ts
/// and https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/apps/runner.ts
pub fn update_bitcoin_app(
    transport: &TransportNativeHID,
    is_testnet: bool,
    mut progress: impl FnMut(AppInstallStep),
) -> Result<(), Error> {
    let device_info = dashboard_device_info(transport)?;
    progress(AppInstallStep::ListingApps);
    let apps = list_installed_apps_raw(transport)?;
    let installed = find_app(&apps, bitcoin_app_name(is_testnet)).ok_or(Error::AppNotInstalled)?;
    progress(AppInstallStep::QueryingApi);
    let installed_version = apps_by_hashes(vec![installed.hash.clone()])?
        .into_iter()
        .next()
        .flatten()
        .map(|a| a.version);
    let latest = latest_app(&device_info, is_testnet)?;
    if !is_app_update_available(
        installed_version.as_deref(),
        &latest.version,
        &hex::encode(&installed.hash),
        &latest.hash,
    ) {
        return Err(Error::AppAlreadyLatest);
    }
    log::info!(
        "Updating {} from {} to {}.",
        latest.version_name,
        installed_version.as_deref().unwrap_or("an unknown version"),
        latest.version
    );

    // https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/uninstallApp.ts
    if latest.delete.is_empty() {
        return Err(Error::Other(format!(
            "No uninstall firmware for the {} app.",
            latest.version_name
        )));
    }
    quit_app(transport)?;
    run_install_socket(
        transport,
        device_info.target_id,
        &latest,
        true,
        &mut progress,
    )?;
    thread::sleep(MANAGER_INSTALL_DELAY);
    install_app(transport, device_info.target_id, &latest, &mut progress)?;
    progress(AppInstallStep::Done);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_entry(name: &str, fill: u8) -> Vec<u8> {
        let mut e = vec![(name.len() + 70) as u8, 0x01, 0x02, 0x0a, 0x00];
        e.extend_from_slice(&[fill; 32]);
        e.extend_from_slice(&[fill.wrapping_add(1); 32]);
        e.push(name.len() as u8);
        e.extend_from_slice(name.as_bytes());
        e
    }

    #[test]
    fn list_apps_parsing() {
        let mut data = vec![0x01];
        data.extend(app_entry("Bitcoin", 0xaa));
        data.extend(app_entry("Bitcoin Test", 0x00));
        let apps = parse_list_apps_response(&data).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "Bitcoin");
        assert_eq!(apps[0].hash_code_data, vec![0xaa; 32]);
        assert_eq!(apps[0].hash, vec![0xab; 32]);
        assert_eq!(apps[1].name, "Bitcoin Test");
        assert_eq!(find_app(&apps, "bitcoin test"), Some(&apps[1]));

        assert!(parse_list_apps_response(&[]).unwrap().is_empty());
        // Unknown format.
        assert!(parse_list_apps_response(&[0x02]).is_err());
        // Truncated.
        assert!(parse_list_apps_response(&data[..data.len() - 1]).is_err());
        // Invalid length.
        let mut bad = vec![0x01];
        let mut e = app_entry("Bitcoin", 1);
        e[0] += 1;
        bad.extend(e);
        assert!(parse_list_apps_response(&bad).is_err());
    }

    #[test]
    fn app_update_available() {
        assert!(is_app_update_available(Some("2.1.2"), "2.1.3", "aa", "bb"));
        assert!(!is_app_update_available(Some("2.1.3"), "2.1.3", "aa", "bb"));
        assert!(!is_app_update_available(Some("2.2.0"), "2.1.3", "aa", "bb"));
        assert!(is_app_update_available(
            Some("2.1.3-rc1"),
            "2.1.3",
            "aa",
            "aa"
        ));
        assert!(!is_app_update_available(
            Some("2.1.3"),
            "2.1.3-rc1",
            "aa",
            "bb"
        ));
        assert!(is_app_update_available(
            Some("2.1.3-rc.2"),
            "2.1.3-rc.10",
            "aa",
            "aa"
        ));
        assert!(is_app_update_available(
            Some("2.1.3-1"),
            "2.1.3-alpha",
            "aa",
            "aa"
        ));
        assert!(is_app_update_available(
            Some("2.1.3-rc"),
            "2.1.3-rc.1",
            "aa",
            "aa"
        ));
        assert!(!is_app_update_available(
            Some("v2.1.3+build"),
            "=2.1.3",
            "aa",
            "bb"
        ));
        // Falls back to comparing hashes.
        assert!(is_app_update_available(None, "2.1.3", "aa", "bb"));
        assert!(!is_app_update_available(None, "2.1.3", "aa", "AA"));
        assert!(is_app_update_available(Some("weird"), "2.1.3", "aa", "bb"));
        assert!(is_app_update_available(Some("01.2.3"), "1.2.3", "aa", "bb"));
        assert!(is_app_update_available(Some("1.2"), "1.2.3", "aa", "bb"));
    }
}
