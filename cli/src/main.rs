//! The command line interface. The command is passed through the `LEDGER_COMMAND` environment
//! variable for a Ledger, `BITBOX_COMMAND` for a BitBox02, `JADE_COMMAND` for a Jade. See the
//! README for the commands and the options.

use std::{
    env,
    fmt::Display,
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use ledger_manager::{
    check_firmware_update_supported, current_firmware, default_backup_dir, find_latest_backup,
    firmware_update_resets_customization, genuine_check, install_bitcoin_app, latest_firmware,
    ledger_transport_hidapi::{hidapi::HidApi, TransportNativeHID},
    list_installed_apps, load_backup, open_bitcoin_app, open_device, repair_firmware,
    restore_device_settings, update_bitcoin_app, update_firmware, update_firmware_and_restore,
    AppInstallStep, BackupStep, DeviceInfo, DeviceModel, Error, FirmwareUpdateStep,
    LanguageInstallStep, LoadImageStep, RestoreReport, RestoreStep, SocketEvent,
    UpdateAndRestoreStep,
};

/// Print on stderr and exit with 1.
macro_rules! fail {
    ($($arg:tt)*) => {{
        eprintln!($($arg)*);
        std::process::exit(1)
    }};
}

mod bitbox;
mod jade;

/// Exit with an error message on error.
trait OrExit<T> {
    fn or_exit(self, context: &str) -> T;
}

impl<T, E: Display> OrExit<T> for Result<T, E> {
    fn or_exit(self, context: &str) -> T {
        self.unwrap_or_else(|e| fail!("{}: {}", context, e))
    }
}

fn main() {
    if let Ok(command) = env::var("BITBOX_COMMAND") {
        return bitbox::run(&command);
    }
    if let Ok(command) = env::var("JADE_COMMAND") {
        return jade::run(&command);
    }
    let command = env::var("LEDGER_COMMAND").unwrap_or_default();
    let testnet = env::var_os("LEDGER_TESTNET").is_some();
    let mut hid_api = HidApi::new().or_exit("Error initializing the HID API");
    match command.as_str() {
        "getinfo" => get_info(&hid_api),
        "genuinecheck" => check_genuine(&hid_api),
        "installapp" => install_app(&hid_api, testnet, false),
        "updateapp" => install_app(&hid_api, testnet, true),
        "openapp" => {
            open_bitcoin_app(&connect(&hid_api).0, testnet).or_exit("Error opening the Bitcoin app")
        }
        "checkfirm" => check_firmware(&hid_api),
        "updatefirm" => update_firm(&mut hid_api),
        "repairfirm" => repair_firm(&mut hid_api),
        "restorebackup" => restore_backup(&hid_api),
        _ => fail!("Invalid or no command specified. The command must be passed through the LEDGER_COMMAND env var (getinfo, genuinecheck, installapp, updateapp, openapp, checkfirm, updatefirm, repairfirm, restorebackup). Set LEDGER_TESTNET to use the Bitcoin Test app instead where applicable."),
    }
}

fn connect(hid_api: &HidApi) -> (TransportNativeHID, Option<DeviceModel>) {
    open_device(hid_api).or_exit("Error connecting to the Ledger device")
}

fn device_info(transport: &TransportNativeHID) -> DeviceInfo {
    DeviceInfo::new(transport).or_exit("Error fetching the device info")
}

fn model_name(info: &DeviceInfo, usb_model: Option<DeviceModel>) -> String {
    match info.model.or(usb_model) {
        Some(m) => m.to_string(),
        None => format!("Unknown model (target id {:#010x})", info.target_id),
    }
}

/// The directory of the backups of the device settings: `LEDGER_BACKUP_DIR` or the default one.
fn backup_dir() -> Option<PathBuf> {
    env::var_os("LEDGER_BACKUP_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(default_backup_dir)
}

fn get_info(hid_api: &HidApi) {
    let (transport, usb_model) = connect(hid_api);
    let info = device_info(&transport);
    println!("Device model: {}", model_name(&info, usb_model));
    println!("Firmware: {}", info.firmware_summary());
    println!("Mode: {}", info.mode());
    if !info.onboarded {
        println!("The device is not set up yet.");
    }
    if info.has_dev_firmware {
        println!("WARNING: this device runs a development firmware.");
    }
    println!("Information about the device: {:#?}", info);
    if !info.is_normal_mode() {
        println!("Device is not running normally, not querying installed applications.");
        return;
    }
    println!("Querying installed applications from your Ledger. You might have to confirm on your device.");
    let apps = list_installed_apps(&transport).or_exit("Error listing installed applications");
    println!("Installed applications:");
    for app in apps {
        println!("  - {:?}", app);
    }
}

fn check_genuine(hid_api: &HidApi) {
    let (transport, _) = connect(hid_api);
    println!("Querying Ledger's remote HSM to perform the genuine check. You might have to confirm the operation on your device.");
    genuine_check(&transport, |e| {
        if e == SocketEvent::DevicePermissionRequested {
            println!("Please allow the Ledger manager on your device.");
        }
    })
    .or_exit("Error when performing the genuine check");
    println!("Success. Your Ledger is genuine.");
}

/// Install (or update, if `update` is set) the Bitcoin (or Bitcoin Test) app.
fn install_app(hid_api: &HidApi, testnet: bool, update: bool) {
    let (transport, _) = connect(hid_api);
    println!("You may have to allow on your device 1) listing installed apps 2) the Ledger manager to install the app.");
    let res = if update {
        update_bitcoin_app(&transport, testnet, print_app_step)
    } else {
        install_bitcoin_app(&transport, testnet, print_app_step)
    };
    match res {
        Ok(()) if update => println!("Successfully updated the app."),
        Ok(()) => println!("Successfully installed the app."),
        Err(Error::AppAlreadyInstalled) => {
            fail!("The app is already installed. Use the updateapp command to update it.")
        }
        Err(Error::AppNotInstalled) => {
            fail!("The app isn't installed. Use the installapp command instead.")
        }
        Err(Error::AppAlreadyLatest) => fail!("The app is already at the latest version."),
        Err(e) if update => fail!("Error updating the app: {}", e),
        Err(e) => fail!("Error installing the app: {}", e),
    }
}

fn check_firmware(hid_api: &HidApi) {
    let (transport, usb_model) = connect(hid_api);
    let info = device_info(&transport);
    println!("Device model: {}", model_name(&info, usb_model));
    if info.is_bootloader {
        fail!("Device is in bootloader mode, a firmware update was probably interrupted. Firmware information can't be queried. Use the repairfirm command to complete the update.");
    }
    if info.is_osu {
        println!(
            "Device is in updater mode: a firmware update to {} is in progress. Use the updatefirm command to complete it.",
            info.version
        );
    } else {
        let current = current_firmware(&info).or_exit("Error querying the current firmware");
        println!(
            "Current firmware: {} ({})",
            current.name,
            info.firmware_summary()
        );
    }
    match latest_firmware(&info).or_exit("Error querying the latest firmware") {
        None => println!("Firmware is up to date."),
        Some(update) => {
            println!("Firmware update available: {}.", update.version());
            if let Some(notes) = update.final_firmware.notes.filter(|n| !n.is_empty()) {
                println!("Release notes:\n{}", notes);
            }
            match check_firmware_update_supported(&info) {
                Ok(()) => println!("Use the updatefirm command to update."),
                Err(e) => println!("However: {}", e),
            }
        }
    }
}

fn update_firm(hid_api: &mut HidApi) {
    let (update, resets_customization) = {
        let info = device_info(&connect(hid_api).0);
        check_firmware_update_supported(&info).or_exit("Cannot update the firmware");
        let Some(update) = latest_firmware(&info).or_exit("Error querying the latest firmware")
        else {
            println!("Firmware is already up to date.");
            return;
        };
        println!(
            "Updating the firmware from {} to {}.",
            info.version,
            update.version()
        );
        let resets = firmware_update_resets_customization(&info, &update);
        (update, resets)
        // The transport is dropped here: the update opens its own connections to the device.
    };
    let keep_connected = "Keep your device connected and unlocked during the whole update. It can take several minutes.";

    if env::var_os("LEDGER_NO_RESTORE").is_some() {
        println!("LEDGER_NO_RESTORE is set: the device settings won't be backed up nor restored.");
        println!("WARNING: the applications installed on your device will be removed by the update. You will need to reinstall them afterwards (your funds are not affected, make sure you have your recovery phrase at hand).");
        if resets_customization {
            println!(
                "NOTE: your device's language and lock screen picture may be reset by the update."
            );
        }
        println!("{}", keep_connected);
        update_firmware(hid_api, &update, print_firmware_step)
            .or_exit("\nError updating the firmware");
        println!("Successfully updated the firmware. You can now reinstall the Bitcoin app with the installapp command.");
        return;
    }

    let backup_dir = if env::var_os("LEDGER_NO_BACKUP_FILE").is_some() {
        println!("LEDGER_NO_BACKUP_FILE is set: the backup of the device settings is only kept in memory, it will be lost if the update is interrupted.");
        None
    } else {
        match backup_dir() {
            Some(dir) => Some(dir),
            None => fail!("Could not determine where to save the backup of the device settings. Set LEDGER_BACKUP_DIR to a directory, or set LEDGER_NO_BACKUP_FILE=1 to update without saving the backup to a file."),
        }
    };
    println!("The apps installed on your device will be removed by the update. Before the update, the installed Bitcoin apps, the language and the custom lock screen picture (if any) are backed up, and they are restored after the update. Your funds are not affected, but make sure you have your recovery phrase at hand.");
    println!("NOTE: the data stored inside the apps is not restored. In particular the wallet policies registered in the Bitcoin app (multisig, Liana, ...) are lost: you may have to register your wallet again from your wallet software.");
    if resets_customization {
        println!("You will have to approve the backup and the restoration of the lock screen picture and of the language on your device.");
    }
    println!("{}", keep_connected);

    let res =
        update_firmware_and_restore(hid_api, &update, backup_dir.as_deref(), print_update_step);
    let result = match res {
        Ok(result) => result,
        Err(e @ Error::BackupNotSaved(_)) => fail!(
            "{}\nThe firmware update was NOT started. Fix the problem (or set LEDGER_BACKUP_DIR to another directory) and retry, or set LEDGER_NO_BACKUP_FILE=1 to update while keeping the backup in memory only.",
            e
        ),
        Err(e) => fail!(
            "\nError updating the firmware: {}\nOnce the update is completed (run updatefirm or repairfirm again), restore the settings with the restorebackup command.",
            e
        ),
    };
    println!(
        "Successfully updated the firmware to {}.",
        result.device_info.version
    );
    match (&result.report, &result.restore_error) {
        (Some(report), _) => print_restore_report(report),
        (None, Some(e)) => {
            println!("The settings could not be restored: {}", e);
            if let Some(path) = &result.backup_path {
                println!(
                    "Restore them with: LEDGER_COMMAND=restorebackup LEDGER_BACKUP_FILE={}",
                    path.display()
                );
            }
        }
        (None, None) => println!("There was no backup to restore. You can reinstall the Bitcoin app with the installapp command."),
    }
}

fn restore_backup(hid_api: &HidApi) {
    let (transport, _) = connect(hid_api);
    let path = match env::var_os("LEDGER_BACKUP_FILE").filter(|f| !f.is_empty()) {
        Some(file) => PathBuf::from(file),
        None => {
            let Some(dir) = backup_dir() else {
                fail!("Set LEDGER_BACKUP_FILE to the backup file to restore.");
            };
            let target_id = device_info(&transport).target_id;
            let Some((path, _)) = find_latest_backup(&dir, target_id, Duration::MAX) else {
                fail!(
                    "No backup of this device found in {}. Set LEDGER_BACKUP_FILE to the backup file to restore.",
                    dir.display()
                );
            };
            println!(
                "LEDGER_BACKUP_FILE is not set, using the latest backup of this device model."
            );
            path
        }
    };
    let backup =
        load_backup(&path).or_exit(&format!("Error loading the backup {}", path.display()));
    println!("Restoring the backup {}:", path.display());
    for line in backup.summary() {
        println!("  {}", line);
    }
    let report = restore_device_settings(&transport, &backup, print_restore_step)
        .or_exit("Error restoring the backup");
    print_restore_report(&report);
}

fn print_restore_report(report: &RestoreReport) {
    println!("Restoration report:");
    for line in report.lines() {
        println!("  {}", line);
    }
    if !report.is_complete() {
        println!("Some settings could not be restored. You can retry with the restorebackup command, or install the Bitcoin app with the installapp command.");
    }
    // Only the Bitcoin apps are reinstalled.
    if !report.reinstalled_apps().is_empty() {
        println!("NOTE: the wallet policies registered in the Bitcoin app are not restored: you may have to register your wallet again from your wallet software.");
    }
}

fn repair_firm(hid_api: &mut HidApi) {
    let forced_version = env::var("LEDGER_REPAIR_VERSION").ok();
    println!("Repairing the firmware of the device in bootloader mode. Keep your device connected during the whole repair.");
    let info = repair_firmware(hid_api, forced_version.as_deref(), print_firmware_step)
        .or_exit("\nError repairing the firmware");
    if info.is_osu {
        println!("The device is now in updater mode. Use the updatefirm command to complete the firmware update.");
    } else {
        println!("Successfully repaired the firmware. Use the checkfirm command to check whether a firmware update is available.");
    }
}

/// Print a progress (between 0 and 1) on a single line.
fn print_progress(label: &str, progress: f32) {
    print!("\r{}: {:>3}%", label, (progress * 100.0).round() as u32);
    let _ = io::stdout().flush();
    if progress >= 1.0 {
        println!();
    }
}

fn print_app_step(step: AppInstallStep) {
    match step {
        AppInstallStep::ListingApps => println!("Querying installed applications from your Ledger. You might have to confirm on your device."),
        AppInstallStep::QueryingApi => println!("Querying the Ledger API."),
        AppInstallStep::AllowManagerRequested => {
            println!("Please allow the Ledger manager on your device.")
        }
        AppInstallStep::AllowManagerGranted => println!("Ledger manager allowed."),
        AppInstallStep::Uninstalling { progress } => {
            print_progress("Uninstalling the previous version", progress)
        }
        AppInstallStep::Installing { progress } => print_progress("Installing", progress),
        AppInstallStep::Retrying { attempt, error } => {
            println!("\nError: {}. Retrying (attempt {}).", error, attempt)
        }
        AppInstallStep::Done => {}
    }
}

fn print_firmware_step(step: FirmwareUpdateStep) {
    match step {
        FirmwareUpdateStep::Preparing => println!("Preparing the update."),
        FirmwareUpdateStep::AllowManagerRequested => {
            println!("Please allow the Ledger manager on your device.")
        }
        FirmwareUpdateStep::AllowManagerGranted => println!("Ledger manager allowed."),
        FirmwareUpdateStep::InstallingOsu { progress } => {
            print_progress("Transferring the update", progress)
        }
        FirmwareUpdateStep::WaitingUserConfirmation { identifier: Some(id) } => println!(
            "\nPlease confirm the update on your device. Check the identifier displayed on the device is: {}",
            id
        ),
        FirmwareUpdateStep::WaitingUserConfirmation { identifier: None } => {
            println!("\nPlease confirm the update on your device.")
        }
        FirmwareUpdateStep::UserConfirmed => println!("Update confirmed on the device."),
        FirmwareUpdateStep::WaitingForReboot => println!("Waiting for the device to restart."),
        FirmwareUpdateStep::WaitingForBootloader => {
            println!("Waiting for the device in bootloader mode.")
        }
        FirmwareUpdateStep::FlashingBootloader { progress } => {
            print_progress("Updating the bootloader", progress)
        }
        FirmwareUpdateStep::FlashingMcu { progress } => print_progress("Updating the MCU", progress),
        FirmwareUpdateStep::InstallingFinal { progress } => {
            print_progress("Installing the firmware", progress)
        }
        FirmwareUpdateStep::WaitingForDevice => println!("The device is installing the update. Waiting for it to restart on the new firmware (this can take several minutes)."),
        FirmwareUpdateStep::DeviceLocked => println!("Please unlock your device."),
        FirmwareUpdateStep::Done { device_info } => {
            println!("Device now running {}.", device_info.firmware_summary())
        }
    }
}

fn print_update_step(step: UpdateAndRestoreStep) {
    match step {
        UpdateAndRestoreStep::Backup(BackupStep::ListingApps) => println!("Backing up the list of installed apps. You might have to allow the Ledger manager on your device."),
        UpdateAndRestoreStep::Backup(BackupStep::FetchingLockScreen) => println!("Backing up the lock screen picture. If your device asks for it, approve the backup on your device."),
        UpdateAndRestoreStep::Backup(BackupStep::FetchingLockScreenProgress { progress }) => {
            print_progress("Backing up the lock screen picture", progress)
        }
        UpdateAndRestoreStep::Backup(BackupStep::Done) => {}
        UpdateAndRestoreStep::BackupSaved { path } => {
            println!("Backup of the device settings saved to {}.", path.display())
        }
        UpdateAndRestoreStep::BackupLoaded { path } => println!(
            "Resuming an interrupted update. The settings will be restored from {}.",
            path.display()
        ),
        UpdateAndRestoreStep::BackupSkipped { reason } => println!(
            "WARNING: the device settings could not be backed up ({}). They won't be restored after the update.",
            reason
        ),
        UpdateAndRestoreStep::Firmware(step) => print_firmware_step(step),
        UpdateAndRestoreStep::Restore(step) => print_restore_step(step),
    }
}

fn print_restore_step(step: RestoreStep) {
    match step {
        RestoreStep::InstallingLanguage { language } => {
            println!("Restoring the language of your device ({}).", language)
        }
        RestoreStep::Language(LanguageInstallStep::Downloading) => {
            println!("Downloading the language pack.")
        }
        RestoreStep::Language(LanguageInstallStep::PermissionRequested) => {
            println!("Please approve the language installation on your device.")
        }
        RestoreStep::Language(LanguageInstallStep::Installing { progress }) => {
            print_progress("Installing the language", progress)
        }
        RestoreStep::RestoringLockScreen => println!("Restoring the lock screen picture."),
        RestoreStep::LockScreen(LoadImageStep::LoadPermissionRequested) => {
            println!("Please approve the lock screen picture restoration on your device.")
        }
        RestoreStep::LockScreen(LoadImageStep::Loading { progress }) => {
            print_progress("Restoring the lock screen picture", progress)
        }
        RestoreStep::LockScreen(LoadImageStep::CommitPermissionRequested) => {
            println!("\nPlease confirm the lock screen picture on your device.")
        }
        RestoreStep::ListingApps => println!(
            "Reinstalling the Bitcoin apps. You might have to allow the Ledger manager on your device."
        ),
        RestoreStep::InstallingApp { name, index, total } => {
            println!("Installing {} ({}/{}).", name, index, total)
        }
        RestoreStep::App(step) => print_app_step(step),
        RestoreStep::Done => {}
    }
}
