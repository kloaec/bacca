//! The Blockstream Jade operations of the worker thread.

use crate::{
    bitbox::format_hash,
    worker::{DeviceState, JadeState, LatestFirmware, Outcome, Reporter},
};

use jade_manager::{
    check_update, get_info, DeviceInfo, Error, Progress, State, UpdateOptions, UpdateOutcome,
    USER_CANCELLED,
};

/// Query the Jade on `port` and the latest firmware for it, reporting it to the GUI.
///
/// Returns whether the device information is displayed. Opening the serial port talks to the
/// device, so on errors (e.g. another device using the same USB serial chip, or the port used by
/// another app) it is not queried again until it is reconnected: this returns true too.
pub fn load(port: &str, r: &Reporter) -> bool {
    r.status("Jade detected, querying the latest firmware...");
    // The messages to display once everything is loaded.
    let mut notes = Vec::new();
    let (info, latest_firmware) = match check_update(Some(port)) {
        Ok(check) => {
            let latest = match check.update_available {
                Some(true) => LatestFirmware::Available(format!("v{}", check.latest.version)),
                Some(false) => LatestFirmware::UpToDate,
                None => {
                    notes.push("Could not compare the installed and latest versions.".to_string());
                    LatestFirmware::Unknown
                }
            };
            (check.info, latest)
        }
        // Maybe only the releases could not be fetched.
        Err(e) => match get_info(Some(port)) {
            Ok(info) => {
                notes.push(format!("Could not check the latest firmware: {}", e));
                (info, LatestFirmware::Unknown)
            }
            Err(e) => {
                r.status(format!(
                    "Cannot query the Jade on {}: {}. Unplug and replug it to retry.",
                    port, e
                ));
                return true;
            }
        },
    };
    match info.state {
        State::Temporary => notes.push("A temporary wallet is loaded: the Jade refuses the update. Restart it (without loading the temporary wallet) to update it.".to_string()),
        State::Uninitialized | State::Unsaved => notes.push("Your Jade is not set up yet.".to_string()),
        _ => {}
    }
    r.device(DeviceState::Jade(state(&info, latest_firmware)));
    r.status(notes.join("\n"));
    true
}

fn state(info: &DeviceInfo, latest_firmware: LatestFirmware) -> JadeState {
    JadeState {
        model: match info.model {
            Some(model) => model.to_string(),
            None => format!("Unknown board ({})", info.board_type),
        },
        firmware: format!("v{}", info.version_string),
        config: match info.config.as_str() {
            "BLE" => "Bluetooth".to_string(),
            "NORADIO" => "No radio (no Bluetooth)".to_string(),
            other => other.to_string(),
        },
        state: match &info.state {
            State::Ready => "Unlocked".to_string(),
            State::Locked => "Locked".to_string(),
            State::Temporary => "Temporary wallet".to_string(),
            State::Unsaved | State::Uninitialized => "Not set up".to_string(),
            State::Other(s) => s.clone(),
        },
        latest_firmware,
    }
}

/// Update the firmware to the latest release.
pub fn update_firmware(port: &str, r: &Reporter) -> Outcome {
    let options = UpdateOptions {
        port: Some(port.to_string()),
        force: false,
    };
    match jade_manager::update_firmware(&options, &mut |p| report_progress(r, p)) {
        Ok(UpdateOutcome::AlreadyUpToDate { installed }) => Outcome::Done(format!(
            "The firmware is already up to date (v{}).",
            installed
        )),
        Ok(UpdateOutcome::Updated {
            version,
            running_version,
            ..
        }) => match running_version {
            Some(v) if v == version => Outcome::Done(format!(
                "Successfully installed the firmware v{} on your Jade.",
                version
            )),
            Some(v) => Outcome::Failed(format!(
                "The firmware v{} was uploaded, but the Jade restarted with v{}. Please check your device.",
                version, v
            )),
            None => Outcome::Failed(format!(
                "The firmware v{} was uploaded, but the Jade did not answer after restarting. Please check your device.",
                version
            )),
        },
        Err(Error::Device { code, .. }) if code == USER_CANCELLED => {
            Outcome::Failed("The update was cancelled on the Jade.".to_string())
        }
        Err(Error::UntrustedPinServer(url)) => Outcome::Failed(format!(
            "Your Jade uses a custom PIN server ({}), which Bacca does not support: it only relays the requests to Blockstream's PIN server. Unlock your Jade with the software you set it up with, then try again.",
            url
        )),
        Err(e) => Outcome::Failed(format!("Error updating the firmware: {}", e)),
    }
}

fn report_progress(r: &Reporter, progress: Progress) {
    match progress {
        Progress::FetchingIndex => r.status("Fetching the firmware index from Blockstream..."),
        Progress::Downloading { version } => {
            r.status(format!("Downloading the firmware v{}...", version))
        }
        Progress::EnterPin => r.status("Please enter your PIN on the Jade to unlock it."),
        Progress::WaitingForConfirmation { version, fwhash } => {
            r.status(format!(
                "Please check that your Jade shows the version {} and the following firmware hash, and confirm the update on the device:",
                version
            ));
            r.info(Some(format_hash(&fwhash)));
        }
        Progress::Uploading { done, total } => r.percent(
            "Uploading (do not unplug the device)",
            done as f32 / total.max(1) as f32,
        ),
        Progress::WaitingForReboot => {
            r.info(None);
            r.status("Firmware uploaded and verified by the Jade. Waiting for it to restart...")
        }
        Progress::Done => {}
    }
}
