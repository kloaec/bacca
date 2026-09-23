//! Update the firmware of BitBox02 devices without the BitBoxApp.
//!
//! On the BitBox02 there is no separate "Bitcoin app": the firmware edition (Multi or
//! Bitcoin-only) is fixed by the device's bootloader, and updating the firmware is updating the
//! "app". Supported: BitBox02 and BitBox02 Nova, Multi and Bitcoin-only editions, in firmware and
//! bootloader mode. The discontinued BitBox01 is not supported.
//!
//! # Threading
//!
//! The whole API is **blocking** (it does USB and HTTP I/O and waits for user confirmations on
//! the device, possibly for minutes). From an async (e.g. tokio) GUI, run it with
//! `tokio::task::spawn_blocking` and forward [`Progress`] events through a channel. Device
//! handles ([`hww::FirmwareDevice`], [`bootloader::Bootloader`]) are `Send` but not `Sync`: use
//! each from one thread at a time.

pub mod bootloader;
pub mod hww;
pub mod noise_config;
pub mod product;
pub mod releases;
pub mod signed_firmware;
pub mod u2fhid;

pub use hidapi;

use std::{
    ffi::CString,
    fmt, thread,
    time::{Duration, Instant},
};

use bootloader::{Bootloader, BootloaderError};
use hww::{FirmwareDevice, HwwError, HwwInfo};
use noise_config::NoiseConfig;
pub use product::{Edition, Mode, Platform, Product, Version};
use releases::{FirmwareRelease, NextStep, ReleaseError};
use signed_firmware::{FirmwareFormatError, SighashScheme, SignedFirmware};

/// How long to wait for the device to show up in bootloader mode after the user confirmed the
/// reboot on the device.
const WAIT_BOOTLOADER_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for the device to come back after a reboot of the bootloader (it may boot an
/// intermediate firmware which upgrades the bootloader).
const WAIT_REBOOT_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Safety net for the upgrade loop (at most 2 intermediates, each needing an install and a boot).
const MAX_UPGRADE_STEPS: usize = 8;

#[derive(Debug)]
pub enum Error {
    Hid(hidapi::HidError),
    NoDevice,
    TooManyDevices(usize),
    Hww(HwwError),
    Bootloader(BootloaderError),
    Release(ReleaseError),
    InvalidFirmware(FirmwareFormatError),
    Io(std::io::Error),
    /// The device reported an unknown product.
    UnknownProduct,
    /// The firmware is not for this device's product (platform or edition).
    WrongProduct {
        device: Product,
        firmware: Product,
    },
    /// The firmware is older than the installed one.
    Downgrade {
        installed: u32,
        firmware: u32,
    },
    /// The firmware's signing keys are older than the installed ones.
    SigningKeysDowngrade {
        installed: u32,
        firmware: u32,
    },
    /// The firmware hash reported by the bootloader after flashing is not the expected one.
    HashMismatch {
        expected: [u8; 32],
        got: [u8; 32],
    },
    Timeout(&'static str),
    /// The upgrade did not converge (e.g. an intermediate firmware did not complete).
    UpgradeStuck,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hid(e) => write!(f, "HID error: {}", e),
            Self::NoDevice => write!(
                f,
                "no BitBox02 found. Is it connected (and not used by another app)?"
            ),
            Self::TooManyDevices(n) => {
                write!(f, "{} BitBox02 devices found, please connect only one", n)
            }
            Self::Hww(e) => write!(f, "{}", e),
            Self::Bootloader(e) => write!(f, "{}", e),
            Self::Release(e) => write!(f, "{}", e),
            Self::InvalidFirmware(e) => write!(f, "invalid firmware: {}", e),
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::UnknownProduct => write!(f, "unknown BitBox product"),
            Self::WrongProduct { device, firmware } => write!(
                f,
                "this firmware is for a {} but the device is a {}",
                firmware, device
            ),
            Self::Downgrade {
                installed,
                firmware,
            } => write!(
                f,
                "downgrades are not possible: the device has firmware version {} (monotonic) \
                 installed, the firmware to flash has version {}",
                installed, firmware
            ),
            Self::SigningKeysDowngrade {
                installed,
                firmware,
            } => write!(
                f,
                "the firmware's signing keys (version {}) are older than the device's (version {})",
                firmware, installed
            ),
            Self::HashMismatch { expected, got } => write!(
                f,
                "firmware hash mismatch after flashing: expected {}, device reports {}",
                hex::encode(expected),
                hex::encode(got)
            ),
            Self::Timeout(what) => write!(f, "timeout {}", what),
            Self::UpgradeStuck => write!(f, "the upgrade did not complete"),
        }
    }
}

impl std::error::Error for Error {}

impl From<hidapi::HidError> for Error {
    fn from(e: hidapi::HidError) -> Self {
        Error::Hid(e)
    }
}
impl From<HwwError> for Error {
    fn from(e: HwwError) -> Self {
        Error::Hww(e)
    }
}
impl From<BootloaderError> for Error {
    fn from(e: BootloaderError) -> Self {
        Error::Bootloader(e)
    }
}
impl From<ReleaseError> for Error {
    fn from(e: ReleaseError) -> Self {
        Error::Release(e)
    }
}
impl From<FirmwareFormatError> for Error {
    fn from(e: FirmwareFormatError) -> Self {
        Error::InvalidFirmware(e)
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// A BitBox02 found on USB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceHandle {
    pub product: Product,
    pub mode: Mode,
    /// Firmware (in firmware mode) or bootloader (in bootloader mode) version, from the HID serial
    /// number string.
    pub version: Option<Version>,
    pub serial_number: String,
    pub path: CString,
}

/// Whether this HID interface is a BitBox02 (firmware or bootloader). See `get_devices()` in
/// `bitbox02-firmware/py/bitbox02/bitbox02/communication/devices.py` and `is_bitbox02()` in
/// `bitbox-api-rs/src/usb.rs`.
pub fn is_bitbox02(info: &hidapi::DeviceInfo) -> Option<(Product, Mode)> {
    if info.vendor_id() != product::VENDOR_ID {
        return None;
    }
    let (product, mode) = Product::from_hid_product_string(info.product_string()?)?;
    let pid_ok = match mode {
        Mode::Firmware => info.product_id() == product::PRODUCT_ID,
        Mode::Bootloader => matches!(
            info.product_id(),
            product::PRODUCT_ID | product::PRODUCT_ID_DEV_BOOTLOADER
        ),
    };
    // The HWW endpoint is on interface 0 (interface 1 is U2F). The usage page check is for
    // platforms where the interface number is not available (macOS).
    let interface_ok = info.usage_page() == 0xffff || info.interface_number() == 0;
    (pid_ok && interface_ok).then_some((product, mode))
}

/// List the connected BitBox02 devices.
pub fn list_devices(api: &hidapi::HidApi) -> Vec<DeviceHandle> {
    let mut out: Vec<DeviceHandle> = api
        .device_list()
        .filter_map(|info| {
            let (product, mode) = is_bitbox02(info)?;
            let serial_number = info.serial_number().unwrap_or_default().to_string();
            Some(DeviceHandle {
                product,
                mode,
                version: Version::find_in(&serial_number),
                serial_number,
                path: info.path().to_owned(),
            })
        })
        .collect();
    // Some platforms list the same interface several times.
    out.dedup_by(|a, b| a.path == b.path);
    out
}

/// Find the single connected BitBox02.
pub fn find_device(api: &hidapi::HidApi) -> Result<DeviceHandle, Error> {
    let mut devices = list_devices(api);
    match devices.len() {
        0 => Err(Error::NoDevice),
        1 => Ok(devices.remove(0)),
        n => Err(Error::TooManyDevices(n)),
    }
}

/// Open a device in firmware mode.
pub fn open_firmware(api: &hidapi::HidApi, handle: &DeviceHandle) -> Result<FirmwareDevice, Error> {
    let dev = api.open_path(&handle.path)?;
    Ok(FirmwareDevice::new(dev)?)
}

/// Open a device in bootloader mode.
pub fn open_bootloader(api: &hidapi::HidApi, handle: &DeviceHandle) -> Result<Bootloader, Error> {
    let version = handle.version.ok_or_else(|| {
        Error::Bootloader(BootloaderError::UnexpectedResponse(format!(
            "could not parse the bootloader version from '{}'",
            handle.serial_number
        )))
    })?;
    let dev = api.open_path(&handle.path)?;
    Ok(Bootloader::new(dev, handle.product, version))
}

/// Information about a device in bootloader mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootloaderInfo {
    pub product: Product,
    pub bootloader_version: Version,
    /// Monotonic version of the installed firmware (not the X.Y.Z version).
    pub firmware_version: u32,
    pub signing_pubkeys_version: u32,
    /// No firmware installed.
    pub erased: bool,
    /// Firmware hash as computed by the bootloader.
    pub firmware_hash: [u8; 32],
    pub show_firmware_hash_on_boot: bool,
}

/// Information about a connected device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceStatus {
    Firmware(HwwInfo),
    Bootloader(BootloaderInfo),
}

impl DeviceStatus {
    pub fn product(&self) -> Option<Product> {
        match self {
            DeviceStatus::Firmware(i) => i.product,
            DeviceStatus::Bootloader(i) => Some(i.product),
        }
    }
}

fn bootloader_info(bl: &Bootloader) -> Result<BootloaderInfo, Error> {
    let (firmware_version, signing_pubkeys_version) = bl.versions()?;
    let (firmware_hash, _) = bl.get_hashes(false, false)?;
    Ok(BootloaderInfo {
        product: bl.product(),
        bootloader_version: bl.version(),
        firmware_version,
        signing_pubkeys_version,
        erased: firmware_hash
            == signed_firmware::empty_firmware_hash(bl.version(), bl.product(), firmware_version),
        firmware_hash,
        show_firmware_hash_on_boot: bl.show_firmware_hash_enabled()?,
    })
}

/// Get information about the connected device. Does not require pairing nor any confirmation.
pub fn get_status() -> Result<DeviceStatus, Error> {
    let api = hidapi::HidApi::new()?;
    let handle = find_device(&api)?;
    match handle.mode {
        Mode::Firmware => Ok(DeviceStatus::Firmware(
            open_firmware(&api, &handle)?.info().clone(),
        )),
        Mode::Bootloader => Ok(DeviceStatus::Bootloader(bootloader_info(
            &open_bootloader(&api, &handle)?,
        )?)),
    }
}

/// Get the detailed device information (name, secure chip, bootloader version, ...) of a device
/// in firmware mode. This requires unlocking the device and the encrypted channel: the user may
/// have to enter their password and confirm the pairing code (passed to `on_pairing_code`).
pub fn get_paired_device_info(
    noise_config: &dyn NoiseConfig,
    on_pairing_code: &mut dyn FnMut(&str),
) -> Result<hww::DeviceInfo, Error> {
    let api = hidapi::HidApi::new()?;
    let handle = find_device(&api)?;
    if handle.mode != Mode::Firmware {
        return Err(Error::Hww(HwwError::UnexpectedResponse(
            "the device is in bootloader mode".to_string(),
        )));
    }
    let mut paired =
        open_firmware(&api, &handle)?.unlock_and_pair(noise_config, on_pairing_code)?;
    Ok(paired.device_info()?)
}

/// Result of [`check_update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    pub status: DeviceStatus,
    pub latest: FirmwareRelease,
    /// Whether the latest release is newer than the installed firmware. In bootloader mode, the
    /// installed marketing version is unknown: this is `true` unless the installed monotonic
    /// version is known to be recent enough, which requires downloading the release (so it is
    /// only reported as `None` there).
    pub update_available: Option<bool>,
}

/// Check the device status and the latest firmware release available for it.
pub fn check_update() -> Result<UpdateCheck, Error> {
    let status = get_status()?;
    let product = status.product().ok_or(Error::UnknownProduct)?;
    let latest = releases::latest_release(product)?;
    let update_available = match &status {
        DeviceStatus::Firmware(info) => Some(latest.version > info.version),
        DeviceStatus::Bootloader(info) if info.erased => Some(true),
        DeviceStatus::Bootloader(_) => None,
    };
    Ok(UpdateCheck {
        status,
        latest,
        update_available,
    })
}

/// Check upfront that `firmware` can be flashed on a device (the bootloader also enforces these,
/// but this gives clearer errors and avoids erasing the device for nothing).
pub fn check_flashable(
    firmware: &SignedFirmware,
    device_product: Product,
    installed_firmware_version: u32,
    installed_signing_pubkeys_version: u32,
) -> Result<(), Error> {
    if firmware.product() != device_product {
        return Err(Error::WrongProduct {
            device: device_product,
            firmware: firmware.product(),
        });
    }
    if firmware.firmware_version() < installed_firmware_version {
        return Err(Error::Downgrade {
            installed: installed_firmware_version,
            firmware: firmware.firmware_version(),
        });
    }
    if firmware.signing_pubkeys_version() < installed_signing_pubkeys_version {
        return Err(Error::SigningKeysDowngrade {
            installed: installed_signing_pubkeys_version,
            firmware: firmware.signing_pubkeys_version(),
        });
    }
    Ok(())
}

/// Read and validate a signed firmware file.
pub fn read_firmware_file(path: impl AsRef<std::path::Path>) -> Result<SignedFirmware, Error> {
    let data = std::fs::read(path)?;
    Ok(SignedFirmware::parse(&data)?)
}

/// Progress of a firmware update, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Querying the GitHub releases.
    FetchingReleases,
    /// Downloading a firmware.
    Downloading {
        version: Version,
        intermediate: bool,
    },
    /// The user must enter their password on the device.
    WaitingForUnlock,
    /// The user must check that the device shows this pairing code and confirm it on the device.
    Pairing {
        code: String,
    },
    /// The user must confirm "Proceed to upgrade?" on the device.
    WaitingRebootConfirmation,
    /// Waiting for the device to appear in bootloader mode.
    WaitingForBootloader,
    /// About to install a firmware. `version` is `None` for a local file. `sighash` is the
    /// firmware hash the device shows on boot if enabled, and published in the release notes.
    Installing {
        version: Option<Version>,
        firmware_version: u32,
        sighash: [u8; 32],
        intermediate: bool,
    },
    Erasing,
    Flashing {
        done: usize,
        total: usize,
    },
    /// Checking the firmware hash reported by the bootloader.
    Verifying,
    /// Rebooting the device.
    Rebooting,
    /// An intermediate firmware is booting (it may upgrade the bootloader). The device will
    /// reappear in bootloader mode, or in firmware mode in which case the user must confirm the
    /// reboot again.
    WaitingForIntermediateBoot {
        version: Version,
    },
    Done,
}

/// Which firmware to install.
#[derive(Debug, Clone)]
pub enum FirmwareSource {
    /// The latest official release for the device's product, from GitHub.
    Latest,
    /// A local signed firmware (e.g. from [`read_firmware_file`]).
    File(SignedFirmware),
}

pub struct UpdateOptions {
    /// Where to persist the pairing (see [`noise_config::PersistedNoiseConfig`]).
    pub noise_config: Box<dyn NoiseConfig + Send>,
    /// If set, enable/disable showing the firmware hash on each boot before the final reboot.
    pub show_firmware_hash: Option<bool>,
    /// Reinstall even if the same firmware is already installed.
    pub force: bool,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        UpdateOptions {
            noise_config: Box::new(noise_config::NoiseConfigNoCache),
            show_firmware_hash: None,
            force: false,
        }
    }
}

/// Outcome of [`update_firmware`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// Firmware mode: the installed version is already the latest release (nothing flashed).
    AlreadyUpToDate { installed: Version, latest: Version },
    /// Bootloader: this exact firmware is already installed (nothing flashed, device rebooted).
    AlreadyInstalled {
        firmware_version: u32,
        sighash: [u8; 32],
    },
    Updated {
        product: Product,
        version: Option<Version>,
        firmware_version: u32,
        sighash: [u8; 32],
    },
}

/// The firmware to install, and its marketing version if known.
struct Target {
    firmware: SignedFirmware,
    version: Option<Version>,
}

fn download_latest(
    product: Product,
    progress: &mut dyn FnMut(Progress),
) -> Result<(FirmwareRelease, SignedFirmware), Error> {
    progress(Progress::FetchingReleases);
    let release = releases::latest_release(product)?;
    progress(Progress::Downloading {
        version: release.version,
        intermediate: false,
    });
    let firmware = releases::download(&release)?;
    Ok((release, firmware))
}

/// Update the firmware of the connected BitBox02 (in firmware or bootloader mode) to the given
/// source, performing required intermediate upgrades. Blocking; see the crate documentation.
///
/// In firmware mode, the user has to unlock the device, possibly confirm the pairing code, and
/// confirm the reboot into the bootloader.
pub fn update_firmware(
    source: FirmwareSource,
    options: &UpdateOptions,
    progress: &mut dyn FnMut(Progress),
) -> Result<UpdateOutcome, Error> {
    let mut api = hidapi::HidApi::new()?;
    let handle = find_device(&api)?;
    let product = handle.product;

    // Resolve and validate the target firmware before touching the device.
    let target = match source {
        FirmwareSource::File(firmware) => {
            if firmware.product() != product {
                return Err(Error::WrongProduct {
                    device: product,
                    firmware: firmware.product(),
                });
            }
            Target {
                firmware,
                version: None,
            }
        }
        FirmwareSource::Latest => {
            if handle.mode == Mode::Firmware && !options.force {
                // Avoid downloading if already up to date.
                let info = open_firmware(&api, &handle)?.info().clone();
                progress(Progress::FetchingReleases);
                let release = releases::latest_release(product)?;
                if release.version <= info.version {
                    return Ok(UpdateOutcome::AlreadyUpToDate {
                        installed: info.version,
                        latest: release.version,
                    });
                }
                progress(Progress::Downloading {
                    version: release.version,
                    intermediate: false,
                });
                Target {
                    firmware: releases::download(&release)?,
                    version: Some(release.version),
                }
            } else {
                let (release, firmware) = download_latest(product, progress)?;
                Target {
                    firmware,
                    version: Some(release.version),
                }
            }
        }
    };

    if handle.mode == Mode::Firmware {
        reboot_to_bootloader(&mut api, &handle, options, progress)?;
    }

    for _ in 0..MAX_UPGRADE_STEPS {
        let handle = wait_for_device(&mut api, product, Duration::ZERO)?;
        if handle.mode == Mode::Firmware {
            // An intermediate firmware booted instead of going back to the bootloader.
            reboot_to_bootloader(&mut api, &handle, options, progress)?;
            continue;
        }
        let bl = open_bootloader(&api, &handle)?;
        let (current, signing_pubkeys_version) = bl.versions()?;
        match releases::next_step(
            product,
            current,
            bl.version(),
            target.firmware.firmware_version(),
        ) {
            NextStep::BootIntermediate(i) => {
                log::info!("Booting the intermediate firmware v{}", i.version);
                progress(Progress::Rebooting);
                bl.reboot()?;
                progress(Progress::WaitingForIntermediateBoot { version: i.version });
                wait_for_reboot(&mut api, product)?;
            }
            NextStep::InstallIntermediate(i) => {
                progress(Progress::Downloading {
                    version: i.version,
                    intermediate: true,
                });
                let firmware = match i.download(product).map_err(Error::from).and_then(|fw| {
                    check_flashable(&fw, product, current, signing_pubkeys_version)?;
                    Ok(fw)
                }) {
                    Ok(fw) => fw,
                    Err(e) => return Err(reboot_on_error(bl, e)),
                };
                flash(&bl, &firmware, Some(i.version), true, progress)?;
                progress(Progress::Rebooting);
                bl.reboot()?;
                progress(Progress::WaitingForIntermediateBoot { version: i.version });
                wait_for_reboot(&mut api, product)?;
            }
            NextStep::InstallTarget => {
                if let Err(e) =
                    check_flashable(&target.firmware, product, current, signing_pubkeys_version)
                {
                    return Err(reboot_on_error(bl, e));
                }
                let target_hash = target.firmware.sighash(bl.sighash_scheme());
                if !options.force
                    && current == target.firmware.firmware_version()
                    && bl.get_hashes(false, false)?.0 == target_hash
                {
                    // Nothing to do, boot the installed firmware.
                    if let Some(show) = options.show_firmware_hash {
                        bl.set_show_firmware_hash(show)?;
                    }
                    progress(Progress::Rebooting);
                    bl.reboot()?;
                    progress(Progress::Done);
                    return Ok(UpdateOutcome::AlreadyInstalled {
                        firmware_version: current,
                        sighash: target_hash,
                    });
                }
                let sighash = flash(&bl, &target.firmware, target.version, false, progress)?;
                if let Some(show) = options.show_firmware_hash {
                    bl.set_show_firmware_hash(show)?;
                }
                progress(Progress::Rebooting);
                bl.reboot()?;
                progress(Progress::Done);
                return Ok(UpdateOutcome::Updated {
                    product,
                    version: target.version,
                    firmware_version: target.firmware.firmware_version(),
                    sighash,
                });
            }
        }
    }
    Err(Error::UpgradeStuck)
}

/// Before anything was written, reboot the device so it boots its (untouched) firmware again,
/// and return the error.
fn reboot_on_error(bl: Bootloader, e: Error) -> Error {
    if let Err(re) = bl.reboot() {
        log::warn!("Could not reboot the device after error: {}", re);
    }
    e
}

/// Flash a firmware and verify the hash reported by the bootloader. Returns the firmware hash.
fn flash(
    bl: &Bootloader,
    firmware: &SignedFirmware,
    version: Option<Version>,
    intermediate: bool,
    progress: &mut dyn FnMut(Progress),
) -> Result<[u8; 32], Error> {
    let expected = firmware.sighash(bl.sighash_scheme());
    progress(Progress::Installing {
        version,
        firmware_version: firmware.firmware_version(),
        sighash: expected,
        intermediate,
    });
    progress(Progress::Erasing);
    bl.flash_signed_firmware(firmware, &mut |done, total| {
        progress(Progress::Flashing { done, total })
    })?;
    progress(Progress::Verifying);
    let (got, _) = bl.get_hashes(false, false)?;
    if got != expected {
        // Be lenient about which hashing scheme the bootloader uses, the signatures were already
        // verified by the bootloader when writing the sigdata.
        let other = match bl.sighash_scheme() {
            SighashScheme::Legacy => SighashScheme::ProductId,
            SighashScheme::ProductId => SighashScheme::Legacy,
        };
        if got != firmware.sighash(other) {
            return Err(Error::HashMismatch { expected, got });
        }
        return Ok(got);
    }
    Ok(expected)
}

/// Pair with a device in firmware mode and make it reboot into its bootloader.
fn reboot_to_bootloader(
    api: &mut hidapi::HidApi,
    handle: &DeviceHandle,
    options: &UpdateOptions,
    progress: &mut dyn FnMut(Progress),
) -> Result<(), Error> {
    let device = open_firmware(api, handle)?;
    let info = device.info().clone();
    if let Some(p) = info.product {
        if p != handle.product {
            return Err(Error::WrongProduct {
                device: handle.product,
                firmware: p,
            });
        }
    }
    if info.initialized == Some(true) && !info.unlocked {
        progress(Progress::WaitingForUnlock);
    }
    let paired = device.unlock_and_pair(options.noise_config.as_ref(), &mut |code| {
        progress(Progress::Pairing {
            code: code.to_string(),
        })
    })?;
    progress(Progress::WaitingRebootConfirmation);
    paired.reboot_to_bootloader()?;
    progress(Progress::WaitingForBootloader);
    wait_for_disconnect(api, &handle.path, Duration::from_secs(10))?;
    let deadline = Instant::now() + WAIT_BOOTLOADER_TIMEOUT;
    loop {
        api.refresh_devices()?;
        if list_devices(api).iter().any(|d| {
            d.mode == Mode::Bootloader && d.product.platform() == handle.product.platform()
        }) {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err(Error::Timeout("waiting for the device in bootloader mode"));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Wait until the device at `path` is gone (best effort: returns Ok on timeout too, as some
/// platforms may keep the same path).
fn wait_for_disconnect(
    api: &mut hidapi::HidApi,
    path: &CString,
    timeout: Duration,
) -> Result<(), Error> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        api.refresh_devices()?;
        if !list_devices(api).iter().any(|d| &d.path == path) {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

/// Wait for the device to disconnect then reappear, after a reboot.
fn wait_for_reboot(api: &mut hidapi::HidApi, product: Product) -> Result<(), Error> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        api.refresh_devices()?;
        if !list_devices(api)
            .iter()
            .any(|d| d.product.platform() == product.platform())
        {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    wait_for_device(api, product, WAIT_REBOOT_TIMEOUT).map(|_| ())
}

/// Wait for a device of the same platform to be connected, in any mode.
fn wait_for_device(
    api: &mut hidapi::HidApi,
    product: Product,
    timeout: Duration,
) -> Result<DeviceHandle, Error> {
    let deadline = Instant::now() + timeout;
    loop {
        api.refresh_devices()?;
        let mut devices: Vec<DeviceHandle> = list_devices(api)
            .into_iter()
            .filter(|d| d.product.platform() == product.platform())
            .collect();
        match devices.len() {
            0 => {}
            1 => {
                let d = devices.remove(0);
                if d.product != product {
                    return Err(Error::WrongProduct {
                        device: d.product,
                        firmware: product,
                    });
                }
                return Ok(d);
            }
            n => return Err(Error::TooManyDevices(n)),
        }
        if Instant::now() >= deadline {
            return Err(if timeout.is_zero() {
                Error::NoDevice
            } else {
                Error::Timeout("waiting for the device to reconnect")
            });
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn fixture() -> SignedFirmware {
        let gz: &[u8] =
            include_bytes!("../tests/data/firmware-bitbox02-btconly.v9.26.2.signed.bin.gz");
        let mut bin = Vec::new();
        flate2::read::GzDecoder::new(gz)
            .read_to_end(&mut bin)
            .unwrap();
        SignedFirmware::parse(&bin).unwrap()
    }

    #[test]
    fn flashable() {
        let fw = fixture();
        // Version 50, signing keys version 3.
        assert!(check_flashable(&fw, Product::BitBox02BtcOnly, 0, 0).is_ok());
        assert!(check_flashable(&fw, Product::BitBox02BtcOnly, 50, 3).is_ok());
        assert!(matches!(
            check_flashable(&fw, Product::BitBox02Multi, 0, 0),
            Err(Error::WrongProduct {
                device: Product::BitBox02Multi,
                firmware: Product::BitBox02BtcOnly
            })
        ));
        assert!(matches!(
            check_flashable(&fw, Product::BitBox02NovaBtcOnly, 0, 0),
            Err(Error::WrongProduct { .. })
        ));
        assert!(matches!(
            check_flashable(&fw, Product::BitBox02BtcOnly, 55, 3),
            Err(Error::Downgrade {
                installed: 55,
                firmware: 50
            })
        ));
        assert!(matches!(
            check_flashable(&fw, Product::BitBox02BtcOnly, 50, 4),
            Err(Error::SigningKeysDowngrade { .. })
        ));
    }
}
