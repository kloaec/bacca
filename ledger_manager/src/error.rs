//! The error type, and the status words returned by the device.

use ledger_transport_hidapi::LedgerHIDError;

use std::{error, fmt, io};

// Status words returned by the device. See
// https://github.com/LedgerHQ/ledger-live/blob/4d1d7bb3462fd0c986ed587f0cf426afc96850c8/libs/ledgerjs/packages/errors/src/index.ts#L233
pub(crate) const SW_OK: u16 = 0x9000;
pub(crate) const SW_LOCKED: u16 = 0x5515;
pub(crate) const SW_USER_REFUSED: u16 = 0x5501;
pub(crate) const SW_CONDITIONS_NOT_SATISFIED: u16 = 0x6985;
pub(crate) const SW_NOT_ENOUGH_SPACE: u16 = 0x5102;
pub(crate) const SW_CLA_NOT_SUPPORTED: u16 = 0x6e00;
pub(crate) const SW_CLA_NOT_SUPPORTED_BOOTLOADER: u16 = 0x6e01;
pub(crate) const SW_INS_NOT_SUPPORTED: u16 = 0x6d00;
pub(crate) const SW_RECOVERY_MODE: u16 = 0x662f;
// Returned by GetVersion when the device isn't set up yet (see hw/getDeviceInfo.ts).
pub(crate) const SW_NOT_ONBOARDED: u16 = 0x6d07;
pub(crate) const SW_NOT_ONBOARDED_LEGACY: u16 = 0x6d06;

/// An error from this library.
#[derive(Debug)]
pub enum Error {
    /// Error communicating with the device through HID.
    Hid(LedgerHIDError),
    DeviceNotFound,
    /// The device returned an unexpected status word.
    DeviceStatus(u16),
    DeviceLocked,
    DeviceNotOnboarded,
    /// The device must be on its dashboard (no app open, not in bootloader or updater mode).
    DeviceOnDashboardExpected,
    /// The device is in bootloader mode, most likely because a firmware update was interrupted.
    DeviceInBootloader,
    /// The user refused this ("The firmware update", ...) on the device.
    RefusedOnDevice(&'static str),
    NotEnoughSpace,
    AppAlreadyInstalled,
    /// The app needs another app to be installed first.
    AppDependencyMissing,
    AppNotInstalled,
    AppAlreadyLatest,
    /// Could not parse data returned by the device.
    InvalidDeviceData(String),
    Http(minreq::Error),
    /// The Ledger API returned an unexpected HTTP status.
    Api {
        status: u16,
        url: String,
    },
    WebSocket(Box<tungstenite::Error>),
    /// Ledger's HSM sent an error message.
    Hsm(String),
    Json(serde_json::Error),
    Io(io::Error),
    /// The genuine check failed: the device is NOT genuine. Contains the result payload.
    NotGenuine(String),
    /// Timed out doing this ("waiting for the device", ...).
    Timeout(&'static str),
    InvalidBackup(String),
    /// The backup of the device settings could not be saved to a file.
    BackupNotSaved(String),
    /// Any other error, described by its message.
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hid(e) => write!(f, "HID communication error: {}", e),
            Error::DeviceNotFound => write!(f, "No Ledger device found."),
            Error::DeviceStatus(s) => write!(f, "Device returned status {:#06x}", s),
            Error::DeviceLocked => write!(f, "Device is locked. Please unlock it."),
            Error::DeviceNotOnboarded => write!(f, "Device is not set up yet."),
            Error::DeviceOnDashboardExpected => write!(
                f,
                "Device must be on its dashboard. Please quit any application."
            ),
            Error::DeviceInBootloader => write!(
                f,
                "Device is in bootloader mode: a firmware update was probably interrupted. Repair the firmware (e.g. with the 'repairfirm' command) to complete the update."
            ),
            Error::RefusedOnDevice(what) => write!(f, "{} was refused on the device.", what),
            Error::NotEnoughSpace => write!(
                f,
                "Not enough space on the device. Uninstall some applications and retry."
            ),
            Error::AppAlreadyInstalled => write!(f, "Application is already installed."),
            Error::AppDependencyMissing => {
                write!(f, "Application requires another application to be installed first.")
            }
            Error::AppNotInstalled => write!(f, "Application is not installed."),
            Error::AppAlreadyLatest => write!(f, "Application is already at the latest version."),
            Error::InvalidDeviceData(s) => write!(f, "Invalid data from device: {}", s),
            Error::Http(e) => write!(f, "HTTP error: {}", e),
            Error::Api { status, url } => {
                write!(f, "Ledger API returned HTTP status {} for {}", status, url)
            }
            Error::WebSocket(e) => write!(f, "Websocket error: {}", e),
            Error::Hsm(s) => write!(f, "Ledger HSM returned an error: {}", s),
            Error::Json(e) => write!(f, "JSON error: {}", e),
            Error::Io(e) => write!(f, "IO error: {}", e),
            Error::NotGenuine(p) => write!(
                f,
                "Genuine check failed: this device is NOT genuine (result: '{}').",
                p
            ),
            Error::Timeout(s) => write!(f, "Timed out {}", s),
            Error::InvalidBackup(s) => write!(f, "Invalid backup: {}", s),
            Error::BackupNotSaved(s) => write!(
                f,
                "Could not save the backup of the device settings: {}",
                s
            ),
            Error::Other(s) => write!(f, "{}", s),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Hid(e) => Some(e),
            Error::Http(e) => Some(e),
            Error::WebSocket(e) => Some(e.as_ref()),
            Error::Json(e) => Some(e),
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<LedgerHIDError> for Error {
    fn from(e: LedgerHIDError) -> Self {
        match e {
            LedgerHIDError::DeviceNotFound => Error::DeviceNotFound,
            e => Error::Hid(e),
        }
    }
}

impl From<ledger_transport_hidapi::hidapi::HidError> for Error {
    fn from(e: ledger_transport_hidapi::hidapi::HidError) -> Self {
        Error::Hid(LedgerHIDError::Hid(e))
    }
}

impl From<minreq::Error> for Error {
    fn from(e: minreq::Error) -> Self {
        Error::Http(e)
    }
}

impl From<tungstenite::Error> for Error {
    fn from(e: tungstenite::Error) -> Self {
        Error::WebSocket(Box::new(e))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Json(e)
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
