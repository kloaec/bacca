//! The BitBox02 operations of the worker thread.

use crate::worker::{BitboxState, DeviceState, LatestFirmware, Outcome, Reporter};

use bitbox_manager::{
    check_update, default_config_dir, get_status, DeviceHandle, DeviceStatus, Mode, Progress,
    UpdateOptions, UpdateOutcome,
};

/// Format a firmware hash in groups of 8 hex characters, easier to compare with the device's
/// screen.
fn format_hash(hash: &[u8; 32]) -> String {
    let hex: Vec<String> = hash.iter().map(|b| format!("{:02x}", b)).collect();
    hex.chunks(4)
        .map(|c| c.concat())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Query the connected BitBox and the latest firmware for it, reporting it to the GUI as it goes.
/// Returns whether the device could be queried.
pub fn load(handle: &DeviceHandle, r: &Reporter) -> bool {
    let mut state = BitboxState {
        model: handle.product.platform().to_string(),
        edition: handle.product.edition(),
        firmware: None,
        bootloader: handle.mode == Mode::Bootloader,
        latest_firmware: LatestFirmware::Unknown,
    };
    r.status("BitBox detected, querying the latest firmware...");
    r.device(DeviceState::Bitbox(state.clone()));

    // The messages to display once everything is loaded.
    let mut notes = Vec::new();
    let status = match check_update() {
        Ok(check) => {
            state.latest_firmware = match check.update_available {
                Some(false) => LatestFirmware::UpToDate,
                // In bootloader mode the installed version is not known: the update installs the
                // latest firmware (or just restarts the device if it's already installed).
                Some(true) | None => {
                    LatestFirmware::Available(format!("v{}", check.latest.version))
                }
            };
            check.status
        }
        // Maybe only the releases could not be fetched.
        Err(e) => match get_status() {
            Ok(status) => {
                notes.push(format!("Could not check the latest firmware: {}", e));
                status
            }
            Err(e) => {
                r.status(format!("Cannot query the BitBox: {}", e));
                return false;
            }
        },
    };
    let initialized = match status {
        DeviceStatus::Firmware(info) => {
            state.firmware = Some(format!("v{}", info.version));
            state.bootloader = false;
            info.initialized
        }
        DeviceStatus::Bootloader(info) => {
            state.firmware = Some(if info.erased {
                format!("None (bootloader v{})", info.bootloader_version)
            } else {
                format!("Bootloader mode (v{})", info.bootloader_version)
            });
            state.bootloader = true;
            None
        }
    };
    if state.bootloader {
        notes.push(
            "Your BitBox is in bootloader mode. Click 'Update' to install the latest firmware."
                .to_string(),
        );
    } else if initialized == Some(false) {
        notes.push("Your BitBox is not set up yet.".to_string());
    }
    r.device(DeviceState::Bitbox(state));
    r.status(notes.join("\n"));
    true
}

/// Update the firmware to the latest release.
pub fn update_firmware(r: &Reporter) -> Outcome {
    let options = UpdateOptions {
        config_dir: default_config_dir(),
        ..Default::default()
    };
    if options.config_dir.is_none() {
        log::warn!("No config directory, the BitBox pairing will not be remembered.");
    }
    match bitbox_manager::update_firmware(&options, &mut |p| report_progress(r, p)) {
        Ok(UpdateOutcome::AlreadyUpToDate { installed, .. }) => Outcome::Done(format!(
            "The firmware is already up to date (v{}).",
            installed
        )),
        Ok(UpdateOutcome::AlreadyInstalled { sighash, .. }) => Outcome::Done(format!(
            "The latest firmware is already installed, the device was restarted. Firmware hash: {}",
            format_hash(&sighash)
        )),
        Ok(UpdateOutcome::Updated {
            product,
            version,
            sighash,
            ..
        }) => Outcome::Done(format!(
            "Successfully installed the firmware v{} on your {}. Firmware hash, to compare with the one shown by your BitBox (if enabled): {}",
            version,
            product,
            format_hash(&sighash)
        )),
        Err(e) => Outcome::Failed(format!("Error updating the firmware: {}", e)),
    }
}

fn report_progress(r: &Reporter, progress: Progress) {
    let intermediate = |i: bool| if i { "intermediate " } else { "" };
    match progress {
        Progress::FetchingReleases => r.status("Fetching the firmware releases from GitHub..."),
        Progress::Downloading {
            version,
            intermediate: i,
        } => r.status(format!(
            "Downloading the {}firmware v{}...",
            intermediate(i),
            version
        )),
        Progress::WaitingForUnlock => r.status("Please unlock your BitBox with your password."),
        Progress::Pairing { code } => {
            r.status("Please check that your BitBox shows the following pairing code, and confirm it on the device:");
            r.info(Some(code));
        }
        Progress::WaitingRebootConfirmation => {
            r.info(None);
            r.status("Please confirm on your BitBox to proceed with the upgrade.")
        }
        Progress::WaitingForBootloader => {
            r.info(None);
            r.status("Waiting for the device to restart in bootloader mode...")
        }
        Progress::Installing {
            version,
            sighash,
            intermediate: i,
            ..
        } => {
            r.status(format!(
                "Installing the {}firmware v{}. Firmware hash, to compare with the one shown by your BitBox (if enabled) and in the release notes:",
                intermediate(i),
                version
            ));
            r.info(Some(format_hash(&sighash)));
        }
        Progress::Erasing => r.status("Erasing the previous firmware..."),
        Progress::Flashing { done, total } => r.percent(
            "Flashing (do not unplug the device)",
            done as f32 / total.max(1) as f32,
        ),
        Progress::Verifying => r.status("Verifying..."),
        Progress::Rebooting => r.status("Rebooting the device..."),
        Progress::WaitingForIntermediateBoot { version } => {
            r.info(None);
            r.status(format!(
                "Booting the intermediate firmware v{}, this can take a minute. Do not unplug the device. If your BitBox shows 'DEV DEVICE' (development bootloader), slide <Continue> (bottom) on the device to boot it. If it then stays on 'Development bootloader', unplug and replug it.",
                version
            ))
        }
        Progress::Done => {}
    }
}

#[cfg(test)]
mod tests {
    use super::format_hash;

    #[test]
    fn hash_format() {
        let mut hash = [0u8; 32];
        hash[0] = 0xab;
        hash[31] = 0x01;
        let s = format_hash(&hash);
        assert_eq!(s.len(), 64 + 7);
        assert!(s.starts_with("ab000000 00000000"));
        assert!(s.ends_with("00000001"));
    }
}
