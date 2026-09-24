//! The window. It displays what the worker thread reports, and sends it the user's requests.

use std::sync::mpsc;

use crate::{
    theme::{self, Theme},
    worker::{
        self, AppState, BitboxState, DeviceState, Event, Installed, JadeState, LatestFirmware,
        LedgerMode, LedgerState, Request, CONNECT_HINT,
    },
};
use bitbox_manager::Edition;
use iced::{
    alignment::Horizontal,
    executor, subscription,
    widget::{
        column, horizontal_space, row, vertical_space, Button, Column, Container, ProgressBar,
        Rule, Space, Text,
    },
    Alignment, Application, Command, Font, Length, Subscription,
};

type Element<'a> = iced::Element<'a, Message, Theme>;

/// The font of the icon of the app buttons (iconex-icons.ttf is named "Untitled1").
pub const ICONEX_ICONS: Font = Font::with_name("Untitled1");

#[derive(Debug, Clone)]
pub enum Message {
    Event(Event),
    Request(Request),
    /// Show the firmware update confirmation.
    AskFirmwareUpdate,
    /// Close the firmware update confirmation (or the backup error).
    Cancel,
    CloseAlarm,
}

pub struct Bacca {
    requests: mpsc::Sender<Request>,
    events: async_channel::Receiver<Event>,
    device: Option<DeviceState>,
    /// The result of the genuine check of the connected Ledger.
    genuine: Option<bool>,
    status: String,
    /// Whether `status` is an error, which the user has to acknowledge.
    alarm: bool,
    /// Progress of the current step, between 0 and 1.
    progress: Option<f32>,
    /// Information to keep displayed during the operation.
    info: Option<String>,
    /// Whether an operation is running.
    busy: bool,
    confirm_firmware_update: bool,
    /// Why the backup of the Ledger settings could not be saved (the firmware update was not
    /// started). The user can retry, or update without a backup file.
    backup_error: Option<String>,
}

impl Application for Bacca {
    type Executor = executor::Default;
    type Message = Message;
    type Theme = Theme;
    type Flags = ();

    fn new(_: ()) -> (Self, Command<Message>) {
        let (requests, events) = worker::start();
        let bacca = Bacca {
            requests,
            events,
            device: None,
            genuine: None,
            status: CONNECT_HINT.to_string(),
            alarm: false,
            progress: None,
            info: None,
            busy: false,
            confirm_firmware_update: false,
            backup_error: None,
        };
        (bacca, Command::none())
    }

    fn title(&self) -> String {
        "Bacca - your hardware wallet Bitcoin companion".to_string()
    }

    fn update(&mut self, message: Message) -> Command<Message> {
        match message {
            Message::Event(Event::Device(device)) => {
                if device.is_none() {
                    self.genuine = None;
                    self.confirm_firmware_update = false;
                    self.backup_error = None;
                }
                self.device = device;
            }
            Message::Event(Event::Status(status)) => {
                self.status = status;
                self.alarm = false;
            }
            Message::Event(Event::Alarm(status)) => {
                self.status = status;
                self.alarm = true;
            }
            Message::Event(Event::Progress(progress)) => self.progress = progress,
            Message::Event(Event::Info(info)) => self.info = info,
            Message::Event(Event::Busy(busy)) => self.busy = busy,
            Message::Event(Event::Genuine(genuine)) => self.genuine = Some(genuine),
            Message::Event(Event::BackupNotSaved(error)) => {
                self.status.clear();
                self.backup_error = Some(error);
            }
            Message::Request(request) => {
                self.confirm_firmware_update = false;
                self.backup_error = None;
                if !self.busy {
                    self.busy = true;
                    let _ = self.requests.send(request);
                }
            }
            Message::AskFirmwareUpdate => self.confirm_firmware_update = !self.busy,
            Message::Cancel => {
                self.confirm_firmware_update = false;
                self.backup_error = None;
            }
            Message::CloseAlarm => {
                self.alarm = false;
                self.status.clear();
            }
        }
        Command::none()
    }

    fn subscription(&self) -> Subscription<Message> {
        subscription::unfold("worker", self.events.clone(), |events| async move {
            let message = match events.recv().await {
                Ok(event) => Message::Event(event),
                Err(_) => Message::Event(Event::Alarm(
                    "Unexpected error: the device worker stopped. Please restart Bacca.".into(),
                )),
            };
            (message, events)
        })
    }

    fn view(&self) -> Element<'_> {
        let content = if let Some(error) = &self.backup_error {
            backup_error_view(error)
        } else if self.confirm_firmware_update {
            self.confirmation_view()
        } else if self.alarm {
            column![
                centered(Text::new(&self.status).horizontal_alignment(Horizontal::Center)),
                Space::with_height(10),
                centered(Button::new(" OK ").on_press(Message::CloseAlarm)),
            ]
            .into()
        } else {
            self.main_view()
        };
        Container::new(column![vertical_space(), content, vertical_space()])
            .padding(10)
            .into()
    }
}

impl Bacca {
    fn main_view(&self) -> Element<'_> {
        let mut column = Column::new();
        match &self.device {
            Some(DeviceState::Ledger(ledger)) => {
                column = column
                    .push(section_title("Device"))
                    .push(Space::with_height(5))
                    .push(ledger_panel(ledger, self.genuine, self.busy))
                    .push(Space::with_height(5))
                    .push(section_title("Apps"))
                    .push(Space::with_height(5))
                    .push(apps_panel(ledger, self.busy))
                    .push(Space::with_height(10));
            }
            Some(DeviceState::Bitbox(bitbox)) => {
                column = column
                    .push(section_title("Device"))
                    .push(Space::with_height(5))
                    .push(bitbox_panel(bitbox, self.busy))
                    .push(Space::with_height(5))
                    .push(section_title("Bitcoin"))
                    .push(Space::with_height(5))
                    .push(bitbox_app_panel(bitbox))
                    .push(Space::with_height(10));
            }
            // There is no Bitcoin app to install on the Jade either: the firmware is the app.
            Some(DeviceState::Jade(jade)) => {
                column = column
                    .push(section_title("Device"))
                    .push(Space::with_height(5))
                    .push(jade_panel(jade, self.busy))
                    .push(Space::with_height(10));
            }
            None => {}
        }
        let status = (!self.status.is_empty()).then(|| {
            row![
                Space::with_width(10),
                Text::new(&self.status).width(Length::Fill)
            ]
        });
        let info = self.info.as_ref().map(|info| {
            frame(
                10,
                Text::new(info)
                    .size(20)
                    .width(Length::Fill)
                    .horizontal_alignment(Horizontal::Center),
            )
            .width(Length::Fill)
        });
        let progress = self
            .progress
            .map(|p| ProgressBar::new(0.0..=1.0, p).height(8));
        column
            .push_maybe(status)
            .push(Space::with_height(5))
            .push_maybe(info)
            .push(Space::with_height(5))
            .push_maybe(progress)
            .into()
    }

    fn confirmation_view(&self) -> Element<'_> {
        let (latest, lines) = match &self.device {
            Some(DeviceState::Ledger(ledger)) => {
                let mut lines = vec!["- All the apps installed on your Ledger will be uninstalled by the update (your funds are not affected). The Bitcoin and Bitcoin Test apps are reinstalled automatically afterwards, if they were installed.".to_string()];
                if ledger.update_resets_customization {
                    lines.push("- The language and the custom lock screen picture of your device are backed up and restored automatically after the update. You will have to approve their backup and restoration on the device.".to_string());
                }
                lines.push("- The data stored inside the apps is NOT restored. In particular the wallet policies registered in the Bitcoin app (and its settings) are lost: you may have to register your wallet again from your wallet software.".to_string());
                if let Some(dir) = ledger_manager::default_backup_dir() {
                    lines.push(format!("- The backup is saved in {} before the update starts, so it is not lost if the update gets interrupted.", dir.display()));
                }
                lines.push("- Keep the device plugged in and unlocked during the whole update. It can take several minutes, the device may restart several times.".to_string());
                lines.push("- You will have to allow the Ledger manager and to confirm the update on the device, after checking the identifier it displays matches the one shown here.".to_string());
                lines.push("- Before proceeding, make sure you have the backup of your recovery phrase (24 words) at hand.".to_string());
                (&ledger.latest_firmware, lines)
            }
            Some(DeviceState::Bitbox(bitbox)) => {
                let mut lines = Vec::new();
                if !bitbox.bootloader {
                    lines.push("- You will have to unlock your BitBox, to confirm the pairing code (compare it with the one shown here) and to confirm the upgrade on the device.".to_string());
                }
                lines.push("- Keep the device plugged in during the whole update. It can take a few minutes, the device may restart several times. The firmware hash will be shown here: compare it with the one shown by your BitBox.".to_string());
                lines.push("- Before proceeding, make sure you have the backup of your wallet (recovery words or microSD card) at hand.".to_string());
                (&bitbox.latest_firmware, lines)
            }
            Some(DeviceState::Jade(jade)) => {
                let lines = vec![
                    "- If your Jade is locked, you will have to enter your PIN on the device. Bacca relays the unlock requests to Blockstream's PIN server only: custom PIN servers are not supported.".to_string(),
                    "- You will have to confirm the update on the device, after checking the version and the firmware hash it displays match the ones shown here.".to_string(),
                    "- Keep the device plugged in and switched on during the whole update. It can take a few minutes, the device restarts at the end.".to_string(),
                    "- Before proceeding, make sure you have the backup of your recovery phrase at hand.".to_string(),
                ];
                (&jade.latest_firmware, lines)
            }
            None => (&LatestFirmware::Unknown, Vec::new()),
        };
        let title = match latest {
            LatestFirmware::Available(v) => format!("Update the firmware to {}?", v),
            _ => "Update the firmware?".to_string(),
        };
        let text = Column::with_children(lines.into_iter().map(|l| Text::new(l).into()));
        let update = Message::Request(Request::UpdateFirmware { backup_file: true });
        dialog(
            title,
            text.spacing(8),
            vec![
                Button::new(" Cancel ").on_press(Message::Cancel),
                Button::new(" Update firmware ").on_press(update),
            ],
        )
    }
}

fn backup_error_view(error: &str) -> Element<'_> {
    let text = column![
        Text::new(format!("The backup of your device settings could not be saved: {}", error)),
        Text::new("The firmware update was not started. You can retry, or update anyway: the settings are then backed up in memory only, and they are lost if the update gets interrupted."),
    ];
    let update = |backup_file| Message::Request(Request::UpdateFirmware { backup_file });
    dialog(
        "Backup failed".to_string(),
        text.spacing(8),
        vec![
            Button::new(" Cancel ").on_press(Message::Cancel),
            Button::new(" Retry ").on_press(update(true)),
            Button::new(" Update without backup file ").on_press(update(false)),
        ],
    )
}

/// A title, a text in a frame and buttons.
fn dialog<'a>(
    title: String,
    text: Column<'a, Message, Theme>,
    buttons: Vec<Button<'a, Message, Theme>>,
) -> Element<'a> {
    let mut buttons_row = row![horizontal_space()];
    for (i, button) in buttons.into_iter().enumerate() {
        if i > 0 {
            buttons_row = buttons_row.push(Space::with_width(30));
        }
        buttons_row = buttons_row.push(button);
    }
    column![
        section_title(title),
        Space::with_height(10),
        frame(15, text).width(Length::Fill),
        Space::with_height(15),
        buttons_row.push(horizontal_space()),
    ]
    .into()
}

fn frame<'a>(padding: u16, content: impl Into<Element<'a>>) -> Container<'a, Message, Theme> {
    Container::new(content)
        .style(theme::Container::Frame)
        .padding(padding)
}

fn centered<'a>(element: impl Into<Element<'a>>) -> Element<'a> {
    row![horizontal_space(), element.into(), horizontal_space()].into()
}

fn section_title<'a>(title: impl ToString) -> Element<'a> {
    centered(Text::new(title.to_string()).size(20))
}

/// A row of the device panel.
fn info_row<'a>(label: &'a str, value: impl Into<Element<'a>>) -> Element<'a> {
    row![
        Space::with_width(60),
        Text::new(label).width(150),
        horizontal_space(),
        value.into(),
        horizontal_space(),
    ]
    .align_items(Alignment::Center)
    .into()
}

fn or_dash(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or(" - ")
}

/// The latest firmware, with a button to update to it (if `can_update`).
fn latest_firmware(latest: &LatestFirmware, can_update: bool) -> Element<'_> {
    match latest {
        LatestFirmware::Unknown => Text::new(" - ").into(),
        LatestFirmware::UpToDate => Text::new("Up to date").into(),
        LatestFirmware::Unsupported(v) => Text::new(format!("{} (not supported)", v)).into(),
        LatestFirmware::Available(v) => row![
            Text::new(v),
            Space::with_width(10),
            Button::new(" Update ")
                .on_press_maybe(can_update.then_some(Message::AskFirmwareUpdate)),
        ]
        .align_items(Alignment::Center)
        .into(),
    }
}

fn ledger_panel(ledger: &LedgerState, genuine: Option<bool>, busy: bool) -> Element<'_> {
    let latest = if ledger.mode == LedgerMode::Bootloader {
        // The device is stuck in an interrupted update: offer to finish it.
        let repair = Message::Request(Request::RepairFirmware);
        Button::new(" Repair ")
            .on_press_maybe((!busy).then_some(repair))
            .into()
    } else {
        // An interrupted update can be resumed from the updater mode.
        latest_firmware(&ledger.latest_firmware, !busy && ledger.firmware.is_some())
    };
    let genuine: Element = match genuine {
        None => {
            let check = Message::Request(Request::GenuineCheck);
            Button::new("Check")
                .on_press_maybe((!busy && ledger.is_ready()).then_some(check))
                .into()
        }
        Some(true) => Text::new(" Yes ").into(),
        Some(false) => Text::new("No!").into(),
    };
    frame(
        10,
        column![
            info_row("Model:", Text::new(&ledger.model)),
            info_row("Firmware:", Text::new(or_dash(&ledger.firmware))),
            info_row("Latest firmware:", latest),
            info_row("Genuine:", genuine),
        ]
        .spacing(5),
    )
    .into()
}

fn bitbox_panel(bitbox: &BitboxState, busy: bool) -> Element<'_> {
    let can_update = !busy && bitbox.firmware.is_some();
    frame(
        10,
        column![
            info_row("Model:", Text::new(&bitbox.model)),
            info_row("Edition:", Text::new(bitbox.edition.to_string())),
            info_row("Firmware:", Text::new(or_dash(&bitbox.firmware))),
            info_row(
                "Latest firmware:",
                latest_firmware(&bitbox.latest_firmware, can_update)
            ),
        ]
        .spacing(5),
    )
    .into()
}

fn jade_panel(jade: &JadeState, busy: bool) -> Element<'_> {
    frame(
        10,
        column![
            info_row("Model:", Text::new(&jade.model)),
            info_row("Firmware:", Text::new(&jade.firmware)),
            info_row("Config:", Text::new(&jade.config)),
            info_row("State:", Text::new(&jade.state)),
            info_row(
                "Latest firmware:",
                latest_firmware(&jade.latest_firmware, !busy)
            ),
        ]
        .spacing(5),
    )
    .into()
}

/// There is no Bitcoin app to install on the BitBox: the firmware edition is the app.
fn bitbox_app_panel(bitbox: &BitboxState) -> Element<'_> {
    let multi = (bitbox.edition == Edition::Multi).then(|| {
        Text::new("This device runs the Multi edition. For Bitcoin, the Bitcoin-only edition is recommended (smaller attack surface). The edition of a device can't be changed.")
            .style(theme::Text::Color(theme::color::GREY_2))
    });
    let text = column![Text::new("There is no separate Bitcoin app on the BitBox: the firmware edition is the app. Keep the firmware up to date.")]
        .push_maybe(multi)
        .spacing(8);
    frame(15, text).width(Length::Fill).into()
}

fn apps_panel(ledger: &LedgerState, busy: bool) -> Element<'_> {
    let enabled = !busy && ledger.is_ready();
    frame(
        10,
        column![
            app_row("Bitcoin", &ledger.bitcoin, false, enabled, (5, 10)),
            row![
                Space::with_width(30),
                Rule::horizontal(2),
                Space::with_width(30)
            ],
            app_row("Bitcoin Test", &ledger.bitcoin_test, true, enabled, (10, 5)),
        ],
    )
    .height(200)
    .into()
}

/// The name and installed version of an app, and a button to install or update it.
/// `rule_padding` is the space above and below the vertical separator.
fn app_row<'a>(
    name: &'a str,
    app: &AppState,
    testnet: bool,
    enabled: bool,
    rule_padding: (u16, u16),
) -> Element<'a> {
    let button = |label: &'a str, request: Request| -> Element<'a> {
        let icon = Text::new('\u{605B}'.to_string())
            .font(ICONEX_ICONS)
            .width(40)
            .size(25)
            .horizontal_alignment(Horizontal::Center);
        Button::new(row![icon, Text::new(label).size(25)])
            .on_press_maybe(enabled.then_some(Message::Request(request)))
            .into()
    };
    let (version, action) = match (&app.installed, &app.latest) {
        (Installed::No, _) => (
            "Not installed".to_string(),
            button(" Install ", Request::InstallApp { testnet }),
        ),
        // The latest version is the one of the Ledger catalog: another installed version (e.g. a
        // pre-release) can be "updated" to it.
        (Installed::Version(v), Some(latest)) if v != latest => (
            format!("Version: {}", v),
            button(" Update ", Request::UpdateApp { testnet }),
        ),
        (Installed::Version(v), Some(_)) => (
            format!("Version: {}", v),
            Text::new("Latest").size(25).into(),
        ),
        (Installed::Version(v), None) => {
            (format!("Version: {}", v), Text::new(" - ").size(25).into())
        }
        (Installed::Unknown, _) => (" - ".to_string(), Text::new(" - ").size(25).into()),
    };
    row![
        column![
            vertical_space(),
            Text::new(name).size(25),
            Text::new(version).style(theme::Text::Color(theme::color::GREY_3)),
            vertical_space(),
        ]
        .width(230)
        .align_items(Alignment::Center),
        column![
            Space::with_height(rule_padding.0),
            Rule::vertical(1).style(theme::Rule::Light),
            Space::with_height(rule_padding.1),
        ],
        horizontal_space(),
        column![vertical_space(), action, vertical_space()],
        horizontal_space(),
    ]
    .into()
}
