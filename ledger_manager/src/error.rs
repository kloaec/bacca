//! Errors and device status codes.

use ledger_transport_hidapi::LedgerHIDError;

use std::{error, fmt, io};

/// The return code when sending an APDU command to a Ledger device. Taken from
/// https://github.com/LedgerHQ/ledger-live/blob/4d1d7bb3462fd0c986ed587f0cf426afc96850c8/libs/ledgerjs/packages/errors/src/index.ts#L233
/// (now published from https://github.com/LedgerHQ/ts-libs, packages/errors/src/index.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusCode {
    //ACCESS_CONDITION_NOT_FULFILLED = 0x9804,
    //ALGORITHM_NOT_SUPPORTED = 0x9484,
    ClaNotSupported = 0x6e00,
    ClaNotSupportedBootloader = 0x6e01,
    //CODE_BLOCKED = 0x9840,
    //CODE_NOT_INITIALIZED = 0x9802,
    //COMMAND_INCOMPATIBLE_FILE_STRUCTURE = 0x6981,
    ConditionsOfUseNotSatisfied = 0x6985,
    //CONTRADICTION_INVALIDATION = 0x9810,
    //CONTRADICTION_SECRET_CODE_STATUS = 0x9808,
    DeviceInRecoveryMode = 0x662f,
    //CUSTOM_IMAGE_EMPTY = 0x662e,
    //FILE_ALREADY_EXISTS = 0x6a89,
    //FILE_NOT_FOUND = 0x9404,
    //GP_AUTH_FAILED = 0x6300,
    //HALTED = 0x6faa,
    //INCONSISTENT_FILE = 0x9408,
    IncorrectData = 0x6a80,
    //INCORRECT_LENGTH = 0x6700,
    //INCORRECT_P1_P2 = 0x6b00,
    InsNotSupported = 0x6d00,
    /// Returned by GetVersion by some firmwares when the device isn't set up yet. See
    /// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/hw/getDeviceInfo.ts
    DeviceNotOnboardedLegacy = 0x6d06,
    DeviceNotOnboarded = 0x6d07,
    DeviceNotOnboarded2 = 0x6611,
    //INVALID_KCV = 0x9485,
    //INVALID_OFFSET = 0x9402,
    //LICENSING = 0x6f42,
    LockedDevice = 0x5515,
    //MAX_VALUE_REACHED = 0x9850,
    //MEMORY_PROBLEM = 0x9240,
    //MISSING_CRITICAL_PARAMETER = 0x6800,
    //NO_EF_SELECTED = 0x9400,
    NotEnoughMemorySpace = 0x6a84,
    OK = 0x9000,
    //PIN_REMAINING_ATTEMPTS = 0x63c0,
    //REFERENCED_DATA_NOT_FOUND = 0x6a88,
    SecurityStatusNotSatisfied = 0x6982,
    //TECHNICAL_PROBLEM = 0x6f00,
    //UNKNOWN_APDU = 0x6d02,
    UserRefusedOnDevice = 0x5501,
    NotEnoughSpace = 0x5102,
}

/// The context in which a websocket session with Ledger's HSM is used. It is used to interpret
/// errors the same way Ledger Live does (`remapSocketError` in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/api.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketContext {
    GenuineCheck,
    InstallApp,
    UninstallApp,
    /// Installing an OSU or a final firmware.
    Firmware,
    /// Flashing the MCU or the bootloader.
    Mcu,
    /// Anything else, no error remapping.
    Other,
}

/// An error from this library.
#[derive(Debug)]
pub enum Error {
    /// Error communicating with the device through HID.
    Hid(LedgerHIDError),
    /// The device returned an unexpected status code.
    DeviceStatus(u16),
    /// The device is locked. Unlock it and retry.
    DeviceLocked,
    /// The device is not set up (onboarded) yet.
    DeviceNotOnboarded,
    /// The device must be on its dashboard (no app opened, not in bootloader or updater mode).
    DeviceOnDashboardExpected,
    /// The device is in bootloader mode, most likely because a firmware update was interrupted
    /// while flashing the MCU or the bootloader. Use `firmware::repair_firmware` to complete it.
    DeviceInBootloader,
    /// The device shows "MCU not genuine" and must be brought back to its dashboard before the
    /// firmware update can be performed (Ledger Live's `MCUNotGenuineToDashboard`).
    McuNotGenuineToDashboard,
    /// The device was expected to be in the OS updater (OSU) mode.
    DeviceInOsuExpected,
    /// The device was expected to be in bootloader mode.
    DeviceOnBootloaderExpected,
    /// No Ledger device found.
    DeviceNotFound,
    /// The user refused the operation on the device.
    UserRefusedOnDevice,
    /// The user refused to allow the Ledger manager on the device.
    UserRefusedAllowManager,
    /// The user refused the firmware update on the device.
    UserRefusedFirmwareUpdate,
    /// Not enough space on the device to install the application. Uninstall some apps first.
    NotEnoughSpace,
    /// Not enough space on the device to install the firmware update. Uninstall some apps first.
    FirmwareNotEnoughSpace,
    /// The application is already installed.
    AppAlreadyInstalled,
    /// The application depends on another application that must be installed first.
    AppDependencyInstallRequired,
    /// Other applications depend on this application, they must be uninstalled first.
    AppDependencyUninstallRequired,
    /// Could not parse data returned by the device.
    InvalidDeviceData(String),
    /// Error from the HTTP client.
    Http(minreq::Error),
    /// The Ledger API returned an unexpected HTTP status.
    Api { status: u16, url: String },
    /// The Ledger API did not recognize the firmware of this device.
    FirmwareNotRecognized,
    /// The current MCU version of the device is unknown to the Ledger API.
    UnknownMcu,
    /// Could not find which MCU or bootloader to flash.
    McuVersionNotFound,
    /// The MCU or bootloader was flashed too many times without leaving the bootloader.
    TooManyMcuOrBootloaderFlashes,
    /// Firmware update for this model or firmware version is not supported.
    FirmwareUpdateNotSupported(String),
    /// Error from the websocket connection.
    WebSocket(Box<tungstenite::Error>),
    /// The websocket connection closed before the operation completed.
    WebSocketClosed,
    /// The HSM sent an error message.
    Hsm(String),
    /// The HSM sent an unexpected message.
    UnexpectedHsmMessage(String),
    /// JSON (de)serialization error.
    Json(serde_json::Error),
    /// The genuine check failed: the device is NOT genuine. Contains the result payload.
    NotGenuine(String),
    /// Timed out waiting for the device.
    Timeout(&'static str),
    /// IO error.
    Io(io::Error),
    /// The device is in recovery mode.
    DeviceInRecoveryMode,
    /// No language pack for this language is available for the firmware of the device.
    LanguageNotFound(String),
    /// The user refused the installation of the language pack on the device.
    LanguageInstallRefusedOnDevice,
    /// The user refused to load the lock screen picture on the device.
    ImageLoadRefusedOnDevice,
    /// The user refused to confirm the new lock screen picture on the device.
    ImageCommitRefusedOnDevice,
    /// The lock screen picture is not in the expected format for this device.
    InvalidImage(String),
    /// The backup of the device settings is invalid or was made for another device.
    InvalidBackup(String),
    /// The backup of the device settings could not be saved to a file.
    BackupNotSaved(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Hid(e) => write!(f, "HID communication error: {}", e),
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
            Error::McuNotGenuineToDashboard => write!(
                f,
                "Device must be on its dashboard to be updated. Disconnect and reconnect the USB cable without pressing any button, then press both buttons together three times to display the dashboard, and update the firmware."
            ),
            Error::DeviceInOsuExpected => write!(f, "Device was expected to be in updater mode."),
            Error::DeviceOnBootloaderExpected => {
                write!(f, "Device was expected to be in bootloader mode.")
            }
            Error::DeviceNotFound => write!(f, "No Ledger device found."),
            Error::UserRefusedOnDevice => write!(f, "Operation refused on the device."),
            Error::UserRefusedAllowManager => {
                write!(f, "Ledger manager was not allowed on the device.")
            }
            Error::UserRefusedFirmwareUpdate => {
                write!(f, "Firmware update was refused on the device.")
            }
            Error::NotEnoughSpace => write!(
                f,
                "Not enough space on the device. Uninstall some applications and retry."
            ),
            Error::FirmwareNotEnoughSpace => write!(
                f,
                "Not enough space on the device for the firmware update. Uninstall some applications and retry."
            ),
            Error::AppAlreadyInstalled => write!(f, "Application is already installed."),
            Error::AppDependencyInstallRequired => write!(
                f,
                "Application requires another application to be installed first."
            ),
            Error::AppDependencyUninstallRequired => write!(
                f,
                "Other applications depend on this one. Uninstall them first."
            ),
            Error::InvalidDeviceData(s) => write!(f, "Invalid data from device: {}", s),
            Error::Http(e) => write!(f, "HTTP error: {}", e),
            Error::Api { status, url } => {
                write!(f, "Ledger API returned HTTP status {} for {}", status, url)
            }
            Error::FirmwareNotRecognized => {
                write!(f, "The Ledger API did not recognize this device's firmware.")
            }
            Error::UnknownMcu => write!(f, "The device's MCU version is unknown to the Ledger API."),
            Error::McuVersionNotFound => write!(f, "Could not find the MCU version to flash."),
            Error::TooManyMcuOrBootloaderFlashes => write!(
                f,
                "The MCU or bootloader was flashed too many times without success."
            ),
            Error::FirmwareUpdateNotSupported(s) => {
                write!(f, "Firmware update not supported: {}", s)
            }
            Error::WebSocket(e) => write!(f, "Websocket error: {}", e),
            Error::WebSocketClosed => write!(f, "Websocket connection closed unexpectedly."),
            Error::Hsm(s) => write!(f, "Ledger HSM returned an error: {}", s),
            Error::UnexpectedHsmMessage(s) => write!(f, "Unexpected message from HSM: {}", s),
            Error::Json(e) => write!(f, "JSON error: {}", e),
            Error::NotGenuine(p) => write!(
                f,
                "Genuine check failed: this device is NOT genuine (result: '{}').",
                p
            ),
            Error::Timeout(s) => write!(f, "Timed out {}", s),
            Error::Io(e) => write!(f, "IO error: {}", e),
            Error::DeviceInRecoveryMode => write!(f, "Device is in recovery mode."),
            Error::LanguageNotFound(l) => write!(
                f,
                "No '{}' language pack is available for the firmware of the device.",
                l
            ),
            Error::LanguageInstallRefusedOnDevice => {
                write!(f, "The language installation was refused on the device.")
            }
            Error::ImageLoadRefusedOnDevice => {
                write!(f, "Loading the lock screen picture was refused on the device.")
            }
            Error::ImageCommitRefusedOnDevice => write!(
                f,
                "The new lock screen picture was not confirmed on the device."
            ),
            Error::InvalidImage(s) => write!(f, "Invalid lock screen picture: {}", s),
            Error::InvalidBackup(s) => write!(f, "Invalid backup: {}", s),
            Error::BackupNotSaved(s) => write!(
                f,
                "Could not save the backup of the device settings: {}",
                s
            ),
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

/// Interpret a status code (as an hex string, as in Ledger Live) according to the context of the
/// socket session. Returns `None` if there is no specific interpretation.
///
/// Ported from `remapSocketError` in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/manager/api.ts
/// and `remapSocketFirmwareError` in
/// https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/src/deviceSDK/commands/firmwareUpdate/installFirmware.ts
/// along with the dependency errors of hw/installApp.ts and hw/uninstallApp.ts.
pub(crate) fn remap_status(status: &str, context: SocketContext) -> Option<Error> {
    let is_firmware = matches!(context, SocketContext::Firmware | SocketContext::Mcu);
    Some(match status {
        "6a83" | "6811" if context == SocketContext::InstallApp => {
            Error::AppDependencyInstallRequired
        }
        "6a83" if context == SocketContext::UninstallApp => Error::AppDependencyUninstallRequired,
        "6a80" | "6a81" | "6a8e" | "6a8f" if !is_firmware => Error::AppAlreadyInstalled,
        "6982" | "5303" => Error::DeviceLocked,
        "6a84" | "5103" => {
            if is_firmware {
                Error::FirmwareNotEnoughSpace
            } else {
                Error::NotEnoughSpace
            }
        }
        "6a85" | "5102" => {
            if is_firmware {
                Error::UserRefusedFirmwareUpdate
            } else {
                Error::NotEnoughSpace
            }
        }
        "6985" | "5501" => {
            if is_firmware {
                Error::UserRefusedFirmwareUpdate
            } else {
                // NOTE: Ledger Live maps these to a "not enough space" error outside of firmware
                // updates. They are the "conditions of use not satisfied" and "user refused" status
                // codes, so we report them as a refusal instead.
                Error::UserRefusedOnDevice
            }
        }
        "5515" => Error::DeviceLocked,
        _ => return None,
    })
}

/// Remap an error arising from a socket session, like Ledger Live's `remapSocketError`.
pub(crate) fn remap_socket_error(e: Error, context: SocketContext) -> Error {
    if context == SocketContext::Other || context == SocketContext::GenuineCheck {
        return e;
    }
    let status = match &e {
        Error::DeviceStatus(s) => format!("{:04x}", s),
        // Ledger Live looks at the last 4 characters of the error message, which for errors sent
        // by the HSM is the data it sent.
        Error::Hsm(msg) => {
            if msg.starts_with("invalid literal") {
                return Error::DeviceOnDashboardExpected;
            }
            let len = msg.len();
            match msg.get(len.saturating_sub(4)..) {
                Some(s) => s.to_lowercase(),
                None => return e,
            }
        }
        _ => return e,
    };
    remap_status(&status, context).unwrap_or(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remapping() {
        assert!(matches!(
            remap_socket_error(Error::DeviceStatus(0x5501), SocketContext::Firmware),
            Error::UserRefusedFirmwareUpdate
        ));
        assert!(matches!(
            remap_socket_error(Error::DeviceStatus(0x5102), SocketContext::InstallApp),
            Error::NotEnoughSpace
        ));
        assert!(matches!(
            remap_socket_error(Error::DeviceStatus(0x6a84), SocketContext::Mcu),
            Error::FirmwareNotEnoughSpace
        ));
        assert!(matches!(
            remap_socket_error(Error::DeviceStatus(0x6a83), SocketContext::InstallApp),
            Error::AppDependencyInstallRequired
        ));
        assert!(matches!(
            remap_socket_error(Error::DeviceStatus(0x6a83), SocketContext::UninstallApp),
            Error::AppDependencyUninstallRequired
        ));
        assert!(matches!(
            remap_socket_error(
                Error::Hsm("Something 6a80".into()),
                SocketContext::InstallApp
            ),
            Error::AppAlreadyInstalled
        ));
        assert!(matches!(
            remap_socket_error(
                Error::Hsm("invalid literal for int()".into()),
                SocketContext::InstallApp
            ),
            Error::DeviceOnDashboardExpected
        ));
        assert!(matches!(
            remap_socket_error(Error::DeviceStatus(0x6d00), SocketContext::InstallApp),
            Error::DeviceStatus(0x6d00)
        ));
        assert!(matches!(
            remap_socket_error(Error::DeviceStatus(0x5501), SocketContext::Other),
            Error::DeviceStatus(0x5501)
        ));
    }
}
