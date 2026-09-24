mod bitbox;

use std::{
    env,
    io::{self, Write},
    path::PathBuf,
    process,
    time::Duration,
};

use ledger_manager::{
    check_firmware_update_supported, current_firmware, default_backup_dir, find_latest_backup,
    firmware_update_resets_customization, genuine_check_with_events,
    install_bitcoin_app_with_progress, latest_firmware,
    ledger_transport_hidapi::{hidapi::HidApi, TransportNativeHID},
    list_installed_apps, load_backup, open_bitcoin_app, open_device, repair_firmware,
    restore_device_settings, update_bitcoin_app_with_progress, update_firmware,
    update_firmware_and_restore, AppInstallStep, BackupLocation, BackupStep, DeviceInfo,
    DeviceModel, Error, FirmwareUpdateOptions, FirmwareUpdateStep, InstallErr, LanguageInstallStep,
    LoadImageStep, RestoreReport, RestoreStep, SocketEvent, UpdateAndRestoreOptions,
    UpdateAndRestoreStep, UpdateErr,
};

// Print on stderr and exit with 1.
macro_rules! error {
    ($($arg:tt)*) => {{
        eprintln!($($arg)*);
        process::exit(1);
    }};
}

#[derive(Debug, Clone, Copy)]
enum Command {
    GetInfo,
    GenuineCheck,
    InstallMainApp,
    UpdateMainApp,
    OpenMainApp,
    InstallTestApp,
    UpdateTestApp,
    OpenTestApp,
    CheckFirmware,
    UpdateFirmware,
    RepairFirmware,
    RestoreBackup,
}

impl Command {
    /// Read command from environment variables.
    pub fn get() -> Option<Self> {
        let is_testnet = env::var("LEDGER_TESTNET").is_ok();
        let cmd_str = env::var("LEDGER_COMMAND").ok()?;

        if cmd_str == "getinfo" {
            Some(Self::GetInfo)
        } else if cmd_str == "genuinecheck" {
            Some(Self::GenuineCheck)
        } else if cmd_str == "installapp" {
            Some(if is_testnet {
                Self::InstallTestApp
            } else {
                Self::InstallMainApp
            })
        } else if cmd_str == "updateapp" {
            Some(if is_testnet {
                Self::UpdateTestApp
            } else {
                Self::UpdateMainApp
            })
        } else if cmd_str == "openapp" {
            Some(if is_testnet {
                Self::OpenTestApp
            } else {
                Self::OpenMainApp
            })
        } else if cmd_str == "checkfirm" {
            Some(Self::CheckFirmware)
        } else if cmd_str == "updatefirm" {
            Some(Self::UpdateFirmware)
        } else if cmd_str == "repairfirm" {
            Some(Self::RepairFirmware)
        } else if cmd_str == "restorebackup" {
            Some(Self::RestoreBackup)
        } else {
            None
        }
    }
}

fn hid_api() -> HidApi {
    match HidApi::new() {
        Ok(a) => a,
        Err(e) => error!("Error initializing HID api: {}.", e),
    }
}

fn ledger_api(hid_api: &HidApi) -> (TransportNativeHID, Option<DeviceModel>) {
    match open_device(hid_api) {
        Ok(a) => a,
        Err(e) => error!("Error connecting to Ledger device: {}", e),
    }
}

fn device_info(ledger_api: &TransportNativeHID) -> DeviceInfo {
    match DeviceInfo::new(ledger_api) {
        Ok(i) => i,
        Err(e) => error!("Error fetching device info: {}", e),
    }
}

fn model_name(device_info: &DeviceInfo, usb_model: Option<DeviceModel>) -> String {
    match device_info.model.or(usb_model) {
        Some(m) => m.to_string(),
        None => format!("Unknown model (target id {:#010x})", device_info.target_id),
    }
}

fn print_progress(label: &str, progress: f32) {
    print!("\r{}: {:>3}%", label, (progress * 100.0).round() as u32);
    let _ = io::stdout().flush();
    if progress >= 1.0 {
        println!();
    }
}

fn print_app_step(step: AppInstallStep) {
    match step {
        AppInstallStep::ListingApps => println!(
            "Querying installed applications from your Ledger. You might have to confirm on your device."
        ),
        AppInstallStep::QueryingApi => println!("Querying the Ledger API."),
        AppInstallStep::AllowManagerRequested => {
            println!("Please allow the Ledger manager on your device.")
        }
        AppInstallStep::AllowManagerGranted => println!("Ledger manager allowed."),
        AppInstallStep::Uninstalling { progress } => {
            print_progress("Uninstalling the previous version", progress)
        }
        AppInstallStep::Installing { progress, .. } => print_progress("Installing", progress),
        AppInstallStep::Retrying { attempt, error } => {
            println!("\nError: {}. Retrying (attempt {}).", error, attempt)
        }
        AppInstallStep::Done => {}
    }
}

fn print_ledger_info(ledger_api: &TransportNativeHID, usb_model: Option<DeviceModel>) {
    let device_info = device_info(ledger_api);
    println!("Device model: {}", model_name(&device_info, usb_model));
    println!("Firmware: {}", device_info.firmware_summary());
    println!("Mode: {}", device_info.mode());
    if !device_info.onboarded {
        println!("The device is not set up yet.");
    }
    if device_info.has_dev_firmware {
        println!("WARNING: this device runs a development firmware.");
    }
    println!("Information about the device: {:#?}", device_info);

    if !device_info.is_normal_mode() {
        println!("Device is not running normally, not querying installed applications.");
        return;
    }
    println!("Querying installed applications from your Ledger. You might have to confirm on your device.");
    let apps = match list_installed_apps(ledger_api) {
        Ok(a) => a,
        Err(e) => error!("Error listing installed applications: {}.", e),
    };
    println!("Installed applications:");
    for app in apps {
        println!("  - {:?}", app);
    }
}

fn perform_genuine_check(ledger_api: &TransportNativeHID) {
    println!("Querying Ledger's remote HSM to perform the genuine check. You might have to confirm the operation on your device.");
    if let Err(e) = genuine_check_with_events(ledger_api, |e| {
        if e == SocketEvent::DevicePermissionRequested {
            println!("Please allow the Ledger manager on your device.");
        }
    }) {
        error!("Error when performing genuine check: {}", e);
    }
    println!("Success. Your Ledger is genuine.");
}

// Install the Bitcoin app on the device.
fn install_app(ledger_api: &TransportNativeHID, is_testnet: bool) {
    println!("You may have to allow on your device 1) listing installed apps 2) the Ledger manager to install the app.");
    match install_bitcoin_app_with_progress(ledger_api, is_testnet, print_app_step) {
        Ok(()) => println!("Successfully installed the app."),
        Err(InstallErr::AlreadyInstalled) => {
            error!("Bitcoin app already installed. Use the update command to update it.")
        }
        Err(InstallErr::AppNotFound) => error!("Could not get info about Bitcoin app."),
        Err(InstallErr::Any(e)) => error!("Error installing Bitcoin app: {}", e),
    }
}

fn update_app(ledger_api: &TransportNativeHID, is_testnet: bool) {
    println!("You may have to allow on your device 1) listing installed apps 2) the Ledger manager to install the app.");
    match update_bitcoin_app_with_progress(ledger_api, is_testnet, print_app_step) {
        Ok(()) => println!("Successfully updated the app."),
        Err(UpdateErr::NotInstalled) => {
            error!("Bitcoin app isn't installed. Use the install command instead.")
        }
        Err(UpdateErr::AppNotFound) => error!("Could not get info about Bitcoin app."),
        Err(UpdateErr::AlreadyLatest) => error!("Bitcoin app is already at the latest version."),
        Err(UpdateErr::Any(e)) => error!("Error updating Bitcoin app: {}", e),
    }
}

fn open_app(ledger_api: &TransportNativeHID, is_testnet: bool) {
    if let Err(e) = open_bitcoin_app(ledger_api, is_testnet) {
        error!("Error opening Bitcoin app: {}", e);
    }
}

fn check_firmware(ledger_api: &TransportNativeHID, usb_model: Option<DeviceModel>) {
    let device_info = device_info(ledger_api);
    println!("Device model: {}", model_name(&device_info, usb_model));
    if device_info.is_bootloader {
        error!("Device is in bootloader mode, a firmware update was probably interrupted. Firmware information can't be queried. Use the repairfirm command to complete the update.");
    }
    if device_info.is_osu {
        println!(
            "Device is in updater mode: a firmware update to {} is in progress. Use the updatefirm command to complete it.",
            device_info.version
        );
    } else {
        let current = match current_firmware(&device_info) {
            Ok(f) => f,
            Err(e) => error!("Error querying the current firmware: {}", e),
        };
        println!(
            "Current firmware: {} ({})",
            current.name,
            device_info.firmware_summary()
        );
    }
    match latest_firmware(&device_info) {
        Ok(Some(update)) => {
            println!("Firmware update available: {}.", update.version());
            if let Some(notes) = update
                .final_firmware
                .notes
                .as_ref()
                .filter(|n| !n.is_empty())
            {
                println!("Release notes:\n{}", notes);
            }
            if let Err(e) = check_firmware_update_supported(&device_info) {
                println!("However: {}", e);
            } else {
                println!("Use the updatefirm command to update.");
            }
        }
        Ok(None) => println!("Firmware is up to date."),
        Err(e) => error!("Error querying the latest firmware: {}", e),
    }
}

/// The directory of the backups of the device settings: `LEDGER_BACKUP_DIR` or the default one.
fn backup_dir() -> Option<PathBuf> {
    env::var_os("LEDGER_BACKUP_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(default_backup_dir)
}

fn update_firm(mut hid_api: HidApi) {
    let (update, resets_customization) = {
        let (ledger_api, _) = ledger_api(&hid_api);
        let device_info = device_info(&ledger_api);
        if let Err(e) = check_firmware_update_supported(&device_info) {
            error!("{}", e);
        }
        let update = match latest_firmware(&device_info) {
            Ok(Some(u)) => u,
            Ok(None) => {
                println!("Firmware is already up to date.");
                return;
            }
            Err(e) => error!("Error querying the latest firmware: {}", e),
        };
        let resets = firmware_update_resets_customization(&device_info, &update);
        println!(
            "Updating the firmware from {} to {}.",
            device_info.version,
            update.version()
        );
        (update, resets)
        // The transport is dropped here: the update opens its own connections to the device.
    };

    if env::var("LEDGER_NO_RESTORE").is_ok() {
        println!("LEDGER_NO_RESTORE is set: the device settings won't be backed up nor restored.");
        println!("WARNING: the applications installed on your device will be removed by the update. You will need to reinstall them afterwards (your funds are not affected, make sure you have your recovery phrase at hand).");
        if resets_customization {
            println!(
                "NOTE: your device's language and lock screen picture may be reset by the update."
            );
        }
        println!("Keep your device connected and unlocked during the whole update. It can take several minutes.");
        let res = update_firmware(&mut hid_api, &update, |step| {
            print_firmware_step(step, false)
        });
        match res {
            Ok(_) => println!("Successfully updated the firmware. You can now reinstall the Bitcoin app with the installapp command."),
            Err(e) => error!("\nError updating the firmware: {}", e),
        }
        return;
    }

    let backup_location = if env::var("LEDGER_NO_BACKUP_FILE").is_ok() {
        println!("LEDGER_NO_BACKUP_FILE is set: the backup of the device settings is only kept in memory, it will be lost if the update is interrupted.");
        BackupLocation::MemoryOnly
    } else {
        match backup_dir() {
            Some(dir) => BackupLocation::Directory(dir),
            None => error!("Could not determine where to save the backup of the device settings. Set LEDGER_BACKUP_DIR to a directory, or set LEDGER_NO_BACKUP_FILE=1 to update without saving the backup to a file."),
        }
    };
    println!("The apps installed on your device will be removed by the update. Before the update, the list of installed apps, the language and the custom lock screen picture (if any) are backed up, and they are restored after the update. Your funds are not affected, but make sure you have your recovery phrase at hand.");
    println!("NOTE: the data stored inside the apps is not restored. In particular the wallet policies registered in the Bitcoin app (multisig, Liana, ...) are lost: you may have to register your wallet again from your wallet software.");
    if resets_customization {
        println!("You will have to approve the backup and the restoration of the lock screen picture and of the language on your device.");
    }
    println!("Keep your device connected and unlocked during the whole update. It can take several minutes.");

    let options = UpdateAndRestoreOptions {
        backup_location,
        firmware: FirmwareUpdateOptions::default(),
    };
    let res = update_firmware_and_restore(&mut hid_api, &update, &options, print_update_step);
    match res {
        Ok(result) => {
            println!("Successfully updated the firmware to {}.", result.device_info.version);
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
        Err(e @ Error::BackupNotSaved(_)) => error!(
            "{}\nThe firmware update was NOT started. Fix the problem (or set LEDGER_BACKUP_DIR to another directory) and retry, or set LEDGER_NO_BACKUP_FILE=1 to update while keeping the backup in memory only.",
            e
        ),
        Err(e) => error!(
            "\nError updating the firmware: {}\nOnce the update is completed (run updatefirm or repairfirm again), restore the settings with the restorebackup command.",
            e
        ),
    }
}

fn restore_backup(hid_api: HidApi) {
    let (ledger_api, _) = ledger_api(&hid_api);
    let path = match env::var_os("LEDGER_BACKUP_FILE").filter(|f| !f.is_empty()) {
        Some(f) => PathBuf::from(f),
        None => {
            let info = device_info(&ledger_api);
            let dir = match backup_dir() {
                Some(d) => d,
                None => error!("Set LEDGER_BACKUP_FILE to the backup file to restore."),
            };
            match find_latest_backup(&dir, info.target_id, Duration::MAX) {
                Some((path, _)) => {
                    println!("LEDGER_BACKUP_FILE is not set, using the latest backup of this device model.");
                    path
                }
                None => error!(
                    "No backup of this device found in {}. Set LEDGER_BACKUP_FILE to the backup file to restore.",
                    dir.display()
                ),
            }
        }
    };
    let backup = match load_backup(&path) {
        Ok(b) => b,
        Err(e) => error!("Error loading the backup {}: {}", path.display(), e),
    };
    println!("Restoring the backup {}:", path.display());
    for line in backup.summary() {
        println!("  {}", line);
    }
    match restore_device_settings(&ledger_api, &backup, print_restore_step) {
        Ok(report) => print_restore_report(&report),
        Err(e) => error!("Error restoring the backup: {}", e),
    }
}

fn print_backup_step(step: BackupStep) {
    match step {
        BackupStep::ListingApps => println!(
            "Backing up the list of installed apps. You might have to allow the Ledger manager on your device."
        ),
        BackupStep::QueryingApi => println!("Querying the Ledger API."),
        BackupStep::FetchingLockScreen => println!(
            "Backing up the lock screen picture. If your device asks for it, approve the backup on your device."
        ),
        BackupStep::FetchingLockScreenProgress { progress } => {
            print_progress("Backing up the lock screen picture", progress)
        }
        BackupStep::Done => {}
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
            "Reinstalling the apps. You might have to allow the Ledger manager on your device."
        ),
        RestoreStep::InstallingApp { name, index, total } => {
            println!("Installing {} ({}/{}).", name, index, total)
        }
        RestoreStep::App(step) => print_app_step(step),
        RestoreStep::Done { .. } => {}
    }
}

fn print_update_step(step: UpdateAndRestoreStep) {
    match step {
        UpdateAndRestoreStep::Backup(step) => print_backup_step(step),
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
        UpdateAndRestoreStep::Firmware(step) => print_firmware_step(step, false),
        UpdateAndRestoreStep::Restore(step) => print_restore_step(step),
    }
}

fn print_restore_report(report: &RestoreReport) {
    println!("Restoration report:");
    for line in report.lines() {
        println!("  {}", line);
    }
    if !report.is_complete() {
        println!("Some settings could not be restored. You can retry with the restorebackup command, or install the Bitcoin app with the installapp command.");
    }
    if report
        .reinstalled_apps()
        .iter()
        .any(|a| *a == "Bitcoin" || *a == "Bitcoin Test")
    {
        println!("NOTE: the wallet policies registered in the Bitcoin app are not restored: you may have to register your wallet again from your wallet software.");
    }
}

fn repair_firm(mut hid_api: HidApi) {
    let forced_version = env::var("LEDGER_REPAIR_VERSION").ok();
    println!("Repairing the firmware of the device in bootloader mode. Keep your device connected during the whole repair.");
    let res = repair_firmware(&mut hid_api, forced_version.as_deref(), |step| {
        print_firmware_step(step, true)
    });
    match res {
        Ok(info) if info.is_osu => println!(
            "The device is now in updater mode. Use the updatefirm command to complete the firmware update."
        ),
        Ok(_) => println!(
            "Successfully repaired the firmware. Use the checkfirm command to check whether a firmware update is available."
        ),
        Err(e) => error!("\nError repairing the firmware: {}", e),
    }
}

fn print_firmware_step(step: FirmwareUpdateStep, is_repair: bool) {
    match step {
        FirmwareUpdateStep::Preparing => println!("Preparing the update."),
        FirmwareUpdateStep::AllowManagerRequested => {
            println!("Please allow the Ledger manager on your device.")
        }
        FirmwareUpdateStep::AllowManagerGranted => println!("Ledger manager allowed."),
        FirmwareUpdateStep::InstallingOsu { progress } => {
            print_progress("Transferring the update", progress)
        }
        FirmwareUpdateStep::WaitingUserConfirmation { identifier } => {
            println!();
            match identifier {
                Some(id) => println!(
                    "Please confirm the update on your device. Check the identifier displayed on the device is: {}",
                    id
                ),
                None => println!("Please confirm the update on your device."),
            }
        }
        FirmwareUpdateStep::UserConfirmed => println!("Update confirmed on the device."),
        FirmwareUpdateStep::WaitingForReboot => println!("Waiting for the device to restart."),
        FirmwareUpdateStep::WaitingForBootloader if is_repair => {
            println!("Waiting for the device in bootloader mode.")
        }
        FirmwareUpdateStep::WaitingForBootloader => {
            println!("Waiting for the device to restart in bootloader mode.")
        }
        FirmwareUpdateStep::FlashingBootloader { progress } => {
            print_progress("Updating the bootloader", progress)
        }
        FirmwareUpdateStep::FlashingMcu { progress } => print_progress("Updating the MCU", progress),
        FirmwareUpdateStep::InstallingFinal { progress } => {
            print_progress("Installing the firmware", progress)
        }
        FirmwareUpdateStep::WaitingForDevice => println!(
            "The device is installing the update. Waiting for it to restart on the new firmware (this can take several minutes)."
        ),
        FirmwareUpdateStep::DeviceLocked => println!("Please unlock your device."),
        FirmwareUpdateStep::Done { device_info } => {
            println!("Device now running {}.", device_info.firmware_summary())
        }
    }
}

fn main() {
    if bitbox::run_if_requested() {
        return;
    }

    let command = if let Some(cmd) = Command::get() {
        cmd
    } else {
        error!("Invalid or no command specified. The command must be passed through the LEDGER_COMMAND env var (getinfo, genuinecheck, installapp, updateapp, openapp, checkfirm, updatefirm, repairfirm, restorebackup). Set LEDGER_TESTNET to use the Bitcoin testnet app instead where applicable.");
    };

    let hid_api = hid_api();
    match command {
        Command::UpdateFirmware => return update_firm(hid_api),
        Command::RepairFirmware => return repair_firm(hid_api),
        Command::RestoreBackup => return restore_backup(hid_api),
        _ => {}
    }

    let (ledger_api, usb_model) = ledger_api(&hid_api);
    match command {
        Command::GetInfo => {
            print_ledger_info(&ledger_api, usb_model);
        }
        Command::GenuineCheck => {
            perform_genuine_check(&ledger_api);
        }
        Command::InstallMainApp => {
            install_app(&ledger_api, false);
        }
        Command::InstallTestApp => {
            install_app(&ledger_api, true);
        }
        Command::OpenMainApp => {
            open_app(&ledger_api, false);
        }
        Command::OpenTestApp => {
            open_app(&ledger_api, true);
        }
        Command::UpdateMainApp => {
            update_app(&ledger_api, false);
        }
        Command::UpdateTestApp => {
            update_app(&ledger_api, true);
        }
        Command::CheckFirmware => {
            check_firmware(&ledger_api, usb_model);
        }
        Command::UpdateFirmware | Command::RepairFirmware | Command::RestoreBackup => {
            unreachable!("Handled above.")
        }
    }
}
