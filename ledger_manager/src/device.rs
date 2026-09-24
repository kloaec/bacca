//! The device: finding it on USB, identifying its model, querying its information (GetVersion)
//! and quitting the open app.

use crate::error::*;

use ledger_apdu::{APDUAnswer, APDUCommand};
use ledger_transport_hidapi::{hidapi, hidapi::HidApi, TransportNativeHID};

use std::{fmt, str, thread, time};

/// The USB vendor id of all the Ledger models (also in bootloader mode).
const LEDGER_USB_VENDOR_ID: u16 = 0x2c97;
/// The USB usage page of the Ledger HID interface.
const LEDGER_USAGE_PAGE: u16 = 0xffa0;

/// An APDU without the `Le` byte, which Ledger devices don't use.
pub(crate) fn apdu(cla: u8, ins: u8, p1: u8, data: Vec<u8>) -> APDUCommand<Vec<u8>> {
    APDUCommand {
        cla,
        ins,
        p1,
        p2: 0x00,
        data,
    }
}

/// Something APDUs can be exchanged with: the device, or a simulated one in the tests.
pub(crate) trait ApduExchange {
    fn exchange_apdu(&self, command: &APDUCommand<Vec<u8>>) -> Result<APDUAnswer<Vec<u8>>, Error>;
}

impl ApduExchange for TransportNativeHID {
    fn exchange_apdu(&self, command: &APDUCommand<Vec<u8>>) -> Result<APDUAnswer<Vec<u8>>, Error> {
        Ok(self.exchange(command)?)
    }
}

/// A model of Ledger device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceModel {
    NanoS,
    NanoSPlus,
    NanoX,
    Stax,
    /// Internally named "europa" by Ledger.
    Flex,
    /// Internally named "apex" by Ledger.
    NanoGen5,
}

// The identifiers of the models are those of the `@ledgerhq/devices` package, now published from
// https://github.com/LedgerHQ/ts-libs (packages/devices/src/index.ts).
impl DeviceModel {
    /// Ledger Live's identifier of this model (`DeviceModelId`).
    pub(crate) fn id(self) -> &'static str {
        match self {
            DeviceModel::NanoS => "nanoS",
            DeviceModel::NanoSPlus => "nanoSP",
            DeviceModel::NanoX => "nanoX",
            DeviceModel::Stax => "stax",
            DeviceModel::Flex => "europa",
            DeviceModel::NanoGen5 => "apex",
        }
    }

    /// Identify the model from the first two bytes of a SE target id (`identifyTargetId`).
    pub(crate) fn from_target_id(target_id: u32) -> Option<Self> {
        Some(match target_id & 0xffff_0000 {
            0x3110_0000 => DeviceModel::NanoS,
            0x3300_0000 => DeviceModel::NanoX,
            0x3310_0000 => DeviceModel::NanoSPlus,
            0x3320_0000 => DeviceModel::Stax,
            0x3330_0000 => DeviceModel::Flex,
            0x3340_0000 => DeviceModel::NanoGen5,
            _ => return None,
        })
    }

    /// Identify the model from a USB product id: either a legacy product id, or 0xMMII with MM
    /// identifying the model (`identifyUSBProductId`).
    pub(crate) fn from_usb_product_id(product_id: u16) -> Option<Self> {
        Some(match product_id {
            0x0001 => DeviceModel::NanoS,
            0x0004 => DeviceModel::NanoX,
            0x0005 => DeviceModel::NanoSPlus,
            0x0006 => DeviceModel::Stax,
            0x0007 => DeviceModel::Flex,
            0x0008 => DeviceModel::NanoGen5,
            _ => match product_id >> 8 {
                0x10 => DeviceModel::NanoS,
                0x40 => DeviceModel::NanoX,
                0x50 => DeviceModel::NanoSPlus,
                0x60 => DeviceModel::Stax,
                0x70 => DeviceModel::Flex,
                0x80 => DeviceModel::NanoGen5,
                _ => return None,
            },
        })
    }

    /// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/capabilities/devicesWithTouchScreen.ts
    pub(crate) fn has_touch_screen(self) -> bool {
        matches!(
            self,
            DeviceModel::Stax | DeviceModel::Flex | DeviceModel::NanoGen5
        )
    }
}

impl fmt::Display for DeviceModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DeviceModel::NanoS => "Ledger Nano S",
            DeviceModel::NanoSPlus => "Ledger Nano S Plus",
            DeviceModel::NanoX => "Ledger Nano X",
            DeviceModel::Stax => "Ledger Stax",
            DeviceModel::Flex => "Ledger Flex",
            DeviceModel::NanoGen5 => "Ledger Nano Gen5",
        })
    }
}

/// Loosely extract a version from a string, like `semver.coerce` which Ledger Live uses to
/// compare versions: the first sequence of up to three dot-separated numbers, the missing parts
/// being 0. "2.1.0-rc1" gives 2.1.0, "1.16" gives 1.16.0.
pub(crate) fn coerce_version(s: &str) -> Option<(u64, u64, u64)> {
    let bytes = s.as_bytes();
    let mut i = bytes.iter().position(|b| b.is_ascii_digit())?;
    let mut nums = Vec::with_capacity(3);
    while nums.len() < 3 {
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        // Like semver, only consider the first 16 digits of each part.
        nums.push(s[start..i.min(start + 16)].parse().ok()?);
        if i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit() {
            i += 1;
        } else {
            break;
        }
    }
    nums.resize(3, 0);
    Some((nums[0], nums[1], nums[2]))
}

/// Whether this version, once coerced, is at least `min`. Ledger Live's
/// `versionSatisfies(semverCoerce(v), ">=x.y.z")`.
pub(crate) fn version_at_least(version: &str, min: (u64, u64, u64)) -> bool {
    coerce_version(version).is_some_and(|v| v >= min)
}

// Ledger Live's node-hid transport filters by usage page on Windows and macOS, and by interface
// number on Linux (where the usage page might not be reported by some hidapi backends).
// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledgerjs/packages/hw-transport-node-hid-noevents/src/TransportNodeHid.ts
// (now published from https://github.com/LedgerHQ/ts-libs).
fn ledger_devices(hid_api: &HidApi) -> Vec<&hidapi::DeviceInfo> {
    let ledgers = || {
        hid_api
            .device_list()
            .filter(|d| d.vendor_id() == LEDGER_USB_VENDOR_ID)
    };
    let devices: Vec<_> = ledgers()
        .filter(|d| d.usage_page() == LEDGER_USAGE_PAGE)
        .collect();
    if !devices.is_empty() {
        return devices;
    }
    ledgers()
        .filter(|d| d.usage_page() == 0 && d.interface_number() == 0)
        .collect()
}

/// The first Ledger device known to the HID API.
pub(crate) fn find_ledger(hid_api: &HidApi) -> Result<&hidapi::DeviceInfo, Error> {
    ledger_devices(hid_api)
        .into_iter()
        .next()
        .ok_or(Error::DeviceNotFound)
}

/// The HID paths of the Ledger devices known to the HID API. Call `HidApi::refresh_devices`
/// first to get an up to date list.
pub fn list_ledger_devices(hid_api: &HidApi) -> Vec<String> {
    ledger_devices(hid_api)
        .into_iter()
        .map(|d| d.path().to_string_lossy().into_owned())
        .collect()
}

/// Open the first Ledger device known to the HID API (without refreshing the list of devices).
/// Also returns its model as detected from its USB product id, if known.
pub fn open_device(hid_api: &HidApi) -> Result<(TransportNativeHID, Option<DeviceModel>), Error> {
    let dev = find_ledger(hid_api)?;
    let model = DeviceModel::from_usb_product_id(dev.product_id());
    log::debug!(
        "Opening Ledger device with product id {:#06x} (model: {:?}).",
        dev.product_id(),
        model
    );
    Ok((TransportNativeHID::open_device(hid_api, dev)?, model))
}

/// Refresh the list of HID devices and open the first Ledger found.
pub(crate) fn connect(hid_api: &mut HidApi) -> Result<TransportNativeHID, Error> {
    hid_api.refresh_devices()?;
    Ok(open_device(hid_api)?.0)
}

/// The information of a device, from its answer to the GetVersion APDU. This merges Ledger Live's
/// `FirmwareInfo` and `DeviceInfo` (libs/device-core/src/commands/entities/FirmwareInfoEntity.ts
/// and libs/device-core/src/managerApi/entities/DeviceInfoEntity.ts).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The SE target id when running the OS, the MCU target id in bootloader mode.
    pub target_id: u32,
    /// The firmware version without the "-osu" suffix, as the Ledger API expects it. In bootloader
    /// mode, the version of the bootloader.
    pub version: String,
    /// The firmware version as returned by the device.
    pub raw_version: String,
    pub flags: Vec<u8>,
    pub is_bootloader: bool,
    /// Whether the device runs the OS updater (in the middle of a firmware update).
    pub is_osu: bool,
    pub se_version: Option<String>,
    pub se_target_id: Option<u32>,
    /// `None` in bootloader mode.
    pub mcu_version: Option<String>,
    /// The "x.y" (or "x.y.z") part of the version.
    pub maj_min: String,
    /// The Ledger API provider id for this device.
    pub provider: u32,
    pub manager_allowed: bool,
    pub pin_validated: bool,
    pub is_recovery_mode: bool,
    pub onboarded: bool,
    pub has_dev_firmware: bool,
    pub bootloader_version: Option<String>,
    pub hardware_version: Option<u8>,
    pub language_id: Option<u8>,
    pub recover_state: Option<Vec<u8>>,
    pub charon_state: Option<Vec<u8>>,
    /// Detected from the SE target id.
    pub model: Option<DeviceModel>,
}

/// A cursor over an APDU answer which never panics.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn has_more(&self) -> bool {
        self.pos < self.data.len()
    }

    fn bytes(&mut self, len: usize, what: &str) -> Result<&'a [u8], Error> {
        let b = self
            .data
            .get(self.pos..self.pos + len)
            .ok_or_else(|| Error::InvalidDeviceData(format!("not enough data for {}", what)))?;
        self.pos += len;
        Ok(b)
    }

    fn u8(&mut self, what: &str) -> Result<u8, Error> {
        Ok(self.bytes(1, what)?[0])
    }

    /// A length-prefixed field.
    fn lv(&mut self, what: &str) -> Result<&'a [u8], Error> {
        let len = self.u8(what)? as usize;
        self.bytes(len, what)
    }

    fn lv_string(&mut self, what: &str) -> Result<String, Error> {
        let b = self.lv(what)?;
        // Some versions are terminated by a NUL byte.
        utf8(b.strip_suffix(&[0]).unwrap_or(b), what)
    }
}

fn utf8(b: &[u8], what: &str) -> Result<String, Error> {
    String::from_utf8(b.to_vec())
        .map_err(|_| Error::InvalidDeviceData(format!("{} is not valid UTF-8", what)))
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

// The minimum firmware version from which the GetVersion answer contains a field, per model. See
// https://github.com/LedgerHQ/ledger-live/tree/develop/libs/device-core/src/commands/use-cases
// (isBootloaderVersionSupported.ts, isHardwareVersionSupported.ts,
// isDeviceLocalizationSupported.ts, isRecoverSupported.ts, isCharonSupported.ts).
type Ranges = &'static [(DeviceModel, (u64, u64, u64))];
const BOOTLOADER_VERSION_RANGES: Ranges = &[
    (DeviceModel::NanoS, (2, 0, 0)),
    (DeviceModel::NanoX, (2, 0, 0)),
    (DeviceModel::NanoSPlus, (1, 0, 0)),
    (DeviceModel::Stax, (1, 0, 0)),
    (DeviceModel::Flex, (0, 0, 0)),
    (DeviceModel::NanoGen5, (0, 0, 0)),
];
const HARDWARE_VERSION_RANGES: Ranges = &[(DeviceModel::NanoX, (2, 0, 0))];
// Also the ranges of the recover state.
const LOCALIZATION_RANGES: Ranges = &[
    (DeviceModel::NanoX, (2, 1, 0)),
    (DeviceModel::NanoSPlus, (1, 1, 0)),
    (DeviceModel::Stax, (1, 0, 0)),
    (DeviceModel::Flex, (0, 0, 0)),
    (DeviceModel::NanoGen5, (0, 0, 0)),
];
const CHARON_RANGES: Ranges = &[
    (DeviceModel::Stax, (1, 7, 0)),
    (DeviceModel::Flex, (1, 3, 0)),
    (DeviceModel::NanoGen5, (0, 0, 0)),
];

fn supports(se_version: &str, model: Option<DeviceModel>, ranges: Ranges) -> bool {
    ranges
        .iter()
        .any(|(m, min)| Some(*m) == model && version_at_least(se_version, *min))
}

/// Whether the device supports changing its language with this firmware version.
pub(crate) fn is_device_localization_supported(
    se_version: &str,
    model: Option<DeviceModel>,
) -> bool {
    supports(se_version, model, LOCALIZATION_RANGES)
}

/// Extract the "maj.min(.patch)" part and the part after the dash of a raw version, like the
/// `/([0-9]+.[0-9]+(.[0-9]+){0,1})?(-(.*))?/` regex of getDeviceInfo.ts. Note that in this regex
/// the dots match any character, and that it always matches at the start (the group is optional).
fn parse_maj_min(raw: &str) -> (String, Option<String>) {
    let b = raw.as_bytes();
    let digits_end = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    let mut end = 0;
    let e1 = digits_end(0);
    if e1 > 0 && e1 < b.len() {
        let e2 = digits_end(e1 + 1);
        if e2 > e1 + 1 {
            end = e2;
            if e2 < b.len() {
                let e3 = digits_end(e2 + 1);
                if e3 > e2 + 1 {
                    end = e3;
                }
            }
        }
    }
    let maj_min = raw.get(..end).unwrap_or("").to_string();
    let post_dash = raw.get(end..).and_then(|r| r.strip_prefix('-'));
    (maj_min, post_dash.map(|s| s.to_string()))
}

impl DeviceInfo {
    /// Query the information of the device.
    pub fn new(transport: &TransportNativeHID) -> Result<Self, Error> {
        // https://github.com/LedgerHQ/ledger-live/blob/dd1d17fd3ce7ed42558204b2f93707fb9b1599de/libs/device-core/src/commands/use-cases/getVersion.ts#L6
        let answer = transport.exchange(&apdu(0xe0, 0x01, 0x00, vec![]))?;
        match answer.retcode() {
            SW_OK => {}
            SW_LOCKED => return Err(Error::DeviceLocked),
            SW_NOT_ONBOARDED | SW_NOT_ONBOARDED_LEGACY => return Err(Error::DeviceNotOnboarded),
            // GetVersion is handled by the dashboard, not by the apps.
            SW_CLA_NOT_SUPPORTED | SW_INS_NOT_SUPPORTED => {
                return Err(Error::DeviceOnDashboardExpected)
            }
            s => return Err(Error::DeviceStatus(s)),
        }
        let info = Self::from_get_version_response(answer.data())?;
        log::debug!("deviceInfo: {}", info.firmware_summary());
        Ok(info)
    }

    /// Parse the answer to the GetVersion APDU (without the status word).
    ///
    /// Ported from https://github.com/LedgerHQ/ledger-live/blob/dd1d17fd3ce7ed42558204b2f93707fb9b1599de/libs/device-core/src/commands/use-cases/parseGetVersionResponse.ts
    /// and https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/getDeviceInfo.ts
    pub fn from_get_version_response(data: &[u8]) -> Result<Self, Error> {
        let mut r = Reader { data, pos: 0 };
        // The target id and version of either the bootloader or the SE.
        let target_id = be_u32(r.bytes(4, "target id")?, "target id")?;
        let raw_version_bytes = r.lv("version")?;
        let mut raw_version = utf8(raw_version_bytes, "version")?;
        let mut flags = r.lv("flags")?.to_vec();
        if raw_version_bytes.is_empty() {
            // To support old firmwares like the bootloader of 1.3.1.
            raw_version = "0.0.0".to_string();
            flags = Vec::new();
        }

        let is_bootloader = (target_id & 0xf000_0000) != 0x3000_0000;
        let is_osu = raw_version.contains("-osu");
        let (mut se_version, mut se_target_id, mut mcu_version) = (None, None, None);
        let (mut bootloader_version, mut hardware_version, mut language_id) = (None, None, None);
        let (mut recover_state, mut charon_state) = (None, None);
        let model;
        if is_bootloader {
            if r.has_more() {
                let part1 = r.lv("SE part 1")?;
                // This is how Ledger Live distinguishes the old format (only the SE target id)
                // from the new one (SE version then SE target id).
                if part1.len() >= 5 {
                    se_version = Some(utf8(part1, "SE version")?);
                    se_target_id = Some(be_u32(r.lv("SE target id")?, "SE target id")?);
                } else {
                    se_target_id = Some(be_u32(part1, "SE target id")?);
                }
            }
            model = se_target_id.and_then(DeviceModel::from_target_id);
        } else {
            se_version = Some(raw_version.clone());
            se_target_id = Some(target_id);
            model = DeviceModel::from_target_id(target_id);

            // Ledger Live tolerates missing trailing fields (it would read empty buffers), so do
            // we: a missing field is left unset.
            if r.has_more() {
                mcu_version = Some(r.lv_string("MCU version")?);
            } else {
                log::warn!("GetVersion response without MCU version.");
            }
            let has = |r: &Reader, ranges| {
                !is_osu && r.has_more() && supports(&raw_version, model, ranges)
            };
            if has(&r, BOOTLOADER_VERSION_RANGES) {
                bootloader_version = Some(r.lv_string("bootloader version")?);
            }
            if has(&r, HARDWARE_VERSION_RANGES) {
                hardware_version = r.lv("hardware version")?.first().copied();
            }
            if has(&r, LOCALIZATION_RANGES) {
                language_id = r.lv("language id")?.first().copied();
            }
            if has(&r, LOCALIZATION_RANGES) {
                recover_state = Some(r.lv("recover state")?.to_vec());
            }
            if has(&r, CHARON_RANGES) {
                charon_state = Some(r.lv("charon state")?.to_vec());
            }
        }

        let (maj_min, post_dash) = parse_maj_min(&raw_version);
        // A firmware version may be suffixed with the name of its provider (e.g. "1.2.3-das").
        // https://github.com/LedgerHQ/ledger-live/blob/develop/libs/device-core/src/managerApi/use-cases/getProviderIdUseCase.ts
        let provider = match post_dash.as_deref() {
            Some("das") => 2,
            Some("club") => 3,
            Some("shitcoins") => 4,
            Some("ee") => 5,
            _ => crate::api::DEFAULT_PROVIDER,
        };
        let flag = flags.first().copied().unwrap_or(0);
        // Nb since the LNS+, unseeded devices are visible and there are extra flags.
        let (is_recovery_mode, onboarded) = if flags.len() == 4 {
            (flag & 0x01 != 0, flag & 0x04 != 0)
        } else {
            (false, true)
        };
        // https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/isDevFirmware.ts
        let has_dev_firmware = se_version
            .as_deref()
            .is_some_and(|v| ["-lo", "-rc", "-il", "-tr"].iter().any(|s| v.contains(s)));

        Ok(Self {
            target_id,
            version: raw_version.replacen("-osu", "", 1),
            raw_version,
            manager_allowed: flag & 0x08 != 0,
            pin_validated: flag & 0x80 != 0,
            flags,
            is_bootloader,
            is_osu,
            se_version,
            se_target_id,
            mcu_version,
            maj_min,
            provider,
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

    /// The running mode of the device, for humans.
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

    /// A summary of the firmware versions, e.g. "se@2.2.3 mcu@2.30 bl@1.16" or
    /// "se@2.2.3 mcu@2.30 (osu)", like the log line of Ledger Live's getDeviceInfo.
    pub fn firmware_summary(&self) -> String {
        let mut s = if self.is_bootloader {
            format!("bootloader@{}", self.version)
        } else {
            format!("se@{}", self.version)
        };
        if let Some(mcu) = self.mcu_version.as_deref().filter(|v| !v.is_empty()) {
            s += &format!(" mcu@{}", mcu);
        }
        if let Some(se) = self.se_version.as_ref().filter(|_| self.is_bootloader) {
            s += &format!(" se@{}", se);
        }
        if let Some(bl) = &self.bootloader_version {
            s += &format!(" bl@{}", bl);
        }
        if self.is_osu {
            s += " (osu)";
        } else if self.is_bootloader {
            s += " (bootloader)";
        }
        s
    }

    /// Whether the device runs its OS normally (neither in bootloader nor in updater mode).
    pub fn is_normal_mode(&self) -> bool {
        !self.is_bootloader && !self.is_osu
    }

    pub(crate) fn check_normal_mode(&self) -> Result<(), Error> {
        if self.is_bootloader {
            Err(Error::DeviceInBootloader)
        } else if self.is_osu {
            Err(Error::DeviceOnDashboardExpected)
        } else {
            Ok(())
        }
    }
}

/// Parse the answer to the GetAppAndVersion APDU (without the status word): the name and version
/// of the running app.
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/getAppAndVersion.ts
fn parse_app_and_version(data: &[u8]) -> Result<(String, String), Error> {
    let mut r = Reader { data, pos: 0 };
    let format = r.u8("format")?;
    if format != 1 {
        return Err(Error::InvalidDeviceData(format!(
            "getAppAndVersion: format {} not supported",
            format
        )));
    }
    let name = String::from_utf8_lossy(r.lv("app name")?).into_owned();
    let version = String::from_utf8_lossy(r.lv("app version")?).into_owned();
    if r.has_more() {
        r.lv("app flags")?;
    }
    Ok((name, version))
}

/// Quit the open app, if any, to go back to the dashboard. Like Ledger Live, this does nothing if
/// already on the dashboard, or if GetAppAndVersion isn't supported (bootloader mode, old
/// firmwares).
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/commands/quitApp.ts
pub(crate) fn quit_app(transport: &TransportNativeHID) -> Result<(), Error> {
    let answer = transport.exchange(&apdu(0xb0, 0x01, 0x00, vec![]))?;
    let (name, version) = match answer.retcode() {
        SW_OK => parse_app_and_version(answer.data())?,
        SW_LOCKED => return Err(Error::DeviceLocked),
        SW_CLA_NOT_SUPPORTED | SW_CLA_NOT_SUPPORTED_BOOTLOADER => return Ok(()),
        s => return Err(Error::DeviceStatus(s)),
    };
    // https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/isDashboardName.ts
    if name == "BOLOS" || name == "OLOS\u{0}" {
        return Ok(());
    }
    log::info!("Quitting app '{}' {}.", name, version);
    match transport
        .exchange(&apdu(0xb0, 0xa7, 0x00, vec![]))?
        .retcode()
    {
        SW_OK => {}
        SW_LOCKED => return Err(Error::DeviceLocked),
        s => return Err(Error::DeviceStatus(s)),
    }
    // Give some time to the device to get back to the dashboard.
    thread::sleep(time::Duration::from_millis(500));
    Ok(())
}

/// Poll the device until `accept` returns true for its information, reconnecting to it each time
/// as it may reboot in the meantime. Like Ledger Live's `withDevicePolling` accepting all errors
/// (hw/deviceAccess.ts), errors are ignored: they are only passed to `on_error` (e.g. to tell the
/// user to unlock the device).
pub(crate) fn wait_for_device(
    hid_api: &mut HidApi,
    timeout: time::Duration,
    accept: impl Fn(&DeviceInfo) -> bool,
    mut on_error: impl FnMut(&Error),
) -> Result<DeviceInfo, Error> {
    let start = time::Instant::now();
    loop {
        match connect(hid_api).and_then(|t| DeviceInfo::new(&t)) {
            Ok(info) if accept(&info) => return Ok(info),
            Ok(info) => log::debug!("Device info not accepted yet: {}", info.firmware_summary()),
            Err(e) => {
                log::debug!("Error while polling the device: {}", e);
                on_error(&e);
            }
        }
        if start.elapsed() >= timeout {
            return Err(Error::Timeout("waiting for the device"));
        }
        // WITH_DEVICE_POLLING_DELAY
        thread::sleep(time::Duration::from_millis(500));
    }
}

#[cfg(test)]
type ApduHandler = Box<dyn FnMut(&APDUCommand<Vec<u8>>) -> (Vec<u8>, u16)>;

/// A simulated device for the tests: answers the APDUs with a handler, and records them.
#[cfg(test)]
pub(crate) struct MockDevice {
    handler: std::cell::RefCell<ApduHandler>,
    /// The APDUs received, serialized.
    pub sent: std::cell::RefCell<Vec<Vec<u8>>>,
}

#[cfg(test)]
impl MockDevice {
    pub fn new(handler: impl FnMut(&APDUCommand<Vec<u8>>) -> (Vec<u8>, u16) + 'static) -> Self {
        Self {
            handler: std::cell::RefCell::new(Box::new(handler)),
            sent: Default::default(),
        }
    }
}

#[cfg(test)]
impl ApduExchange for MockDevice {
    fn exchange_apdu(&self, command: &APDUCommand<Vec<u8>>) -> Result<APDUAnswer<Vec<u8>>, Error> {
        self.sent.borrow_mut().push(command.serialize());
        let (mut data, status) = (self.handler.borrow_mut())(command);
        data.extend_from_slice(&status.to_be_bytes());
        Ok(APDUAnswer::from_answer(data).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &[&str]) -> Vec<u8> {
        hex::decode(s.join("")).unwrap()
    }

    fn parse(s: &[&str]) -> DeviceInfo {
        DeviceInfo::from_get_version_response(&h(s)).unwrap()
    }

    // Fixture from ledger-live's libs/device-core/src/commands/use-cases/parseGetVersionResponse.test.ts
    // (the trailing status word is stripped).
    #[test]
    fn parse_nano_x() {
        let info = parse(&[
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
        assert!(!info.is_bootloader && !info.is_osu);
        assert_eq!(info.target_id, 855638020);
        assert_eq!(info.version, "2.2.3");
        assert_eq!(info.se_version.as_deref(), Some("2.2.3"));
        assert_eq!(info.se_target_id, Some(855638020));
        assert_eq!(info.mcu_version.as_deref(), Some("2.30"));
        assert_eq!(info.flags, vec![238, 0, 0, 0]);
        assert_eq!(info.bootloader_version.as_deref(), Some("1.16"));
        assert_eq!(info.hardware_version, Some(1));
        assert_eq!(info.language_id, Some(0));
        assert_eq!(info.recover_state, Some(vec![0]));
        assert_eq!(info.charon_state, None);
        assert_eq!(info.model, Some(DeviceModel::NanoX));
        assert_eq!(info.maj_min, "2.2.3");
        // 0xee = 0b1110_1110
        assert!(info.manager_allowed && info.pin_validated && info.onboarded);
        assert!(!info.is_recovery_mode && !info.has_dev_firmware);
        assert_eq!(info.provider, 1);
        assert_eq!(info.firmware_summary(), "se@2.2.3 mcu@2.30 bl@1.16");
    }

    #[test]
    fn parse_old_firmware_zero_version_length() {
        let info = parse(&[
            "33000004", "00", "04", "ee000000", "04", "322e3330", "01", "01", "01", "00", "01",
            "00",
        ]);
        assert_eq!(info.version, "0.0.0");
        assert_eq!(info.mcu_version.as_deref(), Some("2.30"));
        assert!(info.flags.is_empty());
        assert_eq!(info.bootloader_version, None);
        assert_eq!(info.language_id, None);
        assert!(!info.manager_allowed && info.onboarded);
    }

    #[test]
    fn parse_bootloader() {
        let info = parse(&[
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
        assert!(info.is_bootloader && !info.is_osu);
        assert_eq!(info.version, "1.16");
        assert_eq!(info.maj_min, "1.16");
        assert_eq!(info.target_id, 83951619);
        assert_eq!(info.se_version.as_deref(), Some("2.2.3"));
        assert_eq!(info.se_target_id, Some(855638020));
        assert_eq!(info.mcu_version, None);
        // The model is detected from the SE target id in bootloader mode.
        assert_eq!(info.model, Some(DeviceModel::NanoX));
        assert_eq!(info.mode(), "bootloader");
        assert_eq!(
            info.firmware_summary(),
            "bootloader@1.16 se@2.2.3 (bootloader)"
        );

        // Old format: only the SE target id follows the flags.
        let info = parse(&["01000001", "03", "302e36", "00", "04", "31100002"]);
        assert_eq!((info.is_bootloader, info.maj_min.as_str()), (true, "0.6"));
        assert_eq!(info.se_version, None);
        assert_eq!(info.model, Some(DeviceModel::NanoS));
        // Nothing after the flags.
        let info = parse(&["01000001", "03", "302e36", "00"]);
        assert_eq!((info.se_target_id, info.model), (None, None));
    }

    #[test]
    fn parse_flex_charon_recover() {
        let info = parse(&[
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
        let info = parse(&[
            "33100004",
            "09",
            &hex::encode("1.1.1-osu"),
            "04",
            "a6000000",
            "05",
            &hex::encode("5.24\0"),
        ]);
        assert!(info.is_osu && !info.is_bootloader && !info.is_normal_mode());
        assert_eq!(info.raw_version, "1.1.1-osu");
        assert_eq!(info.version, "1.1.1");
        assert_eq!(info.maj_min, "1.1.1");
        // "osu" isn't a provider.
        assert_eq!(info.provider, 1);
        assert_eq!(info.mcu_version.as_deref(), Some("5.24"));
        assert_eq!(info.model, Some(DeviceModel::NanoSPlus));
        // 0xa6: pin validated, not manager allowed, onboarded, not in recovery mode.
        assert!(info.pin_validated && !info.manager_allowed && info.onboarded);
        assert_eq!(info.firmware_summary(), "se@1.1.1 mcu@5.24 (osu)");
    }

    #[test]
    fn parse_stax_nano_s_plus_nano_gen5() {
        // Stax 1.8.0 (charon supported from 1.7.0).
        let info = parse(&[
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
        assert_eq!(info.model, Some(DeviceModel::Stax));
        assert_eq!(info.bootloader_version.as_deref(), Some("0.48"));
        assert_eq!(info.charon_state, Some(vec![0]));

        // Nano S Plus 1.0.3: no localization, no recover.
        let info = parse(&[
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
        assert_eq!(info.model, Some(DeviceModel::NanoSPlus));
        assert_eq!(info.bootloader_version.as_deref(), Some("0.11"));
        assert_eq!((info.language_id, info.recover_state), (None, None));

        // Nano Gen5: all fields supported.
        let info = parse(&[
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
        assert_eq!(info.model, Some(DeviceModel::NanoGen5));
        assert_eq!(info.language_id, Some(2));
        assert_eq!(info.charon_state, Some(vec![0]));
        // 0xe2: not onboarded.
        assert!(!info.onboarded);
    }

    #[test]
    fn parse_nano_s_with_provider() {
        let info = parse(&[
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
        assert_eq!(info.model, Some(DeviceModel::NanoS));
        assert_eq!(info.version, "2.1.0-das");
        assert_eq!(info.maj_min, "2.1.0");
        assert_eq!(info.provider, 2);
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
        let info = parse(&[
            "33000004",
            "05",
            "322e322e33",
            "04",
            "ee000000",
            "04",
            "322e3330",
        ]);
        assert_eq!(info.mcu_version.as_deref(), Some("2.30"));
        assert_eq!((info.bootloader_version, info.language_id), (None, None));
        let info = parse(&["33000004", "05", "322e322e33", "04", "ee000000"]);
        assert_eq!(info.mcu_version, None);
    }

    #[test]
    fn maj_min_and_dev_firmware() {
        assert_eq!(parse_maj_min("2.1.0"), ("2.1.0".into(), None));
        assert_eq!(parse_maj_min("1.16"), ("1.16".into(), None));
        assert_eq!(
            parse_maj_min("1.6.0-rc2"),
            ("1.6.0".into(), Some("rc2".into()))
        );
        assert_eq!(parse_maj_min("0.0"), ("0.0".into(), None));
        assert_eq!(parse_maj_min("-osu"), ("".into(), Some("osu".into())));
        assert_eq!(parse_maj_min("abc"), ("".into(), None));
        let dev = parse(&["31100004", "09", &hex::encode("2.1.0-rc1"), "00"]);
        assert!(dev.has_dev_firmware);
    }

    #[test]
    fn app_and_version() {
        let mut data = vec![0x01, 0x07];
        data.extend_from_slice(b"Bitcoin");
        data.push(0x05);
        data.extend_from_slice(b"2.1.3");
        assert_eq!(
            parse_app_and_version(&data).unwrap(),
            ("Bitcoin".into(), "2.1.3".into())
        );
        data.extend_from_slice(&[0x01, 0x02]);
        assert!(parse_app_and_version(&data).is_ok());
        assert!(parse_app_and_version(&[0x02, 0x00]).is_err());
        assert!(parse_app_and_version(&[0x01, 0x05, b'B']).is_err());
    }

    #[test]
    fn models() {
        for (target_id, pid, legacy_pid, model) in [
            (0x3110_0004, 0x1011, 0x0001, DeviceModel::NanoS),
            (0x3300_0004, 0x4015, 0x0004, DeviceModel::NanoX),
            (0x3310_0004, 0x5011, 0x0005, DeviceModel::NanoSPlus),
            (0x3320_0004, 0x6011, 0x0006, DeviceModel::Stax),
            (0x3330_0004, 0x7011, 0x0007, DeviceModel::Flex),
            (0x3340_0004, 0x8011, 0x0008, DeviceModel::NanoGen5),
        ] {
            assert_eq!(DeviceModel::from_target_id(target_id), Some(model));
            assert_eq!(DeviceModel::from_usb_product_id(pid), Some(model));
            assert_eq!(DeviceModel::from_usb_product_id(legacy_pid), Some(model));
        }
        // A bootloader (MCU) target id isn't a SE target id.
        assert_eq!(DeviceModel::from_target_id(0x0501_0003), None);
        assert_eq!(DeviceModel::from_usb_product_id(0xf011), None);
    }

    #[test]
    fn versions() {
        assert_eq!(coerce_version("2.1.0-rc1"), Some((2, 1, 0)));
        assert_eq!(coerce_version("1.16"), Some((1, 16, 0)));
        assert_eq!(coerce_version("v3"), Some((3, 0, 0)));
        assert_eq!(coerce_version("foo 1.2.3.4"), Some((1, 2, 3)));
        assert_eq!(coerce_version("none"), None);
        assert!(version_at_least("2.1.0-lo1", (2, 1, 0)));
        assert!(!version_at_least("2.0.9", (2, 1, 0)));
        assert!(!version_at_least("", (0, 0, 0)));
    }
}
