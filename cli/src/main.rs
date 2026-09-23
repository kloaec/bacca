mod bitbox;

use std::{
    env,
    io::{self, Write},
    process,
};

use ledger_manager::{
    check_firmware_update_supported, current_firmware, firmware_update_resets_customization,
    genuine_check_with_events, install_bitcoin_app_with_progress, latest_firmware,
    ledger_transport_hidapi::{hidapi::HidApi, TransportNativeHID},
    list_installed_apps, open_bitcoin_app, open_device, update_bitcoin_app_with_progress,
    update_firmware, AppInstallStep, DeviceInfo, DeviceModel, FirmwareUpdateStep, InstallErr,
    SocketEvent, UpdateErr,
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
        error!("Device is in bootloader mode. Firmware information can't be queried.");
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
    println!("WARNING: the applications installed on your device will be removed by the update. You will need to reinstall them afterwards (your funds are not affected, make sure you have your recovery phrase at hand).");
    if resets_customization {
        println!("NOTE: your device's language and lock screen may be reset by the update.");
    }
    println!("Keep your device connected and unlocked during the whole update. It can take several minutes.");

    let res = update_firmware(&mut hid_api, &update, |step| {
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
    });
    match res {
        Ok(_) => println!("Successfully updated the firmware. You can now reinstall the Bitcoin app with the installapp command."),
        Err(e) => error!("\nError updating the firmware: {}", e),
    }
}

fn main() {
    if bitbox::run_if_requested() {
        return;
    }

    let command = if let Some(cmd) = Command::get() {
        cmd
    } else {
        error!("Invalid or no command specified. The command must be passed through the LEDGER_COMMAND env var (getinfo, genuinecheck, installapp, updateapp, openapp, checkfirm, updatefirm). Set LEDGER_TESTNET to use the Bitcoin testnet app instead where applicable.");
    };

    let hid_api = hid_api();
    if let Command::UpdateFirmware = command {
        update_firm(hid_api);
        return;
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
        Command::UpdateFirmware => unreachable!("Handled above."),
    }
}
