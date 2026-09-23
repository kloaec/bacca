//! Blocking interactions with a Ledger device. Run from `spawn_blocking` tasks, see
//! `device_service`.

use crate::device_service::{
    DeviceState, LatestFirmware, LedgerMode, LedgerState, Reporter, TaskResult, Version,
};

use ledger_manager::{
    bitcoin_apps_by_hashes, check_firmware_update_supported, firmware_update_resets_customization,
    genuine_check_with_events, get_latest_apps, install_bitcoin_app_with_progress, latest_firmware,
    ledger_transport_hidapi::{hidapi::HidApi, TransportNativeHID},
    list_installed_apps_raw, open_device, update_bitcoin_app_with_progress, AppInstallStep,
    DeviceInfo, DeviceModel, Error, FirmwareUpdateInfo, FirmwareUpdateStep, SocketEvent,
    BITCOIN_APP_NAME, BITCOIN_TEST_APP_NAME,
};

fn connect(reporter: &Reporter) -> Option<(TransportNativeHID, Option<DeviceModel>)> {
    let res = HidApi::new()
        .map_err(|e| e.to_string())
        .and_then(|api| open_device(&api).map_err(|e| e.to_string()));
    match res {
        Ok(r) => Some(r),
        Err(e) => {
            reporter.alarm(format!("Cannot connect to the Ledger device: {}", e));
            None
        }
    }
}

fn percent(p: f32) -> u32 {
    (p * 100.0).round().clamp(0.0, 100.0) as u32
}

/// Query the installed Bitcoin apps. The user may have to allow it on the device.
fn installed_bitcoin_apps(transport: &TransportNativeHID) -> Result<(Version, Version), Error> {
    let apps = list_installed_apps_raw(transport)?;
    let main = apps.iter().find(|a| a.name == BITCOIN_APP_NAME);
    let test = apps.iter().find(|a| a.name == BITCOIN_TEST_APP_NAME);
    let hashes: Vec<Vec<u8>> = [main, test]
        .iter()
        .flatten()
        .map(|a| a.hash.clone())
        .collect();
    // The versions are known from the Ledger API.
    let infos = if hashes.is_empty() {
        Vec::new()
    } else {
        bitcoin_apps_by_hashes(hashes).unwrap_or_else(|e| {
            log::error!("Error querying the installed apps versions: {}", e);
            Vec::new()
        })
    };
    let version = |name: &str, installed: bool| {
        if !installed {
            return Version::NotInstalled;
        }
        infos
            .iter()
            .flatten()
            .find(|i| i.version_name == name)
            .map(|i| Version::Installed(i.version.clone()))
            .unwrap_or_else(|| Version::Installed("unknown".to_string()))
    };
    Ok((
        version(BITCOIN_APP_NAME, main.is_some()),
        version(BITCOIN_TEST_APP_NAME, test.is_some()),
    ))
}

/// Query everything about the connected Ledger: model, firmware, latest firmware, installed and
/// latest Bitcoin apps. Returns whether the device could be queried, the state to display and the
/// firmware update available.
pub fn load(reporter: &Reporter) -> (bool, LedgerState, Option<FirmwareUpdateInfo>) {
    log::info!("ledger::load()");
    let mut state = LedgerState::default();
    reporter.status("Ledger device detected, connecting...");
    let (transport, usb_model) = match HidApi::new()
        .map_err(|e| e.to_string())
        .and_then(|api| open_device(&api).map_err(|e| e.to_string()))
    {
        Ok(r) => r,
        Err(e) => {
            reporter.status(format!("Cannot connect to the Ledger device: {}", e));
            return (false, state, None);
        }
    };
    state.model = usb_model.map(|m| m.to_string());

    let info = match DeviceInfo::new(&transport) {
        Ok(info) => info,
        Err(e) => {
            let msg = match e {
                Error::DeviceLocked => "Your Ledger is locked, please unlock it...".to_string(),
                Error::DeviceOnDashboardExpected => {
                    "Please quit the application opened on your Ledger...".to_string()
                }
                Error::DeviceNotOnboarded => {
                    "Your Ledger is not set up yet. Please set it up first.".to_string()
                }
                e => format!("Error fetching device info: {}. Is the Ledger unlocked?", e),
            };
            reporter.status(msg);
            reporter.state(DeviceState::Ledger(Box::new(state.clone())));
            return (false, state, None);
        }
    };
    log::info!("Ledger connected: {}", info.firmware_summary());
    state.model = Some(
        info.model
            .or(usb_model)
            .map(|m| m.to_string())
            .unwrap_or_else(|| format!("Unknown Ledger (target id {:#010x})", info.target_id)),
    );
    state.mode = if info.is_bootloader {
        LedgerMode::Bootloader
    } else if info.is_osu {
        LedgerMode::Updater
    } else {
        LedgerMode::Normal
    };
    state.firmware = Some(match state.mode {
        LedgerMode::Normal => info.version.clone(),
        LedgerMode::Updater => format!("{} (updater)", info.version),
        LedgerMode::Bootloader => format!("{} (bootloader)", info.version),
    });
    reporter.state(DeviceState::Ledger(Box::new(state.clone())));

    if info.is_bootloader {
        reporter.status(
            "Your Ledger is in bootloader mode: a firmware update was probably interrupted. Click 'Repair' to finish it.",
        );
        return (true, state, None);
    }

    // The final message, once everything is loaded.
    let mut notes: Vec<String> = Vec::new();

    reporter.status("Querying the latest firmware on Ledger API...");
    let update = match latest_firmware(&info) {
        Ok(Some(update)) => {
            let version = update.version().to_string();
            state.latest_firmware = match check_firmware_update_supported(&info) {
                Ok(()) => LatestFirmware::Available(version),
                Err(e) => {
                    notes.push(e.to_string());
                    LatestFirmware::Unsupported(version)
                }
            };
            state.update_resets_customization =
                firmware_update_resets_customization(&info, &update);
            Some(update)
        }
        Ok(None) => {
            state.latest_firmware = LatestFirmware::UpToDate;
            None
        }
        Err(e) => {
            log::error!("Error querying the latest firmware: {}", e);
            notes.push(format!("Could not check the latest firmware: {}", e));
            None
        }
    };
    reporter.state(DeviceState::Ledger(Box::new(state.clone())));

    if info.is_osu {
        reporter.status(
            "A firmware update was interrupted, your Ledger is in updater mode. Click 'Update' to complete it.",
        );
        return (true, state, update);
    }

    reporter.status("Querying latest apps on Ledger API...");
    match get_latest_apps(&info) {
        Ok((bitcoin, test)) => {
            state.latest_mainnet = bitcoin
                .map(|a| Version::Latest(a.version))
                .unwrap_or(Version::None);
            state.latest_testnet = test
                .map(|a| Version::Latest(a.version))
                .unwrap_or(Version::None);
        }
        Err(e) => {
            log::error!("Error querying the latest apps: {}", e);
            notes.push(format!("Fail to get latest apps from Ledger API: {}", e));
        }
    }

    reporter.status("Querying installed apps. Please confirm on device.");
    match installed_bitcoin_apps(&transport) {
        Ok((main, test)) => {
            state.mainnet = main;
            state.testnet = test;
        }
        Err(e) => {
            log::error!("Error listing installed applications: {}", e);
            reporter.state(DeviceState::Ledger(Box::new(state.clone())));
            reporter.alarm(format!("Error listing installed applications: {}.", e));
            return (true, state, update);
        }
    }
    reporter.state(DeviceState::Ledger(Box::new(state.clone())));

    if notes.is_empty()
        && state.mainnet == Version::NotInstalled
        && state.testnet == Version::NotInstalled
    {
        notes.push("The Bitcoin app is not installed. Click 'Install' to install it.".to_string());
    }
    reporter.status(notes.join("\n"));
    (true, state, update)
}

fn report_app_step(reporter: &Reporter, step: AppInstallStep) {
    match step {
        AppInstallStep::ListingApps => {
            reporter.status("Querying installed apps. Please confirm on device.")
        }
        AppInstallStep::QueryingApi => reporter.status("Querying the Ledger API..."),
        AppInstallStep::AllowManagerRequested => {
            reporter.status("Please allow the Ledger manager on your device.")
        }
        AppInstallStep::AllowManagerGranted => reporter.status("Ledger manager allowed."),
        AppInstallStep::Uninstalling { progress } => {
            reporter.status(format!(
                "Uninstalling the previous version: {}%",
                percent(progress)
            ));
            reporter.progress(Some(progress));
        }
        AppInstallStep::Installing { progress, .. } => {
            reporter.status(format!("Installing: {}%", percent(progress)));
            reporter.progress(Some(progress));
        }
        AppInstallStep::Retrying { attempt, error } => {
            reporter.progress(None);
            reporter.status(format!(
                "Error: {}. Retrying (attempt {})...",
                error, attempt
            ));
        }
        AppInstallStep::Done => reporter.progress(None),
    }
}

/// Install (or update, if `update` is set) the Bitcoin app.
pub fn install_app(reporter: &Reporter, testnet: bool, update: bool) -> TaskResult {
    log::info!(
        "ledger::install_app(testnet={}, update={})",
        testnet,
        update
    );
    let name = if testnet {
        BITCOIN_TEST_APP_NAME
    } else {
        BITCOIN_APP_NAME
    };
    let (transport, _) = match connect(reporter) {
        Some(t) => t,
        None => {
            return TaskResult::Operation {
                reload: false,
                message: None,
            }
        }
    };
    let progress = |step| report_app_step(reporter, step);
    let res = if update {
        update_bitcoin_app_with_progress(&transport, testnet, progress).map_err(|e| e.to_string())
    } else {
        install_bitcoin_app_with_progress(&transport, testnet, progress).map_err(|e| e.to_string())
    };
    // Release the device before reloading its information.
    drop(transport);
    let message = match res {
        Ok(()) => (
            format!(
                "Successfully {} the {} app.",
                if update { "updated" } else { "installed" },
                name
            ),
            false,
        ),
        Err(e) => (
            format!(
                "Error {} the {} app: {}",
                if update { "updating" } else { "installing" },
                name,
                e
            ),
            true,
        ),
    };
    TaskResult::Operation {
        reload: true,
        message: Some(message),
    }
}

/// Check the device is genuine.
pub fn genuine_check(reporter: &Reporter) -> Option<bool> {
    log::info!("ledger::genuine_check()");
    let (transport, _) = connect(reporter)?;
    reporter.status("Checking if the device is genuine...");
    let res = genuine_check_with_events(&transport, |e| match e {
        SocketEvent::DevicePermissionRequested => {
            reporter.status("Please allow the Ledger manager on your device.")
        }
        SocketEvent::DevicePermissionGranted => {
            reporter.status("Ledger manager allowed. Checking if the device is genuine...")
        }
        _ => {}
    });
    match res {
        Ok(()) => {
            reporter.status("");
            Some(true)
        }
        Err(Error::NotGenuine(_)) => {
            reporter.alarm("WARNING: your device is NOT genuine!");
            Some(false)
        }
        Err(e) => {
            reporter.alarm(format!("Error when performing the genuine check: {}", e));
            None
        }
    }
}

fn report_firmware_step(reporter: &Reporter, step: FirmwareUpdateStep) {
    let progress = |label: &str, p: f32| {
        reporter.status(format!("{}: {}%", label, percent(p)));
        reporter.progress(Some(p));
    };
    match step {
        FirmwareUpdateStep::Preparing => reporter.status("Preparing the update..."),
        FirmwareUpdateStep::AllowManagerRequested => {
            reporter.status("Please allow the Ledger manager on your device.")
        }
        FirmwareUpdateStep::AllowManagerGranted => reporter.status("Ledger manager allowed."),
        FirmwareUpdateStep::InstallingOsu { progress: p } => {
            progress("Transferring the update to the device", p)
        }
        FirmwareUpdateStep::WaitingUserConfirmation { identifier } => {
            reporter.progress(None);
            match identifier {
                Some(id) => {
                    reporter.status(
                        "Please confirm the update on your device, after checking the identifier it displays matches:",
                    );
                    reporter.info(Some(id));
                }
                None => reporter.status("Please confirm the update on your device."),
            }
        }
        FirmwareUpdateStep::UserConfirmed => {
            reporter.info(None);
            reporter.status("Update confirmed on the device.")
        }
        FirmwareUpdateStep::WaitingForReboot => {
            reporter.progress(None);
            reporter.status("Waiting for the device to restart. Keep it plugged in...")
        }
        FirmwareUpdateStep::WaitingForBootloader => {
            reporter.progress(None);
            reporter.status("Waiting for the device to restart in bootloader mode...")
        }
        FirmwareUpdateStep::FlashingBootloader { progress: p } => {
            progress("Updating the bootloader", p)
        }
        FirmwareUpdateStep::FlashingMcu { progress: p } => progress("Updating the MCU", p),
        FirmwareUpdateStep::InstallingFinal { progress: p } => {
            progress("Installing the firmware", p)
        }
        FirmwareUpdateStep::WaitingForDevice => {
            reporter.progress(None);
            reporter.status(
                "The device is installing the update. Waiting for it to restart on the new firmware, this can take several minutes. Keep it plugged in...",
            )
        }
        FirmwareUpdateStep::DeviceLocked => {
            reporter.status("Your device is locked: please unlock it to continue the update.")
        }
        FirmwareUpdateStep::Done { device_info } => {
            reporter.progress(None);
            reporter.status(format!(
                "Firmware updated, the device now runs {}.",
                device_info.version
            ))
        }
    }
}

/// Update the firmware. This takes minutes, the device restarts several times.
pub fn update_firmware(reporter: &Reporter, update: FirmwareUpdateInfo) -> TaskResult {
    log::info!("ledger::update_firmware({})", update.version());
    let message = match HidApi::new() {
        Err(e) => (format!("Error initializing HID api: {}.", e), true),
        Ok(mut api) => match ledger_manager::update_firmware(&mut api, &update, |step| {
            report_firmware_step(reporter, step)
        }) {
            Ok(info) => (
                format!(
                    "Successfully updated the firmware to {}. The apps were removed by the update: click 'Install' to reinstall the Bitcoin app.",
                    info.version
                ),
                false,
            ),
            Err(e) => (format!("Error updating the firmware: {}", e), true),
        },
    };
    TaskResult::Operation {
        reload: true,
        message: Some(message),
    }
}

/// Finish a firmware update interrupted while the device was in bootloader mode, by flashing the
/// MCU / bootloader it needs.
pub fn repair_firmware(reporter: &Reporter) -> TaskResult {
    log::info!("ledger::repair_firmware()");
    let message = match HidApi::new() {
        Err(e) => (format!("Error initializing HID api: {}.", e), true),
        Ok(mut api) => match ledger_manager::repair_firmware(&mut api, None, |step| {
            report_firmware_step(reporter, step)
        }) {
            Ok(info) => (
                format!(
                    "Successfully repaired the device, it now runs {}.",
                    info.version
                ),
                false,
            ),
            Err(e) => (format!("Error repairing the firmware: {}", e), true),
        },
    };
    TaskResult::Operation {
        reload: true,
        message: Some(message),
    }
}
