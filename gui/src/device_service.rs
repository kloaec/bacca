//! The service talking to the hardware wallet, on behalf of the GUI.
//!
//! The device libraries are blocking (USB and HTTP I/O, waiting for the user to confirm on the
//! device, a firmware update takes minutes). Every interaction with a device is therefore run in
//! a `tokio::task::spawn_blocking` task, which reports its progress to the GUI directly through
//! the (unbounded) channel and returns its result to the service loop. Only one such task runs at
//! a time: while an operation is running the device is not polled.

use crate::listener;
use crate::{bitbox, gui::Message, gui::Message::DeviceServiceMsg, ledger, service::ServiceFn};

use bitbox_manager::{DeviceHandle, Edition};
use ledger_manager::{ledger_transport_hidapi::hidapi::HidApi, FirmwareUpdateInfo};

use std::fmt::{Display, Formatter};
use std::time::Duration;

listener!(DeviceListener, DeviceMessage, Message, DeviceServiceMsg);

/// How often to look for a connected device.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub const CONNECT_HINT: &str = "Please connect your Ledger or BitBox device and unlock it...";

#[derive(Debug, Clone, Default)]
pub enum Version {
    Installed(String),
    Latest(String),
    NotInstalled,
    #[default]
    None,
}

impl Display for Version {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Version::Installed(version) => write!(f, "{}", version),
            Version::Latest(version) => write!(f, "{}", version),
            Version::NotInstalled => write!(f, "Not installed!"),
            Version::None => write!(f, " - "),
        }
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.to_string() == other.to_string()
    }
}

/// The latest firmware available for the connected device.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum LatestFirmware {
    /// Not queried (yet), or the query failed.
    #[default]
    Unknown,
    /// The device runs the latest firmware.
    UpToDate,
    /// This version can be installed.
    Available(String),
    /// This version is available but can't be installed by Bacca (the reason is displayed in the
    /// status message).
    Unsupported(String),
}

/// The mode a Ledger device is running in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LedgerMode {
    #[default]
    Normal,
    /// The OS updater: a firmware update is in progress (or was interrupted).
    Updater,
    Bootloader,
}

/// What the GUI displays about a connected Ledger.
#[derive(Debug, Clone, Default)]
pub struct LedgerState {
    pub model: Option<String>,
    /// The firmware version. `None` if the device could not be queried (e.g. locked).
    pub firmware: Option<String>,
    pub mode: LedgerMode,
    pub latest_firmware: LatestFirmware,
    /// Whether the firmware update may reset the language and the custom lock screen.
    pub update_resets_customization: bool,
    pub mainnet: Version,
    pub testnet: Version,
    pub latest_mainnet: Version,
    pub latest_testnet: Version,
    pub genuine: Option<bool>,
}

impl LedgerState {
    /// Whether the device could be queried, and is running its OS normally.
    pub fn is_ready(&self) -> bool {
        self.firmware.is_some() && self.mode == LedgerMode::Normal
    }
}

/// What the GUI displays about a connected BitBox.
#[derive(Debug, Clone, Default)]
pub struct BitboxState {
    /// The platform: BitBox02 or BitBox02 Nova.
    pub product: String,
    pub edition: Option<Edition>,
    /// The firmware version, or a description of the bootloader state. `None` if the device
    /// could not be queried.
    pub firmware: Option<String>,
    /// Whether the device is in bootloader mode.
    pub bootloader: bool,
    /// Whether the device is set up, if known.
    pub initialized: Option<bool>,
    pub latest_firmware: LatestFirmware,
}

/// The device connected, as displayed by the GUI.
#[derive(Debug, Clone, Default)]
pub enum DeviceState {
    #[default]
    None,
    Ledger(Box<LedgerState>),
    Bitbox(BitboxState),
}

/// A device found when enumerating the USB HID devices.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    /// Identified by its HID path.
    Ledger(String),
    Bitbox(DeviceHandle),
}

impl Detected {
    fn key(&self) -> String {
        match self {
            Detected::Ledger(path) => format!("ledger:{}", path),
            // The mode is part of the key: the device information must be queried again when it
            // switches between firmware and bootloader.
            Detected::Bitbox(h) => format!("bitbox:{:?}:{}", h.mode, h.path.to_string_lossy()),
        }
    }
}

/// The result of a blocking task, returned to the service loop.
#[derive(Debug, Clone)]
pub enum TaskResult {
    /// The device found (if any), and whether it is the one already loaded.
    Probe {
        device: Option<Detected>,
        same: bool,
    },
    /// The information about a newly detected device. `ok` is false if it could not be fully
    /// queried (e.g. the device is locked), in which case it is queried again at the next poll.
    Loaded {
        key: String,
        ok: bool,
        state: DeviceState,
        ledger_update: Option<Box<FirmwareUpdateInfo>>,
    },
    /// Result of the genuine check.
    Genuine(Option<bool>),
    /// An operation (app install, firmware update, ...) completed. If `reload` is set the device
    /// information is queried again, and `message` displayed afterwards.
    Operation {
        reload: bool,
        message: Option<(String, bool)>,
    },
    /// The backup of the device settings could not be saved to a file: the firmware update was
    /// not started.
    BackupNotSaved(String),
    /// The task panicked.
    Failed,
}

#[derive(Debug, Clone)]
pub enum DeviceMessage {
    // From the GUI.
    InstallApp {
        testnet: bool,
    },
    UpdateApp {
        testnet: bool,
    },
    GenuineCheck,
    UpdateFirmware,
    /// Update the firmware of a Ledger without saving the backup of its settings to a file (it is
    /// only kept in memory).
    UpdateFirmwareWithoutBackupFile,
    /// Resume a Ledger firmware update interrupted while the device was in bootloader mode.
    RepairFirmware,

    // To the GUI.
    State(DeviceState),
    /// A status message, and whether it is an error.
    Status(String, bool),
    /// Progress of the current step, between 0 and 1.
    Progress(Option<f32>),
    /// Information to keep displayed during the operation (e.g. a hash to compare with the
    /// device's screen).
    Info(Option<String>),
    /// Whether an operation is running (the GUI must not allow to start another one).
    Busy(bool),
    /// The backup of the Ledger settings could not be saved, the firmware update was not started.
    BackupNotSaved(String),

    // Internal.
    Poll,
    TaskDone(Box<TaskResult>),
}

/// Used by the blocking tasks to report to the GUI.
#[derive(Clone)]
pub struct Reporter {
    sender: Sender<DeviceMessage>,
}

impl Reporter {
    fn send(&self, msg: DeviceMessage) {
        log::debug!("Reporter::send({:?})", &msg);
        // The channel is unbounded: this never blocks, and keeps the messages ordered.
        if self.sender.try_send(msg).is_err() {
            log::debug!("Reporter::send() -> Fail to send Message")
        }
    }

    pub fn status(&self, msg: impl Into<String>) {
        self.send(DeviceMessage::Status(msg.into(), false));
    }

    pub fn alarm(&self, msg: impl Into<String>) {
        self.send(DeviceMessage::Status(msg.into(), true));
    }

    pub fn progress(&self, progress: Option<f32>) {
        self.send(DeviceMessage::Progress(progress));
    }

    pub fn info(&self, info: Option<String>) {
        self.send(DeviceMessage::Info(info));
    }

    pub fn state(&self, state: DeviceState) {
        self.send(DeviceMessage::State(state));
    }
}

/// Enumerate the HID devices and return the device to use. The already loaded device (if still
/// connected) takes precedence.
fn probe(loaded: Option<String>) -> TaskResult {
    let api = match HidApi::new() {
        Ok(a) => a,
        Err(e) => {
            log::error!("Error initializing HID api: {}.", e);
            return TaskResult::Probe {
                device: None,
                same: false,
            };
        }
    };
    let found: Vec<Detected> = ledger_manager::list_ledger_devices(&api)
        .into_iter()
        .map(Detected::Ledger)
        .chain(
            bitbox_manager::list_devices(&api)
                .into_iter()
                .map(Detected::Bitbox),
        )
        .collect();
    if let Some(d) = found.iter().find(|d| Some(d.key()) == loaded) {
        return TaskResult::Probe {
            device: Some(d.clone()),
            same: true,
        };
    }
    TaskResult::Probe {
        device: found.into_iter().next(),
        same: false,
    }
}

fn load(device: Detected, reporter: &Reporter) -> TaskResult {
    let key = device.key();
    match device {
        Detected::Ledger(_) => {
            let (ok, state, update) = ledger::load(reporter);
            TaskResult::Loaded {
                key,
                ok,
                state: DeviceState::Ledger(Box::new(state)),
                ledger_update: update.map(Box::new),
            }
        }
        Detected::Bitbox(handle) => {
            let (ok, state) = bitbox::load(&handle, reporter);
            TaskResult::Loaded {
                key,
                ok,
                state: DeviceState::Bitbox(state),
                ledger_update: None,
            }
        }
    }
}

pub struct DeviceService {
    sender: Sender<DeviceMessage>,
    receiver: Receiver<DeviceMessage>,
    loopback: Sender<DeviceMessage>,
    /// What the GUI is currently displaying.
    state: DeviceState,
    /// The key of the device whose information is loaded.
    loaded: Option<String>,
    /// The firmware update available for the loaded Ledger.
    ledger_update: Option<Box<FirmwareUpdateInfo>>,
    /// Result of the genuine check of the connected Ledger.
    genuine: Option<bool>,
    /// Whether a blocking task is running.
    task_running: bool,
    /// Whether the GUI was told an operation is running.
    busy: bool,
    /// An operation requested while a probe was running.
    pending: Option<DeviceMessage>,
    /// A message to display once the device information was reloaded after an operation.
    after_reload: Option<(String, bool)>,
}

impl DeviceService {
    pub fn start(mut self) {
        tokio::spawn(async move {
            self.run().await;
        });
    }

    fn send_to_gui(&self, msg: DeviceMessage) {
        log::info!("DeviceService::send_to_gui({:?})", &msg);
        if self.sender.try_send(msg).is_err() {
            log::debug!("DeviceService.send_to_gui() -> Fail to send Message")
        }
    }

    fn set_busy(&mut self, busy: bool) {
        if self.busy != busy {
            self.busy = busy;
            self.send_to_gui(DeviceMessage::Busy(busy));
        }
    }

    /// An operation requested by the GUI (which then considers itself busy) is not performed.
    fn drop_operation(&mut self, op: &DeviceMessage) {
        log::debug!("DeviceService: not performing {:?}", op);
        self.busy = false;
        self.send_to_gui(DeviceMessage::Busy(false));
    }

    fn send_state(&mut self, mut state: DeviceState) {
        if let DeviceState::Ledger(l) = &mut state {
            l.genuine = self.genuine;
        }
        self.state = state.clone();
        self.send_to_gui(DeviceMessage::State(state));
    }

    /// Run `task` in a blocking thread. Its result is sent back to the service loop.
    fn spawn_task<F>(&mut self, task: F)
    where
        F: FnOnce(Reporter) -> TaskResult + Send + 'static,
    {
        self.task_running = true;
        let reporter = Reporter {
            sender: self.sender.clone(),
        };
        let loopback = self.loopback.clone();
        tokio::spawn(async move {
            let res = tokio::task::spawn_blocking(move || task(reporter))
                .await
                .unwrap_or_else(|e| {
                    log::error!("Device task failed: {}", e);
                    TaskResult::Failed
                });
            if loopback
                .send(DeviceMessage::TaskDone(Box::new(res)))
                .await
                .is_err()
            {
                log::debug!("DeviceService: fail to send task result");
            }
        });
    }

    fn handle_message(&mut self, msg: DeviceMessage) {
        match msg {
            DeviceMessage::Poll => {
                if !self.task_running {
                    let loaded = self.loaded.clone();
                    self.spawn_task(move |_| probe(loaded));
                }
            }
            DeviceMessage::TaskDone(res) => {
                self.task_running = false;
                self.handle_result(*res);
            }
            op @ (DeviceMessage::InstallApp { .. }
            | DeviceMessage::UpdateApp { .. }
            | DeviceMessage::GenuineCheck
            | DeviceMessage::UpdateFirmware
            | DeviceMessage::UpdateFirmwareWithoutBackupFile
            | DeviceMessage::RepairFirmware) => {
                if self.busy {
                    // Another operation is running (the GUI should not have allowed it).
                    log::debug!("DeviceService: busy, ignoring {:?}", op);
                } else if self.task_running {
                    // A probe is running, run the operation once it is done.
                    self.pending = Some(op);
                } else {
                    self.start_operation(op);
                }
            }
            msg => {
                log::debug!("DeviceService.handle_message({:?}) -> unhandled!", msg)
            }
        }
    }

    fn handle_result(&mut self, res: TaskResult) {
        match res {
            TaskResult::Probe { device: None, .. } => {
                if let Some(op) = self.pending.take() {
                    self.drop_operation(&op);
                }
                self.after_reload = None;
                self.loaded = None;
                self.ledger_update = None;
                self.genuine = None;
                self.set_busy(false);
                if !matches!(self.state, DeviceState::None) {
                    self.send_state(DeviceState::None);
                    self.send_to_gui(DeviceMessage::Status(CONNECT_HINT.to_string(), false));
                }
            }
            TaskResult::Probe {
                device: Some(_),
                same: true,
            } => {
                if let Some(op) = self.pending.take() {
                    self.start_operation(op);
                }
            }
            TaskResult::Probe {
                device: Some(device),
                same: false,
            } => {
                if let Some(op) = self.pending.take() {
                    self.drop_operation(&op);
                }
                if self.loaded.is_some() {
                    // Another device was connected.
                    self.genuine = None;
                }
                self.set_busy(true);
                self.spawn_task(move |r| load(device, &r));
            }
            TaskResult::Loaded {
                key,
                ok,
                state,
                ledger_update,
            } => {
                self.loaded = ok.then_some(key);
                self.ledger_update = ledger_update;
                self.send_state(state);
                self.set_busy(false);
                if ok {
                    if let Some((msg, alarm)) = self.after_reload.take() {
                        self.send_to_gui(DeviceMessage::Status(msg, alarm));
                    }
                }
            }
            TaskResult::Genuine(genuine) => {
                self.genuine = genuine;
                let state = self.state.clone();
                self.send_state(state);
                self.set_busy(false);
            }
            TaskResult::Operation { reload, message } => {
                self.send_to_gui(DeviceMessage::Progress(None));
                self.send_to_gui(DeviceMessage::Info(None));
                if reload {
                    // Stay busy until the device information is reloaded.
                    self.loaded = None;
                    self.after_reload = message;
                    self.handle_message(DeviceMessage::Poll);
                } else {
                    self.set_busy(false);
                    if let Some((msg, alarm)) = message {
                        self.send_to_gui(DeviceMessage::Status(msg, alarm));
                    }
                }
            }
            TaskResult::BackupNotSaved(e) => {
                self.set_busy(false);
                self.send_to_gui(DeviceMessage::Progress(None));
                self.send_to_gui(DeviceMessage::Info(None));
                self.send_to_gui(DeviceMessage::Status(String::new(), false));
                self.send_to_gui(DeviceMessage::BackupNotSaved(e));
            }
            TaskResult::Failed => {
                self.loaded = None;
                self.set_busy(false);
                self.send_to_gui(DeviceMessage::Progress(None));
                self.send_to_gui(DeviceMessage::Info(None));
                self.send_to_gui(DeviceMessage::Status(
                    "Unexpected error while communicating with the device.".to_string(),
                    true,
                ));
            }
        }
    }

    fn start_operation(&mut self, op: DeviceMessage) {
        let ledger = match &self.state {
            DeviceState::Ledger(l) if self.loaded.is_some() => l.clone(),
            DeviceState::Bitbox(b) if self.loaded.is_some() => {
                if matches!(op, DeviceMessage::UpdateFirmware)
                    && matches!(b.latest_firmware, LatestFirmware::Available(_))
                {
                    self.set_busy(true);
                    self.spawn_task(|r| bitbox::update(&r));
                } else {
                    // There are no apps to manage on the BitBox.
                    self.drop_operation(&op);
                }
                return;
            }
            _ => {
                self.drop_operation(&op);
                return;
            }
        };
        self.set_busy(true);
        match op {
            DeviceMessage::InstallApp { testnet } => {
                self.spawn_task(move |r| ledger::install_app(&r, testnet, false))
            }
            DeviceMessage::UpdateApp { testnet } => {
                self.spawn_task(move |r| ledger::install_app(&r, testnet, true))
            }
            DeviceMessage::GenuineCheck => {
                self.spawn_task(move |r| TaskResult::Genuine(ledger::genuine_check(&r)))
            }
            DeviceMessage::UpdateFirmware | DeviceMessage::UpdateFirmwareWithoutBackupFile => {
                match self.ledger_update.clone() {
                    Some(update)
                        if matches!(ledger.latest_firmware, LatestFirmware::Available(_)) =>
                    {
                        let save_backup = matches!(op, DeviceMessage::UpdateFirmware);
                        self.spawn_task(move |r| ledger::update_firmware(&r, *update, save_backup))
                    }
                    _ => {
                        self.set_busy(false);
                        self.send_to_gui(DeviceMessage::Status(
                            "No firmware update available for this device.".to_string(),
                            true,
                        ));
                    }
                }
            }
            DeviceMessage::RepairFirmware if ledger.mode == LedgerMode::Bootloader => {
                self.spawn_task(|r| ledger::repair_firmware(&r))
            }
            op => self.drop_operation(&op),
        }
    }
}

impl ServiceFn<DeviceMessage, Sender<DeviceMessage>> for DeviceService {
    fn new(
        sender: Sender<DeviceMessage>,
        receiver: Receiver<DeviceMessage>,
        loopback: Sender<DeviceMessage>,
    ) -> Self {
        DeviceService {
            sender,
            receiver,
            loopback,
            state: DeviceState::None,
            loaded: None,
            ledger_update: None,
            genuine: None,
            task_running: false,
            busy: false,
            pending: None,
            after_reload: None,
        }
    }

    async fn run(&mut self) {
        // Look for a device periodically.
        let loopback = self.loopback.clone();
        tokio::spawn(async move {
            loop {
                if loopback.send(DeviceMessage::Poll).await.is_err() {
                    break;
                }
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        });
        while let Ok(msg) = self.receiver.recv().await {
            self.handle_message(msg);
        }
    }
}
