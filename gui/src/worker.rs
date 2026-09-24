//! The worker thread, which talks to the devices on behalf of the GUI.
//!
//! The device libraries are blocking (USB and HTTP I/O, waiting for the user to confirm on the
//! device; a firmware update takes minutes), so they run on their own thread. The GUI sends it
//! [`Request`]s and it reports back with [`Event`]s. Between two requests it looks for a connected
//! device every [`POLL_INTERVAL`]. It does one thing at a time.

use std::{sync::mpsc, thread, time::Duration};

use bitbox_manager::{DeviceHandle, Edition};
use ledger_manager::{ledger_transport_hidapi::hidapi::HidApi, FirmwareUpdateInfo};

use crate::{bitbox, ledger};

const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub const CONNECT_HINT: &str = "Please connect your Ledger or BitBox device and unlock it...";

/// An operation requested by the GUI.
#[derive(Debug, Clone, Copy)]
pub enum Request {
    InstallApp {
        testnet: bool,
    },
    UpdateApp {
        testnet: bool,
    },
    GenuineCheck,
    /// For a Ledger, `backup_file` tells whether to save the backup of the device settings to a
    /// file before the update (otherwise it is only kept in memory).
    UpdateFirmware {
        backup_file: bool,
    },
    /// Finish a Ledger firmware update interrupted while the device was in bootloader mode.
    RepairFirmware,
}

/// A report from the worker to the GUI.
#[derive(Debug, Clone)]
pub enum Event {
    /// The connected device, if any.
    Device(Option<DeviceState>),
    Status(String),
    /// An error message, which the user has to acknowledge.
    Alarm(String),
    /// Progress of the current step, between 0 and 1.
    Progress(Option<f32>),
    /// Information to keep displayed during the operation (e.g. a code to compare with the one
    /// shown by the device).
    Info(Option<String>),
    /// Whether an operation is running (the GUI must not request another one).
    Busy(bool),
    /// Result of the Ledger genuine check.
    Genuine(bool),
    /// The backup of the Ledger settings could not be saved: the firmware update was not started.
    BackupNotSaved(String),
}

/// The latest firmware for the connected device.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum LatestFirmware {
    /// Not queried (yet), or the query failed.
    #[default]
    Unknown,
    UpToDate,
    /// This version can be installed.
    Available(String),
    /// This version can't be installed by Bacca (the reason is in the status message).
    Unsupported(String),
}

#[derive(Debug, Clone)]
pub enum DeviceState {
    Ledger(LedgerState),
    Bitbox(BitboxState),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LedgerMode {
    #[default]
    Normal,
    /// The OS updater: a firmware update is in progress (or was interrupted).
    Updater,
    Bootloader,
}

/// Whether a Bitcoin app is installed on the Ledger.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Installed {
    #[default]
    Unknown,
    No,
    Version(String),
}

#[derive(Debug, Clone, Default)]
pub struct AppState {
    pub installed: Installed,
    pub latest: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LedgerState {
    pub model: String,
    /// `None` if the device could not be queried (e.g. it is locked).
    pub firmware: Option<String>,
    pub mode: LedgerMode,
    pub latest_firmware: LatestFirmware,
    /// Whether the firmware update may reset the language and the custom lock screen picture.
    pub update_resets_customization: bool,
    pub bitcoin: AppState,
    pub bitcoin_test: AppState,
}

impl LedgerState {
    /// Whether the device could be queried and runs its OS normally.
    pub fn is_ready(&self) -> bool {
        self.firmware.is_some() && self.mode == LedgerMode::Normal
    }
}

#[derive(Debug, Clone)]
pub struct BitboxState {
    /// BitBox02 or BitBox02 Nova.
    pub model: String,
    pub edition: Edition,
    /// The firmware version, or the bootloader state. `None` if the device could not be queried.
    pub firmware: Option<String>,
    pub bootloader: bool,
    pub latest_firmware: LatestFirmware,
}

/// Sends [`Event`]s to the GUI.
pub struct Reporter(async_channel::Sender<Event>);

impl Reporter {
    pub fn send(&self, event: Event) {
        match &event {
            Event::Status(msg) | Event::Alarm(msg) if !msg.is_empty() => log::info!("{}", msg),
            _ => log::debug!("{:?}", event),
        }
        // The channel is unbounded: this never blocks. It only fails if the GUI is closed.
        let _ = self.0.try_send(event);
    }

    /// A status message, without progress.
    pub fn status(&self, msg: impl Into<String>) {
        self.send(Event::Progress(None));
        self.send(Event::Status(msg.into()));
    }

    pub fn alarm(&self, msg: impl Into<String>) {
        self.send(Event::Progress(None));
        self.send(Event::Alarm(msg.into()));
    }

    /// A status message with a percentage, and the progress bar. `progress` is between 0 and 1.
    pub fn percent(&self, label: &str, progress: f32) {
        let percent = (progress * 100.0).round().clamp(0.0, 100.0);
        self.send(Event::Status(format!("{}: {}%", label, percent)));
        self.send(Event::Progress(Some(progress)));
    }

    pub fn info(&self, info: Option<String>) {
        self.send(Event::Info(info));
    }

    pub fn device(&self, state: DeviceState) {
        self.send(Event::Device(Some(state)));
    }
}

/// How an operation ended.
pub enum Outcome {
    /// Query the device information again, then display this message.
    Done(String),
    /// Query the device information again, then display this error.
    Failed(String),
    /// Nothing more to do, the result was already reported to the GUI.
    Reported,
}

/// Start the worker thread. Returns the channels to send it requests and to receive its events.
pub fn start() -> (mpsc::Sender<Request>, async_channel::Receiver<Event>) {
    let (request_sender, requests) = mpsc::channel();
    let (event_sender, events) = async_channel::unbounded();
    let worker = Worker {
        reporter: Reporter(event_sender),
        requests,
        current: None,
        result: None,
    };
    thread::spawn(move || worker.run());
    (request_sender, events)
}

/// A device found on USB.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Detected {
    /// By its HID path.
    Ledger(String),
    /// A change of mode (firmware or bootloader) makes it a different device, to query it again.
    Bitbox(DeviceHandle),
}

/// The device the GUI displays.
struct Current {
    device: Detected,
    /// Whether its information could be queried. If not (e.g. it's locked), it's queried again at
    /// the next poll.
    loaded: bool,
    /// The firmware update available for a Ledger.
    ledger_update: Option<FirmwareUpdateInfo>,
}

struct Worker {
    reporter: Reporter,
    requests: mpsc::Receiver<Request>,
    current: Option<Current>,
    /// The result of the last operation, displayed once the device information is reloaded.
    result: Option<Event>,
}

impl Worker {
    fn run(mut self) {
        loop {
            match self.requests.recv_timeout(POLL_INTERVAL) {
                Ok(request) => {
                    self.reporter.send(Event::Busy(true));
                    self.perform(request);
                    // Requests made during the operation are stale (the GUI doesn't allow them).
                    while self.requests.try_recv().is_ok() {}
                    self.reporter.send(Event::Busy(false));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    self.poll();
                }
                // The GUI is closed.
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }

    /// Look for a connected device, and query the information of a new one. Returns whether the
    /// device already loaded is still connected.
    fn poll(&mut self) -> bool {
        let found = detect(self.current.as_ref().map(|c| &c.device));
        if let Some(current) = &self.current {
            if found.as_ref() == Some(&current.device) {
                if current.loaded {
                    return true;
                }
            } else {
                // Disconnected, or replaced by another device.
                self.current = None;
                self.result = None;
                self.reporter.send(Event::Device(None));
                self.reporter.status(CONNECT_HINT);
            }
        }
        if let Some(device) = found {
            self.load(device);
        }
        false
    }

    fn load(&mut self, device: Detected) {
        self.reporter.send(Event::Busy(true));
        let (loaded, ledger_update) = match &device {
            Detected::Ledger(_) => ledger::load(&self.reporter),
            Detected::Bitbox(handle) => (bitbox::load(handle, &self.reporter), None),
        };
        if loaded {
            if let Some(result) = self.result.take() {
                self.reporter.send(result);
            }
        }
        self.reporter.send(Event::Busy(false));
        self.current = Some(Current {
            device,
            loaded,
            ledger_update,
        });
    }

    fn perform(&mut self, request: Request) {
        // Make sure the request is for the device the GUI displays.
        if !self.poll() {
            return;
        }
        let Some(current) = &self.current else {
            return;
        };
        log::info!("{:?}", request);
        let r = &self.reporter;
        let outcome = match (&current.device, request, &current.ledger_update) {
            (Detected::Ledger(_), Request::InstallApp { testnet }, _) => {
                ledger::install_app(r, testnet, false)
            }
            (Detected::Ledger(_), Request::UpdateApp { testnet }, _) => {
                ledger::install_app(r, testnet, true)
            }
            (Detected::Ledger(_), Request::GenuineCheck, _) => ledger::genuine_check(r),
            (Detected::Ledger(_), Request::UpdateFirmware { backup_file }, Some(update)) => {
                ledger::update_firmware(r, update, backup_file)
            }
            (Detected::Ledger(_), Request::RepairFirmware, _) => ledger::repair_firmware(r),
            (Detected::Bitbox(_), Request::UpdateFirmware { .. }, _) => bitbox::update_firmware(r),
            // The GUI doesn't offer anything else.
            _ => return,
        };
        r.send(Event::Progress(None));
        r.info(None);
        let result = match outcome {
            Outcome::Done(msg) => Event::Status(msg),
            Outcome::Failed(msg) => Event::Alarm(msg),
            Outcome::Reported => return,
        };
        log::info!("{:?}", result);
        self.result = Some(result);
        if let Some(current) = &mut self.current {
            current.loaded = false;
        }
        self.poll();
    }
}

/// The device to use: the current one if it is still connected, else the first one found.
fn detect(current: Option<&Detected>) -> Option<Detected> {
    let api = HidApi::new()
        .map_err(|e| log::error!("Error initializing the HID API: {}", e))
        .ok()?;
    let ledgers = ledger_manager::list_ledger_devices(&api).into_iter();
    let bitboxes = bitbox_manager::list_devices(&api).into_iter();
    let found: Vec<Detected> = ledgers
        .map(Detected::Ledger)
        .chain(bitboxes.map(Detected::Bitbox))
        .collect();
    found
        .iter()
        .find(|d| Some(*d) == current)
        .or(found.first())
        .cloned()
}
