//! The Ledger operations of the worker thread.

use crate::worker::{
    AppState, DeviceState, Event, Installed, LatestFirmware, LedgerMode, LedgerState, Outcome,
    Reporter,
};

use ledger_manager::{
    apps_by_hashes, check_firmware_update_supported, default_backup_dir,
    firmware_update_resets_customization, get_latest_apps, install_bitcoin_app, latest_firmware,
    ledger_transport_hidapi::{hidapi::HidApi, TransportNativeHID},
    list_installed_apps_raw, open_device, update_bitcoin_app, update_firmware_and_restore,
    AppInstallStep, BackupStep, DeviceInfo, DeviceModel, Error, FirmwareUpdateInfo,
    FirmwareUpdateStep, LanguageInstallStep, LoadImageStep, RestoreStep, SocketEvent,
    UpdateAndRestoreResult, UpdateAndRestoreStep, BITCOIN_APP_NAME, BITCOIN_TEST_APP_NAME,
};

fn connect() -> Result<(TransportNativeHID, Option<DeviceModel>), String> {
    let api = HidApi::new().map_err(|e| format!("Error initializing the HID API: {}", e))?;
    open_device(&api).map_err(|e| format!("Cannot connect to the Ledger device: {}", e))
}

/// Query everything about the connected Ledger, reporting it to the GUI as it goes. Returns whether
/// the device could be queried, and the firmware update available.
pub fn load(r: &Reporter) -> (bool, Option<FirmwareUpdateInfo>) {
    r.status("Ledger device detected, connecting...");
    let (transport, usb_model) = match connect() {
        Ok(t) => t,
        Err(e) => {
            r.status(e);
            return (false, None);
        }
    };
    let mut state = LedgerState {
        model: usb_model.map_or("Ledger".to_string(), |m| m.to_string()),
        ..Default::default()
    };
    let send = |state: &LedgerState| r.device(DeviceState::Ledger(state.clone()));

    let info = match DeviceInfo::new(&transport) {
        Ok(info) => info,
        Err(e) => {
            send(&state);
            r.status(match e {
                Error::DeviceLocked => "Your Ledger is locked, please unlock it...".to_string(),
                Error::DeviceOnDashboardExpected => {
                    "Please quit the application opened on your Ledger...".to_string()
                }
                Error::DeviceNotOnboarded => {
                    "Your Ledger is not set up yet. Please set it up first.".to_string()
                }
                e => format!("Error fetching device info: {}. Is the Ledger unlocked?", e),
            });
            return (false, None);
        }
    };
    log::info!("Ledger connected: {}", info.firmware_summary());
    if let Some(model) = info.model.or(usb_model) {
        state.model = model.to_string();
    } else {
        state.model = format!("Unknown Ledger (target id {:#010x})", info.target_id);
    }
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
    send(&state);
    if info.is_bootloader {
        r.status("Your Ledger is in bootloader mode: a firmware update was probably interrupted. Click 'Repair' to finish it.");
        return (true, None);
    }

    // The messages to display once everything is loaded.
    let mut notes: Vec<String> = Vec::new();
    r.status("Querying the latest firmware on Ledger API...");
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
            notes.push(format!("Could not check the latest firmware: {}", e));
            None
        }
    };
    send(&state);
    if info.is_osu {
        r.status("A firmware update was interrupted, your Ledger is in updater mode. Click 'Update' to complete it.");
        return (true, update);
    }

    r.status("Querying latest apps on Ledger API...");
    match get_latest_apps(&info) {
        Ok((bitcoin, test)) => {
            state.bitcoin.latest = bitcoin.map(|a| a.version);
            state.bitcoin_test.latest = test.map(|a| a.version);
        }
        Err(e) => notes.push(format!("Fail to get latest apps from Ledger API: {}", e)),
    }
    r.status("Querying installed apps. Please confirm on device.");
    let res = installed_bitcoin_apps(&transport);
    if let Ok((bitcoin, test)) = &res {
        state.bitcoin.installed = bitcoin.clone();
        state.bitcoin_test.installed = test.clone();
    }
    send(&state);
    match res {
        Err(e) => r.alarm(format!("Error listing installed applications: {}.", e)),
        Ok(_) => {
            let not_installed = |app: &AppState| app.installed == Installed::No;
            if notes.is_empty()
                && not_installed(&state.bitcoin)
                && not_installed(&state.bitcoin_test)
            {
                notes.push(
                    "The Bitcoin app is not installed. Click 'Install' to install it.".into(),
                );
            }
            r.status(notes.join("\n"));
        }
    }
    (true, update)
}

/// Whether the Bitcoin and Bitcoin Test apps are installed. The user may have to allow it on the
/// device.
fn installed_bitcoin_apps(transport: &TransportNativeHID) -> Result<(Installed, Installed), Error> {
    let apps = list_installed_apps_raw(transport)?;
    let bitcoin = apps.iter().find(|a| a.name == BITCOIN_APP_NAME);
    let test = apps.iter().find(|a| a.name == BITCOIN_TEST_APP_NAME);
    // The Ledger API gives the versions of the apps from their hashes.
    let hashes: Vec<Vec<u8>> = [bitcoin, test]
        .iter()
        .flatten()
        .map(|a| a.hash.clone())
        .collect();
    let infos = if hashes.is_empty() {
        Vec::new()
    } else {
        apps_by_hashes(hashes).unwrap_or_else(|e| {
            log::error!("Error querying the installed apps versions: {}", e);
            Vec::new()
        })
    };
    let installed = |name: &str, found: bool| {
        if !found {
            return Installed::No;
        }
        let info = infos.iter().flatten().find(|i| i.version_name == name);
        Installed::Version(info.map_or("unknown".to_string(), |i| i.version.clone()))
    };
    Ok((
        installed(BITCOIN_APP_NAME, bitcoin.is_some()),
        installed(BITCOIN_TEST_APP_NAME, test.is_some()),
    ))
}

/// Install (or update, if `update` is set) the Bitcoin (or Bitcoin Test) app.
pub fn install_app(r: &Reporter, testnet: bool, update: bool) -> Outcome {
    let name = if testnet {
        BITCOIN_TEST_APP_NAME
    } else {
        BITCOIN_APP_NAME
    };
    let (transport, _) = match connect() {
        Ok(t) => t,
        Err(e) => return Outcome::Failed(e),
    };
    let progress = |step| report_app_step(r, step);
    let res = if update {
        update_bitcoin_app(&transport, testnet, progress)
    } else {
        install_bitcoin_app(&transport, testnet, progress)
    };
    match (res, update) {
        (Ok(()), false) => Outcome::Done(format!("Successfully installed the {} app.", name)),
        (Ok(()), true) => Outcome::Done(format!("Successfully updated the {} app.", name)),
        (Err(e), false) => Outcome::Failed(format!("Error installing the {} app: {}", name, e)),
        (Err(e), true) => Outcome::Failed(format!("Error updating the {} app: {}", name, e)),
    }
}

pub fn genuine_check(r: &Reporter) -> Outcome {
    let (transport, _) = match connect() {
        Ok(t) => t,
        Err(e) => {
            r.alarm(e);
            return Outcome::Reported;
        }
    };
    r.status("Checking if the device is genuine...");
    let res = ledger_manager::genuine_check(&transport, |e| match e {
        SocketEvent::DevicePermissionRequested => {
            r.status("Please allow the Ledger manager on your device.")
        }
        SocketEvent::DevicePermissionGranted => {
            r.status("Ledger manager allowed. Checking if the device is genuine...")
        }
        _ => {}
    });
    match res {
        Ok(()) => {
            r.send(Event::Genuine(true));
            r.status("");
        }
        Err(Error::NotGenuine(_)) => {
            r.send(Event::Genuine(false));
            r.alarm("WARNING: your device is NOT genuine!");
        }
        Err(e) => r.alarm(format!("Error when performing the genuine check: {}", e)),
    }
    Outcome::Reported
}

/// Update the firmware, backing up the device settings before and restoring them after. This
/// takes minutes, the device restarts several times. With `backup_file`, the backup is saved to a
/// file before the update starts, and the update is not started if it can't be saved.
pub fn update_firmware(r: &Reporter, update: &FirmwareUpdateInfo, backup_file: bool) -> Outcome {
    let backup_dir = match default_backup_dir() {
        _ if !backup_file => None,
        Some(dir) => Some(dir),
        None => {
            r.send(Event::BackupNotSaved(
                "could not determine the configuration directory".to_string(),
            ));
            return Outcome::Reported;
        }
    };
    let mut backup_saved = false;
    let res = HidApi::new().map_err(Error::from).and_then(|mut api| {
        update_firmware_and_restore(&mut api, update, backup_dir.as_deref(), |step| {
            if let UpdateAndRestoreStep::BackupSaved { .. }
            | UpdateAndRestoreStep::BackupLoaded { .. } = step
            {
                backup_saved = true;
            }
            report_update_step(r, step)
        })
    });
    match res {
        Ok(result) => update_result(&result),
        Err(Error::BackupNotSaved(e)) => {
            r.send(Event::BackupNotSaved(e));
            Outcome::Reported
        }
        Err(e) if backup_saved => Outcome::Failed(format!(
            "Error updating the firmware: {}. The backup of the device settings is saved: complete the update ('Update' or 'Repair'), the settings are restored after it. If the device then runs the new firmware without having restored them, use the restorebackup command of the CLI.",
            e
        )),
        Err(e) => Outcome::Failed(format!("Error updating the firmware: {}.", e)),
    }
}

/// The message at the end of the update. It is an error if something could not be restored.
fn update_result(result: &UpdateAndRestoreResult) -> Outcome {
    let mut lines = vec![format!(
        "Successfully updated the firmware to {}.",
        result.device_info.version
    )];
    let mut failed = false;
    let mut bitcoin_reinstalled = false;
    match (&result.report, &result.restore_error) {
        (Some(report), _) => {
            lines.push("Restoration of the device settings:".to_string());
            lines.extend(report.lines().into_iter().map(|l| format!("- {}", l)));
            // Only the Bitcoin apps are reinstalled.
            bitcoin_reinstalled = !report.reinstalled_apps().is_empty();
            failed = !report.is_complete();
        }
        (None, Some(e)) => {
            failed = true;
            lines.push(format!("The device settings could not be restored: {}", e));
            if let Some(path) = &result.backup_path {
                lines.push(format!(
                    "The backup is saved in {}, it can be restored with the CLI (restorebackup command).",
                    path.display()
                ));
            }
        }
        (None, None) => lines.push(
            "The device settings were not backed up, so they could not be restored.".to_string(),
        ),
    }
    if bitcoin_reinstalled {
        lines.push("The wallet policies registered in the Bitcoin app are not restored: you may have to register your wallet again from your wallet software.".to_string());
    } else {
        lines.push("Click 'Install' to install the Bitcoin app if you need it.".to_string());
    }
    let msg = lines.join("\n");
    if failed {
        Outcome::Failed(msg)
    } else {
        Outcome::Done(msg)
    }
}

/// Finish a firmware update interrupted while the device was in bootloader mode.
pub fn repair_firmware(r: &Reporter) -> Outcome {
    let res = HidApi::new().map_err(Error::from).and_then(|mut api| {
        ledger_manager::repair_firmware(&mut api, None, |step| report_firmware_step(r, step))
    });
    match res {
        Ok(info) => Outcome::Done(format!(
            "Successfully repaired the device, it now runs {}.",
            info.version
        )),
        Err(e) => Outcome::Failed(format!("Error repairing the firmware: {}", e)),
    }
}

fn report_app_step(r: &Reporter, step: AppInstallStep) {
    match step {
        AppInstallStep::ListingApps => {
            r.status("Querying installed apps. Please confirm on device.")
        }
        AppInstallStep::QueryingApi => r.status("Querying the Ledger API..."),
        AppInstallStep::AllowManagerRequested => {
            r.status("Please allow the Ledger manager on your device.")
        }
        AppInstallStep::AllowManagerGranted => r.status("Ledger manager allowed."),
        AppInstallStep::Uninstalling { progress } => {
            r.percent("Uninstalling the previous version", progress)
        }
        AppInstallStep::Installing { progress } => r.percent("Installing", progress),
        AppInstallStep::Retrying { attempt, error } => r.status(format!(
            "Error: {}. Retrying (attempt {})...",
            error, attempt
        )),
        AppInstallStep::Done => r.send(Event::Progress(None)),
    }
}

fn report_firmware_step(r: &Reporter, step: FirmwareUpdateStep) {
    match step {
        FirmwareUpdateStep::Preparing => r.status("Preparing the update..."),
        FirmwareUpdateStep::AllowManagerRequested => {
            r.status("Please allow the Ledger manager on your device.")
        }
        FirmwareUpdateStep::AllowManagerGranted => r.status("Ledger manager allowed."),
        FirmwareUpdateStep::InstallingOsu { progress } => {
            r.percent("Transferring the update to the device", progress)
        }
        FirmwareUpdateStep::WaitingUserConfirmation { identifier: Some(id) } => {
            r.status("Please confirm the update on your device, after checking the identifier it displays matches:");
            r.info(Some(id));
        }
        FirmwareUpdateStep::WaitingUserConfirmation { identifier: None } => {
            r.status("Please confirm the update on your device.")
        }
        FirmwareUpdateStep::UserConfirmed => {
            r.info(None);
            r.status("Update confirmed on the device.")
        }
        FirmwareUpdateStep::WaitingForReboot => {
            r.status("Waiting for the device to restart. Keep it plugged in...")
        }
        FirmwareUpdateStep::WaitingForBootloader => {
            r.status("Waiting for the device to restart in bootloader mode...")
        }
        FirmwareUpdateStep::FlashingBootloader { progress } => {
            r.percent("Updating the bootloader", progress)
        }
        FirmwareUpdateStep::FlashingMcu { progress } => r.percent("Updating the MCU", progress),
        FirmwareUpdateStep::InstallingFinal { progress } => {
            r.percent("Installing the firmware", progress)
        }
        FirmwareUpdateStep::WaitingForDevice => r.status("The device is installing the update. Waiting for it to restart on the new firmware, this can take several minutes. Keep it plugged in..."),
        FirmwareUpdateStep::DeviceLocked => {
            r.status("Your device is locked: please unlock it to continue the update.")
        }
        FirmwareUpdateStep::Done { device_info } => r.status(format!(
            "Firmware updated, the device now runs {}.",
            device_info.version
        )),
    }
}

fn report_update_step(r: &Reporter, step: UpdateAndRestoreStep) {
    match step {
        UpdateAndRestoreStep::Backup(BackupStep::ListingApps) => r.status("Backing up the list of installed apps. Please allow the Ledger manager on your device if it asks for it."),
        UpdateAndRestoreStep::Backup(BackupStep::FetchingLockScreen) => r.status("Backing up the lock screen picture. Please approve the backup on your device if it asks for it."),
        UpdateAndRestoreStep::Backup(BackupStep::FetchingLockScreenProgress { progress }) => {
            r.percent("Backing up the lock screen picture", progress)
        }
        UpdateAndRestoreStep::Backup(BackupStep::Done) => r.send(Event::Progress(None)),
        UpdateAndRestoreStep::BackupSaved { path } => r.status(format!(
            "Backup of the device settings saved to {}.",
            path.display()
        )),
        UpdateAndRestoreStep::BackupLoaded { path } => r.status(format!(
            "Resuming the interrupted update. The settings will be restored from {}.",
            path.display()
        )),
        UpdateAndRestoreStep::BackupSkipped { reason } => {
            log::warn!("The device settings were not backed up: {}", reason)
        }
        UpdateAndRestoreStep::Firmware(step) => report_firmware_step(r, step),
        UpdateAndRestoreStep::Restore(step) => report_restore_step(r, step),
    }
}

fn report_restore_step(r: &Reporter, step: RestoreStep) {
    match step {
        RestoreStep::InstallingLanguage { language } => r.status(format!(
            "Restoring the language of your device ({})...",
            language
        )),
        RestoreStep::Language(LanguageInstallStep::Downloading) => {
            r.status("Downloading the language pack...")
        }
        RestoreStep::Language(LanguageInstallStep::PermissionRequested) => {
            r.status("Please approve the language installation on your device.")
        }
        RestoreStep::Language(LanguageInstallStep::Installing { progress }) => {
            r.percent("Installing the language", progress)
        }
        RestoreStep::RestoringLockScreen => r.status("Restoring the lock screen picture..."),
        RestoreStep::LockScreen(LoadImageStep::LoadPermissionRequested) => {
            r.status("Please approve the restoration of the lock screen picture on your device.")
        }
        RestoreStep::LockScreen(LoadImageStep::Loading { progress }) => {
            r.percent("Restoring the lock screen picture", progress)
        }
        RestoreStep::LockScreen(LoadImageStep::CommitPermissionRequested) => {
            r.status("Please confirm the lock screen picture on your device.")
        }
        RestoreStep::ListingApps => r.status("Reinstalling the apps. Please allow the Ledger manager on your device if it asks for it."),
        RestoreStep::InstallingApp { name, index, total } => {
            r.status(format!("Reinstalling {} ({}/{})...", name, index, total))
        }
        // Keep the "Reinstalling" message.
        RestoreStep::App(AppInstallStep::Installing { progress }) => {
            r.send(Event::Progress(Some(progress)))
        }
        RestoreStep::App(step) => report_app_step(r, step),
        RestoreStep::Done => r.send(Event::Progress(None)),
    }
}
