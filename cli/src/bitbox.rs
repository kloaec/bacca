//! BitBox02 commands, selected with the `BITBOX_COMMAND` env var.

use std::{env, io::Write, path::PathBuf, process};

use bitbox_manager::{
    check_update, default_config_dir, get_status, reboot_bootloader, update_firmware, DeviceStatus,
    Progress, UpdateOptions, UpdateOutcome,
};

// Print on stderr and exit with 1.
macro_rules! error {
    ($($arg:tt)*) => {{
        eprintln!($($arg)*);
        process::exit(1);
    }};
}

/// If `BITBOX_COMMAND` is set, run the BitBox command and return `true`.
pub fn run_if_requested() -> bool {
    let command = match env::var("BITBOX_COMMAND") {
        Ok(c) => c,
        Err(_) => return false,
    };
    match command.as_str() {
        "getinfo" => print_info(),
        "checkfirm" => check_firmware(),
        "updatefirm" => update(),
        // Leave the bootloader, clearing its "start in bootloader mode" flag.
        "reboot" => match reboot_bootloader() {
            Ok(()) => println!("Rebooted the device."),
            Err(e) => error!("Error rebooting the device: {}.", e),
        },
        _ => error!(
            "Invalid BITBOX_COMMAND '{}'. Valid commands: getinfo, checkfirm, updatefirm, reboot.",
            command
        ),
    }
    true
}

fn print_info() {
    match get_status() {
        Ok(DeviceStatus::Firmware(info)) => {
            println!("BitBox02 in firmware mode:");
            match info.product {
                Some(p) => println!("  - Product: {} ({} edition)", p.platform(), p.edition()),
                None => println!("  - Product: unknown"),
            }
            println!("  - Firmware version: v{}", info.version);
            match info.initialized {
                Some(i) => println!("  - Initialized: {}", i),
                None => println!("  - Initialized: unknown (firmware older than v9.20.0)"),
            }
            println!("  - Unlocked: {}", info.unlocked);
        }
        Ok(DeviceStatus::Bootloader(info)) => {
            println!("BitBox02 in bootloader mode:");
            println!(
                "  - Product: {} ({} edition)",
                info.product.platform(),
                info.product.edition()
            );
            println!("  - Bootloader version: v{}", info.bootloader_version);
            if info.erased {
                println!("  - No firmware installed");
            } else {
                println!(
                    "  - Installed firmware monotonic version: {}",
                    info.firmware_version
                );
                println!("  - Firmware hash: {}", hex::encode(info.firmware_hash));
            }
            println!("  - Signing keys version: {}", info.signing_pubkeys_version);
            println!(
                "  - Show firmware hash on boot: {}",
                info.show_firmware_hash_on_boot
            );
        }
        Err(e) => error!("Error getting device info: {}", e),
    }
}

fn check_firmware() {
    let check = match check_update() {
        Ok(c) => c,
        Err(e) => error!("Error checking the latest firmware: {}", e),
    };
    let product = check.status.product().expect("checked by check_update");
    println!(
        "Latest firmware for {}: v{} ({})",
        product, check.latest.version, check.latest.asset_name
    );
    if let Some(h) = check.latest.published_sighash {
        println!("  Published firmware hash: {}", hex::encode(h));
    }
    if let DeviceStatus::Firmware(info) = &check.status {
        println!("Installed firmware: v{}", info.version);
    }
    match check.update_available {
        Some(true) => println!("An update is available. Use BITBOX_COMMAND=updatefirm to install it."),
        Some(false) => println!("The firmware is up to date."),
        None => println!(
            "The device is in bootloader mode: use BITBOX_COMMAND=updatefirm to install the latest firmware."
        ),
    }
}

fn options() -> UpdateOptions {
    let config_dir = env::var_os("BITBOX_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(default_config_dir);
    if config_dir.is_none() {
        eprintln!("Warning: no config directory, the pairing will not be remembered.");
    }
    let show_firmware_hash = match env::var("BITBOX_SHOW_HASH").ok().as_deref() {
        None => None,
        Some("0") | Some("false") => Some(false),
        Some(_) => Some(true),
    };
    UpdateOptions {
        config_dir,
        show_firmware_hash,
        force: env::var_os("BITBOX_FORCE").is_some(),
    }
}

fn print_progress(p: Progress) {
    match p {
        Progress::FetchingReleases => println!("Fetching the firmware releases from GitHub..."),
        Progress::Downloading {
            version,
            intermediate,
        } => println!(
            "Downloading {}firmware v{}...",
            if intermediate { "intermediate " } else { "" },
            version
        ),
        Progress::WaitingForUnlock => println!("Please unlock your BitBox02 with your password."),
        Progress::Pairing { code } => println!(
            "Please check that the BitBox02 shows the following pairing code and confirm it on the device:\n\n{}\n",
            code
        ),
        Progress::WaitingRebootConfirmation => {
            println!("Please confirm on your BitBox02 to proceed with the upgrade.")
        }
        Progress::WaitingForBootloader => println!("Waiting for the device to reboot into the bootloader..."),
        Progress::Installing {
            version,
            firmware_version,
            sighash,
            intermediate,
        } => {
            println!(
                "Installing {}firmware v{} (monotonic version {}).",
                if intermediate { "intermediate " } else { "" },
                version,
                firmware_version
            );
            println!("Firmware hash: {}", hex::encode(sighash));
        }
        Progress::Erasing => println!("Erasing..."),
        Progress::Flashing { done, total } => {
            print!("\rFlashing: {}/{} chunks", done, total);
            let _ = std::io::stdout().flush();
            if done == total {
                println!();
            }
        }
        Progress::Verifying => println!("Verifying..."),
        Progress::Rebooting => println!("Rebooting the device..."),
        Progress::WaitingForIntermediateBoot { version } => println!(
            "Booting the intermediate firmware v{}, this can take a minute. Do not unplug the device. If your BitBox shows 'DEV DEVICE' (development bootloader), slide <Continue> (bottom) on the device to boot it. If it then stays on 'Development bootloader', unplug and replug it.",
            version
        ),
        Progress::Done => println!("Done."),
    }
}

fn update() {
    let options = options();
    match update_firmware(&options, &mut print_progress) {
        Ok(UpdateOutcome::AlreadyUpToDate { installed, latest }) => println!(
            "The firmware is already up to date (installed v{}, latest v{}). Set BITBOX_FORCE to reinstall.",
            installed, latest
        ),
        Ok(UpdateOutcome::AlreadyInstalled {
            firmware_version,
            sighash,
        }) => println!(
            "This firmware (monotonic version {}, hash {}) is already installed, the device was \
             rebooted. Set BITBOX_FORCE to reinstall.",
            firmware_version,
            hex::encode(sighash)
        ),
        Ok(UpdateOutcome::Updated {
            product,
            version,
            sighash,
            ..
        }) => {
            println!("Successfully installed firmware v{} on your {}.", version, product);
            println!("Firmware hash: {}", hex::encode(sighash));
        }
        Err(e) => error!("Error updating the firmware: {}", e),
    }
}
