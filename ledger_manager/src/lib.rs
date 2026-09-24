//! Ledger Manager: install and update the Bitcoin apps and the firmware of a Ledger device.
//!
//! This talks to the device connected by USB, makes HTTP requests to the Ledger API used by
//! Ledger Live, and relays commands from Ledger's HSM to the device through a websocket.
//!
//! Supported devices: Ledger Nano S, Nano S Plus, Nano X, Stax, Flex and Nano Gen5.
//!
//! The modules, from the lowest level:
//! - `device`: finding the device, its model and information (GetVersion);
//! - `hid`: a HID transport with a timeout, for the firmware updates;
//! - `api`: the Ledger Manager API;
//! - `socket`: the websocket sessions with Ledger's HSM;
//! - `apps`: the Bitcoin apps and the genuine check;
//! - `firmware`: the firmware update and repair;
//! - `language`, `lock_screen` and `restore`: backing up the settings of the device before a
//!   firmware update and restoring them after.

pub use ledger_transport_hidapi;

mod api;
mod apps;
mod device;
mod error;
mod firmware;
mod hid;
mod language;
mod lock_screen;
mod restore;
mod socket;

pub use api::{
    apps_by_hashes, current_firmware, fetch_mcus, latest_firmware, AppInfo, FirmwareUpdateInfo,
};
pub use apps::{
    genuine_check, get_latest_apps, install_bitcoin_app, list_installed_apps,
    list_installed_apps_raw, open_bitcoin_app, update_bitcoin_app, AppInstallStep, InstalledApp,
    BITCOIN_APP_NAME, BITCOIN_TEST_APP_NAME,
};
pub use device::{list_ledger_devices, open_device, DeviceInfo, DeviceModel};
pub use error::Error;
pub use firmware::{
    check_firmware_update_supported, firmware_update_resets_customization, repair_firmware,
    update_firmware, FirmwareUpdateStep,
};
pub use language::{language_packages_for_device, LanguageInstallStep, LanguagePackage};
pub use lock_screen::LoadImageStep;
pub use restore::{
    default_backup_dir, find_latest_backup, load_backup, restore_device_settings,
    update_firmware_and_restore, BackupStep, DeviceBackup, RestoreOutcome, RestoreReport,
    RestoreStep, UpdateAndRestoreResult, UpdateAndRestoreStep,
};
pub use socket::SocketEvent;
