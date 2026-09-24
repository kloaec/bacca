//! Update the firmware of Blockstream Jade devices without the Blockstream app.
//!
//! Supported: Jade (v1), Jade (v1.1), Jade Plus (board `JADE_V2`) and Jade Core (`JADE_V2C`),
//! over USB serial. The DIY boards running the Jade firmware (M5Stack, TTGO...) are not supported.
//!
//! The update follows `update_jade_fw.py` of the Jade repository: get the device's version info,
//! download the latest stable full firmware for its hardware target and config (Bluetooth or not)
//! from the Blockstream firmware server, unlock the device if needed (the PIN is entered on the
//! device, we only relay its requests to the PIN server, see `pinserver`), and upload the
//! compressed firmware with the `ota`, `ota_data` and `ota_complete` calls. The user confirms the
//! new version and firmware hash on the device, which checks the hash before booting it.
//!
//! The whole API is **blocking** (serial and HTTP I/O, waiting for the user on the device for
//! minutes). From a GUI, run it on a worker thread and forward the [`Progress`] events.
//!
//! Modules: `cbor` (message encoding), `rpc` (serial transport), `pinserver` (PIN server relay),
//! `releases` (firmware server). This file ties them together.

mod cbor;
mod pinserver;
pub mod releases;
mod rpc;

use std::{
    fmt, thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cbor::Value;
use releases::FirmwareRelease;
use rpc::Jade;
pub use rpc::{HW_LOCKED, USER_CANCELLED};

/// For calls answered at once.
const INFO_TIMEOUT: Duration = Duration::from_secs(10);
/// `DEFAULT_SERIAL_TIMEOUT` in `jadepy/jade.py`.
const RPC_TIMEOUT: Duration = Duration::from_secs(120);
/// For calls waiting for the user: entering the PIN, confirming the update (on the first
/// `ota_data` calls, see `ota_user_validate()` in `main/process/ota_util.c`).
const USER_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to wait for the device to come back after the update (it reboots 2.5s after
/// replying to `ota_complete`).
const REBOOT_DELAY: Duration = Duration::from_secs(5);
const REBOOT_TIMEOUT: Duration = Duration::from_secs(90);
/// Bound on the chunk size given by the device (`JADE_OTA_BUF_SIZE` is 4096 in
/// `main/process/ota_defines.h`).
const MAX_CHUNK_SIZE: usize = 32 * 1024;

#[derive(Debug)]
pub enum Error {
    Serial(serialport::Error),
    Io(std::io::Error),
    Http(minreq::Error),
    NoDevice,
    /// Several possible Jade serial ports: pick one.
    TooManyDevices(Vec<String>),
    /// No reply from the device in time.
    Timeout,
    /// Unexpected message from the device.
    Protocol(String),
    /// An error returned by the device, e.g. [`USER_CANCELLED`] if the user refused the update.
    Device {
        code: i64,
        message: String,
        data: Option<String>,
    },
    /// Not a Jade model we can update.
    UnsupportedBoard {
        board_type: String,
        features: String,
    },
    /// The latest release is older than the installed firmware.
    Downgrade {
        installed: Version,
        latest: Version,
    },
    /// The PIN entered on the device is wrong.
    IncorrectPin,
    /// The device asked to contact a PIN server other than the default Blockstream one.
    UntrustedPinServer(String),
    /// Any other error, with a message for the user.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serial(e) => write!(f, "serial port error: {}", e),
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::Http(e) => write!(f, "HTTP error: {}", e),
            Self::NoDevice => write!(
                f,
                "no Jade found. Is it connected, switched on, and not used by another app?"
            ),
            Self::TooManyDevices(ports) => write!(
                f,
                "several possible Jade serial ports found ({}), please connect only one",
                ports.join(", ")
            ),
            Self::Timeout => write!(f, "the device did not answer in time"),
            Self::Protocol(s) => write!(f, "unexpected message from the device: {}", s),
            Self::Device { code, .. } if *code == USER_CANCELLED => {
                write!(f, "cancelled on the device")
            }
            Self::Device { code, message, .. } if *code == HW_LOCKED => write!(
                f,
                "{}. Unlock the Jade from this computer (not over Bluetooth), and don't use a temporary wallet",
                message
            ),
            Self::Device { message, data, .. } => match data {
                Some(d) => write!(f, "{} ({})", message, d),
                None => write!(f, "{}", message),
            },
            Self::UnsupportedBoard {
                board_type,
                features,
            } => write!(f, "unsupported hardware: board {}, features {}", board_type, features),
            Self::Downgrade { installed, latest } => write!(
                f,
                "the installed firmware v{} is newer than the latest release v{}",
                installed, latest
            ),
            Self::IncorrectPin => write!(
                f,
                "incorrect PIN. Careful: the Jade erases its wallet after 3 incorrect PINs in a row"
            ),
            Self::UntrustedPinServer(s) => write!(
                f,
                "the Jade asked to contact a PIN server other than Blockstream's ({}). Bacca only relays requests to the default PIN server: unlock the Jade with the software you set it up with",
                s
            ),
            Self::Other(s) => write!(f, "{}", s),
        }
    }
}

impl std::error::Error for Error {}

impl From<serialport::Error> for Error {
    fn from(e: serialport::Error) -> Self {
        Error::Serial(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<minreq::Error> for Error {
    fn from(e: minreq::Error) -> Self {
        Error::Http(e)
    }
}

/// A firmware version, `X.Y.Z`. A suffix (development builds, e.g. `1.0.36-3-gabcdef`) is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Version {
            major,
            minor,
            patch,
        }
    }

    pub fn parse(s: &str) -> Option<Version> {
        let base = s.split(['-', '+']).next()?;
        let mut parts = base.split('.').map(|p| p.parse::<u32>().ok());
        let v = Version::new(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(v)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The Jade models, from the `BOARD_TYPE` of the version info (see `main/process/ota_defines.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// `JADE`: the original Jade, with a wheel.
    JadeV1,
    /// `JADE_V1.1`: Jade with a jog wheel.
    JadeV1Dot1,
    /// `JADE_V2`: Jade Plus, with buttons.
    JadePlus,
    /// `JADE_V2C`: Jade Core, a Jade Plus without camera and battery.
    JadeCore,
}

impl Model {
    fn from_board_type(board_type: &str) -> Option<Model> {
        match board_type {
            "JADE" => Some(Model::JadeV1),
            "JADE_V1.1" => Some(Model::JadeV1Dot1),
            "JADE_V2" => Some(Model::JadePlus),
            "JADE_V2C" => Some(Model::JadeCore),
            _ => None,
        }
    }

    /// The directory of the firmware server, see `download_file()` in `update_jade_fw.py`.
    fn hw_target(&self) -> &'static str {
        match self {
            Model::JadeV1 => "jade",
            Model::JadeV1Dot1 => "jade1.1",
            Model::JadePlus => "jade2.0",
            Model::JadeCore => "jade2.0c",
        }
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Model::JadeV1 => "Jade (v1)",
            Model::JadeV1Dot1 => "Jade (v1.1)",
            Model::JadePlus => "Jade Plus",
            Model::JadeCore => "Jade Core",
        })
    }
}

/// `JADE_STATE` of the version info, relative to this connection (see `main/versioninfo.c`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State {
    /// Unlocked from this connection.
    Ready,
    /// PIN set, not unlocked from this connection (maybe unlocked over Bluetooth).
    Locked,
    /// Temporary wallet ("emergency restore") in memory.
    Temporary,
    /// Wallet in memory but no PIN set yet.
    Unsaved,
    /// No wallet.
    Uninitialized,
    Other(String),
}

impl State {
    fn parse(s: &str) -> State {
        match s {
            "READY" => State::Ready,
            "LOCKED" => State::Locked,
            "TEMP" => State::Temporary,
            "UNSAVED" => State::Unsaved,
            "UNINIT" => State::Uninitialized,
            _ => State::Other(s.to_string()),
        }
    }
}

/// The version info of a connected Jade (`get_version_info`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The serial port.
    pub port: String,
    /// `JADE_VERSION`, parsed.
    pub version: Option<Version>,
    pub version_string: String,
    pub board_type: String,
    pub model: Option<Model>,
    /// "SB" (secure boot, production devices) or "DEV".
    pub features: String,
    /// "BLE" or "NORADIO": whether the firmware has Bluetooth. Kept by the update.
    pub config: String,
    pub state: State,
    /// "MAIN", "TEST" or "ALL".
    pub networks: String,
    pub ota_max_chunk: usize,
    pub efusemac: Option<String>,
}

impl DeviceInfo {
    fn parse(port: &str, info: &Value) -> Result<DeviceInfo, Error> {
        let text = |key: &str| info.get(key).and_then(Value::as_str).map(str::to_string);
        let required = |key: &str| {
            text(key).ok_or_else(|| Error::Protocol(format!("no {} in the version info", key)))
        };
        let version_string = required("JADE_VERSION")?;
        // Older firmwares have no board type (see `download_file()` in `update_jade_fw.py`).
        let board_type = text("BOARD_TYPE").unwrap_or_else(|| "JADE".into());
        let ota_max_chunk = info
            .get("JADE_OTA_MAX_CHUNK")
            .and_then(Value::as_int)
            .and_then(|c| usize::try_from(c).ok())
            .filter(|c| (1..=MAX_CHUNK_SIZE).contains(c))
            .ok_or_else(|| Error::Protocol("invalid JADE_OTA_MAX_CHUNK".into()))?;
        Ok(DeviceInfo {
            port: port.to_string(),
            version: Version::parse(&version_string),
            version_string,
            model: Model::from_board_type(&board_type),
            board_type,
            features: required("JADE_FEATURES")?,
            config: required("JADE_CONFIG")?,
            state: State::parse(&required("JADE_STATE")?),
            networks: text("JADE_NETWORKS").unwrap_or_else(|| "ALL".into()),
            ota_max_chunk,
            efusemac: text("EFUSEMAC"),
        })
    }

    /// The directory of the firmware server for this device, e.g. "jade2.0" (or "jade2.0dev" for
    /// a development device).
    pub fn hw_target(&self) -> Result<String, Error> {
        let suffix = match self.features.as_str() {
            "SB" => Some(""),
            "DEV" => Some("dev"),
            _ => None,
        };
        match (self.model, suffix) {
            (Some(m), Some(s)) => Ok(format!("{}{}", m.hw_target(), s)),
            _ => Err(Error::UnsupportedBoard {
                board_type: self.board_type.clone(),
                features: self.features.clone(),
            }),
        }
    }
}

/// The serial ports with the USB IDs of a Jade (it may also be another device using the same
/// serial chip).
pub fn list_ports() -> Result<Vec<String>, Error> {
    let mut ports: Vec<String> = serialport::available_ports()?
        .into_iter()
        .filter(|p| match &p.port_type {
            serialport::SerialPortType::UsbPort(usb) => rpc::USB_IDS.contains(&(usb.vid, usb.pid)),
            _ => false,
        })
        .map(|p| p.port_name)
        // macOS lists each port as /dev/cu.* and /dev/tty.*, the latter waits for a carrier.
        .filter(|name| !name.starts_with("/dev/tty."))
        .collect();
    ports.sort();
    ports.dedup();
    Ok(ports)
}

/// The given port, or the single port with a Jade's USB IDs.
fn find_port(port: Option<&str>) -> Result<String, Error> {
    if let Some(p) = port {
        return Ok(p.to_string());
    }
    let mut ports = list_ports()?;
    match ports.len() {
        0 => Err(Error::NoDevice),
        1 => Ok(ports.remove(0)),
        _ => Err(Error::TooManyDevices(ports)),
    }
}

fn version_info(jade: &mut Jade, port: &str) -> Result<DeviceInfo, Error> {
    let info = jade.call("get_version_info", None, INFO_TIMEOUT)?;
    DeviceInfo::parse(port, &info)
}

/// Get the version info of the Jade on `port`, or of the single connected Jade. Does not require
/// unlocking the device.
pub fn get_info(port: Option<&str>) -> Result<DeviceInfo, Error> {
    let port = find_port(port)?;
    version_info(&mut Jade::open(&port)?, &port)
}

/// Result of [`check_update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    pub info: DeviceInfo,
    pub latest: FirmwareRelease,
    /// Whether the latest release is newer than the installed firmware. `None` if the installed
    /// version could not be parsed.
    pub update_available: Option<bool>,
}

/// Get the device info and the latest firmware release for it.
pub fn check_update(port: Option<&str>) -> Result<UpdateCheck, Error> {
    let info = get_info(port)?;
    let latest = releases::latest_release(&info.hw_target()?, &info.config)?;
    Ok(UpdateCheck {
        update_available: info.version.map(|v| latest.version > v),
        info,
        latest,
    })
}

#[derive(Debug, Clone, Default)]
pub struct UpdateOptions {
    /// The serial port. If `None`, the single port with a Jade's USB IDs.
    pub port: Option<String>,
    /// Install the latest release even if it is not newer than the installed firmware.
    pub force: bool,
}

/// Progress of a firmware update, for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    FetchingIndex,
    Downloading {
        version: Version,
    },
    /// The user must enter their PIN on the device (or set one on a device not set up yet).
    EnterPin,
    /// The user must check that the device shows this version and firmware hash, and confirm
    /// the update on the device. Lasts until the first [`Progress::Uploading`].
    WaitingForConfirmation {
        version: Version,
        fwhash: [u8; 32],
    },
    /// Bytes of the compressed firmware uploaded.
    Uploading {
        done: usize,
        total: usize,
    },
    /// The device checked the firmware and is rebooting. Waiting for it to come back.
    WaitingForReboot,
    Done,
}

/// Outcome of [`update_firmware`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The installed version is the latest release (nothing uploaded).
    AlreadyUpToDate { installed: Version },
    Updated {
        version: Version,
        fwhash: [u8; 32],
        /// The version the device reports after rebooting, `None` if it could not be queried.
        running_version: Option<Version>,
    },
}

/// Update the firmware of the connected Jade to the latest stable release, keeping its config.
///
/// If the device is locked, the user enters their PIN on the device ([`Progress::EnterPin`]).
/// Then the user confirms the update on the device ([`Progress::WaitingForConfirmation`]).
pub fn update_firmware(
    options: &UpdateOptions,
    progress: &mut dyn FnMut(Progress),
) -> Result<UpdateOutcome, Error> {
    let port = find_port(options.port.as_deref())?;
    let mut jade = Jade::open(&port)?;
    let info = version_info(&mut jade, &port)?;
    let hw_target = info.hw_target()?;

    progress(Progress::FetchingIndex);
    let release = releases::latest_release(&hw_target, &info.config)?;
    if !options.force {
        let installed = info.version.ok_or_else(|| {
            Error::Other(format!(
                "could not parse the installed version '{}'",
                info.version_string
            ))
        })?;
        if release.version < installed {
            return Err(Error::Downgrade {
                installed,
                latest: release.version,
            });
        }
        if release.version == installed {
            return Ok(UpdateOutcome::AlreadyUpToDate { installed });
        }
    }
    progress(Progress::Downloading {
        version: release.version,
    });
    let firmware = releases::download(&release)?;

    // As `ota()` in `update_jade_fw.py`.
    if !matches!(info.state, State::Ready | State::Uninitialized) {
        progress(Progress::EnterPin);
        let network = if info.networks == "TEST" {
            "testnet"
        } else {
            "mainnet"
        };
        auth_user(&mut jade, network)?;
    }

    // As `ota_update()` in `jadepy/jade.py`, for a full firmware.
    let params = Value::map([
        ("fwsize", u64::from(release.fwsize).into()),
        ("cmpsize", (firmware.len() as u64).into()),
        ("cmphash", release.cmphash[..].into()),
        ("extended_replies", false.into()),
        ("fwhash", release.fwhash[..].into()),
    ]);
    expect_true(jade.call("ota", Some(params), RPC_TIMEOUT)?, "ota")?;
    progress(Progress::WaitingForConfirmation {
        version: release.version,
        fwhash: release.fwhash,
    });
    let mut done = 0;
    for chunk in firmware.chunks(info.ota_max_chunk) {
        let reply = jade.call("ota_data", Some(chunk.into()), USER_TIMEOUT)?;
        expect_true(reply, "ota_data")?;
        done += chunk.len();
        progress(Progress::Uploading {
            done,
            total: firmware.len(),
        });
    }
    expect_true(
        jade.call("ota_complete", None, RPC_TIMEOUT)?,
        "ota_complete",
    )?;
    drop(jade);

    progress(Progress::WaitingForReboot);
    let running_version = wait_for_reboot(&port, release.version);
    progress(Progress::Done);
    Ok(UpdateOutcome::Updated {
        version: release.version,
        fwhash: release.fwhash,
        running_version,
    })
}

fn expect_true(reply: Value, method: &str) -> Result<(), Error> {
    match reply.as_bool() {
        Some(true) => Ok(()),
        _ => Err(Error::Protocol(format!(
            "unexpected reply to {}: {:?}",
            method, reply
        ))),
    }
}

/// Unlock the device, relaying its requests to the PIN server. See `auth_user()` and `_jadeRpc()`
/// in `jadepy/jade.py`, and `auth_user_process()` in `main/process/auth_user.c`.
fn auth_user(jade: &mut Jade, network: &str) -> Result<(), Error> {
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut method = "auth_user";
    let mut params = Some(Value::map([
        ("network", network.into()),
        ("epoch", epoch.into()),
    ]));
    // The last PIN server error, to report if the device gives up.
    let mut http_error = None;
    loop {
        let result = match jade.call(method, params.take(), USER_TIMEOUT) {
            Ok(r) => r,
            Err(e @ Error::Device { .. }) => return Err(http_error.unwrap_or(e)),
            Err(e) => return Err(e),
        };
        let request = match (result.as_bool(), result.get("http_request")) {
            (Some(true), _) => return Ok(()),
            (Some(false), _) => return Err(Error::IncorrectPin),
            (None, Some(req)) => pinserver::parse_request(req),
            (None, None) => {
                return Err(Error::Protocol(format!(
                    "unexpected reply to auth_user: {:?}",
                    result
                )))
            }
        };
        let request = match request {
            Ok(r) => r,
            Err(e) => {
                // Make the device stop waiting for the server's reply (`handle_pin()` in
                // `pinclient.c`). No reply is sent to a cancel.
                if let Err(ce) = jade.send("cancel", "0", None) {
                    log::warn!("Could not cancel the PIN entry: {}", ce);
                }
                return Err(e);
            }
        };
        // Without parameters, the device offers the user to retry (as jadepy on HTTP errors).
        method = "pin";
        match pinserver::post(&request) {
            Ok(reply) => {
                params = Some(reply);
                http_error = None;
            }
            Err(e) => {
                log::warn!("Error contacting the PIN server: {}", e);
                http_error = Some(e);
            }
        }
    }
}

/// Wait for the device to reboot and return the version it reports, if any.
fn wait_for_reboot(port: &str, expected: Version) -> Option<Version> {
    thread::sleep(REBOOT_DELAY);
    let deadline = Instant::now() + REBOOT_TIMEOUT;
    let mut reported = None;
    while Instant::now() < deadline {
        // The port may be renamed when the device reconnects.
        let port = match list_ports() {
            Ok(ports) if !ports.iter().any(|p| p == port) && ports.len() == 1 => ports[0].clone(),
            _ => port.to_string(),
        };
        match Jade::open(&port).and_then(|mut j| version_info(&mut j, &port)) {
            Ok(info) if info.version == Some(expected) => return info.version,
            Ok(info) => reported = info.version,
            Err(e) => log::debug!("Device not back yet: {}", e),
        }
        thread::sleep(Duration::from_secs(1));
    }
    reported
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_info() -> DeviceInfo {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/data/version_info_reply.cbor"
        ))
        .unwrap();
        let (reply, _) = cbor::decode(&bytes).unwrap();
        DeviceInfo::parse("/dev/ttyACM0", reply.get("result").unwrap()).unwrap()
    }

    #[test]
    fn versions() {
        assert_eq!(Version::parse("1.0.41"), Some(Version::new(1, 0, 41)));
        assert_eq!(
            Version::parse("1.0.36-3-gabcdef-dirty"),
            Some(Version::new(1, 0, 36))
        );
        for s in ["", "1", "1.0", "1.0.x", "1.0.41.2", "v1.0.41", "1..2"] {
            assert_eq!(Version::parse(s), None, "{}", s);
        }
        assert!(Version::new(1, 0, 10) > Version::new(1, 0, 9));
        assert!(Version::new(1, 1, 0) > Version::new(1, 0, 41));
        assert_eq!(Version::new(1, 0, 41).to_string(), "1.0.41");
    }

    #[test]
    fn device_info() {
        let info = fixture_info();
        assert_eq!(
            info,
            DeviceInfo {
                port: "/dev/ttyACM0".into(),
                version: Some(Version::new(1, 0, 36)),
                version_string: "1.0.36".into(),
                board_type: "JADE_V2".into(),
                model: Some(Model::JadePlus),
                features: "SB".into(),
                config: "BLE".into(),
                state: State::Locked,
                networks: "MAIN".into(),
                ota_max_chunk: 4096,
                efusemac: Some("A1B2C3D4E5F6".into()),
            }
        );
        assert_eq!(info.hw_target().unwrap(), "jade2.0");
    }

    #[test]
    fn hw_targets() {
        let mut info = fixture_info();
        for (board, target) in [
            ("JADE", "jade"),
            ("JADE_V1.1", "jade1.1"),
            ("JADE_V2", "jade2.0"),
            ("JADE_V2C", "jade2.0c"),
        ] {
            info.model = Model::from_board_type(board);
            info.features = "SB".into();
            assert_eq!(info.hw_target().unwrap(), target);
            info.features = "DEV".into();
            assert_eq!(info.hw_target().unwrap(), format!("{}dev", target));
            info.features = "OTHER".into();
            assert!(info.hw_target().is_err());
        }
        info.features = "SB".into();
        for board in ["M5FIRE", "TTGO_TDISPLAY", "QEMU", "UNKNOWN", "jade"] {
            info.model = Model::from_board_type(board);
            info.board_type = board.into();
            assert!(matches!(
                info.hw_target(),
                Err(Error::UnsupportedBoard { .. })
            ));
        }
    }

    #[test]
    fn device_info_errors() {
        let parse = |entries: Vec<(&str, Value)>| {
            let v = Value::Map(entries.into_iter().map(|(k, v)| (k.into(), v)).collect());
            DeviceInfo::parse("p", &v)
        };
        let base = || {
            vec![
                ("JADE_VERSION", Value::from("1.0.41")),
                ("JADE_OTA_MAX_CHUNK", 4096u64.into()),
                ("JADE_CONFIG", "NORADIO".into()),
                ("JADE_FEATURES", "SB".into()),
                ("JADE_STATE", "UNINIT".into()),
            ]
        };
        // Older firmwares: no board type, no networks.
        let info = parse(base()).unwrap();
        assert_eq!(info.model, Some(Model::JadeV1));
        assert_eq!(info.state, State::Uninitialized);
        assert_eq!(info.networks, "ALL");
        for chunk in [0u64, 1 << 20] {
            let mut entries = base();
            entries[1].1 = chunk.into();
            assert!(parse(entries).is_err());
        }
        let mut entries = base();
        entries.remove(0);
        assert!(parse(entries).is_err());
    }
}
