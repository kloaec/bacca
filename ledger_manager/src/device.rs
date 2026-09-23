//! Talking to the device: connecting through HID, querying its information, quitting apps.

use crate::{
    error::{Error, StatusCode},
    model::{DeviceModel, LEDGER_USB_VENDOR_ID},
    version::coerced_at_least,
};

use ledger_apdu::APDUCommand;
use ledger_transport_hidapi::{hidapi::HidApi, TransportNativeHID};

use std::{str, thread, time};

// https://github.com/LedgerHQ/ledger-live/blob/dd1d17fd3ce7ed42558204b2f93707fb9b1599de/libs/device-core/src/commands/use-cases/getVersion.ts#L6
pub(crate) const GET_VERSION_COMMAND: APDUCommand<&[u8]> = APDUCommand {
    cla: 0xe0,
    ins: 0x01,
    p1: 0x00,
    p2: 0x00,
    data: &[],
};

// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/getAppAndVersion.ts
const GET_APP_AND_VERSION_COMMAND: APDUCommand<&[u8]> = APDUCommand {
    cla: 0xb0,
    ins: 0x01,
    p1: 0x00,
    p2: 0x00,
    data: &[],
};

// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/commands/quitApp.ts
const QUIT_APP_COMMAND: APDUCommand<&[u8]> = APDUCommand {
    cla: 0xb0,
    ins: 0xa7,
    p1: 0x00,
    p2: 0x00,
    data: &[],
};

/// The USB usage page of the Ledger HID interface.
const LEDGER_USAGE_PAGE: u16 = 0xffa0;

// Flags of the first byte of the GetVersion flags. See
// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/getDeviceInfo.ts
const MANAGER_ALLOWED_FLAG: u8 = 0x08;
const PIN_VALIDATED_FLAG: u8 = 0x80;
const RECOVERY_MODE_FLAG: u8 = 0x01;
const ONBOARDED_FLAG: u8 = 0x04;

/// The providers of the Ledger API, by name. A firmware version may be suffixed with the name of
/// its provider (e.g. "1.2.3-das").
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/use-cases/getProviderIdUseCase.ts
const PROVIDERS: &[(&str, u32)] = &[
    ("default", 1),
    ("das", 2),
    ("club", 3),
    ("shitcoins", 4),
    ("ee", 5),
];

/// Information about a Ledger device connected through HID, before opening it.
#[derive(Debug, Clone)]
pub struct LedgerHidDevice {
    pub product_id: u16,
    pub model: Option<DeviceModel>,
    pub path: String,
}

// Ledger Live's node-hid transport filters by usage page on Windows and macOS, and by interface
// number on Linux (where the usage page might not be reported by some hidapi backends).
// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledgerjs/packages/hw-transport-node-hid-noevents/src/TransportNodeHid.ts
// (now published from https://github.com/LedgerHQ/ts-libs).
// ledger-transport-hidapi only filters on the vendor id and usage page. The vendor id is the same
// for all Ledger models (and in bootloader mode), so newer models are picked up too.
fn is_ledger_usage_page(dev: &ledger_transport_hidapi::hidapi::DeviceInfo) -> bool {
    dev.vendor_id() == LEDGER_USB_VENDOR_ID && dev.usage_page() == LEDGER_USAGE_PAGE
}

fn is_ledger_interface(dev: &ledger_transport_hidapi::hidapi::DeviceInfo) -> bool {
    dev.vendor_id() == LEDGER_USB_VENDOR_ID && dev.usage_page() == 0 && dev.interface_number() == 0
}

fn find_ledger(hid_api: &HidApi) -> Option<&ledger_transport_hidapi::hidapi::DeviceInfo> {
    hid_api
        .device_list()
        .find(|d| is_ledger_usage_page(d))
        .or_else(|| hid_api.device_list().find(|d| is_ledger_interface(d)))
}

/// List the Ledger devices currently known to the HID API. Call `HidApi::refresh_devices` first
/// to get an up to date list.
pub fn list_ledger_devices(hid_api: &HidApi) -> Vec<LedgerHidDevice> {
    let mut devices: Vec<_> = hid_api
        .device_list()
        .filter(|d| is_ledger_usage_page(d))
        .collect();
    if devices.is_empty() {
        devices = hid_api
            .device_list()
            .filter(|d| is_ledger_interface(d))
            .collect();
    }
    devices
        .into_iter()
        .map(|d| LedgerHidDevice {
            product_id: d.product_id(),
            model: DeviceModel::from_usb_product_id(d.product_id()),
            path: d.path().to_string_lossy().into_owned(),
        })
        .collect()
}

/// Open a HID transport to the first Ledger device found. Also returns the model of the device
/// as detected from its USB product id, if known.
///
/// Note this does not refresh the list of devices of the HID API. Call
/// `HidApi::refresh_devices` beforehand if needed.
pub fn open_device(hid_api: &HidApi) -> Result<(TransportNativeHID, Option<DeviceModel>), Error> {
    let dev = find_ledger(hid_api).ok_or(Error::DeviceNotFound)?;
    let model = DeviceModel::from_usb_product_id(dev.product_id());
    log::debug!(
        "Opening Ledger device with product id {:#06x} (model: {:?}).",
        dev.product_id(),
        model
    );
    let transport = TransportNativeHID::open_device(hid_api, dev)?;
    Ok((transport, model))
}

/// Refresh the list of HID devices and open a transport to the first Ledger found.
pub fn connect(hid_api: &mut HidApi) -> Result<TransportNativeHID, Error> {
    hid_api
        .refresh_devices()
        .map_err(|e| Error::Hid(ledger_transport_hidapi::LedgerHIDError::Hid(e)))?;
    Ok(open_device(hid_api)?.0)
}

/// Information queried from a Ledger device, through the GetVersion APDU.
///
/// This merges Ledger Live's `FirmwareInfo`
/// (https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/commands/entities/FirmwareInfoEntity.ts)
/// and `DeviceInfo`
/// (https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/entities/DeviceInfoEntity.ts).
// NOTE: MCU target id is always == target_id in Ledger Live
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The target id. The SE target id when running the OS, the MCU target id in bootloader mode.
    pub target_id: u32,
    /// The firmware version, without the "-osu" suffix. This is what the Ledger API expects.
    /// In bootloader mode, this is the version of the bootloader.
    pub version: String,
    /// The raw firmware version as returned by the device (may be suffixed with "-osu").
    pub raw_version: String,
    /// The SE flags.
    pub flags: Vec<u8>,
    /// Whether the device is in bootloader mode.
    pub is_bootloader: bool,
    /// Whether the device is running the OS updater (in the middle of a firmware update).
    pub is_osu: bool,
    /// The version of the Secure Element firmware.
    pub se_version: Option<String>,
    /// The target id of the Secure Element.
    pub se_target_id: Option<u32>,
    /// The MCU version. `None` in bootloader mode.
    pub mcu_version: Option<String>,
    /// The MCU bootloader version (in bootloader mode only).
    pub mcu_bl_version: Option<String>,
    /// The MCU target id (in bootloader mode only).
    pub mcu_target_id: Option<u32>,
    /// The "x.y" (or "x.y.z") part of the version.
    pub maj_min: String,
    /// The provider name, if the version is suffixed with a known provider.
    pub provider_name: Option<String>,
    pub manager_allowed: bool,
    pub pin_validated: bool,
    pub is_recovery_mode: bool,
    pub onboarded: bool,
    /// Whether this looks like a development firmware.
    pub has_dev_firmware: bool,
    pub bootloader_version: Option<String>,
    pub hardware_version: Option<u8>,
    pub language_id: Option<u8>,
    pub recover_state: Option<Vec<u8>>,
    pub charon_state: Option<Vec<u8>>,
    /// The device model, as detected from the target id.
    pub model: Option<DeviceModel>,
}

/// A cursor over the GetVersion response which never panics.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn has_more(&self) -> bool {
        self.pos < self.data.len()
    }

    fn u8(&mut self, what: &str) -> Result<u8, Error> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or_else(|| Error::InvalidDeviceData(format!("missing {}", what)))?;
        self.pos += 1;
        Ok(b)
    }

    fn bytes(&mut self, len: usize, what: &str) -> Result<&'a [u8], Error> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| Error::InvalidDeviceData(format!("not enough data for {}", what)))?;
        let b = &self.data[self.pos..end];
        self.pos = end;
        Ok(b)
    }

    /// Read a length-prefixed field.
    fn lv(&mut self, what: &str) -> Result<&'a [u8], Error> {
        let len = self.u8(what)? as usize;
        self.bytes(len, what)
    }
}

fn be_u32(b: &[u8], what: &str) -> Result<u32, Error> {
    if b.is_empty() || b.len() > 4 {
        return Err(Error::InvalidDeviceData(format!(
            "invalid {} length {}",
            what,
            b.len()
        )));
    }
    Ok(b.iter().fold(0u32, |acc, x| (acc << 8) | *x as u32))
}

fn string(b: &[u8], what: &str) -> Result<String, Error> {
    str::from_utf8(b)
        .map(|s| s.to_string())
        .map_err(|_| Error::InvalidDeviceData(format!("{} is not valid UTF-8", what)))
}

fn strip_trailing_nul(b: &[u8]) -> &[u8] {
    match b.last() {
        Some(0) => &b[..b.len() - 1],
        _ => b,
    }
}

// Version ranges from which the GetVersion response contains a given field. See
// https://github.com/LedgerHQ/ledger-live/tree/develop/libs/device-core/src/commands/use-cases
// (isBootloaderVersionSupported.ts, isHardwareVersionSupported.ts,
// isDeviceLocalizationSupported.ts, isRecoverSupported.ts, isCharonSupported.ts).
fn supports(
    se_version: &str,
    model: Option<DeviceModel>,
    ranges: &[(DeviceModel, (u64, u64, u64))],
) -> bool {
    model
        .and_then(|m| ranges.iter().find(|(rm, _)| *rm == m))
        .map(|(_, min)| coerced_at_least(se_version, *min))
        .unwrap_or(false)
}

const BOOTLOADER_VERSION_RANGES: &[(DeviceModel, (u64, u64, u64))] = &[
    (DeviceModel::NanoS, (2, 0, 0)),
    (DeviceModel::NanoX, (2, 0, 0)),
    (DeviceModel::NanoSPlus, (1, 0, 0)),
    (DeviceModel::Stax, (1, 0, 0)),
    (DeviceModel::Flex, (0, 0, 0)),
    (DeviceModel::NanoGen5, (0, 0, 0)),
];
const HARDWARE_VERSION_RANGES: &[(DeviceModel, (u64, u64, u64))] =
    &[(DeviceModel::NanoX, (2, 0, 0))];
const LOCALIZATION_RANGES: &[(DeviceModel, (u64, u64, u64))] = &[
    (DeviceModel::NanoX, (2, 1, 0)),
    (DeviceModel::NanoSPlus, (1, 1, 0)),
    (DeviceModel::Stax, (1, 0, 0)),
    (DeviceModel::Flex, (0, 0, 0)),
    (DeviceModel::NanoGen5, (0, 0, 0)),
];
const RECOVER_RANGES: &[(DeviceModel, (u64, u64, u64))] = LOCALIZATION_RANGES;
const CHARON_RANGES: &[(DeviceModel, (u64, u64, u64))] = &[
    (DeviceModel::Stax, (1, 7, 0)),
    (DeviceModel::Flex, (1, 3, 0)),
    (DeviceModel::NanoGen5, (0, 0, 0)),
];

/// Whether the device supports changing its language, for this firmware version.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/commands/use-cases/isDeviceLocalizationSupported.ts
pub fn is_device_localization_supported(se_version: &str, model: Option<DeviceModel>) -> bool {
    supports(se_version, model, LOCALIZATION_RANGES)
}

/// Whether this is a development firmware.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/isDevFirmware.ts
pub fn is_dev_firmware(se_version: Option<&str>) -> bool {
    match se_version {
        Some(v) => ["lo", "rc", "il", "tr"]
            .iter()
            .any(|suffix| v.contains(&format!("-{}", suffix))),
        None => false,
    }
}

/// Extract the "maj.min(.patch)" part and the part after the dash of a raw version, like the
/// `/([0-9]+.[0-9]+(.[0-9]+){0,1})?(-(.*))?/` regex in getDeviceInfo.ts. Note that in this regex
/// the dots match any character, and that it is not anchored at the end.
fn parse_maj_min(raw: &str) -> (String, Option<String>) {
    let b = raw.as_bytes();
    let digits = |mut i: usize| -> usize {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        i
    };

    // The regex is anchored at the start (String.match returns the first match, and the
    // optional group means it always matches at index 0).
    let mut maj_min_end = 0;
    let e1 = digits(0);
    if e1 > 0 && e1 < b.len() {
        // "any char" then digits
        let s2 = e1 + 1;
        let e2 = digits(s2);
        if e2 > s2 {
            maj_min_end = e2;
            if e2 < b.len() {
                let s3 = e2 + 1;
                let e3 = digits(s3);
                if e3 > s3 {
                    maj_min_end = e3;
                }
            }
        }
    }
    let maj_min = raw.get(..maj_min_end).unwrap_or("").to_string();
    let post_dash = raw
        .get(maj_min_end..)
        .and_then(|rest| rest.strip_prefix('-'))
        .map(|s| s.to_string());
    (maj_min, post_dash)
}

impl DeviceInfo {
    /// Query information about this device.
    ///
    /// Adapted from https://github.com/LedgerHQ/ledger-live/blob/dd1d17fd3ce7ed42558204b2f93707fb9b1599de/libs/device-core/src/commands/use-cases/parseGetVersionResponse.ts
    /// and https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/getDeviceInfo.ts
    pub fn new(ledger_api: &TransportNativeHID) -> Result<Self, Error> {
        let ver_answer = ledger_api.exchange(&GET_VERSION_COMMAND)?;
        let ret = ver_answer.retcode();
        if ret == StatusCode::LockedDevice as u16 {
            return Err(Error::DeviceLocked);
        } else if ret == StatusCode::DeviceNotOnboardedLegacy as u16
            || ret == StatusCode::DeviceNotOnboarded as u16
        {
            return Err(Error::DeviceNotOnboarded);
        } else if ret == StatusCode::ClaNotSupported as u16
            || ret == StatusCode::InsNotSupported as u16
        {
            // The GetVersion APDU is handled by the dashboard, not by applications.
            return Err(Error::DeviceOnDashboardExpected);
        } else if ret != StatusCode::OK as u16 {
            return Err(Error::DeviceStatus(ret));
        }

        let info = Self::from_get_version_response(ver_answer.data())?;
        log::debug!("deviceInfo: {}", info.firmware_summary());
        Ok(info)
    }

    /// Parse the data of a response to the GetVersion APDU (without the trailing status code).
    ///
    /// Ported from https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/commands/use-cases/parseGetVersionResponse.ts
    /// and https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/getDeviceInfo.ts
    pub fn from_get_version_response(data: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(data);

        // The target id of either the BL or the SE.
        let target_id = be_u32(r.bytes(4, "target id")?, "target id")?;

        // The version of either the BL or the SE.
        let raw_version_bytes = r.lv("version")?;
        let mut raw_version = string(raw_version_bytes, "version")?;

        // The flags. Gives information about manager allowed in SE mode.
        let flags_len = r.u8("flags length")? as usize;
        let mut flags = r.bytes(flags_len, "flags")?.to_vec();

        if raw_version_bytes.is_empty() {
            // To support old firmware like bootloader of 1.3.1
            raw_version = "0.0.0".to_string();
            flags = Vec::new();
        }

        let mut mcu_version = None;
        let mut mcu_bl_version = None;
        let mut mcu_target_id = None;
        let mut se_version = None;
        let mut se_target_id = None;
        let mut bootloader_version = None;
        let mut hardware_version = None;
        let mut language_id = None;
        let mut recover_state = None;
        let mut charon_state = None;

        let is_bootloader = (target_id & 0xf000_0000) != 0x3000_0000;
        let is_osu = raw_version.contains("-osu");
        let model;

        if is_bootloader {
            mcu_bl_version = Some(raw_version.clone());
            mcu_target_id = Some(target_id);

            if r.has_more() {
                // SE part 1
                let part1 = r.lv("SE part 1")?;
                // At this time, this is how we branch old & new format.
                if part1.len() >= 5 {
                    se_version = Some(string(part1, "SE version")?);
                    // SE part 2
                    let part2 = r.lv("SE target id")?;
                    se_target_id = Some(be_u32(part2, "SE target id")?);
                } else {
                    se_target_id = Some(be_u32(part1, "SE target id")?);
                }
            }
            model = se_target_id.and_then(DeviceModel::from_target_id);
        } else {
            se_version = Some(raw_version.clone());
            se_target_id = Some(target_id);
            model = DeviceModel::from_target_id(target_id);

            // If SE: the MCU version. Ledger Live tolerates missing trailing fields (it would
            // read empty buffers), so do we: a missing field is left unset.
            if r.has_more() {
                let mcu = strip_trailing_nul(r.lv("MCU version")?);
                mcu_version = Some(string(mcu, "MCU version")?);
            } else {
                log::warn!("GetVersion response without MCU version.");
            }

            if !is_osu {
                if supports(&raw_version, model, BOOTLOADER_VERSION_RANGES) && r.has_more() {
                    let bl = strip_trailing_nul(r.lv("bootloader version")?);
                    bootloader_version = Some(string(bl, "bootloader version")?);
                }
                if supports(&raw_version, model, HARDWARE_VERSION_RANGES) && r.has_more() {
                    hardware_version = r.lv("hardware version")?.first().copied();
                }
                if supports(&raw_version, model, LOCALIZATION_RANGES) && r.has_more() {
                    language_id = r.lv("language id")?.first().copied();
                }
                if supports(&raw_version, model, RECOVER_RANGES) && r.has_more() {
                    recover_state = Some(r.lv("recover state")?.to_vec());
                }
                if supports(&raw_version, model, CHARON_RANGES) && r.has_more() {
                    charon_state = Some(r.lv("charon state")?.to_vec());
                }
            }
        }

        let version = raw_version.replacen("-osu", "", 1);
        let (maj_min, post_dash) = parse_maj_min(&raw_version);
        let provider_name = post_dash.filter(|p| PROVIDERS.iter().any(|(name, _)| name == p));
        let flag = flags.first().copied().unwrap_or(0);
        let manager_allowed = flag & MANAGER_ALLOWED_FLAG != 0;
        let pin_validated = flag & PIN_VALIDATED_FLAG != 0;
        let (is_recovery_mode, onboarded) = if flags.len() == 4 {
            // Nb Since LNS+ unseeded devices are visible + extra flags
            (flag & RECOVERY_MODE_FLAG != 0, flag & ONBOARDED_FLAG != 0)
        } else {
            (false, true)
        };
        let has_dev_firmware = is_dev_firmware(se_version.as_deref());

        Ok(Self {
            target_id,
            version,
            raw_version,
            flags,
            is_bootloader,
            is_osu,
            se_version,
            se_target_id,
            mcu_version,
            mcu_bl_version,
            mcu_target_id,
            maj_min,
            provider_name,
            manager_allowed,
            pin_validated,
            is_recovery_mode,
            onboarded,
            has_dev_firmware,
            bootloader_version,
            hardware_version,
            language_id,
            recover_state,
            charon_state,
            model,
        })
    }

    /// The model of this device, if known.
    pub fn model(&self) -> Option<DeviceModel> {
        self.model
    }

    /// The Ledger API provider id to use for this device.
    /// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/use-cases/getProviderIdUseCase.ts
    pub fn provider_id(&self) -> u32 {
        self.provider_name
            .as_deref()
            .and_then(|name| PROVIDERS.iter().find(|(n, _)| *n == name))
            .map(|(_, id)| *id)
            .unwrap_or(crate::PROVIDER)
    }

    /// The version of the Secure Element firmware (the "OS version"), if known.
    pub fn se_version_str(&self) -> Option<&str> {
        self.se_version.as_deref()
    }

    /// The version of the MCU firmware, if known.
    pub fn mcu_version_str(&self) -> Option<&str> {
        self.mcu_version.as_deref().filter(|v| !v.is_empty())
    }

    /// The running mode of the device as a human-readable string.
    pub fn mode(&self) -> &'static str {
        if self.is_osu {
            "updater (OSU)"
        } else if self.is_bootloader {
            "bootloader"
        } else if self.is_recovery_mode {
            "recovery"
        } else {
            "normal"
        }
    }

    /// A short human-readable summary of the firmware versions, e.g. "se@2.2.3 mcu@2.30" or
    /// "se@2.2.3 mcu@2.30 (osu)". Mirrors the log line of Ledger Live's getDeviceInfo.
    pub fn firmware_summary(&self) -> String {
        let mut s = if self.is_bootloader {
            format!("bootloader@{}", self.version)
        } else {
            format!("se@{}", self.version)
        };
        if let Some(mcu) = self.mcu_version_str() {
            s.push_str(&format!(" mcu@{}", mcu));
        }
        if self.is_bootloader {
            if let Some(se) = &self.se_version {
                s.push_str(&format!(" se@{}", se));
            }
        }
        if let Some(bl) = &self.bootloader_version {
            s.push_str(&format!(" bl@{}", bl));
        }
        if self.is_osu {
            s.push_str(" (osu)");
        } else if self.is_bootloader {
            s.push_str(" (bootloader)");
        }
        s
    }

    /// Whether the device runs its OS normally (neither in bootloader nor updater mode).
    pub fn is_normal_mode(&self) -> bool {
        !self.is_bootloader && !self.is_osu
    }
}

/// The name and version of the application currently running on the device ("BOLOS" when on
/// the dashboard).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppAndVersion {
    pub name: String,
    pub version: String,
    pub flags: Vec<u8>,
}

impl AppAndVersion {
    /// Parse the response to the GetAppAndVersion APDU (without the status code).
    /// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/getAppAndVersion.ts
    pub fn from_response(data: &[u8]) -> Result<Self, Error> {
        let mut r = Reader::new(data);
        let format = r.u8("format")?;
        if format != 1 {
            return Err(Error::InvalidDeviceData(format!(
                "getAppAndVersion: format {} not supported",
                format
            )));
        }
        let name = String::from_utf8_lossy(r.lv("app name")?).into_owned();
        let version = String::from_utf8_lossy(r.lv("app version")?).into_owned();
        let flags = if r.has_more() {
            r.lv("app flags")?.to_vec()
        } else {
            Vec::new()
        };
        Ok(Self {
            name,
            version,
            flags,
        })
    }

    /// Whether this is the dashboard.
    /// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/isDashboardName.ts
    pub fn is_dashboard(&self) -> bool {
        self.name == "BOLOS" || self.name == "OLOS\u{0}"
    }
}

/// Get the name and version of the currently running application.
pub fn get_app_and_version(ledger_api: &TransportNativeHID) -> Result<AppAndVersion, Error> {
    let resp = ledger_api.exchange(&GET_APP_AND_VERSION_COMMAND)?;
    match resp.retcode() {
        r if r == StatusCode::OK as u16 => AppAndVersion::from_response(resp.data()),
        r if r == StatusCode::LockedDevice as u16 => Err(Error::DeviceLocked),
        r => Err(Error::DeviceStatus(r)),
    }
}

/// Quit the currently opened application, if any, to go back to the dashboard.
///
/// Like https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/commands/quitApp.ts
/// this does nothing if already on the dashboard. Errors of the GetAppAndVersion APDU because of
/// an unsupported CLA are ignored (they happen in bootloader mode or on old firmwares).
pub fn quit_app(ledger_api: &TransportNativeHID) -> Result<(), Error> {
    match get_app_and_version(ledger_api) {
        Ok(app) if app.is_dashboard() => return Ok(()),
        Ok(app) => log::info!("Quitting app '{}' {}.", app.name, app.version),
        Err(Error::DeviceStatus(s))
            if s == StatusCode::ClaNotSupported as u16
                || s == StatusCode::ClaNotSupportedBootloader as u16 =>
        {
            return Ok(())
        }
        Err(e) => return Err(e),
    }
    let resp = ledger_api.exchange(&QUIT_APP_COMMAND)?;
    if resp.retcode() == StatusCode::LockedDevice as u16 {
        return Err(Error::DeviceLocked);
    }
    if resp.retcode() != StatusCode::OK as u16 {
        return Err(Error::DeviceStatus(resp.retcode()));
    }
    // Give some time to the device to get back to the dashboard.
    thread::sleep(time::Duration::from_millis(500));
    Ok(())
}

/// Poll the device until `accept` returns true for its information, reconnecting to it every
/// `interval` (as it may reboot in the meantime). Any error when connecting to or querying the
/// device is ignored, as in Ledger Live's `withDevicePolling` with an accept-all-errors
/// predicate (https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/deviceAccess.ts).
///
/// `on_error` is called with each error encountered while polling (useful to tell the user to
/// unlock the device for instance).
pub fn wait_for_device<F, E>(
    hid_api: &mut HidApi,
    timeout: time::Duration,
    interval: time::Duration,
    mut accept: F,
    mut on_error: E,
) -> Result<DeviceInfo, Error>
where
    F: FnMut(&DeviceInfo) -> bool,
    E: FnMut(&Error),
{
    let start = time::Instant::now();
    loop {
        match connect(hid_api).and_then(|t| DeviceInfo::new(&t)) {
            Ok(info) => {
                if accept(&info) {
                    return Ok(info);
                }
                log::debug!("Device info not accepted yet: {}", info.firmware_summary());
            }
            Err(e) => {
                log::debug!("Error while polling the device: {}", e);
                on_error(&e);
            }
        }
        if start.elapsed() >= timeout {
            return Err(Error::Timeout("waiting for the device"));
        }
        thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &[&str]) -> Vec<u8> {
        hex::decode(s.join("")).unwrap()
    }

    // Fixture from ledger-live's libs/device-core/src/commands/use-cases/parseGetVersionResponse.test.ts
    // (the trailing status code is stripped).
    #[test]
    fn parse_nano_x() {
        let data = h(&[
            "33000004",   // targetId
            "05",         // device version length
            "322e322e33", // device version aka `rawVersion`
            "04",         // flags length
            "ee000000",   // flags
            "04",         // mcu version length
            "322e3330",   // mcu version aka `mcuVersion`
            "04",         // booloader version length
            "312e3136",   // bootloader version aka `bootloaderVersion`
            "01",         // hw version length
            "01",         // hw version aka `hardwareVersion`
            "01",         // language id length
            "00",         // language id
            "01",         // recoverState length
            "00",         // recoverState
        ]);
        let info = DeviceInfo::from_get_version_response(&data).unwrap();
        assert!(!info.is_bootloader);
        assert!(!info.is_osu);
        assert_eq!(info.target_id, 855638020);
        assert_eq!(info.raw_version, "2.2.3");
        assert_eq!(info.version, "2.2.3");
        assert_eq!(info.se_version.as_deref(), Some("2.2.3"));
        assert_eq!(info.se_target_id, Some(855638020));
        assert_eq!(info.mcu_version.as_deref(), Some("2.30"));
        assert_eq!(info.mcu_bl_version, None);
        assert_eq!(info.mcu_target_id, None);
        assert_eq!(info.flags, vec![238, 0, 0, 0]);
        assert_eq!(info.bootloader_version.as_deref(), Some("1.16"));
        assert_eq!(info.hardware_version, Some(1));
        assert_eq!(info.language_id, Some(0));
        assert_eq!(info.recover_state, Some(vec![0]));
        assert_eq!(info.charon_state, None);
        assert_eq!(info.model, Some(DeviceModel::NanoX));
        assert_eq!(info.maj_min, "2.2.3");
        // 0xee = 0b1110_1110
        assert!(info.manager_allowed);
        assert!(info.pin_validated);
        assert!(info.onboarded);
        assert!(!info.is_recovery_mode);
        assert!(!info.has_dev_firmware);
        assert_eq!(info.provider_id(), 1);
        assert_eq!(info.firmware_summary(), "se@2.2.3 mcu@2.30 bl@1.16");
    }

    #[test]
    fn parse_old_firmware_zero_version_length() {
        let data = h(&[
            "33000004", // targetId
            "00",       // device version length
            "04",       // flags length
            "ee000000", // flags
            "04",       // mcu version length
            "322e3330", // mcu version aka `mcuVersion`
            "01", "01", "01", "00", "01", "00",
        ]);
        let info = DeviceInfo::from_get_version_response(&data).unwrap();
        assert!(!info.is_bootloader);
        assert_eq!(info.raw_version, "0.0.0");
        assert_eq!(info.se_version.as_deref(), Some("0.0.0"));
        assert_eq!(info.mcu_version.as_deref(), Some("2.30"));
        assert!(info.flags.is_empty());
        assert_eq!(info.bootloader_version, None);
        assert_eq!(info.hardware_version, None);
        assert_eq!(info.language_id, None);
        assert_eq!(info.recover_state, None);
        assert!(!info.manager_allowed);
        assert!(info.onboarded);
    }

    #[test]
    fn parse_bootloader() {
        let data = h(&[
            "05010003",   // targetId, also mcuTargetId
            "04",         // rawVersion length
            "312e3136",   // rawVersion, also mcuBlVersion
            "04",         // flags length
            "f4d8aa43",   // flags
            "05",         // seVersion length
            "322e322e33", // seVersion
            "04",         // seTargetId length
            "33000004",   // seTargetId
        ]);
        let info = DeviceInfo::from_get_version_response(&data).unwrap();
        assert!(info.is_bootloader);
        assert!(!info.is_osu);
        assert_eq!(info.raw_version, "1.16");
        assert_eq!(info.version, "1.16");
        assert_eq!(info.maj_min, "1.16");
        assert_eq!(info.target_id, 83951619);
        assert_eq!(info.se_version.as_deref(), Some("2.2.3"));
        assert_eq!(info.mcu_version, None);
        assert_eq!(info.mcu_bl_version.as_deref(), Some("1.16"));
        assert_eq!(info.mcu_target_id, Some(83951619));
        assert_eq!(info.se_target_id, Some(855638020));
        assert_eq!(info.flags, vec![244, 216, 170, 67]);
        assert_eq!(info.bootloader_version, None);
        // Model is detected from the SE target id in bootloader mode.
        assert_eq!(info.model, Some(DeviceModel::NanoX));
        assert_eq!(info.mode(), "bootloader");
        assert_eq!(
            info.firmware_summary(),
            "bootloader@1.16 se@2.2.3 (bootloader)"
        );
    }

    #[test]
    fn parse_bootloader_old_format() {
        // Old bootloader format: only the SE target id follows the flags.
        let data = h(&["01000001", "03", "302e36", "00", "04", "31100002"]);
        let info = DeviceInfo::from_get_version_response(&data).unwrap();
        assert!(info.is_bootloader);
        assert_eq!(info.version, "0.6");
        assert_eq!(info.maj_min, "0.6");
        assert_eq!(info.se_version, None);
        assert_eq!(info.se_target_id, Some(0x3110_0002));
        assert_eq!(info.model, Some(DeviceModel::NanoS));

        // Nothing after the flags.
        let data = h(&["01000001", "03", "302e36", "00"]);
        let info = DeviceInfo::from_get_version_response(&data).unwrap();
        assert!(info.is_bootloader);
        assert_eq!(info.se_target_id, None);
        assert_eq!(info.model, None);
    }

    #[test]
    fn parse_flex_charon_recover() {
        let data = h(&[
            "33300004",   // targetId
            "05",         // device version length
            "312e342e30", // device version aka `rawVersion`
            "04",         // flags length
            "ee000000",   // flags
            "05",
            "362e352e32", // mcu version aka `mcuVersion`
            "05",         // bootloader version length
            "352e352e32", // bootloader version aka `bootloaderVersion`
            "01",         // language id length
            "00",         // language id
            "01",         // recoverState length
            "06",         // recoverState
            "01",         // charonLength
            "02",         // charon
        ]);
        let info = DeviceInfo::from_get_version_response(&data).unwrap();
        assert!(!info.is_bootloader);
        assert_eq!(info.target_id, 858783748);
        assert_eq!(info.model, Some(DeviceModel::Flex));
        assert_eq!(info.version, "1.4.0");
        assert_eq!(info.mcu_version.as_deref(), Some("6.5.2"));
        assert_eq!(info.bootloader_version.as_deref(), Some("5.5.2"));
        assert_eq!(info.hardware_version, None);
        assert_eq!(info.language_id, Some(0));
        assert_eq!(info.recover_state, Some(vec![6]));
        assert_eq!(info.charon_state, Some(vec![2]));
    }

    #[test]
    fn parse_osu() {
        // A Nano S Plus in the middle of a firmware update: the extra fields aren't parsed.
        let data = h(&[
            "33100004",
            "09",
            &hex::encode("1.1.1-osu"),
            "04",
            "a6000000",
            "05",
            &hex::encode("5.24\0"),
        ]);
        let info = DeviceInfo::from_get_version_response(&data).unwrap();
        assert!(info.is_osu);
        assert!(!info.is_bootloader);
        assert!(!info.is_normal_mode());
        assert_eq!(info.raw_version, "1.1.1-osu");
        assert_eq!(info.version, "1.1.1");
        assert_eq!(info.maj_min, "1.1.1");
        // "osu" isn't a provider.
        assert_eq!(info.provider_name, None);
        assert_eq!(info.mcu_version.as_deref(), Some("5.24"));
        assert_eq!(info.model, Some(DeviceModel::NanoSPlus));
        assert_eq!(info.bootloader_version, None);
        // 0xa6: pin validated, not manager allowed, onboarded, not in recovery mode.
        assert!(info.pin_validated);
        assert!(!info.manager_allowed);
        assert!(info.onboarded);
        assert_eq!(info.firmware_summary(), "se@1.1.1 mcu@5.24 (osu)");
    }

    #[test]
    fn parse_stax_nano_s_plus_nano_gen5() {
        // Stax 1.8.0 (charon supported from 1.7.0).
        let stax = h(&[
            "33200004",
            "05",
            &hex::encode("1.8.0"),
            "04",
            "ea000000",
            "05",
            &hex::encode("5.24\0"),
            "05",
            &hex::encode("0.48\0"),
            "01",
            "00",
            "01",
            "00",
            "01",
            "00",
        ]);
        let info = DeviceInfo::from_get_version_response(&stax).unwrap();
        assert_eq!(info.model, Some(DeviceModel::Stax));
        assert_eq!(info.bootloader_version.as_deref(), Some("0.48"));
        assert_eq!(info.charon_state, Some(vec![0]));

        // Nano S Plus 1.0.3: no localization, no recover.
        let nsp = h(&[
            "33100004",
            "05",
            &hex::encode("1.0.3"),
            "04",
            "ee000000",
            "04",
            &hex::encode("4.03"),
            "04",
            &hex::encode("0.11"),
        ]);
        let info = DeviceInfo::from_get_version_response(&nsp).unwrap();
        assert_eq!(info.model, Some(DeviceModel::NanoSPlus));
        assert_eq!(info.bootloader_version.as_deref(), Some("0.11"));
        assert_eq!(info.language_id, None);
        assert_eq!(info.recover_state, None);

        // Nano Gen5: all fields supported.
        let gen5 = h(&[
            "33400004",
            "05",
            &hex::encode("1.0.0"),
            "04",
            "e2000000",
            "04",
            &hex::encode("1.00"),
            "04",
            &hex::encode("1.00"),
            "01",
            "02",
            "01",
            "00",
            "01",
            "00",
        ]);
        let info = DeviceInfo::from_get_version_response(&gen5).unwrap();
        assert_eq!(info.model, Some(DeviceModel::NanoGen5));
        assert_eq!(info.language_id, Some(2));
        assert_eq!(info.charon_state, Some(vec![0]));
        // 0xe2: not onboarded.
        assert!(!info.onboarded);
    }

    #[test]
    fn parse_nano_s_with_provider() {
        let data = h(&[
            "31100004",
            "09",
            &hex::encode("2.1.0-das"),
            "04",
            "a6000000",
            "04",
            &hex::encode("2.30"),
            "04",
            &hex::encode("1.16"),
        ]);
        let info = DeviceInfo::from_get_version_response(&data).unwrap();
        assert_eq!(info.model, Some(DeviceModel::NanoS));
        assert_eq!(info.version, "2.1.0-das");
        assert_eq!(info.maj_min, "2.1.0");
        assert_eq!(info.provider_name.as_deref(), Some("das"));
        assert_eq!(info.provider_id(), 2);
        assert_eq!(info.bootloader_version.as_deref(), Some("1.16"));
    }

    #[test]
    fn parse_invalid_does_not_panic() {
        for data in [
            &b""[..],
            &[0x33, 0x00],
            &[0x33, 0x00, 0x00, 0x04, 0x05, 0x32],
            &[0x33, 0x00, 0x00, 0x04, 0x00, 0x04, 0xee],
            &[0x33, 0x00, 0x00, 0x04, 0x01, 0xff, 0x00, 0x00],
            // Bootloader with a truncated SE part.
            &[0x05, 0x01, 0x00, 0x03, 0x00, 0x00, 0x05, 0x32],
            &[0x05, 0x01, 0x00, 0x03, 0x00, 0x00, 0x00],
        ] {
            assert!(DeviceInfo::from_get_version_response(data).is_err());
        }
        // Nano X 2.2.3 with a truncated field.
        let data = h(&[
            "33000004",
            "05",
            "322e322e33",
            "04",
            "ee000000",
            "04",
            "322e3330",
            "04",
            "3132",
        ]);
        assert!(DeviceInfo::from_get_version_response(&data).is_err());
        // Nano X 2.2.3 with missing trailing fields: they are left unset.
        let data = h(&[
            "33000004",
            "05",
            "322e322e33",
            "04",
            "ee000000",
            "04",
            "322e3330",
        ]);
        let info = DeviceInfo::from_get_version_response(&data).unwrap();
        assert_eq!(info.mcu_version.as_deref(), Some("2.30"));
        assert_eq!(info.bootloader_version, None);
        assert_eq!(info.language_id, None);
        let data = h(&["33000004", "05", "322e322e33", "04", "ee000000"]);
        let info = DeviceInfo::from_get_version_response(&data).unwrap();
        assert_eq!(info.mcu_version, None);
    }

    #[test]
    fn maj_min() {
        assert_eq!(parse_maj_min("2.1.0"), ("2.1.0".into(), None));
        assert_eq!(parse_maj_min("1.16"), ("1.16".into(), None));
        assert_eq!(
            parse_maj_min("1.6.0-rc2"),
            ("1.6.0".into(), Some("rc2".into()))
        );
        assert_eq!(parse_maj_min("0.0"), ("0.0".into(), None));
        assert_eq!(parse_maj_min("-osu"), ("".into(), Some("osu".into())));
        assert_eq!(parse_maj_min("abc"), ("".into(), None));
    }

    #[test]
    fn dev_firmware() {
        assert!(is_dev_firmware(Some("2.1.0-rc1")));
        assert!(is_dev_firmware(Some("1.1.0-lo3")));
        assert!(!is_dev_firmware(Some("2.1.0")));
        assert!(!is_dev_firmware(None));
    }

    #[test]
    fn app_and_version() {
        let mut data = vec![0x01, 0x05];
        data.extend_from_slice(b"BOLOS");
        data.push(0x05);
        data.extend_from_slice(b"2.2.3");
        let app = AppAndVersion::from_response(&data).unwrap();
        assert!(app.is_dashboard());
        assert_eq!(app.version, "2.2.3");
        assert!(app.flags.is_empty());

        let mut data = vec![0x01, 0x07];
        data.extend_from_slice(b"Bitcoin");
        data.push(0x05);
        data.extend_from_slice(b"2.1.3");
        data.extend_from_slice(&[0x01, 0x02]);
        let app = AppAndVersion::from_response(&data).unwrap();
        assert!(!app.is_dashboard());
        assert_eq!(app.name, "Bitcoin");
        assert_eq!(app.flags, vec![0x02]);

        assert!(AppAndVersion::from_response(&[0x02, 0x00]).is_err());
        assert!(AppAndVersion::from_response(&[0x01, 0x05, b'B']).is_err());
    }
}
