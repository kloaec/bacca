//! Updating the firmware (OS) of the device, and repairing a device stuck in bootloader mode.
//!
//! This follows the flow of Ledger Live Desktop:
//! - the "prepare" step installs the OS Updater (OSU) on the device through the HSM. The user has
//!   to allow the Ledger manager and to confirm the update (checking its identifier) on the
//!   device. The device then reboots in updater mode.
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/firmwareUpdate-prepare.ts
//! - the "main" step, where depending on the update the MCU and/or the bootloader are flashed
//!   (device in bootloader mode), and for some legacy firmwares the final firmware is installed
//!   separately. For most updates the device installs the final firmware itself after the OSU,
//!   and we only have to wait for it to reboot.
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/firmwareUpdate-main.ts
//! - finally we wait for the device to come back running the new firmware.
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/apps/ledger-live-desktop/src/renderer/modals/UpdateFirmwareModal/steps/02-step-updating.tsx
//!
//! The flashing loop also takes from the device SDK implementation:
//! https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/tasks/updateFirmware.ts

use crate::{
    api::{
        best_mcu_for_final_firmware, device_version_id, fetch_mcus, find_best_mcu,
        get_final_firmware, get_final_firmware_by_id, get_osu, FirmwareUpdateInfo, McuVersion,
    },
    device::*,
    error::Error,
    hid::HidTransport,
    socket::{run_socket, socket_url, Context, SocketEvent, Transport},
};

use ledger_transport_hidapi::hidapi::HidApi;

use std::{
    thread,
    time::{Duration, Instant},
};

/// How long to wait for the device each time it reboots. Ledger Live waits forever for the
/// bootloader, and 5 minutes for the final reboot.
const REBOOT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long to wait for the device to disconnect after the OSU was installed, when no MCU needs
/// to be flashed (Ledger Live's `potentialAutoFlash` step).
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Maximum number of MCU or bootloader flashes before giving up (deviceSDK/tasks/updateFirmware.ts).
const MAX_FLASH_REPETITIONS: usize = 5;

/// A step of the firmware update, for progress display.
#[derive(Debug, Clone, PartialEq)]
pub enum FirmwareUpdateStep {
    /// Checking the state of the device.
    Preparing,
    /// The user must allow the Ledger manager on the device.
    AllowManagerRequested,
    AllowManagerGranted,
    /// Transferring the OS updater to the device. `progress` is between 0 and 1.
    InstallingOsu {
        progress: f32,
    },
    /// The user must confirm the firmware update on the device, after checking the identifier
    /// displayed on the device matches `identifier` (its lines separated by spaces), if any.
    WaitingUserConfirmation {
        identifier: Option<String>,
    },
    UserConfirmed,
    /// The device is rebooting.
    WaitingForReboot,
    /// Waiting for the device to reboot in bootloader mode, to flash the MCU.
    WaitingForBootloader,
    /// `progress` is between 0 and 1.
    FlashingBootloader {
        progress: f32,
    },
    /// `progress` is between 0 and 1.
    FlashingMcu {
        progress: f32,
    },
    /// Installing the final firmware (legacy flow). `progress` is between 0 and 1.
    InstallingFinal {
        progress: f32,
    },
    /// Waiting for the device to finish the update and to reboot on the new firmware. This can
    /// take several minutes, the device must stay connected.
    WaitingForDevice,
    /// The device is locked: the user must unlock it for the update to complete.
    DeviceLocked,
    /// The update completed. Contains the new device information.
    Done {
        device_info: Box<DeviceInfo>,
    },
}

/// Check whether updating the firmware of this device is supported. Returns an error telling why
/// if not.
///
/// See `isUsbUpdateSupported` in https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/isFirmwareUpdateVersionSupported.ts
pub fn check_firmware_update_supported(device_info: &DeviceInfo) -> Result<(), Error> {
    if device_info.is_bootloader {
        return Err(Error::DeviceInBootloader);
    }
    if device_info.is_osu {
        // An interrupted update can always be resumed.
        return Ok(());
    }
    let not_supported = |s: String| Error::Other(format!("Firmware update not supported: {}", s));
    let model = device_info.model.ok_or_else(|| {
        not_supported(format!(
            "unknown device model (target id {:#010x})",
            device_info.target_id
        ))
    })?;
    let min = match model {
        DeviceModel::NanoS => (1, 6, 1),
        DeviceModel::NanoX => (1, 3, 0),
        DeviceModel::NanoSPlus | DeviceModel::Stax => (1, 0, 0),
        DeviceModel::Flex | DeviceModel::NanoGen5 => (0, 0, 0),
    };
    if !version_at_least(&device_info.version, min) {
        return Err(not_supported(format!(
            "{} firmware {} is too old to be updated through USB (minimum {}.{}.{}). Please use Ledger Live.",
            model, device_info.version, min.0, min.1, min.2
        )));
    }
    Ok(())
}

/// Whether the device may lose its custom lock screen picture and language during the update
/// (`update_firmware_and_restore` backs them up and restores them).
/// https://github.com/LedgerHQ/ledger-live/blob/develop/apps/ledger-live-desktop/src/renderer/modals/UpdateFirmwareModal/helpers/createFirmwareUpdateSteps.ts
pub fn firmware_update_resets_customization(
    device_info: &DeviceInfo,
    update: &FirmwareUpdateInfo,
) -> bool {
    device_info.model.is_some_and(|m| {
        m.has_touch_screen() || is_device_localization_supported(update.version(), Some(m))
    })
}

/// The MCU or bootloader version to flash on a device in bootloader mode.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FlashTarget {
    /// The version to pass to the `/mcu` endpoint.
    version: String,
    /// Whether this is the MCU (or else the bootloader).
    is_mcu: bool,
}

impl FlashTarget {
    fn mcu(version: &str) -> Self {
        Self {
            version: version.to_string(),
            is_mcu: true,
        }
    }

    fn bootloader(version: &str) -> Self {
        Self {
            version: version.to_string(),
            is_mcu: false,
        }
    }
}

/// The bootloader version alias of hw/flash.ts: a device with the 0.0 bootloader is flashed the
/// 0.6 bootloader.
fn aliased_bootloader(maj_min: &str) -> Option<FlashTarget> {
    (maj_min == "0.0").then(|| FlashTarget::bootloader("0.6"))
}

fn mcu_not_found() -> Error {
    Error::Other("Could not find the MCU version to flash.".into())
}

/// What to flash during an update, given the bootloader version (`maj_min`) of the device: the
/// best MCU for the new firmware if the bootloader is the one it requires, else this bootloader.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/flash.ts
/// and `getFlashMcuOrBootloaderDetails` in deviceSDK/tasks/updateFirmware.ts
fn flash_target(
    maj_min: &str,
    mcus: &[McuVersion],
    update: &FirmwareUpdateInfo,
    provider: u32,
) -> Result<FlashTarget, Error> {
    let mcu = best_mcu_for_final_firmware(mcus, &update.final_firmware, provider)
        .ok_or_else(mcu_not_found)?;
    // Converts the version into the majMin format.
    let mcu_from_bootloader = mcu
        .from_bootloader_version
        .split('.')
        .take(3)
        .collect::<Vec<_>>()
        .join(".");
    Ok(if maj_min == mcu_from_bootloader {
        FlashTarget::mcu(&mcu.name)
    } else {
        FlashTarget::bootloader(&mcu_from_bootloader)
    })
}

/// Format the identifier (hash) of a firmware the way the device displays it, so the user can
/// compare them: uppercased and, depending on the model and firmware version of the device, split
/// into lines (Nano X, Nano S 1.6.0 and later) or ellipsized (Nano S before 1.6.0, or when the
/// model or version is unknown). Newer models display the full hash. The lines are joined with
/// spaces.
///
/// Ported from `formatHashName` in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/index.ts
fn format_hash_name(hash: &str, model: Option<DeviceModel>, firmware_version: &str) -> String {
    // Ledger Live would throw for an invalid version, we show the full hash then.
    let nano_s_lt_1_6 = coerce_version(firmware_version).map(|v| v < (1, 6, 0));
    let (should_ellipsis, should_split) = match model {
        Some(DeviceModel::NanoS) => (nano_s_lt_1_6 == Some(true), nano_s_lt_1_6 == Some(false)),
        Some(DeviceModel::NanoX) => (false, true),
        Some(_) => (false, false),
        None => (true, false),
    };
    let hash: Vec<char> = hash.to_uppercase().chars().collect();
    if should_split {
        let split_length = if model == Some(DeviceModel::NanoS) {
            16
        } else {
            17
        };
        let lines: Vec<String> = hash
            .chunks(split_length)
            .map(|c| c.iter().collect())
            .collect();
        lines.join(" ")
    } else if hash.len() > 8 && should_ellipsis {
        let start: String = hash[..4].iter().collect();
        let end: String = hash[hash.len() - 4..].iter().collect();
        format!("{}...{}", start, end)
    } else {
        hash.into_iter().collect()
    }
}

/// Map the bulk progress of the OSU installation to update steps. The penultimate APDU of the
/// bulk waits for the user to confirm the update, and the last one means the user confirmed it.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/commands/firmwareUpdate/installFirmware.ts
fn osu_step(index: usize, total: usize, identifier: &Option<String>) -> FirmwareUpdateStep {
    if total > 0 && index + 1 == total {
        FirmwareUpdateStep::WaitingUserConfirmation {
            identifier: identifier.clone(),
        }
    } else if index == total {
        FirmwareUpdateStep::UserConfirmed
    } else {
        FirmwareUpdateStep::InstallingOsu {
            progress: index as f32 / total as f32,
        }
    }
}

/// Install a firmware (OSU or final firmware) through the HSM. `on_bulk` maps the progress of the
/// bulk of APDUs to the update steps.
fn install_firmware(
    hid_api: &mut HidApi,
    target_id: u32,
    (firmware, perso, firmware_key): (&str, &str, &str),
    on_bulk: impl Fn(usize, usize) -> FirmwareUpdateStep,
    progress: &mut impl FnMut(FirmwareUpdateStep),
) -> Result<(), Error> {
    let hid = HidTransport::connect(hid_api)?;
    let url = socket_url(
        "install",
        &[
            ("targetId", &target_id.to_string()),
            ("firmware", firmware),
            ("perso", perso),
            ("firmwareKey", firmware_key),
        ],
    );
    run_socket(Transport::Raw(&hid), &url, Context::Firmware, |e| match e {
        SocketEvent::DevicePermissionRequested => {
            progress(FirmwareUpdateStep::AllowManagerRequested)
        }
        SocketEvent::DevicePermissionGranted => progress(FirmwareUpdateStep::AllowManagerGranted),
        SocketEvent::BulkProgress { index, total } => progress(on_bulk(index, total)),
    })?;
    Ok(())
}

/// Flash this MCU or bootloader version on a device in bootloader mode, retrying to connect to the
/// device if it can't be opened (it may be rebooting).
fn flash(
    hid_api: &mut HidApi,
    device_info: &DeviceInfo,
    target: &FlashTarget,
    progress: &mut impl FnMut(FirmwareUpdateStep),
) -> Result<(), Error> {
    log::info!(
        "Flashing {} {} (bootloader version {}).",
        if target.is_mcu { "MCU" } else { "bootloader" },
        target.version,
        device_info.maj_min
    );
    let step = |p| {
        if target.is_mcu {
            FirmwareUpdateStep::FlashingMcu { progress: p }
        } else {
            FirmwareUpdateStep::FlashingBootloader { progress: p }
        }
    };
    progress(step(0.0));

    let start = Instant::now();
    let hid = loop {
        match HidTransport::connect(hid_api) {
            Ok(t) => break t,
            Err(e) if start.elapsed() < REBOOT_TIMEOUT => {
                log::debug!("Could not open the device, retrying: {}", e);
                thread::sleep(POLL_INTERVAL);
            }
            Err(e) => return Err(e),
        }
    };
    let url = socket_url(
        "mcu",
        &[
            ("targetId", &device_info.target_id.to_string()),
            ("version", &target.version),
        ],
    );
    run_socket(Transport::Raw(&hid), &url, Context::Firmware, |e| {
        if let SocketEvent::BulkProgress { .. } = e {
            progress(step(e.bulk_progress()));
        }
    })?;
    Ok(())
}

/// Fetch the MCU versions from the Ledger API, once.
fn cached_mcus(cache: &mut Option<Vec<McuVersion>>) -> Result<&[McuVersion], Error> {
    if cache.is_none() {
        *cache = Some(fetch_mcus()?);
    }
    Ok(cache.as_deref().unwrap_or_default())
}

/// Wait for the device to disconnect, up to `DISCONNECT_TIMEOUT`.
fn wait_for_disconnect(hid_api: &mut HidApi) {
    let start = Instant::now();
    while start.elapsed() < DISCONNECT_TIMEOUT {
        if hid_api.refresh_devices().is_ok() && list_ledger_devices(hid_api).is_empty() {
            log::debug!("Device disconnected.");
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
    log::debug!("Device didn't disconnect after {:?}.", DISCONNECT_TIMEOUT);
}

/// Update the firmware of the connected device to `update` (as returned by `latest_firmware`).
/// Returns the information of the device running the new firmware.
///
/// The device reboots (possibly several times) during the update, which is why this takes the
/// `HidApi` to reconnect to it: no other transport to the device should be open.
///
/// WARNING: the apps installed on the device are removed by the update, and the language and the
/// custom lock screen picture may be reset. Use `update_firmware_and_restore` to back them up
/// and restore them.
pub fn update_firmware(
    hid_api: &mut HidApi,
    update: &FirmwareUpdateInfo,
    mut progress: impl FnMut(FirmwareUpdateStep),
) -> Result<DeviceInfo, Error> {
    progress(FirmwareUpdateStep::Preparing);
    let device_info = {
        let transport = connect(hid_api)?;
        quit_app(&transport)?;
        DeviceInfo::new(&transport)?
    };
    check_firmware_update_supported(&device_info)?;
    log::info!(
        "Updating firmware of {} ({}) to {}. OSU: {}, flash MCU: {}, final firmware: {}.",
        device_info
            .model
            .map_or("unknown device".to_string(), |m| m.to_string()),
        device_info.firmware_summary(),
        update.version(),
        update.osu.name,
        update.should_flash_mcu,
        update.final_firmware.has_final_firmware()
    );

    // Prepare step: install the OSU. If the device is already in OSU mode (for instance if a
    // previous update was interrupted) we directly jump to the main step, as Ledger Live does.
    if !device_info.is_osu {
        let identifier = update
            .osu
            .hash
            .as_deref()
            .filter(|h| !h.is_empty())
            .map(|h| format_hash_name(h, device_info.model, &device_info.version));
        progress(FirmwareUpdateStep::InstallingOsu { progress: 0.0 });
        let osu = &update.osu;
        install_firmware(
            hid_api,
            device_info.target_id,
            (&osu.firmware, &osu.perso, &osu.firmware_key),
            |index, total| osu_step(index, total, &identifier),
            &mut progress,
        )?;
        // The device is likely rebooting now, we give it some time.
        progress(FirmwareUpdateStep::WaitingForReboot);
        thread::sleep(Duration::from_secs(3));
    }

    // Poll the device, telling the user to unlock it if needed.
    let mut locked_reported = false;
    let mut wait = |hid_api: &mut HidApi,
                    progress: &mut dyn FnMut(FirmwareUpdateStep),
                    accept: fn(&DeviceInfo) -> bool| {
        wait_for_device(hid_api, REBOOT_TIMEOUT, accept, |e| {
            if matches!(e, Error::DeviceLocked) && !locked_reported {
                locked_reported = true;
                progress(FirmwareUpdateStep::DeviceLocked);
            }
        })
    };

    // Main step.
    if update.should_flash_mcu {
        progress(FirmwareUpdateStep::WaitingForBootloader);
        let mut info = wait(hid_api, &mut progress, |i| i.is_bootloader)?;
        let mut mcus = None;
        let mut repetitions = 0;
        while info.is_bootloader {
            if repetitions >= MAX_FLASH_REPETITIONS {
                return Err(too_many_flashes());
            }
            repetitions += 1;
            let target = match aliased_bootloader(&info.maj_min) {
                Some(target) => target,
                None => flash_target(
                    &info.maj_min,
                    cached_mcus(&mut mcus)?,
                    update,
                    info.provider,
                )?,
            };
            flash(hid_api, &info, &target, &mut progress)?;
            progress(FirmwareUpdateStep::WaitingForReboot);
            thread::sleep(Duration::from_secs(2));
            info = wait(hid_api, &mut progress, |_| true)?;
        }
    } else {
        // The device may flash things by itself: wait for it to disconnect (or for a timeout).
        let info = wait(hid_api, &mut progress, |_| true)?;
        if !info.is_osu {
            wait_for_disconnect(hid_api);
        }
    }

    if update.final_firmware.has_final_firmware() {
        let info = wait(hid_api, &mut progress, |_| true)?;
        if !info.is_osu {
            return Err(Error::Other(
                "Device was expected to be in updater mode.".into(),
            ));
        }
        install_final_firmware(hid_api, &info, &mut progress)?;
    }

    // Wait for the device to come back running the new firmware.
    progress(FirmwareUpdateStep::WaitingForDevice);
    let info = wait(hid_api, &mut progress, |i| i.is_normal_mode())?;
    if coerce_version(&info.version) != coerce_version(update.version()) {
        log::warn!(
            "Device is running firmware {} after the update, expected {}.",
            info.version,
            update.version()
        );
    }
    log::info!("Firmware update done: {}.", info.firmware_summary());
    progress(FirmwareUpdateStep::Done {
        device_info: Box::new(info.clone()),
    });
    Ok(info)
}

fn too_many_flashes() -> Error {
    Error::Other("The MCU or bootloader was flashed too many times without success.".into())
}

/// Install the final firmware on a device in OSU mode (legacy flow).
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/installFinalFirmware.ts
fn install_final_firmware(
    hid_api: &mut HidApi,
    device_info: &DeviceInfo,
    progress: &mut impl FnMut(FirmwareUpdateStep),
) -> Result<(), Error> {
    let provider = device_info.provider;
    let device_version = device_version_id(device_info.target_id, provider)?;
    let osu = get_osu(&device_info.version, device_version, provider)?;
    let next = get_final_firmware_by_id(osu.next_se_firmware_final_version)?;
    let (firmware, firmware_key) = match (&next.firmware, &next.firmware_key) {
        (Some(f), Some(k)) if !f.is_empty() => (f, k),
        _ => {
            return Err(Error::Other(
                "Firmware update not supported: no final firmware to install".into(),
            ))
        }
    };
    log::info!("Installing final firmware {}.", next.name);
    progress(FirmwareUpdateStep::InstallingFinal { progress: 0.0 });
    install_firmware(
        hid_api,
        device_info.target_id,
        (firmware, &next.perso, firmware_key),
        |index, total| FirmwareUpdateStep::InstallingFinal {
            progress: if total > 0 {
                index as f32 / total as f32
            } else {
                1.0
            },
        },
        progress,
    )
}

/// What to flash to repair this device in bootloader mode.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/firmwareUpdate-repair.ts
fn repair_target(
    device_info: &DeviceInfo,
    mcus: &mut Option<Vec<McuVersion>>,
) -> Result<FlashTarget, Error> {
    // Fixed versions for these bootloader versions.
    let fixed = match device_info.maj_min.as_str() {
        "0.6" => Some(FlashTarget::mcu("1.5")),
        "0.7" => Some(FlashTarget::mcu("1.6")),
        "0.9" => Some(FlashTarget::mcu("1.7")),
        maj_min => aliased_bootloader(maj_min),
    };
    if let Some(target) = fixed {
        return Ok(target);
    }
    let provider = device_info.provider;
    match (&device_info.se_version, device_info.se_target_id) {
        (Some(se_version), Some(se_target_id)) => {
            log::debug!(
                "Repair: SE version {} and SE target id {:#010x} found.",
                se_version,
                se_target_id
            );
            let device_version = device_version_id(se_target_id, provider)?;
            let final_firmware = get_final_firmware(se_version, device_version, provider)?;
            let mcus = cached_mcus(mcus)?;
            repair_target_for_final_firmware(
                &device_info.raw_version,
                mcus,
                &final_firmware,
                provider,
            )
        }
        _ => repair_target_compatible(device_info, cached_mcus(mcus)?),
    }
    .ok_or_else(mcu_not_found)
}

/// The best MCU for the current firmware of a device in bootloader mode, or the bootloader this
/// MCU requires if the device (`bootloader_version`) runs another one.
fn repair_target_for_final_firmware(
    bootloader_version: &str,
    mcus: &[McuVersion],
    final_firmware: &crate::api::FinalFirmware,
    provider: u32,
) -> Option<FlashTarget> {
    let mcu = best_mcu_for_final_firmware(mcus, final_firmware, provider)?;
    // Ledger Live compares the coerced versions (so two unparsable versions are equal).
    Some(
        if coerce_version(&mcu.from_bootloader_version) == coerce_version(bootloader_version) {
            FlashTarget::mcu(&mcu.name)
        } else {
            FlashTarget::bootloader(&mcu.from_bootloader_version)
        },
    )
}

/// The best MCU which can be installed from the bootloader version of the device, when its SE
/// version is unknown (`compatibleMCUForDeviceInfo` in manager/api.ts).
fn repair_target_compatible(device_info: &DeviceInfo, mcus: &[McuVersion]) -> Option<FlashTarget> {
    let bl = [device_info.maj_min.as_str(), device_info.version.as_str()];
    find_best_mcu(mcus.iter().filter(|m| {
        bl.contains(&m.from_bootloader_version.as_str())
            && m.providers.contains(&(device_info.provider as i64))
    }))
    .map(|m| FlashTarget::mcu(&m.name))
}

/// Repair the firmware of a device in bootloader mode (`Error::DeviceInBootloader`), typically
/// after a firmware update was interrupted while flashing the MCU or the bootloader. Returns the
/// information of the device once it left the bootloader.
///
/// This waits for the device in bootloader mode, then flashes the MCU or the bootloader until the
/// device leaves the bootloader. `forced_version` forces the first version to flash, as Ledger
/// Live's repair choices do ("0.7" if the device shows "MCU outdated" or "MCU not genuine", "0.9"
/// if it tells to follow the repair or update instructions). The device may then still need a
/// firmware update.
///
/// Ported from https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/firmwareUpdate-repair.ts
pub fn repair_firmware(
    hid_api: &mut HidApi,
    forced_version: Option<&str>,
    mut progress: impl FnMut(FirmwareUpdateStep),
) -> Result<DeviceInfo, Error> {
    progress(FirmwareUpdateStep::WaitingForBootloader);
    let mut info = wait_for_device(hid_api, REBOOT_TIMEOUT, |i| i.is_bootloader, |_| {})?;
    log::info!(
        "Repairing the firmware of the device ({}).",
        info.firmware_summary()
    );
    let mut forced_version = forced_version.filter(|v| !v.is_empty());
    let mut mcus = None;
    let mut repetitions = 0;
    while info.is_bootloader {
        if repetitions >= MAX_FLASH_REPETITIONS {
            return Err(too_many_flashes());
        }
        repetitions += 1;
        let target = match forced_version.take() {
            // A device on firmware 1.3.1 showing "MCU not genuine": the user needs to go back to
            // the dashboard to continue the update (Ledger Live's `MCUNotGenuineToDashboard`).
            Some("0.7") if info.maj_min == "0.6" || info.maj_min == "0.7" => {
                return Err(Error::Other("Device must be on its dashboard to be updated. Disconnect and reconnect the USB cable without pressing any button, then press both buttons together three times to display the dashboard, and update the firmware.".into()))
            }
            Some(version) => FlashTarget::mcu(version),
            None => repair_target(&info, &mut mcus)?,
        };
        flash(hid_api, &info, &target, &mut progress)?;
        progress(FirmwareUpdateStep::WaitingForReboot);
        thread::sleep(Duration::from_secs(2));
        info = wait_for_device(hid_api, REBOOT_TIMEOUT, |_| true, |_| {})?;
    }
    log::info!("Firmware repair done: {}.", info.firmware_summary());
    progress(FirmwareUpdateStep::Done {
        device_info: Box::new(info.clone()),
    });
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::FinalFirmware;

    fn mcus(list: &[(i64, &str, &str)]) -> Vec<McuVersion> {
        list.iter()
            .map(|(id, name, from)| {
                serde_json::from_value(serde_json::json!({
                    "id": id, "name": name, "from_bootloader_version": from, "providers": [1],
                }))
                .unwrap()
            })
            .collect()
    }

    fn final_fw(mcu_versions: Vec<i64>) -> FinalFirmware {
        serde_json::from_value(serde_json::json!({
            "id": 1,
            "name": "2.1.0",
            "perso": "perso_11",
            "mcu_versions": mcu_versions,
        }))
        .unwrap()
    }

    fn update(mcu_versions: Vec<i64>) -> FirmwareUpdateInfo {
        FirmwareUpdateInfo {
            osu: serde_json::from_value(serde_json::json!({
                "id": 1, "name": "2.1.0-osu", "perso": "p", "firmware": "f", "firmware_key": "k",
                "next_se_firmware_final_version": 1,
            }))
            .unwrap(),
            final_firmware: final_fw(mcu_versions),
            should_flash_mcu: true,
        }
    }

    fn device(hex_data: &str) -> DeviceInfo {
        DeviceInfo::from_get_version_response(&hex::decode(hex_data).unwrap()).unwrap()
    }

    #[test]
    fn flash_targets() {
        let list = mcus(&[(10, "2.30", "1.16"), (11, "2.12", "1.12")]);
        let up = update(vec![10, 11]);
        // Bootloader matches the MCU requirements: flash the MCU.
        assert_eq!(
            flash_target("1.16", &list, &up, 1).unwrap(),
            FlashTarget::mcu("2.30")
        );
        // Bootloader too old: flash the bootloader first.
        assert_eq!(
            flash_target("1.12", &list, &up, 1).unwrap(),
            FlashTarget::bootloader("1.16")
        );
        assert_eq!(
            aliased_bootloader("0.0"),
            Some(FlashTarget::bootloader("0.6"))
        );
        assert_eq!(aliased_bootloader("1.16"), None);
        // No compatible MCU.
        assert!(flash_target("1.16", &list, &update(vec![]), 1).is_err());
        assert!(flash_target("1.16", &list, &up, 4).is_err());
        // Only the first three parts of from_bootloader_version are considered.
        let list = mcus(&[(12, "3.1", "2.0.1.4")]);
        assert_eq!(
            flash_target("2.0.1", &list, &update(vec![12]), 1).unwrap(),
            FlashTarget::mcu("3.1")
        );
    }

    #[test]
    fn repair_targets() {
        // Fixed versions.
        let bl = |maj_min: &str| {
            let hex_version = hex::encode(maj_min);
            device(&format!("01000001{:02x}{}00", maj_min.len(), hex_version))
        };
        // An empty MCU list, not to fetch it.
        let target = |info| repair_target(&info, &mut Some(vec![])).ok();
        assert_eq!(target(bl("0.0")), Some(FlashTarget::bootloader("0.6")));
        assert_eq!(target(bl("0.6")), Some(FlashTarget::mcu("1.5")));
        assert_eq!(target(bl("0.7")), Some(FlashTarget::mcu("1.6")));
        assert_eq!(target(bl("0.9")), Some(FlashTarget::mcu("1.7")));
        assert_eq!(target(bl("0.8")), None);

        // With a known final firmware.
        let list = mcus(&[
            (10, "2.30", "1.16"),
            (11, "2.12", "1.12"),
            (12, "2.40", "none"),
        ]);
        let fw = final_fw(vec![10, 11, 12]);
        let t = |bl, fw: &FinalFirmware, provider| {
            repair_target_for_final_firmware(bl, &list, fw, provider)
        };
        // The bootloader matches the best MCU: flash the MCU.
        assert_eq!(t("1.16", &fw, 1), Some(FlashTarget::mcu("2.30")));
        assert_eq!(t("1.16.0", &fw, 1), Some(FlashTarget::mcu("2.30")));
        // Otherwise flash the bootloader it requires.
        assert_eq!(t("1.12", &fw, 1), Some(FlashTarget::bootloader("1.16")));
        // No MCU for this final firmware or provider.
        assert_eq!(t("1.16", &final_fw(vec![12]), 1), None);
        assert_eq!(t("1.16", &fw, 2), None);
        // The bootloader version is not truncated.
        let list4 = mcus(&[(13, "3.1", "2.0.1.4")]);
        assert_eq!(
            repair_target_for_final_firmware("1.0", &list4, &final_fw(vec![13]), 1),
            Some(FlashTarget::bootloader("2.0.1.4"))
        );

        // Without SE information: the MCUs compatible with the bootloader version.
        let list = mcus(&[
            (10, "2.30", "1.16"),
            (14, "2.31", "1.16"),
            (11, "2.12", "1.12"),
            (15, "1.9", "1.4.2"),
        ]);
        assert_eq!(
            repair_target_compatible(&bl("1.16"), &list),
            Some(FlashTarget::mcu("2.31"))
        );
        assert_eq!(
            repair_target_compatible(&bl("1.4.2"), &list),
            Some(FlashTarget::mcu("1.9"))
        );
        assert_eq!(repair_target_compatible(&bl("1.20"), &list), None);
    }

    #[test]
    fn hash_names() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let upper = hash.to_uppercase();
        let f = |model, version| format_hash_name(hash, model, version);
        // Nano X: lines of 17.
        assert_eq!(
            f(Some(DeviceModel::NanoX), "2.2.3"),
            "0123456789ABCDEF0 123456789ABCDEF01 23456789ABCDEF012 3456789ABCDEF"
        );
        // Nano S 1.6.0 and later: lines of 16.
        assert_eq!(
            f(Some(DeviceModel::NanoS), "1.6.0"),
            ["0123456789ABCDEF"; 4].join(" ")
        );
        // Older Nano S, unknown model: ellipsis.
        assert_eq!(f(Some(DeviceModel::NanoS), "1.5.5"), "0123...CDEF");
        assert_eq!(f(None, "1.0.0"), "0123...CDEF");
        assert_eq!(format_hash_name("abcd1234", None, "1.0.0"), "ABCD1234");
        // Newer models, or an invalid version: the full hash.
        for m in [
            DeviceModel::NanoSPlus,
            DeviceModel::Stax,
            DeviceModel::Flex,
            DeviceModel::NanoGen5,
        ] {
            assert_eq!(f(Some(m), "1.1.0"), upper);
        }
        assert_eq!(f(Some(DeviceModel::NanoS), "invalid"), upper);
    }

    #[test]
    fn osu_steps() {
        let id = Some("ABCD".to_string());
        assert_eq!(
            osu_step(0, 4, &id),
            FirmwareUpdateStep::InstallingOsu { progress: 0.0 }
        );
        assert_eq!(
            osu_step(2, 4, &id),
            FirmwareUpdateStep::InstallingOsu { progress: 0.5 }
        );
        assert_eq!(
            osu_step(3, 4, &id),
            FirmwareUpdateStep::WaitingUserConfirmation {
                identifier: id.clone()
            }
        );
        assert_eq!(osu_step(4, 4, &id), FirmwareUpdateStep::UserConfirmed);
    }

    #[test]
    fn update_supported() {
        // Nano X 2.2.3.
        let nano_x = device("3300000405322e322e3304ee00000004322e333004312e3136010101000100");
        assert!(check_firmware_update_supported(&nano_x).is_ok());
        // Nano S 1.5.5: too old for USB update.
        let data = format!(
            "3110000405{}04a600000004{}",
            hex::encode("1.5.5"),
            hex::encode("1.12")
        );
        assert!(check_firmware_update_supported(&device(&data)).is_err());
        // Bootloader.
        let bl = device("0501000304312e313604f4d8aa4305322e322e330433000004");
        assert!(matches!(
            check_firmware_update_supported(&bl),
            Err(Error::DeviceInBootloader)
        ));
    }
}
