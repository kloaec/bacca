//! Blocking interactions with a BitBox02 device. Run from `spawn_blocking` tasks, see
//! `device_service`.

use crate::device_service::{BitboxState, DeviceState, LatestFirmware, Reporter, TaskResult};

use bitbox_manager::{
    check_update, get_status,
    noise_config::{default_config_dir, NoiseConfig, NoiseConfigNoCache, PersistedNoiseConfig},
    update_firmware, DeviceHandle, DeviceStatus, FirmwareSource, Mode, Product, Progress,
    UpdateOptions, UpdateOutcome,
};

/// Format a firmware hash in groups of 8 hex characters, easier to compare with the device's
/// screen.
pub fn format_hash(hash: &[u8; 32]) -> String {
    let hex: String = hash.iter().map(|b| format!("{:02x}", b)).collect();
    hex.as_bytes()
        .chunks(8)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn set_product(state: &mut BitboxState, product: Product) {
    state.product = product.platform().to_string();
    state.edition = Some(product.edition());
}

fn set_status(state: &mut BitboxState, status: &DeviceStatus) {
    if let Some(product) = status.product() {
        set_product(state, product);
    }
    match status {
        DeviceStatus::Firmware(info) => {
            state.firmware = Some(format!("v{}", info.version));
            state.bootloader = false;
            state.initialized = info.initialized;
        }
        DeviceStatus::Bootloader(info) => {
            state.firmware = Some(if info.erased {
                format!("None (bootloader v{})", info.bootloader_version)
            } else {
                format!("Bootloader mode (v{})", info.bootloader_version)
            });
            state.bootloader = true;
            state.initialized = None;
        }
    }
}

/// Query the connected BitBox and the latest firmware available for it. Returns whether the
/// device could be queried and the state to display.
pub fn load(handle: &DeviceHandle, reporter: &Reporter) -> (bool, BitboxState) {
    log::info!("bitbox::load({:?})", handle.product);
    let mut state = BitboxState::default();
    set_product(&mut state, handle.product);
    state.bootloader = handle.mode == Mode::Bootloader;
    reporter.status("BitBox detected, querying the latest firmware...");
    reporter.state(DeviceState::Bitbox(state.clone()));

    let mut notes = Vec::new();
    match check_update() {
        Ok(check) => {
            set_status(&mut state, &check.status);
            let latest = format!("v{}", check.latest.version);
            state.latest_firmware = match check.update_available {
                Some(false) => LatestFirmware::UpToDate,
                // In bootloader mode the installed version is not known: the update installs
                // the latest firmware (or just reboots the device if it's already installed).
                Some(true) | None => LatestFirmware::Available(latest),
            };
        }
        Err(e) => {
            log::error!("Error checking the latest BitBox firmware: {}", e);
            match get_status() {
                Ok(status) => {
                    set_status(&mut state, &status);
                    notes.push(format!("Could not check the latest firmware: {}", e));
                }
                Err(e) => {
                    reporter.status(format!("Cannot query the BitBox: {}", e));
                    reporter.state(DeviceState::Bitbox(state.clone()));
                    return (false, state);
                }
            }
        }
    }
    reporter.state(DeviceState::Bitbox(state.clone()));

    if state.bootloader {
        notes.push(
            "Your BitBox is in bootloader mode. Click 'Update' to install the latest firmware."
                .to_string(),
        );
    } else if state.initialized == Some(false) {
        notes.push("Your BitBox is not set up yet.".to_string());
    }
    reporter.status(notes.join("\n"));
    (true, state)
}

fn report_progress(reporter: &Reporter, progress: Progress) {
    let intermediate = |i: bool| if i { "intermediate " } else { "" };
    match progress {
        Progress::FetchingReleases => {
            reporter.status("Fetching the firmware releases from GitHub...")
        }
        Progress::Downloading {
            version,
            intermediate: i,
        } => reporter.status(format!(
            "Downloading the {}firmware v{}...",
            intermediate(i),
            version
        )),
        Progress::WaitingForUnlock => {
            reporter.status("Please unlock your BitBox with your password.")
        }
        Progress::Pairing { code } => {
            reporter.status(
                "Please check that your BitBox shows the following pairing code, and confirm it on the device:",
            );
            reporter.info(Some(code));
        }
        Progress::WaitingRebootConfirmation => {
            reporter.info(None);
            reporter.status("Please confirm on your BitBox to proceed with the upgrade.")
        }
        Progress::WaitingForBootloader => {
            reporter.info(None);
            reporter.status("Waiting for the device to restart in bootloader mode...")
        }
        Progress::Installing {
            version,
            firmware_version,
            sighash,
            intermediate: i,
        } => {
            reporter.status(match version {
                Some(v) => format!(
                    "Installing the {}firmware v{}. Firmware hash, to compare with the one shown by your BitBox (if enabled) and in the release notes:",
                    intermediate(i),
                    v
                ),
                None => format!(
                    "Installing the firmware (monotonic version {}). Firmware hash:",
                    firmware_version
                ),
            });
            reporter.info(Some(format_hash(&sighash)));
        }
        Progress::Erasing => reporter.status("Erasing the previous firmware..."),
        Progress::Flashing { done, total } => {
            let p = if total == 0 {
                0.0
            } else {
                done as f32 / total as f32
            };
            reporter.status(format!(
                "Flashing: {}%. Do not unplug the device.",
                (p * 100.0).round() as u32
            ));
            reporter.progress(Some(p));
        }
        Progress::Verifying => {
            reporter.progress(None);
            reporter.status("Verifying...")
        }
        Progress::Rebooting => reporter.status("Rebooting the device..."),
        Progress::WaitingForIntermediateBoot { version } => {
            reporter.info(None);
            reporter.status(format!(
                "Booting the intermediate firmware v{}, this can take a minute. Do not unplug the device. If your BitBox shows 'DEV DEVICE' (development bootloader), slide <Continue> (bottom) on the device to boot it. If it then stays on 'Development bootloader', unplug and replug it.",
                version
            ))
        }
        Progress::BootloaderUpgradeSkipped {
            version,
            bootloader_version,
        } => reporter.status(format!(
            "Your BitBox kept its bootloader v{}: the bootloader upgrade of the intermediate firmware v{} was refused (it always is on a development bootloader). Installing the firmware directly instead.",
            bootloader_version, version
        )),
        Progress::Done => reporter.progress(None),
    }
}

/// Update the firmware to the latest release.
pub fn update(reporter: &Reporter) -> TaskResult {
    log::info!("bitbox::update()");
    let noise_config: Box<dyn NoiseConfig + Send> = match default_config_dir() {
        Some(dir) => Box::new(PersistedNoiseConfig::new(dir)),
        None => {
            log::warn!("No config directory, the BitBox pairing will not be remembered.");
            Box::new(NoiseConfigNoCache)
        }
    };
    let options = UpdateOptions {
        noise_config,
        ..Default::default()
    };
    let res = update_firmware(FirmwareSource::Latest, &options, &mut |p| {
        report_progress(reporter, p)
    });
    let message = match res {
        Ok(UpdateOutcome::AlreadyUpToDate { installed, .. }) => (
            format!("The firmware is already up to date (v{}).", installed),
            false,
        ),
        Ok(UpdateOutcome::AlreadyInstalled { sighash, .. }) => (
            format!(
                "The latest firmware is already installed, the device was restarted. Firmware hash: {}",
                format_hash(&sighash)
            ),
            false,
        ),
        Ok(UpdateOutcome::Updated {
            product,
            version,
            sighash,
            ..
        }) => (
            format!(
                "Successfully installed the firmware{} on your {}. Firmware hash, to compare with the one shown by your BitBox (if enabled): {}",
                version.map(|v| format!(" v{}", v)).unwrap_or_default(),
                product,
                format_hash(&sighash)
            ),
            false,
        ),
        Err(e) => (format!("Error updating the firmware: {}", e), true),
    };
    TaskResult::Operation {
        reload: true,
        message: Some(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
