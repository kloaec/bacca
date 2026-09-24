//! Blockstream Jade commands, selected with the `JADE_COMMAND` env var.

use std::{env, io::Write};

use jade_manager::{
    check_update, get_info, update_firmware, Progress, UpdateOptions, UpdateOutcome,
};

use crate::OrExit;

pub fn run(command: &str) {
    let port = env::var("JADE_PORT").ok().filter(|p| !p.is_empty());
    match command {
        "getinfo" => print_info(port.as_deref()),
        "checkfirm" => check_firmware(port.as_deref()),
        "updatefirm" => update(port),
        _ => fail!(
            "Invalid JADE_COMMAND '{}'. Valid commands: getinfo, checkfirm, updatefirm.",
            command
        ),
    }
}

fn print_info(port: Option<&str>) {
    let info = get_info(port).or_exit("Error getting the device info");
    match info.model {
        Some(m) => println!("{} on {}:", m, info.port),
        None => println!("Unknown Jade board '{}' on {}:", info.board_type, info.port),
    }
    println!("  - Firmware version: {}", info.version_string);
    println!(
        "  - Config: {}, features: {}",
        info.config.to_lowercase(),
        info.features
    );
    println!("  - State: {:?}", info.state);
    println!("  - Networks: {}", info.networks);
    if let Some(mac) = &info.efusemac {
        println!("  - Id (EFUSEMAC): {}", mac);
    }
}

fn check_firmware(port: Option<&str>) {
    let check = check_update(port).or_exit("Error checking the latest firmware");
    println!("Installed firmware: {}", check.info.version_string);
    println!(
        "Latest firmware: v{} ({})",
        check.latest.version, check.latest.url
    );
    println!("  Firmware hash: {}", hex::encode(check.latest.fwhash));
    match check.update_available {
        Some(true) => {
            println!("An update is available. Use JADE_COMMAND=updatefirm to install it.")
        }
        Some(false) => println!("The firmware is up to date."),
        None => println!("Could not compare the versions."),
    }
}

fn print_progress(p: Progress) {
    match p {
        Progress::FetchingIndex => println!("Fetching the firmware index from Blockstream..."),
        Progress::Downloading { version } => println!("Downloading firmware v{}...", version),
        Progress::EnterPin => println!("Please enter your PIN on the Jade to unlock it."),
        Progress::WaitingForConfirmation { version, fwhash } => println!(
            "Please check that the Jade shows version {} and the firmware hash\n\n{}\n\nand confirm the update on the device.",
            version,
            hex::encode(fwhash)
        ),
        Progress::Uploading { done, total } => {
            print!("\rUploading: {}/{} bytes", done, total);
            let _ = std::io::stdout().flush();
            if done == total {
                println!();
            }
        }
        Progress::WaitingForReboot => {
            println!("Firmware uploaded and verified by the device. Waiting for it to restart...")
        }
        Progress::Done => {}
    }
}

fn update(port: Option<String>) {
    let options = UpdateOptions {
        port,
        force: env::var_os("JADE_FORCE").is_some(),
    };
    let outcome =
        update_firmware(&options, &mut print_progress).or_exit("\nError updating the firmware");
    match outcome {
        UpdateOutcome::AlreadyUpToDate { installed } => println!(
            "The firmware is already up to date (v{}). Set JADE_FORCE to reinstall it.",
            installed
        ),
        UpdateOutcome::Updated {
            version,
            running_version,
            ..
        } => match running_version {
            Some(v) if v == version => println!("Successfully updated the firmware to v{}.", v),
            Some(v) => println!(
                "Firmware v{} installed, but the device restarted with v{}. Check the device.",
                version, v
            ),
            None => println!(
                "Firmware v{} installed. The device did not answer after restarting: check its version with JADE_COMMAND=getinfo.",
                version
            ),
        },
    }
}
