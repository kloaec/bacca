//! Update the firmware of BitBox02 devices without the BitBoxApp.
//!
//! On the BitBox02 there is no separate "Bitcoin app": the firmware edition (Multi or
//! Bitcoin-only) is fixed by the device's bootloader, and updating the firmware is updating the
//! "app". Supported: BitBox02 and BitBox02 Nova, Multi and Bitcoin-only editions, in firmware and
//! bootloader mode. The discontinued BitBox01 is not supported.
//!
//! The whole API is **blocking** (it does USB and HTTP I/O and waits for user confirmations on
//! the device, possibly for minutes). From an async GUI, run it with `spawn_blocking` and forward
//! the [`Progress`] events through a channel.
//!
//! Modules, bottom-up: `product` (identifiers), `u2fhid` (USB framing), `hww` (firmware mode:
//! pairing and reboot to the bootloader), `bootloader` (flashing), `signed_firmware` (format and
//! hashes), `releases` (GitHub releases and intermediate upgrades). This file ties them together.

mod bootloader;
mod hww;
mod product;
pub mod releases;
mod signed_firmware;
mod u2fhid;

use std::{
    ffi::CString,
    fmt,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use bootloader::Bootloader;
use hww::FirmwareDevice;
pub use hww::{default_config_dir, FirmwareInfo};
pub use product::{Edition, Mode, Platform, Product, Version};
use releases::{FirmwareRelease, IntermediateCompletion, NextStep};
pub use signed_firmware::SignedFirmware;
use signed_firmware::{empty_firmware_hash, SighashScheme};

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
    Http(minreq::Error),
    NoDevice,
    TooManyDevices(usize),
    /// An error returned by the firmware, e.g. the user rejected the upgrade on the device.
    Device(String),
    /// The firmware is not for this device's product (platform or edition).
    WrongProduct {
        device: Product,
        firmware: Product,
    },
    /// The firmware is older than the installed one (monotonic versions).
    Downgrade {
        installed: u32,
        firmware: u32,
    },
    /// The firmware's signing keys are older than the installed ones.
    SigningKeysDowngrade {
        installed: u32,
        firmware: u32,
    },
    /// The firmware isn't signed for the device's (old) bootloader.
    BootloaderTooOld {
        bootloader_version: Version,
        firmware_version: u32,
    },
    /// Any other error, with a message for the user.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hid(e) => write!(f, "HID error: {}", e),
            Self::Http(e) => write!(f, "HTTP error: {}", e),
            Self::NoDevice => write!(
                f,
                "no BitBox02 found. Is it connected (and not used by another app)?"
            ),
            Self::TooManyDevices(n) => {
                write!(f, "{} BitBox02 devices found, please connect only one", n)
            }
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
            Self::BootloaderTooOld {
                bootloader_version,
                firmware_version,
            } => write!(
                f,
                "this firmware (monotonic version {}) requires a bootloader v1.2.0 or later, the device has v{}. Install the intermediate firmware v9.26.2 first to upgrade the bootloader",
                firmware_version, bootloader_version
            ),
            Self::Device(s) | Self::Other(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for Error {}

impl From<hidapi::HidError> for Error {
    fn from(e: hidapi::HidError) -> Self {
        Error::Hid(e)
    }
}

impl From<minreq::Error> for Error {
    fn from(e: minreq::Error) -> Self {
        Error::Http(e)
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
    pub path: CString,
}

/// List the connected BitBox02 devices. See `get_devices()` in
/// `bitbox02-firmware/py/bitbox02/bitbox02/communication/devices.py` and `is_bitbox02()` in
/// `bitbox-api-rs/src/usb.rs`.
pub fn list_devices(api: &hidapi::HidApi) -> Vec<DeviceHandle> {
    let mut out: Vec<DeviceHandle> = api
        .device_list()
        .filter_map(|info| {
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
            (pid_ok && interface_ok).then(|| DeviceHandle {
                product,
                mode,
                version: Version::find_in(info.serial_number().unwrap_or_default()),
                path: info.path().to_owned(),
            })
        })
        .collect();
    // Some platforms list the same interface several times.
    out.dedup_by(|a, b| a.path == b.path);
    out
}

/// Find the single connected BitBox02.
fn find_device(api: &hidapi::HidApi) -> Result<DeviceHandle, Error> {
    let mut devices = list_devices(api);
    match devices.len() {
        0 => Err(Error::NoDevice),
        1 => Ok(devices.remove(0)),
        n => Err(Error::TooManyDevices(n)),
    }
}

fn open_firmware(api: &hidapi::HidApi, handle: &DeviceHandle) -> Result<FirmwareDevice, Error> {
    FirmwareDevice::open(api.open_path(&handle.path)?)
}

fn open_bootloader(api: &hidapi::HidApi, handle: &DeviceHandle) -> Result<Bootloader, Error> {
    let version = handle.version.ok_or_else(|| {
        Error::Other("could not parse the bootloader version from the USB serial number".into())
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
    Firmware(FirmwareInfo),
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

/// Get information about the connected device. Does not require pairing nor any confirmation.
pub fn get_status() -> Result<DeviceStatus, Error> {
    let api = hidapi::HidApi::new()?;
    let handle = find_device(&api)?;
    if handle.mode == Mode::Firmware {
        return Ok(DeviceStatus::Firmware(open_firmware(&api, &handle)?.info));
    }
    let bl = open_bootloader(&api, &handle)?;
    let (firmware_version, signing_pubkeys_version) = bl.versions()?;
    let firmware_hash = bl.firmware_hash()?;
    Ok(DeviceStatus::Bootloader(BootloaderInfo {
        product: bl.product,
        bootloader_version: bl.version,
        firmware_version,
        signing_pubkeys_version,
        erased: firmware_hash == empty_firmware_hash(bl.version, bl.product, firmware_version),
        firmware_hash,
        show_firmware_hash_on_boot: bl.show_firmware_hash_enabled()?,
    }))
}

/// Reboot a device in bootloader mode, which also clears its "start in bootloader mode" flag.
pub fn reboot_bootloader() -> Result<(), Error> {
    let api = hidapi::HidApi::new()?;
    let handle = find_device(&api)?;
    if handle.mode != Mode::Bootloader {
        return Err(Error::Other("the BitBox is not in bootloader mode".into()));
    }
    open_bootloader(&api, &handle)?.reboot()
}

/// Result of [`check_update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    pub status: DeviceStatus,
    pub latest: FirmwareRelease,
    /// Whether the latest release is newer than the installed firmware. `None` in bootloader mode
    /// with a firmware installed, as its X.Y.Z version is unknown.
    pub update_available: Option<bool>,
}

/// Check the device status and the latest firmware release available for it.
pub fn check_update() -> Result<UpdateCheck, Error> {
    let status = get_status()?;
    let product = status
        .product()
        .ok_or_else(|| Error::Other("unknown BitBox product".into()))?;
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

/// Check that `firmware` can be flashed on a device. The bootloader also enforces the product and
/// downgrade checks, but this gives clearer errors and avoids erasing the device for nothing.
fn check_flashable(
    firmware: &SignedFirmware,
    device_product: Product,
    installed_firmware_version: u32,
    installed_signing_pubkeys_version: u32,
    bootloader_version: Version,
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
    if !SighashScheme::bootloader_accepts(bootloader_version, firmware.firmware_version()) {
        return Err(Error::BootloaderTooOld {
            bootloader_version,
            firmware_version: firmware.firmware_version(),
        });
    }
    Ok(())
}

/// Progress of a firmware update, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    FetchingReleases,
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
    WaitingForBootloader,
    /// About to install a firmware. `sighash` is the firmware hash the device shows on boot if
    /// enabled, and published in the release notes.
    Installing {
        version: Version,
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
    Rebooting,
    /// An intermediate firmware is booting (it may upgrade the bootloader). The device will
    /// reappear in bootloader mode, or in firmware mode in which case the user must confirm the
    /// reboot again.
    WaitingForIntermediateBoot {
        version: Version,
    },
    Done,
}

#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    /// Where to persist the pairing (see [`default_config_dir`]). If `None`, the pairing code
    /// must be confirmed on every update.
    pub config_dir: Option<PathBuf>,
    /// If set, enable/disable showing the firmware hash on each boot before the final reboot.
    pub show_firmware_hash: Option<bool>,
    /// Reinstall even if the same firmware is already installed.
    pub force: bool,
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
        version: Version,
        firmware_version: u32,
        sighash: [u8; 32],
    },
}

/// Update the firmware of the connected BitBox02 (in firmware or bootloader mode) to the latest
/// release, performing the required intermediate upgrades.
///
/// In firmware mode, the user has to unlock the device, possibly confirm the pairing code, and
/// confirm the reboot into the bootloader.
pub fn update_firmware(
    options: &UpdateOptions,
    progress: &mut dyn FnMut(Progress),
) -> Result<UpdateOutcome, Error> {
    let mut api = hidapi::HidApi::new()?;
    let handle = find_device(&api)?;
    let product = handle.product;

    // Get and validate the target firmware before touching the device.
    let installed = if handle.mode == Mode::Firmware && !options.force {
        Some(open_firmware(&api, &handle)?.info.version)
    } else {
        None
    };
    progress(Progress::FetchingReleases);
    let release = releases::latest_release(product)?;
    if let Some(installed) = installed.filter(|v| release.version <= *v) {
        return Ok(UpdateOutcome::AlreadyUpToDate {
            installed,
            latest: release.version,
        });
    }
    progress(Progress::Downloading {
        version: release.version,
        intermediate: false,
    });
    let target = releases::download(&release)?;

    if handle.mode == Mode::Firmware {
        reboot_to_bootloader(&mut api, &handle, options, progress)?;
    }

    // Intermediates (by monotonic version) installed or booted during this upgrade.
    let mut booted: Vec<u32> = Vec::new();
    for _ in 0..MAX_UPGRADE_STEPS {
        let handle = wait_for_device(&mut api, product, Duration::ZERO)?;
        if handle.mode == Mode::Firmware {
            // An intermediate firmware booted instead of going back to the bootloader.
            reboot_to_bootloader(&mut api, &handle, options, progress)?;
            continue;
        }
        let bl = open_bootloader(&api, &handle)?;
        let (current, signing_pubkeys_version) = bl.versions()?;
        let installed_hash = bl.firmware_hash()?;
        let erased = installed_hash == empty_firmware_hash(bl.version, product, current);
        if erased {
            log::info!("No firmware installed.");
        }
        let bl_version = bl.version;
        let check = |fw: &SignedFirmware| {
            check_flashable(fw, product, current, signing_pubkeys_version, bl_version)
        };
        let target_version = target.firmware_version();
        let step = releases::next_step(product, current, bl.version, target_version, erased);
        let intermediate = match step {
            NextStep::InstallTarget => {
                if let Err(e) = check(&target) {
                    return Err(reboot_on_error(bl, e));
                }
                let target_hash = target.sighash(bl.sighash_scheme());
                let already_installed =
                    !options.force && current == target_version && installed_hash == target_hash;
                let sighash = if already_installed {
                    target_hash
                } else {
                    flash(&bl, &target, release.version, false, progress)?
                };
                if let Some(show) = options.show_firmware_hash {
                    bl.set_show_firmware_hash(show)?;
                }
                progress(Progress::Rebooting);
                bl.reboot()?;
                progress(Progress::Done);
                return Ok(if already_installed {
                    UpdateOutcome::AlreadyInstalled {
                        firmware_version: current,
                        sighash,
                    }
                } else {
                    UpdateOutcome::Updated {
                        product,
                        version: release.version,
                        firmware_version: target_version,
                        sighash,
                    }
                });
            }
            NextStep::InstallIntermediate(i) => {
                progress(Progress::Downloading {
                    version: i.version,
                    intermediate: true,
                });
                let firmware = match i.download(product).and_then(|fw| check(&fw).map(|_| fw)) {
                    Ok(fw) => fw,
                    Err(e) => return Err(reboot_on_error(bl, e)),
                };
                flash(&bl, &firmware, i.version, true, progress)?;
                i
            }
            NextStep::BootIntermediate(i) if booted.contains(&i.monotonic_version) => {
                // We already booted it, and the device is back in the bootloader without the
                // expected effect.
                let e = match i.completion {
                    // The bootloader upgrade was refused. It always is on a development
                    // bootloader ("Development bootloader" on the device's screen), see
                    // `bootloader_upgrade_install_or_reboot()` in
                    // bitbox02-firmware/src/bootloader_upgrade/firmware_installer.c. The releases
                    // after this intermediate can't be installed on the old bootloader (see
                    // `SighashScheme::bootloader_accepts`), so stop here.
                    IntermediateCompletion::BootloaderVersion(_) => format!(
                        "the intermediate firmware v{} did not upgrade the bootloader (still v{}). A development bootloader can't be upgraded: the firmware releases after v9.26.2 are only signed for bootloaders v1.2.0 and later, so they can't be installed on this device. Nothing else was written: unplug and replug the device",
                        i.version, bl.version
                    ),
                    IntermediateCompletion::MonotonicVersionBump => format!(
                        "the intermediate firmware v{} did not boot. If your BitBox shows 'DEV DEVICE' when it starts, slide <Continue> (bottom) to boot the firmware, then run the update again",
                        i.version
                    ),
                };
                return Err(reboot_on_error(bl, Error::Other(e)));
            }
            NextStep::BootIntermediate(i) => {
                log::info!("Booting the intermediate firmware v{}", i.version);
                i
            }
        };
        booted.push(intermediate.monotonic_version);
        progress(Progress::Rebooting);
        bl.reboot()?;
        progress(Progress::WaitingForIntermediateBoot {
            version: intermediate.version,
        });
        wait_for_reboot(&mut api, product)?;
    }
    Err(Error::Other("the upgrade did not complete".into()))
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
    version: Version,
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
    bl.flash(firmware, &mut |done, total| {
        progress(Progress::Flashing { done, total })
    })?;
    progress(Progress::Verifying);
    let got = bl.firmware_hash()?;
    // Be lenient about which hashing scheme the bootloader uses, the signatures were already
    // verified by the bootloader when writing the sigdata.
    if got != firmware.sighash(SighashScheme::Legacy)
        && got != firmware.sighash(SighashScheme::ProductId)
    {
        return Err(Error::Other(format!(
            "firmware hash mismatch after flashing: expected {}, device reports {}",
            hex::encode(expected),
            hex::encode(got)
        )));
    }
    Ok(got)
}

/// Pair with a device in firmware mode and make it reboot into its bootloader.
fn reboot_to_bootloader(
    api: &mut hidapi::HidApi,
    handle: &DeviceHandle,
    options: &UpdateOptions,
    progress: &mut dyn FnMut(Progress),
) -> Result<(), Error> {
    let device = open_firmware(api, handle)?;
    if let Some(p) = device.info.product.filter(|p| *p != handle.product) {
        return Err(Error::WrongProduct {
            device: handle.product,
            firmware: p,
        });
    }
    if device.info.initialized == Some(true) && !device.info.unlocked {
        progress(Progress::WaitingForUnlock);
    }
    let paired = device.unlock_and_pair(options.config_dir.as_deref(), &mut |code| {
        progress(Progress::Pairing {
            code: code.to_string(),
        })
    })?;
    progress(Progress::WaitingRebootConfirmation);
    paired.reboot_to_bootloader()?;
    progress(Progress::WaitingForBootloader);
    // Wait for the device to disconnect. Best effort: some platforms may keep the same path.
    poll_devices(api, Duration::from_secs(10), |devices| {
        Ok((!devices.iter().any(|d| d.path == handle.path)).then_some(()))
    })?;
    let platform = handle.product.platform();
    poll_devices(api, WAIT_BOOTLOADER_TIMEOUT, |devices| {
        Ok(devices
            .iter()
            .any(|d| d.mode == Mode::Bootloader && d.product.platform() == platform)
            .then_some(()))
    })?
    .ok_or_else(|| Error::Other("timeout waiting for the device in bootloader mode".into()))
}

/// Wait for the device to disconnect then reappear, after a reboot.
fn wait_for_reboot(api: &mut hidapi::HidApi, product: Product) -> Result<(), Error> {
    // Best effort, in case we miss the disconnection.
    poll_devices(api, Duration::from_secs(15), |devices| {
        Ok((!devices
            .iter()
            .any(|d| d.product.platform() == product.platform()))
        .then_some(()))
    })?;
    wait_for_device(api, product, WAIT_REBOOT_TIMEOUT).map(|_| ())
}

/// Wait for a device of the same platform to be connected, in any mode.
fn wait_for_device(
    api: &mut hidapi::HidApi,
    product: Product,
    timeout: Duration,
) -> Result<DeviceHandle, Error> {
    let found = poll_devices(api, timeout, |devices| {
        let mut devices: Vec<DeviceHandle> = devices
            .into_iter()
            .filter(|d| d.product.platform() == product.platform())
            .collect();
        match devices.len() {
            0 => Ok(None),
            1 if devices[0].product != product => Err(Error::WrongProduct {
                device: devices[0].product,
                firmware: product,
            }),
            1 => Ok(devices.pop()),
            n => Err(Error::TooManyDevices(n)),
        }
    })?;
    found.ok_or_else(|| {
        if timeout.is_zero() {
            Error::NoDevice
        } else {
            Error::Other("timeout waiting for the device to reconnect".into())
        }
    })
}

/// Call `f` on the connected devices until it returns something or `timeout` expires (it is
/// called at least once).
fn poll_devices<T>(
    api: &mut hidapi::HidApi,
    timeout: Duration,
    mut f: impl FnMut(Vec<DeviceHandle>) -> Result<Option<T>, Error>,
) -> Result<Option<T>, Error> {
    let deadline = Instant::now() + timeout;
    loop {
        api.refresh_devices()?;
        if let Some(t) = f(list_devices(api))? {
            return Ok(Some(t));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flashable() {
        let fw = SignedFirmware::parse(&signed_firmware::tests::fixture()).unwrap();
        let bl = Version::new(1, 2, 2);
        // Version 50, signing keys version 3.
        let check = |product, fw_version, keys_version, bl| {
            check_flashable(&fw, product, fw_version, keys_version, bl)
        };
        assert!(check(Product::BitBox02BtcOnly, 0, 0, bl).is_ok());
        assert!(check(Product::BitBox02BtcOnly, 50, 3, bl).is_ok());
        assert!(matches!(
            check(Product::BitBox02Multi, 0, 0, bl),
            Err(Error::WrongProduct {
                device: Product::BitBox02Multi,
                firmware: Product::BitBox02BtcOnly
            })
        ));
        assert!(matches!(
            check(Product::BitBox02NovaBtcOnly, 0, 0, bl),
            Err(Error::WrongProduct { .. })
        ));
        assert!(matches!(
            check(Product::BitBox02BtcOnly, 55, 3, bl),
            Err(Error::Downgrade {
                installed: 55,
                firmware: 50
            })
        ));
        assert!(matches!(
            check(Product::BitBox02BtcOnly, 50, 4, bl),
            Err(Error::SigningKeysDowngrade { .. })
        ));
        // v9.26.2 (the fixture) is still signed for the old bootloaders, later releases aren't.
        let old_bl = Version::new(1, 0, 5);
        assert!(check(Product::BitBox02BtcOnly, 36, 3, old_bl).is_ok());
        assert!(SighashScheme::bootloader_accepts(old_bl, 50));
        assert!(!SighashScheme::bootloader_accepts(old_bl, 51));
        assert!(!SighashScheme::bootloader_accepts(
            Version::new(1, 1, 9),
            55
        ));
        assert!(SighashScheme::bootloader_accepts(Version::new(1, 2, 0), 55));
    }
}
