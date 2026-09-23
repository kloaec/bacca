//! Ledger Manager.
//!
//! This implements utility functions to manage the applications installed on your Ledger device
//! and to update its firmware. This is performed by both talking to the Ledger device connected by
//! USB but also by making HTTP request to the Ledger API used by Ledger Live, and by relaying
//! commands from Ledger's HSM to the device through a websocket.
//!
//! Supported devices: Ledger Nano S, Nano S Plus, Nano X, Stax, Flex and Nano Gen5.

pub use ledger_apdu;
pub use ledger_transport_hidapi;

pub mod api;
pub mod apps;
pub mod device;
pub mod error;
pub mod firmware;
mod hid;
pub mod model;
pub mod socket;
pub mod version;

pub use api::{
    bitcoin_apps_by_hashes, current_firmware, fetch_mcus, get_current_firmware, get_current_osu,
    get_device_version, get_final_firmware_by_id, get_latest_firmware, latest_firmware,
    BitcoinAppInfo, DeviceVersion, FinalFirmware, FirmwareInfo, FirmwareUpdateInfo, McuVersion,
    OsuFirmware,
};
pub use apps::{
    bitcoin_app_installed, bitcoin_app_name, bitcoin_latest_app, genuine_check,
    genuine_check_with_events, get_latest_apps, install_app, install_bitcoin_app,
    install_bitcoin_app_with_progress, is_app_update_available, is_bitcoin_app_installed,
    list_installed_apps, list_installed_apps_raw, open_bitcoin_app, uninstall_app,
    update_bitcoin_app, update_bitcoin_app_with_progress, AppInstallStep, InstallErr, InstalledApp,
    UpdateErr, BITCOIN_APP_NAME, BITCOIN_TEST_APP_NAME,
};
pub use device::{
    connect, get_app_and_version, list_ledger_devices, open_device, quit_app, wait_for_device,
    AppAndVersion, DeviceInfo, LedgerHidDevice,
};
pub use error::{Error, SocketContext, StatusCode};
pub use firmware::{
    check_firmware_update_supported, firmware_update_resets_customization,
    firmware_update_will_uninstall_apps, format_hash_name, update_firmware,
    update_firmware_with_options, FirmwareUpdateOptions, FirmwareUpdateStep,
};
pub use model::{DeviceModel, LEDGER_USB_VENDOR_ID};
pub use socket::{query_via_websocket, run_device_socket, SocketEvent};

/// The Ledger Live API requires request to set their claimed version of Ledger Live. This is the
/// version of ledger-live-common at the time of writing
/// (https://github.com/LedgerHQ/ledger-live/blob/develop/libs/ledger-live-common/package.json).
pub const LIVE_COMMON_VERSION: &str = "38.0.0";

/// The Ledger Live API has multiple channels to download binaries. This sets which one to use. 1
/// is default. 4 is "shitcoins". The rest is unclear. Defined here:
/// https://github.com/LedgerHQ/ledger-live/blob/4d1d7bb3462fd0c986ed587f0cf426afc96850c8/libs/device-core/src/managerApi/use-cases/getProviderIdUseCase.ts#L3-L9
/// This is the default provider, a device with a firmware version suffixed with the name of
/// another provider will use that one instead (see `DeviceInfo::provider_id`).
pub const PROVIDER: u32 = 1;

pub const BASE_API_V1_URL: &str = "https://manager.api.live.ledger.com/api";
pub const BASE_API_V2_URL: &str = "https://manager.api.live.ledger.com/api/v2";
pub const BASE_SOCKET_URL: &str = "wss://scriptrunner.api.live.ledger.com/update";
