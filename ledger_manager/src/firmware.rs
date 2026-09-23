//! Updating the firmware (OS) of the device.
//!
//! This follows the flow of Ledger Live Desktop:
//! - the "prepare" step installs the OS Updater (OSU) on the device through the HSM. The user has
//!   to allow the Ledger manager and to confirm the update (checking its identifier) on the
//!   device. The device then reboots in updater mode.
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/firmwareUpdate-prepare.ts
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/installOsuFirmware.ts
//! - the "main" step, where depending on the update the MCU and/or the bootloader are flashed
//!   (device in bootloader mode), and for some legacy firmwares the final firmware is installed
//!   separately. For most updates (and all updates of recent devices), the device installs the
//!   final firmware itself after the OSU was installed and we only have to wait for it to reboot.
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/firmwareUpdate-main.ts
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/flash.ts
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/installFinalFirmware.ts
//! - finally we wait for the device to come back running the new firmware.
//!   https://github.com/LedgerHQ/ledger-live/blob/develop/apps/ledger-live-desktop/src/renderer/modals/UpdateFirmwareModal/steps/02-step-updating.tsx
//!
//! The flashing loop also takes from the (mobile) device SDK implementation:
//! https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/tasks/updateFirmware.ts

use crate::{
    api::{
        fetch_mcus, find_best_mcu, get_current_firmware, get_current_osu, get_device_version,
        get_final_firmware_by_id, mcus_for_final_firmware, FinalFirmware, FirmwareUpdateInfo,
        McuVersion,
    },
    device::{connect, list_ledger_devices, quit_app, wait_for_device, DeviceInfo},
    error::{Error, SocketContext},
    hid::HidTransport,
    model::DeviceModel,
    socket::{run_hsm_session, socket_url, SocketEvent, TimeoutTransport},
    version::{coerced_at_least, SemVer},
};

use ledger_transport_hidapi::hidapi::HidApi;

use std::{thread, time::Duration, time::Instant};

/// Bootloader versions aliases. https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/flash.ts
const BL_VERSION_ALIASES: &[(&str, &str)] = &[("0.0", "0.6")];

/// The MCU or bootloader version the repair flashes for these bootloader versions (`majMin`).
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/firmwareUpdate-repair.ts
const REPAIR_VERSIONS: &[(&str, &str)] = &[
    ("0.0", "0.6"),
    ("0.6", "1.5"),
    ("0.7", "1.6"),
    ("0.9", "1.7"),
];

/// Maximum number of MCU or bootloader flashes before giving up.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/tasks/updateFirmware.ts
const MAX_FLASH_REPETITIONS: usize = 5;

/// A step of the firmware update, reported to the caller for progress display.
#[derive(Debug, Clone, PartialEq)]
pub enum FirmwareUpdateStep {
    /// Checking the state of the device.
    Preparing,
    /// The user must allow the Ledger manager on the device.
    AllowManagerRequested,
    /// The user allowed the Ledger manager.
    AllowManagerGranted,
    /// Transferring the OS updater to the device. `progress` is between 0 and 1.
    InstallingOsu { progress: f32 },
    /// The user must confirm the firmware update on the device. If present, the user should check
    /// the identifier displayed on the device matches `identifier`. It is formatted like the
    /// device displays it (see `format_hash_name`), with its lines separated by spaces.
    WaitingUserConfirmation { identifier: Option<String> },
    /// The user confirmed the update on the device.
    UserConfirmed,
    /// The device is rebooting.
    WaitingForReboot,
    /// Waiting for the device to reboot in bootloader mode, to flash the MCU.
    WaitingForBootloader,
    /// Flashing the bootloader. `progress` is between 0 and 1.
    FlashingBootloader { progress: f32 },
    /// Flashing the MCU. `progress` is between 0 and 1.
    FlashingMcu { progress: f32 },
    /// Installing the final firmware (legacy flow). `progress` is between 0 and 1.
    InstallingFinal { progress: f32 },
    /// Waiting for the device to finish the update and to reboot on the new firmware. The update
    /// can take several minutes, the device must stay connected.
    WaitingForDevice,
    /// The device is locked. The user must unlock it for the update to complete.
    DeviceLocked,
    /// The update completed. Contains the new device information.
    Done { device_info: Box<DeviceInfo> },
}

/// Options for the firmware update.
#[derive(Debug, Clone)]
pub struct FirmwareUpdateOptions {
    /// How long to wait for the device to come back each time it reboots.
    pub reboot_timeout: Duration,
    /// How often to poll the device while waiting for it.
    pub poll_interval: Duration,
    /// How long to wait for the device to disconnect after the OSU was installed, when no MCU
    /// needs to be flashed (Ledger Live's `potentialAutoFlash` step).
    pub disconnect_timeout: Duration,
    /// How long to wait for the device to answer an APDU relayed from Ledger's HSM, except for
    /// the APDUs which wait for the user (allowing the manager, confirming the update) which have
    /// no timeout. Avoids hanging forever if the device stops answering in the middle of the
    /// update.
    pub apdu_timeout: Duration,
}

impl Default for FirmwareUpdateOptions {
    fn default() -> Self {
        Self {
            // Ledger Live waits forever for the bootloader, and 5 minutes for the final reboot.
            reboot_timeout: Duration::from_secs(10 * 60),
            // WITH_DEVICE_POLLING_DELAY
            poll_interval: Duration::from_millis(500),
            disconnect_timeout: Duration::from_secs(20),
            apdu_timeout: Duration::from_secs(2 * 60),
        }
    }
}

/// Check whether updating the firmware of this device is supported. Returns an error describing
/// why if not.
///
/// See `isUsbUpdateSupported` in https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/isFirmwareUpdateVersionSupported.ts
/// and `firmwareUnsupported`, `firmwareUpdateNeedsLegacyBlueResetInstructions` in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/index.ts
pub fn check_firmware_update_supported(device_info: &DeviceInfo) -> Result<(), Error> {
    if device_info.is_bootloader {
        return Err(Error::DeviceInBootloader);
    }
    if device_info.is_osu {
        // An interrupted update can always be resumed.
        return Ok(());
    }
    let model = device_info.model.ok_or_else(|| {
        Error::FirmwareUpdateNotSupported(format!(
            "unknown device model (target id {:#010x})",
            device_info.target_id
        ))
    })?;
    let min = model
        .usb_update_min_version()
        .ok_or_else(|| Error::FirmwareUpdateNotSupported(format!("{} is not supported", model)))?;
    if !coerced_at_least(&device_info.version, min) {
        return Err(Error::FirmwareUpdateNotSupported(format!(
            "{} firmware {} is too old to be updated through USB (minimum {}.{}.{}). Please use Ledger Live.",
            model, device_info.version, min.0, min.1, min.2
        )));
    }
    Ok(())
}

/// Whether the firmware update will uninstall the applications installed on the device. Ledger
/// Live considers it always does (`firmwareUpdateWillUninstallApps`), the apps have to be
/// reinstalled afterwards.
pub fn firmware_update_will_uninstall_apps(_device_info: &DeviceInfo) -> bool {
    true
}

/// Whether the device will lose its custom lock screen and language settings during the update.
/// Ledger Live backs them up and restores them for these devices; we don't.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/apps/ledger-live-desktop/src/renderer/modals/UpdateFirmwareModal/helpers/createFirmwareUpdateSteps.ts
pub fn firmware_update_resets_customization(
    device_info: &DeviceInfo,
    update: &FirmwareUpdateInfo,
) -> bool {
    device_info
        .model
        .map(|m| {
            m.has_touch_screen()
                || crate::device::is_device_localization_supported(update.version(), Some(m))
        })
        .unwrap_or(false)
}

/// The MCU or bootloader to flash, as determined from the current bootloader version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FlashTarget {
    /// The version to pass to the `/mcu` endpoint.
    pub version: String,
    /// Whether this is the MCU (or else the bootloader).
    pub is_mcu: bool,
}

/// Determine what to flash given the bootloader version (`maj_min`) of the device.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/flash.ts
/// and `getFlashMcuOrBootloaderDetails` in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/tasks/updateFirmware.ts
pub(crate) fn flash_target(
    maj_min: &str,
    mcus: &[McuVersion],
    final_firmware: &FinalFirmware,
    provider: u32,
) -> Result<FlashTarget, Error> {
    if let Some((_, alias)) = BL_VERSION_ALIASES.iter().find(|(v, _)| *v == maj_min) {
        return Ok(FlashTarget {
            version: alias.to_string(),
            is_mcu: false,
        });
    }
    let available = mcus_for_final_firmware(mcus, final_firmware, provider);
    let mcu = find_best_mcu(&available).ok_or(Error::McuVersionNotFound)?;
    // Converts the version into the majMin format.
    let mcu_from_bootloader = mcu
        .from_bootloader_version
        .split('.')
        .take(3)
        .collect::<Vec<_>>()
        .join(".");
    let is_mcu = maj_min == mcu_from_bootloader;
    Ok(FlashTarget {
        version: if is_mcu {
            mcu.name.clone()
        } else {
            mcu_from_bootloader
        },
        is_mcu,
    })
}

/// Format the identifier (hash) of a firmware the way the device displays it, so the user can
/// compare them. Returns the lines of the identifier: the hash is uppercased and, depending on the
/// model and firmware version of the device, split into chunks (Nano X, Nano S 1.6.0 and later)
/// or ellipsized (Blue and Nano S before 1.6.0, or when the model or version is unknown). Newer
/// models display the full hash.
///
/// Ported from `formatHashName` in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/index.ts
pub fn format_hash_name(
    hash: &str,
    model: Option<DeviceModel>,
    firmware_version: Option<&str>,
) -> Vec<String> {
    let version = firmware_version.and_then(SemVer::coerce);
    // Ledger Live would throw for an invalid version, we show the full hash then.
    let nano_s_lt_1_6 = version.as_ref().map(|v| *v < SemVer::new(1, 6, 0));
    let (should_ellipsis, should_split) = match (model, firmware_version) {
        (Some(model), Some(_)) => (
            model == DeviceModel::Blue
                || (model == DeviceModel::NanoS && nano_s_lt_1_6 == Some(true)),
            (model == DeviceModel::NanoS && nano_s_lt_1_6 == Some(false))
                || model == DeviceModel::NanoX,
        ),
        _ => (true, false),
    };
    let hash = hash.to_uppercase();
    if should_split {
        let split_length = if model == Some(DeviceModel::NanoS) {
            16
        } else {
            17
        };
        hash.chars()
            .collect::<Vec<_>>()
            .chunks(split_length)
            .map(|c| c.iter().collect())
            .collect()
    } else if hash.chars().count() > 8 && should_ellipsis {
        let chars: Vec<char> = hash.chars().collect();
        let start: String = chars[..4].iter().collect();
        let end: String = chars[chars.len() - 4..].iter().collect();
        vec![format!("{}...{}", start, end)]
    } else {
        vec![hash]
    }
}

/// The OSU identifier to show to the user for comparison with the one the device displays, if
/// the Ledger API gave one. The lines of `format_hash_name` are joined with spaces.
pub(crate) fn osu_identifier(
    update: &FirmwareUpdateInfo,
    device_info: &DeviceInfo,
) -> Option<String> {
    let hash = update.osu.hash.as_deref().filter(|h| !h.is_empty())?;
    Some(format_hash_name(hash, device_info.model, Some(&device_info.version)).join(" "))
}

/// Map the bulk progress of the OSU installation to update steps. The penultimate APDU of the
/// bulk is a blocking APDU which requires the user to confirm the update, and the last one means
/// the user validated it.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/commands/firmwareUpdate/installFirmware.ts
pub(crate) fn osu_step(
    index: usize,
    total: usize,
    identifier: &Option<String>,
) -> FirmwareUpdateStep {
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

#[allow(clippy::too_many_arguments)]
fn install_firmware_socket<P: FnMut(FirmwareUpdateStep)>(
    hid_api: &mut HidApi,
    options: &FirmwareUpdateOptions,
    target_id: u32,
    firmware: &str,
    perso: &str,
    firmware_key: &str,
    mut on_bulk: impl FnMut(usize, usize) -> FirmwareUpdateStep,
    progress: &mut P,
) -> Result<(), Error> {
    let hid = HidTransport::connect(hid_api)?;
    let transport = TimeoutTransport {
        transport: &hid,
        timeout: options.apdu_timeout,
    };
    let target_id = target_id.to_string();
    let url = socket_url(
        "install",
        &[
            ("targetId", &target_id),
            ("firmware", firmware),
            ("perso", perso),
            ("firmwareKey", firmware_key),
        ],
    );
    run_hsm_session(&transport, &url, SocketContext::Firmware, |e| match e {
        SocketEvent::DevicePermissionRequested => {
            progress(FirmwareUpdateStep::AllowManagerRequested)
        }
        SocketEvent::DevicePermissionGranted => progress(FirmwareUpdateStep::AllowManagerGranted),
        SocketEvent::BulkProgress { index, total } => progress(on_bulk(index, total)),
        _ => {}
    })?;
    Ok(())
}

/// Wait for the device to disconnect, up to `timeout`.
fn wait_for_disconnect(hid_api: &mut HidApi, timeout: Duration, interval: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if hid_api.refresh_devices().is_ok() && list_ledger_devices(hid_api).is_empty() {
            log::debug!("Device disconnected.");
            return;
        }
        thread::sleep(interval);
    }
    log::debug!("Device didn't disconnect after {:?}.", timeout);
}

/// Flash the MCU or the bootloader of a device in bootloader mode, as needed for this update.
fn flash_mcu_or_bootloader<P: FnMut(FirmwareUpdateStep)>(
    hid_api: &mut HidApi,
    device_info: &DeviceInfo,
    update: &FirmwareUpdateInfo,
    mcus: &mut Option<Vec<McuVersion>>,
    options: &FirmwareUpdateOptions,
    progress: &mut P,
) -> Result<(), Error> {
    let target = if BL_VERSION_ALIASES
        .iter()
        .any(|(v, _)| *v == device_info.maj_min)
    {
        flash_target(&device_info.maj_min, &[], &update.final_firmware, 1)?
    } else {
        flash_target(
            &device_info.maj_min,
            cached_mcus(mcus)?,
            &update.final_firmware,
            device_info.provider_id(),
        )?
    };
    install_mcu(hid_api, device_info, &target, options, progress)
}

/// Fetch the MCU versions from the Ledger API, once.
fn cached_mcus(mcus: &mut Option<Vec<McuVersion>>) -> Result<&[McuVersion], Error> {
    if mcus.is_none() {
        *mcus = Some(fetch_mcus()?);
    }
    Ok(mcus.as_deref().unwrap_or_default())
}

/// Flash this MCU or bootloader version on a device in bootloader mode, retrying to connect to the
/// device if it can't be opened (it may be rebooting).
fn install_mcu<P: FnMut(FirmwareUpdateStep)>(
    hid_api: &mut HidApi,
    device_info: &DeviceInfo,
    target: &FlashTarget,
    options: &FirmwareUpdateOptions,
    progress: &mut P,
) -> Result<(), Error> {
    log::info!(
        "Flashing {} {} (bootloader version {}).",
        if target.is_mcu { "MCU" } else { "bootloader" },
        target.version,
        device_info.maj_min
    );
    let step = |p: f32| {
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
            Err(e) if start.elapsed() < options.reboot_timeout => {
                log::debug!("Could not open the device, retrying: {}", e);
                thread::sleep(options.poll_interval);
            }
            Err(e) => return Err(e),
        }
    };
    let transport = TimeoutTransport {
        transport: &hid,
        timeout: options.apdu_timeout,
    };
    let target_id = device_info.target_id.to_string();
    let url = socket_url(
        "mcu",
        &[("targetId", &target_id), ("version", &target.version)],
    );
    run_hsm_session(&transport, &url, SocketContext::Mcu, |e| {
        if let Some(p) = e.bulk_progress() {
            progress(step(p));
        }
    })?;
    Ok(())
}

/// Install the final firmware on a device in OSU mode (legacy flow).
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/installFinalFirmware.ts
fn install_final_firmware<P: FnMut(FirmwareUpdateStep)>(
    hid_api: &mut HidApi,
    options: &FirmwareUpdateOptions,
    device_info: &DeviceInfo,
    progress: &mut P,
) -> Result<(), Error> {
    let provider = device_info.provider_id();
    let device_version = get_device_version(device_info.target_id, provider)?;
    let osu = get_current_osu(&device_info.version, device_version.id, provider)?;
    let next = get_final_firmware_by_id(osu.next_se_firmware_final_version)?;
    let (firmware, firmware_key) = match (&next.firmware, &next.firmware_key) {
        (Some(f), Some(k)) if !f.is_empty() => (f.clone(), k.clone()),
        _ => {
            return Err(Error::FirmwareUpdateNotSupported(
                "no final firmware to install".into(),
            ))
        }
    };
    log::info!("Installing final firmware {}.", next.name);
    progress(FirmwareUpdateStep::InstallingFinal { progress: 0.0 });
    install_firmware_socket(
        hid_api,
        options,
        device_info.target_id,
        &firmware,
        &next.perso,
        &firmware_key,
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

/// Update the firmware of the connected device to the version given by `update` (as returned by
/// `latest_firmware`), using the default options. Returns the information of the device running
/// the new firmware.
///
/// Progress is reported through the `progress` callback. The device reboots (possibly several
/// times) during the update, which is why this takes the `HidApi` to reconnect to it: it must
/// not be used concurrently to talk to the device, and no other transport to the device should be
/// open.
///
/// WARNING: the applications installed on the device are removed by the update, and they need to
/// be reinstalled afterwards. On Stax, Flex and Nano Gen5, the custom lock screen is not backed
/// up and restored (Ledger Live does), and the language may have to be set again on the device.
pub fn update_firmware<P: FnMut(FirmwareUpdateStep)>(
    hid_api: &mut HidApi,
    update: &FirmwareUpdateInfo,
    progress: P,
) -> Result<DeviceInfo, Error> {
    update_firmware_with_options(hid_api, update, &FirmwareUpdateOptions::default(), progress)
}

/// Same as `update_firmware` with custom options.
pub fn update_firmware_with_options<P: FnMut(FirmwareUpdateStep)>(
    hid_api: &mut HidApi,
    update: &FirmwareUpdateInfo,
    options: &FirmwareUpdateOptions,
    mut progress: P,
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
            .map(|m| m.product_name())
            .unwrap_or("unknown device"),
        device_info.firmware_summary(),
        update.version(),
        update.osu.name,
        update.should_flash_mcu,
        update.has_final_firmware()
    );

    // Prepare step: install the OSU. If the device is already in OSU mode (for instance if a
    // previous update was interrupted) we directly jump to the main step, as Ledger Live does.
    if !device_info.is_osu {
        let identifier = osu_identifier(update, &device_info);
        progress(FirmwareUpdateStep::InstallingOsu { progress: 0.0 });
        install_firmware_socket(
            hid_api,
            options,
            device_info.target_id,
            &update.osu.firmware,
            &update.osu.perso,
            &update.osu.firmware_key,
            |index, total| osu_step(index, total, &identifier),
            &mut progress,
        )?;
        // The device is likely rebooting now, we give it some time.
        progress(FirmwareUpdateStep::WaitingForReboot);
        thread::sleep(Duration::from_secs(3));
    }

    let mut locked_reported = false;
    let mut on_poll_error = |e: &Error, progress: &mut P| {
        if matches!(e, Error::DeviceLocked) && !locked_reported {
            locked_reported = true;
            progress(FirmwareUpdateStep::DeviceLocked);
        }
    };

    // Main step.
    if update.should_flash_mcu {
        progress(FirmwareUpdateStep::WaitingForBootloader);
        let mut info = wait_for_device(
            hid_api,
            options.reboot_timeout,
            options.poll_interval,
            |i| i.is_bootloader,
            |e| on_poll_error(e, &mut progress),
        )?;
        let mut mcus = None;
        let mut repetitions = 0;
        while info.is_bootloader {
            if repetitions >= MAX_FLASH_REPETITIONS {
                return Err(Error::TooManyMcuOrBootloaderFlashes);
            }
            repetitions += 1;
            flash_mcu_or_bootloader(hid_api, &info, update, &mut mcus, options, &mut progress)?;
            progress(FirmwareUpdateStep::WaitingForReboot);
            thread::sleep(Duration::from_secs(2));
            info = wait_for_device(
                hid_api,
                options.reboot_timeout,
                options.poll_interval,
                |_| true,
                |e| on_poll_error(e, &mut progress),
            )?;
        }
    } else {
        // The device may flash things by itself: wait for it to disconnect (or for a timeout).
        let info = wait_for_device(
            hid_api,
            options.reboot_timeout,
            options.poll_interval,
            |_| true,
            |e| on_poll_error(e, &mut progress),
        )?;
        if !info.is_osu {
            wait_for_disconnect(hid_api, options.disconnect_timeout, options.poll_interval);
        }
    }

    if update.has_final_firmware() {
        let info = wait_for_device(
            hid_api,
            options.reboot_timeout,
            options.poll_interval,
            |_| true,
            |e| on_poll_error(e, &mut progress),
        )?;
        if !info.is_osu {
            return Err(Error::DeviceInOsuExpected);
        }
        install_final_firmware(hid_api, options, &info, &mut progress)?;
    }

    // Wait for the device to come back running the new firmware.
    progress(FirmwareUpdateStep::WaitingForDevice);
    let info = wait_for_device(
        hid_api,
        options.reboot_timeout,
        options.poll_interval,
        |i| i.is_normal_mode(),
        |e| on_poll_error(e, &mut progress),
    )?;
    if SemVer::coerce(&info.version) != SemVer::coerce(update.version()) {
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

/// The MCU or bootloader to flash to repair a device in bootloader mode running a recent
/// firmware (whose SE version is known): the best MCU for its current final firmware, or the
/// bootloader this MCU requires if the device runs another bootloader version.
/// `mcu_bl_version` is the bootloader version of the device.
///
/// From the `seVersion && seTargetId` branch of
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/firmwareUpdate-repair.ts
pub(crate) fn repair_target_for_final_firmware(
    mcu_bl_version: Option<&str>,
    mcus: &[McuVersion],
    final_firmware: &FinalFirmware,
    provider: u32,
) -> Option<FlashTarget> {
    let available = mcus_for_final_firmware(mcus, final_firmware, provider);
    let mcu = find_best_mcu(&available)?;
    // Ledger Live compares the coerced versions, as strings (so two unparsable versions are
    // equal).
    let coerce = |v: &str| SemVer::coerce(v).map(|v| (v.major, v.minor, v.patch));
    let expected = coerce(&mcu.from_bootloader_version);
    let current = mcu_bl_version.and_then(coerce);
    Some(if expected == current {
        FlashTarget {
            version: mcu.name.clone(),
            is_mcu: true,
        }
    } else {
        FlashTarget {
            version: mcu.from_bootloader_version.clone(),
            is_mcu: false,
        }
    })
}

/// The MCU to flash to repair a device in bootloader mode whose SE version is unknown: the best
/// MCU which can be installed from its bootloader version.
///
/// From `compatibleMCUForDeviceInfo` in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/api.ts
pub(crate) fn repair_target_compatible(
    maj_min: &str,
    version: &str,
    mcus: &[McuVersion],
    provider: u32,
) -> Option<FlashTarget> {
    let compatible: Vec<McuVersion> = mcus
        .iter()
        .filter(|m| {
            (m.from_bootloader_version == maj_min || m.from_bootloader_version == version)
                && m.providers.contains(&(provider as i64))
        })
        .cloned()
        .collect();
    find_best_mcu(&compatible).map(|m| FlashTarget {
        version: m.name.clone(),
        is_mcu: true,
    })
}

/// The MCU or bootloader version to flash for these bootloader versions, if fixed.
pub(crate) fn repair_fixed_target(maj_min: &str) -> Option<FlashTarget> {
    REPAIR_VERSIONS
        .iter()
        .find(|(v, _)| *v == maj_min)
        .map(|(_, target)| FlashTarget {
            version: target.to_string(),
            // 0.0 is aliased to the 0.6 bootloader (see `BL_VERSION_ALIASES`).
            is_mcu: maj_min != "0.0",
        })
}

/// Determine what to flash to repair this device in bootloader mode.
fn repair_target(
    device_info: &DeviceInfo,
    mcus: &mut Option<Vec<McuVersion>>,
) -> Result<FlashTarget, Error> {
    if let Some(target) = repair_fixed_target(&device_info.maj_min) {
        return Ok(target);
    }
    let provider = device_info.provider_id();
    let target = match (&device_info.se_version, device_info.se_target_id) {
        (Some(se_version), Some(se_target_id)) => {
            log::debug!(
                "Repair: SE version {} and SE target id {:#010x} found.",
                se_version,
                se_target_id
            );
            let device_version = get_device_version(se_target_id, provider)?;
            let final_firmware = get_current_firmware(se_version, device_version.id, provider)?;
            repair_target_for_final_firmware(
                device_info.mcu_bl_version.as_deref(),
                cached_mcus(mcus)?,
                &final_firmware,
                provider,
            )
        }
        _ => repair_target_compatible(
            &device_info.maj_min,
            &device_info.version,
            cached_mcus(mcus)?,
            provider,
        ),
    };
    target.ok_or(Error::McuVersionNotFound)
}

/// Repair the firmware of a device in bootloader mode, typically after a firmware update was
/// interrupted while flashing the MCU or the bootloader (`Error::DeviceInBootloader`), using the
/// default options. Returns the information of the device once it left the bootloader.
///
/// This first waits for the device to be connected in bootloader mode, then flashes the MCU or
/// the bootloader as needed until the device leaves the bootloader. `forced_version` forces the
/// first version to flash, as Ledger Live's repair choices do ("0.7" if the device shows "MCU
/// outdated" or "MCU not genuine", "0.9" if it tells to follow the repair or update
/// instructions). Progress is reported through the same steps as `update_firmware`
/// (`WaitingForBootloader`, `FlashingMcu`, `FlashingBootloader`, `WaitingForReboot`, `Done`).
///
/// If the device isn't in bootloader mode, this waits for it to be (up to the reboot timeout).
/// If it runs its firmware normally after the repair, it may still be necessary to update the
/// firmware (see `latest_firmware`).
///
/// Ported from https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/firmwareUpdate-repair.ts
pub fn repair_firmware<P: FnMut(FirmwareUpdateStep)>(
    hid_api: &mut HidApi,
    forced_version: Option<&str>,
    progress: P,
) -> Result<DeviceInfo, Error> {
    repair_firmware_with_options(
        hid_api,
        forced_version,
        &FirmwareUpdateOptions::default(),
        progress,
    )
}

/// Same as `repair_firmware` with custom options.
pub fn repair_firmware_with_options<P: FnMut(FirmwareUpdateStep)>(
    hid_api: &mut HidApi,
    forced_version: Option<&str>,
    options: &FirmwareUpdateOptions,
    mut progress: P,
) -> Result<DeviceInfo, Error> {
    progress(FirmwareUpdateStep::WaitingForBootloader);
    let mut info = wait_for_device(
        hid_api,
        options.reboot_timeout,
        options.poll_interval,
        |i| i.is_bootloader,
        |_| {},
    )?;
    log::info!(
        "Repairing the firmware of the device ({}).",
        info.firmware_summary()
    );

    let mut forced_version = forced_version.filter(|v| !v.is_empty());
    let mut mcus = None;
    let mut repetitions = 0;
    while info.is_bootloader {
        if repetitions >= MAX_FLASH_REPETITIONS {
            return Err(Error::TooManyMcuOrBootloaderFlashes);
        }
        repetitions += 1;
        let target = match forced_version.take() {
            // This is a special case where the user is in firmware 1.3.1 and the device shows
            // "MCU not genuine". The user needs to go back to the dashboard to continue the
            // update process.
            Some("0.7") if info.maj_min == "0.6" || info.maj_min == "0.7" => {
                return Err(Error::McuNotGenuineToDashboard)
            }
            Some(version) => FlashTarget {
                version: version.to_string(),
                is_mcu: true,
            },
            None => repair_target(&info, &mut mcus)?,
        };
        install_mcu(hid_api, &info, &target, options, &mut progress)?;
        progress(FirmwareUpdateStep::WaitingForReboot);
        thread::sleep(Duration::from_secs(2));
        info = wait_for_device(
            hid_api,
            options.reboot_timeout,
            options.poll_interval,
            |_| true,
            |_| {},
        )?;
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

    fn mcu(id: i64, name: &str, from: &str) -> McuVersion {
        McuVersion {
            id,
            mcu: None,
            name: name.to_string(),
            description: None,
            providers: vec![1],
            from_bootloader_version: from.to_string(),
            device_versions: vec![],
            se_firmware_final_versions: vec![],
        }
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

    #[test]
    fn flash_targets() {
        let mcus = vec![mcu(10, "2.30", "1.16"), mcu(11, "2.12", "1.12")];
        let fw = final_fw(vec![10, 11]);

        // Bootloader matches the MCU requirements: flash the MCU.
        assert_eq!(
            flash_target("1.16", &mcus, &fw, 1).unwrap(),
            FlashTarget {
                version: "2.30".into(),
                is_mcu: true
            }
        );
        // Bootloader too old: flash the bootloader first.
        assert_eq!(
            flash_target("1.12", &mcus, &fw, 1).unwrap(),
            FlashTarget {
                version: "1.16".into(),
                is_mcu: false
            }
        );
        // Alias.
        assert_eq!(
            flash_target("0.0", &[], &fw, 1).unwrap(),
            FlashTarget {
                version: "0.6".into(),
                is_mcu: false
            }
        );
        // No compatible MCU.
        assert!(matches!(
            flash_target("1.16", &mcus, &final_fw(vec![]), 1),
            Err(Error::McuVersionNotFound)
        ));
        assert!(matches!(
            flash_target("1.16", &mcus, &fw, 4),
            Err(Error::McuVersionNotFound)
        ));
        // Only the first three parts of from_bootloader_version are considered.
        let mcus = vec![mcu(12, "3.1", "2.0.1.4")];
        assert_eq!(
            flash_target("2.0.1", &mcus, &final_fw(vec![12]), 1).unwrap(),
            FlashTarget {
                version: "3.1".into(),
                is_mcu: true
            }
        );
    }

    #[test]
    fn repair_targets() {
        // Fixed versions.
        let t = |v: &str, is_mcu| {
            Some(FlashTarget {
                version: v.into(),
                is_mcu,
            })
        };
        assert_eq!(repair_fixed_target("0.0"), t("0.6", false));
        assert_eq!(repair_fixed_target("0.6"), t("1.5", true));
        assert_eq!(repair_fixed_target("0.7"), t("1.6", true));
        assert_eq!(repair_fixed_target("0.9"), t("1.7", true));
        assert_eq!(repair_fixed_target("1.16"), None);
        assert_eq!(repair_fixed_target("0.8"), None);

        // With a known final firmware.
        let mcus = vec![
            mcu(10, "2.30", "1.16"),
            mcu(11, "2.12", "1.12"),
            mcu(12, "2.40", "none"),
        ];
        let fw = final_fw(vec![10, 11, 12]);
        // The bootloader matches the best MCU: flash the MCU.
        assert_eq!(
            repair_target_for_final_firmware(Some("1.16"), &mcus, &fw, 1),
            t("2.30", true)
        );
        assert_eq!(
            repair_target_for_final_firmware(Some("1.16.0"), &mcus, &fw, 1),
            t("2.30", true)
        );
        // Otherwise flash the bootloader it requires.
        assert_eq!(
            repair_target_for_final_firmware(Some("1.12"), &mcus, &fw, 1),
            t("1.16", false)
        );
        assert_eq!(
            repair_target_for_final_firmware(None, &mcus, &fw, 1),
            t("1.16", false)
        );
        // The bootloader version is not truncated.
        let mcus4 = vec![mcu(13, "3.1", "2.0.1.4")];
        assert_eq!(
            repair_target_for_final_firmware(Some("1.0"), &mcus4, &final_fw(vec![13]), 1),
            t("2.0.1.4", false)
        );
        // No MCU for this final firmware or provider.
        assert_eq!(
            repair_target_for_final_firmware(Some("1.16"), &mcus, &final_fw(vec![12]), 1),
            None
        );
        assert_eq!(
            repair_target_for_final_firmware(Some("1.16"), &mcus, &fw, 2),
            None
        );

        // Without SE information: the MCUs compatible with the bootloader version.
        let mcus = vec![
            mcu(10, "2.30", "1.16"),
            mcu(14, "2.31", "1.16"),
            mcu(11, "2.12", "1.12"),
            mcu(15, "1.9", "1.4.2"),
        ];
        assert_eq!(
            repair_target_compatible("1.16", "1.16", &mcus, 1),
            t("2.31", true)
        );
        assert_eq!(
            repair_target_compatible("1.4", "1.4.2", &mcus, 1),
            t("1.9", true)
        );
        assert_eq!(repair_target_compatible("1.16", "1.16", &mcus, 2), None);
        assert_eq!(repair_target_compatible("1.20", "1.20", &mcus, 1), None);
    }

    #[test]
    fn hash_names() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let upper = hash.to_uppercase();
        // Nano X: chunks of 17.
        assert_eq!(
            format_hash_name(hash, Some(DeviceModel::NanoX), Some("2.2.3")),
            vec![
                "0123456789ABCDEF0",
                "123456789ABCDEF01",
                "23456789ABCDEF012",
                "3456789ABCDEF",
            ]
        );
        // Nano S 1.6.0 and later: chunks of 16.
        assert_eq!(
            format_hash_name(hash, Some(DeviceModel::NanoS), Some("2.1.0")),
            vec!["0123456789ABCDEF"; 4]
        );
        assert_eq!(
            format_hash_name(hash, Some(DeviceModel::NanoS), Some("1.6.0")),
            vec!["0123456789ABCDEF"; 4]
        );
        // Older Nano S and Blue: ellipsis.
        assert_eq!(
            format_hash_name(hash, Some(DeviceModel::NanoS), Some("1.5.5")),
            vec!["0123...CDEF"]
        );
        assert_eq!(
            format_hash_name(hash, Some(DeviceModel::Blue), Some("2.1.1")),
            vec!["0123...CDEF"]
        );
        assert_eq!(
            format_hash_name("abcd1234", Some(DeviceModel::Blue), Some("2.1.1")),
            vec!["ABCD1234"]
        );
        // Unknown model or version: ellipsis.
        assert_eq!(
            format_hash_name(hash, None, Some("1.0.0")),
            vec!["0123...CDEF"]
        );
        assert_eq!(
            format_hash_name(hash, Some(DeviceModel::Stax), None),
            vec!["0123...CDEF"]
        );
        // Newer models: the full hash.
        for m in [
            DeviceModel::NanoSPlus,
            DeviceModel::Stax,
            DeviceModel::Flex,
            DeviceModel::NanoGen5,
        ] {
            assert_eq!(
                format_hash_name(hash, Some(m), Some("1.1.0")),
                vec![upper.clone()]
            );
        }
        assert_eq!(
            format_hash_name("", Some(DeviceModel::NanoX), Some("2.2.3")),
            Vec::<String>::new()
        );
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

    fn device(hex_data: &str) -> DeviceInfo {
        DeviceInfo::from_get_version_response(&hex::decode(hex_data).unwrap()).unwrap()
    }

    #[test]
    fn update_supported() {
        // Nano X 2.2.3.
        let nano_x = device("3300000405322e322e3304ee00000004322e333004312e3136010101000100");
        assert!(check_firmware_update_supported(&nano_x).is_ok());
        // Nano S 1.5.5: too old for USB update.
        let data = format!(
            "31100004{:02x}{}04a600000004{}",
            5,
            hex::encode("1.5.5"),
            hex::encode("1.12")
        );
        let nano_s = device(&data);
        assert!(matches!(
            check_firmware_update_supported(&nano_s),
            Err(Error::FirmwareUpdateNotSupported(_))
        ));
        // Blue.
        let data = format!(
            "31000002{:02x}{}04a600000004{}",
            5,
            hex::encode("2.1.1"),
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
