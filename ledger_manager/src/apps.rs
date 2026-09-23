//! Managing the Bitcoin applications installed on the device.

use crate::{
    api::{bitcoin_apps_by_hashes, bitcoin_apps_catalog, current_firmware, BitcoinAppInfo},
    device::{quit_app, DeviceInfo},
    error::{Error, SocketContext, StatusCode},
    socket::{run_device_socket, socket_url, SocketEvent},
    version::SemVer,
};

use ledger_apdu::APDUCommand;
use ledger_transport_hidapi::TransportNativeHID;

use std::{str, thread, time};

// https://github.com/LedgerHQ/ledger-live/blob/99879eb5bada1ecaea7a02d8886e16b44657af6d/libs/ledger-live-common/src/hw/listApps.ts#L5
const LIST_APPS_COMMAND: APDUCommand<&[u8]> = APDUCommand {
    cla: 0xe0,
    ins: 0xde,
    p1: 0x00,
    p2: 0x00,
    data: &[],
};

// https://github.com/LedgerHQ/ledger-live/blob/99879eb5bada1ecaea7a02d8886e16b44657af6d/libs/ledger-live-common/src/hw/listApps.ts#L47
const CONTINUE_LIST_APPS_COMMAND: APDUCommand<&[u8]> = APDUCommand {
    cla: 0xe0,
    ins: 0xdf,
    p1: 0x00,
    p2: 0x00,
    data: &[],
};

// https://github.com/LedgerHQ/ledger-live/blob/5a0a1aa5dc183116839851b79bceb6704f1de4b9/libs/ledger-live-common/src/hw/openApp.ts#L3
const OPEN_APP_COMMAND_TEMPLATE: APDUCommand<&[u8]> = APDUCommand {
    cla: 0xe0,
    ins: 0xd8,
    p1: 0x00,
    p2: 0x00,
    data: &[],
};

/// The name of the Bitcoin application.
pub const BITCOIN_APP_NAME: &str = "Bitcoin";
/// The name of the Bitcoin testnet application.
pub const BITCOIN_TEST_APP_NAME: &str = "Bitcoin Test";

// Retry parameters of the app installation.
// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/installApp.ts
const APP_INSTALL_RETRY_DELAY: time::Duration = time::Duration::from_millis(500);
const APP_INSTALL_RETRY_LIMIT: usize = 5;

/// Ledger Live waits a bit between two app operations, as older firmwares misbehave when actions
/// are performed too closely (`MANAGER_INSTALL_DELAY`, 1s by default).
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/apps/runner.ts
const MANAGER_INSTALL_DELAY: time::Duration = time::Duration::from_millis(1000);

/// The name of the Bitcoin app to use.
pub fn bitcoin_app_name(is_testnet: bool) -> &'static str {
    if is_testnet {
        BITCOIN_TEST_APP_NAME
    } else {
        BITCOIN_APP_NAME
    }
}

pub(crate) fn is_bitcoin_app_name(name: &str) -> bool {
    name == BITCOIN_APP_NAME || name == BITCOIN_TEST_APP_NAME
}

/// Information about an application as queried directly from the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    pub name: String,
    pub hash: Vec<u8>,
    pub hash_code_data: Vec<u8>,
    pub blocks: u16,
    pub flags: u16,
}

/// Parse a page of the response to the ListApps APDU (without the status code).
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/listApps.ts
pub(crate) fn parse_list_apps_response(data: &[u8]) -> Result<Vec<InstalledApp>, Error> {
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
        if data.len() < i + 1 + 2 + 2 + 32 + 32 + 1 {
            return Err(invalid("not enough data"));
        }

        let len = data[i] as usize;
        i += 1;
        let blocks = u16::from_be_bytes([data[i], data[i + 1]]);
        i += 2;
        let flags = u16::from_be_bytes([data[i], data[i + 1]]);
        i += 2;
        let hash_code_data = data[i..i + 32].to_vec();
        i += 32;
        let hash = data[i..i + 32].to_vec();
        i += 32;
        let name_len = data[i] as usize;
        i += 1;

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
            blocks,
            flags,
        });
    }

    Ok(apps)
}

/// Get a list of applications installed on this device.
pub fn list_installed_apps_raw(
    ledger_api: &TransportNativeHID,
) -> Result<Vec<InstalledApp>, Error> {
    let check = |answer: &ledger_apdu::APDUAnswer<Vec<u8>>| match answer.retcode() {
        r if r == StatusCode::OK as u16 => Ok(()),
        r if r == StatusCode::LockedDevice as u16 => Err(Error::DeviceLocked),
        r if r == StatusCode::UserRefusedOnDevice as u16
            || r == StatusCode::ConditionsOfUseNotSatisfied as u16 =>
        {
            Err(Error::UserRefusedAllowManager)
        }
        r => Err(Error::DeviceStatus(r)),
    };

    let mut answer = ledger_api.exchange(&LIST_APPS_COMMAND)?;
    check(&answer)?;

    // See https://github.com/LedgerHQ/ledger-live/blob/99879eb5bada1ecaea7a02d8886e16b44657af6d/libs/ledger-live-common/src/hw/listApps.ts#L9
    let mut installed_apps = Vec::new();
    while !answer.data().is_empty() {
        installed_apps.extend(parse_list_apps_response(answer.data())?);
        answer = ledger_api.exchange(&CONTINUE_LIST_APPS_COMMAND)?;
        check(&answer)?;
    }

    Ok(installed_apps)
}

/// Get the metadata of the applications installed on the device. This calls the Ledger API, to
/// only query the data available from the device see `list_installed_apps_raw`.
pub fn list_installed_apps(
    ledger_api: &TransportNativeHID,
) -> Result<Vec<Option<BitcoinAppInfo>>, Error> {
    // Empty hash data can come from apps that are not real apps (such as language packs) or custom
    // applications that have been sideloaded. Ledger Live filters them out.
    // https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/apps/listApps.ts
    let hashes = list_installed_apps_raw(ledger_api)?
        .into_iter()
        .filter(|a| a.hash_code_data.iter().any(|b| *b != 0))
        .map(|a| a.hash)
        .collect::<Vec<_>>();
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    bitcoin_apps_by_hashes(hashes)
}

/// Get the installed Bitcoin app, if any. Set `is_testnet` to look for the testnet Bitcoin app.
pub fn bitcoin_app_installed(
    ledger_api: &TransportNativeHID,
    is_testnet: bool,
) -> Result<Option<InstalledApp>, Error> {
    let lowercase_app_name = bitcoin_app_name(is_testnet).to_lowercase();
    Ok(list_installed_apps_raw(ledger_api)?
        .into_iter()
        .find(|app| app.name.to_lowercase() == lowercase_app_name))
}

/// Whether the Bitcoin app is installed on this device.
pub fn is_bitcoin_app_installed(
    ledger_api: &TransportNativeHID,
    is_testnet: bool,
) -> Result<bool, Error> {
    Ok(bitcoin_app_installed(ledger_api, is_testnet)?.is_some())
}

/// Get the Bitcoin apps information for this device from the "catalog" (as Ledger Live calls it).
/// Returns the (mainnet, testnet) apps.
pub fn get_latest_apps(
    device_info: &DeviceInfo,
) -> Result<(Option<BitcoinAppInfo>, Option<BitcoinAppInfo>), Error> {
    device_info.check_normal_mode()?;
    let mut bitcoin = None;
    let mut test = None;
    for app in bitcoin_apps_catalog(device_info)? {
        if app.version_name == BITCOIN_APP_NAME {
            bitcoin = Some(app);
        } else if app.version_name == BITCOIN_TEST_APP_NAME {
            test = Some(app);
        }
    }
    Ok((bitcoin, test))
}

/// Get the Bitcoin app information for this device from the "catalog" (as Ledger Live calls it).
/// Set `is_testnet` to `true` to get the Test app instead.
pub fn bitcoin_latest_app(
    device_info: &DeviceInfo,
    is_testnet: bool,
) -> Result<Option<BitcoinAppInfo>, Error> {
    let apps = get_latest_apps(device_info)?;
    Ok(if is_testnet { apps.1 } else { apps.0 })
}

/// Open the given application on the device.
pub fn open_bitcoin_app(ledger_api: &TransportNativeHID, is_testnet: bool) -> Result<(), Error> {
    let mut command = OPEN_APP_COMMAND_TEMPLATE;
    command.data = bitcoin_app_name(is_testnet).as_bytes();

    let resp = ledger_api.exchange(&command)?;
    match resp.retcode() {
        r if r == StatusCode::OK as u16 => Ok(()),
        r if r == StatusCode::LockedDevice as u16 => Err(Error::DeviceLocked),
        r if r == StatusCode::UserRefusedOnDevice as u16 => Err(Error::UserRefusedOnDevice),
        r => Err(Error::DeviceStatus(r)),
    }
}

/// Check whether the Ledger device is genuine. Returns an error if the check could not be
/// performed or if the device is not genuine (`Error::NotGenuine`).
///
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/genuineCheck.ts
/// and https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/hooks/useGenuineCheck.ts
pub fn genuine_check(ledger_api: &TransportNativeHID) -> Result<(), Error> {
    genuine_check_with_events(ledger_api, |_| {})
}

/// Same as `genuine_check`, reporting the socket events (e.g. to tell the user to allow the Ledger
/// manager when `SocketEvent::DevicePermissionRequested` is received).
pub fn genuine_check_with_events<F: FnMut(SocketEvent)>(
    ledger_api: &TransportNativeHID,
    on_event: F,
) -> Result<(), Error> {
    let device_info = DeviceInfo::new(ledger_api)?;
    device_info.check_normal_mode()?;
    let firmware_info = current_firmware(&device_info)?;

    let target_id = device_info.target_id.to_string();
    let url = socket_url(
        "genuine",
        &[("targetId", &target_id), ("perso", &firmware_info.perso)],
    );
    let result = run_device_socket(ledger_api, &url, SocketContext::GenuineCheck, on_event)?;
    let payload = match result {
        Some(serde_json::Value::String(s)) => s,
        Some(v) => v.to_string(),
        None => String::new(),
    };
    // https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/hooks/useGenuineCheck.ts
    if payload == "0000" {
        Ok(())
    } else {
        Err(Error::NotGenuine(payload))
    }
}

/// An error arising when installing the Bitcoin app.
#[derive(Debug)]
pub enum InstallErr {
    /// The Bitcoin application is already installed.
    AlreadyInstalled,
    /// Couldn't get info about the Bitcoin app.
    AppNotFound,
    Any(Error),
}

impl std::fmt::Display for InstallErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallErr::AlreadyInstalled => write!(f, "Application is already installed."),
            InstallErr::AppNotFound => write!(f, "Could not get info about the application."),
            InstallErr::Any(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for InstallErr {}

/// An error arising when updating the Bitcoin app.
#[derive(Debug)]
pub enum UpdateErr {
    /// The Bitcoin application is not installed yet.
    NotInstalled,
    /// Couldn't get info about the Bitcoin app.
    AppNotFound,
    /// The installed app is already the latest.
    AlreadyLatest,
    Any(Error),
}

impl std::fmt::Display for UpdateErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateErr::NotInstalled => write!(f, "Application is not installed."),
            UpdateErr::AppNotFound => write!(f, "Could not get info about the application."),
            UpdateErr::AlreadyLatest => write!(f, "Application is already at the latest version."),
            UpdateErr::Any(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for UpdateErr {}

/// A step of an app installation or update, for progress reporting.
#[derive(Debug, Clone, PartialEq)]
pub enum AppInstallStep {
    /// Listing the installed apps (the user may have to allow the Ledger manager).
    ListingApps,
    /// Querying the Ledger API.
    QueryingApi,
    /// The user must allow the Ledger manager on the device.
    AllowManagerRequested,
    /// The user allowed the Ledger manager.
    AllowManagerGranted,
    /// Uninstalling the previous version of the app. `progress` is between 0 and 1.
    Uninstalling { progress: f32 },
    /// Installing the app. `progress` is between 0 and 1. `done` APDUs out of `total` were sent.
    Installing {
        progress: f32,
        done: usize,
        total: usize,
    },
    /// Retrying after an error.
    Retrying { attempt: usize, error: String },
    /// The operation completed.
    Done,
}

/// Whether an update is available for an installed app.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/apps/listApps.ts (`isUpdateAvailable`)
pub fn is_app_update_available(
    installed_version: Option<&str>,
    available_version: &str,
    installed_hash: &str,
    available_hash: &str,
) -> bool {
    let installed = installed_version.and_then(SemVer::parse);
    let available = SemVer::parse(available_version);
    if let (Some(installed), Some(available)) = (installed, available) {
        return available > installed;
    }
    !available_hash.eq_ignore_ascii_case(installed_hash)
}

fn forward_socket_event<P: FnMut(AppInstallStep)>(
    progress: &mut P,
    event: SocketEvent,
    uninstall: bool,
) {
    match event {
        SocketEvent::DevicePermissionRequested => progress(AppInstallStep::AllowManagerRequested),
        SocketEvent::DevicePermissionGranted => progress(AppInstallStep::AllowManagerGranted),
        e @ SocketEvent::BulkProgress { .. } => {
            let p = e.bulk_progress().unwrap_or(0.0);
            if let SocketEvent::BulkProgress { index, total } = e {
                progress(if uninstall {
                    AppInstallStep::Uninstalling { progress: p }
                } else {
                    AppInstallStep::Installing {
                        progress: p,
                        done: index,
                        total,
                    }
                })
            }
        }
        _ => {}
    }
}

/// Install this application on the device, retrying on errors as Ledger Live does.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/installApp.ts
pub fn install_app<P: FnMut(AppInstallStep)>(
    ledger_api: &TransportNativeHID,
    target_id: u32,
    app: &BitcoinAppInfo,
    mut progress: P,
) -> Result<(), Error> {
    // Run quitApp just before the install.
    quit_app(ledger_api)?;

    // Make sure to properly escape the parameters in the request's parameter.
    let target_id = target_id.to_string();
    let url = socket_url(
        "install",
        &[
            ("targetId", &target_id),
            ("perso", &app.perso),
            ("deleteKey", &app.delete_key),
            ("firmware", &app.firmware),
            ("firmwareKey", &app.firmware_key),
            ("hash", &app.hash),
        ],
    );
    let mut attempt = 0;
    loop {
        let res = run_device_socket(ledger_api, &url, SocketContext::InstallApp, |e| {
            forward_socket_event(&mut progress, e, false)
        });
        match res {
            Ok(_) => return Ok(()),
            // Ledger Live does not retry on locked device errors. We also don't retry on errors
            // that won't go away by retrying (in Ledger Live the user would retry manually).
            Err(e @ Error::DeviceLocked)
            | Err(e @ Error::UserRefusedAllowManager)
            | Err(e @ Error::UserRefusedOnDevice)
            | Err(e @ Error::NotEnoughSpace)
            | Err(e @ Error::AppAlreadyInstalled)
            | Err(e @ Error::AppDependencyInstallRequired)
            | Err(e @ Error::DeviceOnDashboardExpected) => return Err(e),
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

/// Uninstall this application from the device. `app` must be the catalog entry of the app (its
/// `delete` and `delete_key` are used), as Ledger Live does.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/uninstallApp.ts
pub fn uninstall_app<P: FnMut(AppInstallStep)>(
    ledger_api: &TransportNativeHID,
    target_id: u32,
    app: &BitcoinAppInfo,
    mut progress: P,
) -> Result<(), Error> {
    if app.delete.is_empty() {
        return Err(Error::UnexpectedHsmMessage(format!(
            "no uninstall firmware for app {}",
            app.version_name
        )));
    }
    quit_app(ledger_api)?;
    let target_id = target_id.to_string();
    let url = socket_url(
        "install",
        &[
            ("targetId", &target_id),
            ("perso", &app.perso),
            ("deleteKey", &app.delete_key),
            ("firmware", &app.delete),
            ("firmwareKey", &app.delete_key),
            ("hash", &app.hash),
        ],
    );
    run_device_socket(ledger_api, &url, SocketContext::UninstallApp, |e| {
        forward_socket_event(&mut progress, e, true)
    })?;
    Ok(())
}

/// Install the Bitcoin application on this device. Set `is_testnet` to `true` to install the
/// testnet app instead.
pub fn install_bitcoin_app(
    ledger_api: &TransportNativeHID,
    is_testnet: bool,
) -> Result<(), InstallErr> {
    install_bitcoin_app_with_progress(ledger_api, is_testnet, |_| {})
}

/// Same as `install_bitcoin_app`, reporting progress through the `progress` callback.
pub fn install_bitcoin_app_with_progress<P: FnMut(AppInstallStep)>(
    ledger_api: &TransportNativeHID,
    is_testnet: bool,
    mut progress: P,
) -> Result<(), InstallErr> {
    // First of all make sure it's not already installed.
    progress(AppInstallStep::ListingApps);
    if is_bitcoin_app_installed(ledger_api, is_testnet).map_err(InstallErr::Any)? {
        return Err(InstallErr::AlreadyInstalled);
    }

    // Get the app info, necessary for the websocket query below.
    progress(AppInstallStep::QueryingApi);
    let device_info = DeviceInfo::new(ledger_api).map_err(InstallErr::Any)?;
    let bitcoin_app = bitcoin_latest_app(&device_info, is_testnet)
        .map_err(InstallErr::Any)?
        .ok_or(InstallErr::AppNotFound)?;
    if let Some(parent) = &bitcoin_app.parent_name {
        log::warn!(
            "App {} depends on app {}. It must be installed first.",
            bitcoin_app.version_name,
            parent
        );
    }

    // Now install the app by connecting through their websocket thing to their HSM.
    install_app(
        ledger_api,
        device_info.target_id,
        &bitcoin_app,
        &mut progress,
    )
    .map_err(|e| match e {
        Error::AppAlreadyInstalled => InstallErr::AlreadyInstalled,
        e => InstallErr::Any(e),
    })?;

    progress(AppInstallStep::Done);
    Ok(())
}

/// Update the Bitcoin application on this device. Set `is_testnet` to `true` to update the
/// testnet app instead.
pub fn update_bitcoin_app(
    ledger_api: &TransportNativeHID,
    is_testnet: bool,
) -> Result<(), UpdateErr> {
    update_bitcoin_app_with_progress(ledger_api, is_testnet, |_| {})
}

/// Same as `update_bitcoin_app`, reporting progress through the `progress` callback.
///
/// Like Ledger Live, the update is performed by uninstalling the app (using the `delete` firmware
/// of the latest version from the catalog) and then installing the latest version. See the
/// "updateAll" action and the action plan (uninstalls, then installs) in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/apps/logic.ts
/// and the runner in https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/apps/runner.ts
pub fn update_bitcoin_app_with_progress<P: FnMut(AppInstallStep)>(
    ledger_api: &TransportNativeHID,
    is_testnet: bool,
    mut progress: P,
) -> Result<(), UpdateErr> {
    // First of all make sure the app is installed. Get its details.
    progress(AppInstallStep::ListingApps);
    let app = bitcoin_app_installed(ledger_api, is_testnet)
        .map_err(UpdateErr::Any)?
        .ok_or(UpdateErr::NotInstalled)?;
    let installed_hash = hex::encode(&app.hash);
    progress(AppInstallStep::QueryingApi);
    let installed_app = bitcoin_apps_by_hashes(vec![app.hash])
        .map_err(UpdateErr::Any)?
        .into_iter()
        .next()
        .flatten();

    // Get the latest app info, necessary for the websocket query below.
    let device_info = DeviceInfo::new(ledger_api).map_err(UpdateErr::Any)?;
    let latest_app = bitcoin_latest_app(&device_info, is_testnet)
        .map_err(UpdateErr::Any)?
        .ok_or(UpdateErr::AppNotFound)?;

    if !is_app_update_available(
        installed_app.as_ref().map(|a| a.version.as_str()),
        &latest_app.version,
        &installed_hash,
        &latest_app.hash,
    ) {
        return Err(UpdateErr::AlreadyLatest);
    }
    log::info!(
        "Updating {} from {} to {}.",
        latest_app.version_name,
        installed_app
            .as_ref()
            .map(|a| a.version.as_str())
            .unwrap_or("an unknown version"),
        latest_app.version
    );

    // Uninstall the current version, then install the latest one, by connecting through their
    // websocket thing to their HSM.
    uninstall_app(
        ledger_api,
        device_info.target_id,
        &latest_app,
        &mut progress,
    )
    .map_err(UpdateErr::Any)?;
    thread::sleep(MANAGER_INSTALL_DELAY);
    install_app(
        ledger_api,
        device_info.target_id,
        &latest_app,
        &mut progress,
    )
    .map_err(UpdateErr::Any)?;

    progress(AppInstallStep::Done);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_entry(name: &str, blocks: u16, flags: u16, fill: u8) -> Vec<u8> {
        let mut e = vec![(name.len() + 70) as u8];
        e.extend_from_slice(&blocks.to_be_bytes());
        e.extend_from_slice(&flags.to_be_bytes());
        e.extend_from_slice(&[fill; 32]);
        e.extend_from_slice(&[fill.wrapping_add(1); 32]);
        e.push(name.len() as u8);
        e.extend_from_slice(name.as_bytes());
        e
    }

    #[test]
    fn list_apps_parsing() {
        let mut data = vec![0x01];
        data.extend(app_entry("Bitcoin", 0x0102, 0x0a00, 0xaa));
        data.extend(app_entry("Bitcoin Test", 3, 4, 0x00));
        let apps = parse_list_apps_response(&data).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "Bitcoin");
        assert_eq!(apps[0].blocks, 0x0102);
        assert_eq!(apps[0].flags, 0x0a00);
        assert_eq!(apps[0].hash_code_data, vec![0xaa; 32]);
        assert_eq!(apps[0].hash, vec![0xab; 32]);
        assert_eq!(apps[1].name, "Bitcoin Test");

        assert!(parse_list_apps_response(&[]).unwrap().is_empty());
        // Unknown format.
        assert!(parse_list_apps_response(&[0x02]).is_err());
        // Truncated.
        assert!(parse_list_apps_response(&data[..data.len() - 1]).is_err());
        // Invalid length.
        let mut bad = vec![0x01];
        let mut e = app_entry("Bitcoin", 1, 1, 1);
        e[0] += 1;
        bad.extend(e);
        assert!(parse_list_apps_response(&bad).is_err());
    }

    #[test]
    fn app_update_available() {
        assert!(is_app_update_available(Some("2.1.2"), "2.1.3", "aa", "bb"));
        assert!(!is_app_update_available(Some("2.1.3"), "2.1.3", "aa", "bb"));
        assert!(!is_app_update_available(Some("2.2.0"), "2.1.3", "aa", "bb"));
        // Falls back to comparing hashes.
        assert!(is_app_update_available(None, "2.1.3", "aa", "bb"));
        assert!(!is_app_update_available(None, "2.1.3", "aa", "AA"));
        assert!(is_app_update_available(Some("weird"), "2.1.3", "aa", "bb"));
    }

    #[test]
    fn app_names() {
        assert_eq!(bitcoin_app_name(false), "Bitcoin");
        assert_eq!(bitcoin_app_name(true), "Bitcoin Test");
        assert!(is_bitcoin_app_name("Bitcoin Test"));
        assert!(!is_bitcoin_app_name("Bitcoin Cash"));
    }
}
