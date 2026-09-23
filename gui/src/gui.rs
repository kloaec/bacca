use crate::{
    device_service::{
        DeviceListener, DeviceMessage, DeviceState, LatestFirmware, LedgerMode, LedgerState,
        Version, CONNECT_HINT,
    },
    theme::{self, Theme},
};
use async_channel::{Receiver, Sender};
use iced::{
    alignment, executor,
    widget::{Button, Column, Container, ProgressBar, Row, Rule, Space, Text},
    Alignment, Application, Element, Font, Length, Renderer,
};
use iced_runtime::{futures::Subscription, Command};

const ICONEX_ICONS_BYTES: &[u8] = include_bytes!("iconex-icons.ttf");

const FIRST_COLUMN_OFFSET: u16 = 60;
const FIRST_COLUMN_WIDTH: u16 = 150;

#[derive(Debug)]
pub struct Flags {
    pub device_sender: Sender<DeviceMessage>,
    pub device_receiver: Receiver<DeviceMessage>,
}

#[derive(Debug, Clone)]
pub enum Message {
    DeviceServiceMsg(DeviceMessage),

    UpdateMain,
    InstallMain,
    UpdateTest,
    InstallTest,
    GenuineCheck,
    /// Ask confirmation for the firmware update.
    UpdateFirmware,
    ConfirmFirmwareUpdate,
    CancelFirmwareUpdate,

    ResetAlarm,
    Result,
}

impl From<Result<(), iced::font::Error>> for Message {
    fn from(_: Result<(), iced::font::Error>) -> Self {
        Self::Result
    }
}

pub struct Bacca {
    device_sender: Sender<DeviceMessage>,
    device_receiver: Receiver<DeviceMessage>,
    device: DeviceState,
    user_message: Option<String>,
    /// Progress of the current step of the operation, between 0 and 1.
    progress: Option<f32>,
    /// Information to keep displayed during the operation.
    info: Option<String>,
    /// Whether the firmware update confirmation is displayed.
    confirm_firmware_update: bool,
    device_busy: bool,
    alarm: bool,
}

impl Bacca {
    pub fn send_device_msg(&self, msg: DeviceMessage) {
        let sender = self.device_sender.clone();
        tokio::spawn(async move { sender.send(msg).await });
    }

    /// Start an operation on the device.
    fn operation(&mut self, msg: DeviceMessage) {
        if !self.device_busy {
            self.device_busy = true;
            self.send_device_msg(msg);
        }
    }
}

impl Application for Bacca {
    type Executor = executor::Default;
    type Message = Message;
    type Theme = Theme;
    type Flags = Flags;

    fn new(args: Self::Flags) -> (Self, Command<Self::Message>) {
        let bacca = Bacca {
            device_sender: args.device_sender,
            device_receiver: args.device_receiver,
            device: DeviceState::None,
            user_message: Some(CONNECT_HINT.to_string()),
            progress: None,
            info: None,
            confirm_firmware_update: false,
            device_busy: false,
            alarm: false,
        };

        let cmd = iced::font::load(ICONEX_ICONS_BYTES).map(Message::from);
        (bacca, cmd)
    }

    fn title(&self) -> String {
        "Bacca - your hardware wallet Bitcoin companion".to_string()
    }

    fn update(&mut self, event: Message) -> Command<Message> {
        log::debug!("Gui receive: {:?}", event.clone());
        match event {
            Message::DeviceServiceMsg(msg) => match msg {
                DeviceMessage::State(state) => {
                    if matches!(state, DeviceState::None) {
                        self.confirm_firmware_update = false;
                    }
                    self.device = state;
                }
                DeviceMessage::Status(s, alarm) => {
                    log::info!("Bacca::update(Status({}), {:?})", s, alarm);
                    self.user_message = Some(s);
                    self.alarm = alarm;
                }
                DeviceMessage::Progress(p) => self.progress = p,
                DeviceMessage::Info(i) => self.info = i,
                DeviceMessage::Busy(busy) => self.device_busy = busy,
                msg => {
                    log::debug!(
                        "Bacca.update() => Unhandled message from device service: {:?}!",
                        msg
                    )
                }
            },
            Message::ResetAlarm => {
                self.alarm = false;
                self.user_message = None;
            }
            Message::UpdateMain => self.operation(DeviceMessage::UpdateApp { testnet: false }),
            Message::InstallMain => self.operation(DeviceMessage::InstallApp { testnet: false }),
            Message::UpdateTest => self.operation(DeviceMessage::UpdateApp { testnet: true }),
            Message::InstallTest => self.operation(DeviceMessage::InstallApp { testnet: true }),
            Message::GenuineCheck => self.operation(DeviceMessage::GenuineCheck),
            Message::UpdateFirmware => {
                if !self.device_busy {
                    self.confirm_firmware_update = true;
                }
            }
            Message::CancelFirmwareUpdate => self.confirm_firmware_update = false,
            Message::ConfirmFirmwareUpdate => {
                self.confirm_firmware_update = false;
                self.operation(DeviceMessage::UpdateFirmware);
            }
            Message::Result => {}
        }
        Command::none()
    }

    fn view(&self) -> Element<'_, Message, Theme> {
        let content = if self.confirm_firmware_update {
            self.confirmation_view()
        } else if self.alarm {
            self.alarm_view()
        } else {
            self.main_view()
        };

        Container::new(
            Column::new()
                .push(Space::with_height(Length::Fill))
                .push(content)
                .push(Space::with_height(Length::Fill)),
        )
        .padding(10)
        .into()
    }

    fn theme(&self) -> Theme {
        Theme::Dark
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        Subscription::from_recipe(DeviceListener {
            receiver: self.device_receiver.clone(),
        })
    }
}

impl Bacca {
    fn main_view(&self) -> Column<'_, Message, Theme> {
        let mut column = Column::new();
        if let DeviceState::Ledger(ledger) = &self.device {
            column = column
                .push(section_title("Device"))
                .push(Space::with_height(5))
                .push(ledger_device_container(ledger, self.device_busy))
                .push(Space::with_height(5))
                .push(section_title("Apps"))
                .push(Space::with_height(5))
                .push(apps_container(
                    ledger.mainnet.clone(),
                    ledger.latest_mainnet.clone(),
                    ledger.testnet.clone(),
                    ledger.latest_testnet.clone(),
                    self.device_busy || !ledger.is_ready(),
                ))
                .push(Space::with_height(10));
        }

        let info = self.info.clone().map(|info| {
            Container::new(
                Text::new(info)
                    .size(20)
                    .width(Length::Fill)
                    .horizontal_alignment(alignment::Horizontal::Center),
            )
            .style(theme::Container::Frame)
            .padding(10)
            .width(Length::Fill)
        });
        let user_message = self
            .user_message
            .clone()
            .filter(|m| !m.is_empty())
            .map(|msg| {
                Row::new()
                    .push(Space::with_width(10))
                    .push(Text::new(msg).width(Length::Fill))
            });
        let progress = self
            .progress
            .map(|p| ProgressBar::new(0.0..=1.0, p).height(8));

        column
            .push_maybe(user_message)
            .push(Space::with_height(5))
            .push_maybe(info)
            .push(Space::with_height(5))
            .push_maybe(progress)
    }

    fn alarm_view(&self) -> Column<'_, Message, Theme> {
        Column::new()
            .push_maybe(self.user_message.clone().map(|msg| {
                centered(Text::new(msg).horizontal_alignment(alignment::Horizontal::Center))
            }))
            .push(Space::with_height(10))
            .push(centered(Button::new(" OK ").on_press(Message::ResetAlarm)))
    }

    fn confirmation_view(&self) -> Column<'_, Message, Theme> {
        let mut lines: Vec<String> = Vec::new();
        let mut target = None;
        if let DeviceState::Ledger(ledger) = &self.device {
            if let LatestFirmware::Available(v) = &ledger.latest_firmware {
                target = Some(v.clone());
            }
            lines.push(
                "- All the apps installed on your Ledger will be uninstalled. You will have to reinstall the Bitcoin app afterwards (your funds are not affected).".to_string(),
            );
            if ledger.update_resets_customization {
                lines.push(
                    "- The language and the custom lock screen of your device may be reset."
                        .to_string(),
                );
            }
            lines.push(
                "- Keep the device plugged in and unlocked during the whole update. It can take several minutes, the device may restart several times.".to_string(),
            );
            lines.push(
                "- You will have to allow the Ledger manager and to confirm the update on the device, after checking the identifier it displays matches the one shown here.".to_string(),
            );
            lines.push(
                "- Before proceeding, make sure you have the backup of your recovery phrase (24 words) at hand.".to_string(),
            );
        }
        let title = match target {
            Some(v) => format!("Update the firmware to {}?", v),
            None => "Update the firmware?".to_string(),
        };

        let text = lines.into_iter().fold(Column::new().spacing(8), |col, l| {
            col.push(Text::new(l).width(Length::Fill))
        });

        Column::new()
            .push(section_title(&title))
            .push(Space::with_height(10))
            .push(
                Container::new(text)
                    .style(theme::Container::Frame)
                    .padding(15)
                    .width(Length::Fill),
            )
            .push(Space::with_height(15))
            .push(
                Row::new()
                    .push(Space::with_width(Length::Fill))
                    .push(Button::new(" Cancel ").on_press(Message::CancelFirmwareUpdate))
                    .push(Space::with_width(30))
                    .push(Button::new(" Update firmware ").on_press(Message::ConfirmFirmwareUpdate))
                    .push(Space::with_width(Length::Fill)),
            )
    }
}

fn centered<'a>(
    element: impl Into<Element<'a, Message, Theme>>,
) -> Row<'a, Message, Theme, Renderer> {
    Row::new()
        .push(Space::with_width(Length::Fill))
        .push(element)
        .push(Space::with_width(Length::Fill))
}

fn section_title<'a>(title: &str) -> Row<'a, Message, Theme, Renderer> {
    centered(Text::new(title.to_string()).size(20))
}

/// A row of the device information.
fn info_row<'a>(
    label: &str,
    value: impl Into<Element<'a, Message, Theme>>,
) -> Row<'a, Message, Theme, Renderer> {
    Row::new()
        .push(Space::with_width(FIRST_COLUMN_OFFSET))
        .push(Text::new(label.to_string()).width(FIRST_COLUMN_WIDTH))
        .push(Space::with_width(Length::Fill))
        .push(value)
        .push(Space::with_width(Length::Fill))
        .align_items(Alignment::Center)
}

/// The latest firmware, with a button to update to it if possible.
fn latest_firmware_value<'a>(
    latest: &LatestFirmware,
    update_msg: Option<Message>,
) -> Row<'a, Message, Theme, Renderer> {
    match latest {
        LatestFirmware::Unknown => Row::new().push(Text::new(" - ")),
        LatestFirmware::UpToDate => Row::new().push(Text::new("Up to date")),
        LatestFirmware::Unsupported(v) => {
            Row::new().push(Text::new(format!("{} (not supported)", v)))
        }
        LatestFirmware::Available(v) => Row::new()
            .push(Text::new(v.clone()))
            .push(Space::with_width(10))
            .push(Button::new(" Update ").on_press_maybe(update_msg))
            .align_items(Alignment::Center),
    }
}

fn ledger_device_container(
    ledger: &LedgerState,
    device_busy: bool,
) -> Container<'_, Message, Theme, Renderer> {
    let model = ledger.model.clone().unwrap_or(" - ".to_string());
    let version = ledger.firmware.clone().unwrap_or(" - ".to_string());

    // We do not allow user to click the buttons if service still processing a task w/ device
    let genuine_msg = (!device_busy && ledger.is_ready()).then_some(Message::GenuineCheck);
    // An interrupted update can be resumed from the updater mode.
    let can_update_firmware = ledger.firmware.is_some()
        && matches!(ledger.mode, LedgerMode::Normal | LedgerMode::Updater);
    let update_msg = (!device_busy && can_update_firmware).then_some(Message::UpdateFirmware);

    // allow user to check if device is genuine only once per launch
    let genuine: Element<'_, Message, Theme> = match ledger.genuine {
        None => Button::new("Check").on_press_maybe(genuine_msg).into(),
        Some(true) => Text::new(" Yes ").into(),
        // FIXME: should we display in a more obvious way?
        Some(false) => Text::new("No!").into(),
    };

    Container::new(
        Column::new()
            .spacing(5)
            .push(info_row("Model:", Text::new(model)))
            .push(info_row("Firmware:", Text::new(version)))
            .push(info_row(
                "Latest firmware:",
                latest_firmware_value(&ledger.latest_firmware, update_msg),
            ))
            .push(info_row("Genuine:", genuine)),
    )
    .style(theme::Container::Frame)
    .padding(10)
}

fn apps_container<'a>(
    bitcoin_version: Version,
    bitcoin_latest: Version,
    test_version: Version,
    test_latest: Version,
    device_busy: bool,
) -> Container<'a, Message, Theme, Renderer> {
    let network_size = 25;
    let version_color = theme::color::GREY_3;
    let vertical_rule_position = 230;

    // It looks weird that we load iconex-icons.ttf by its name: Untitled1
    const ICONEX_ICONS: Font = Font::with_name("Untitled1");

    fn raw_btn(txt: &str, msg: Option<Message>) -> Button<'_, Message, Theme> {
        Button::new(
            Row::new()
                .push(
                    Text::new('\u{605B}'.to_string())
                        .font(ICONEX_ICONS)
                        .width(Length::Fixed(40.0))
                        .size(25)
                        .horizontal_alignment(alignment::Horizontal::Center),
                )
                .push(Text::new(txt).size(25)),
        )
        .on_press_maybe(msg)
    }

    fn btn(
        version: &Version,
        latest: &Version,
        install_msg: Option<Message>,
        update_msg: Option<Message>,
    ) -> Container<'static, Message, Theme> {
        match (version, latest) {
            (Version::NotInstalled, _) => Container::new(raw_btn(" Install ", install_msg)),
            (Version::Installed(_), Version::Latest(_)) => {
                // FIXME: Here we only check if installed version differ from `latest` in Ledger catalog(stable), so if
                //     //  user have an `alpha` version installed we still offer him to `update` to the `stable` version
                if version != latest {
                    Container::new(raw_btn(" Update ", update_msg))
                } else {
                    Container::new(Text::new("Latest").size(25))
                }
            }
            _ => Container::new(Text::new(" - ").size(25)),
        }
    }

    fn version(version: Version) -> String {
        match version {
            Version::Installed(v) => format!("Version: {}", v),
            Version::NotInstalled => "Not installed".to_string(),
            _ => " - ".to_string(),
        }
    }

    // We do not allow user to click buttons if service still processing a task w/ device
    let enabled = |msg: Message| (!device_busy).then_some(msg);

    let bitcoin_button = btn(
        &bitcoin_version,
        &bitcoin_latest,
        enabled(Message::InstallMain),
        enabled(Message::UpdateMain),
    );

    let test_button = btn(
        &test_version,
        &test_latest,
        enabled(Message::InstallTest),
        enabled(Message::UpdateTest),
    );

    let bitcoin_version = version(bitcoin_version);

    let test_version = version(test_version);

    Container::new(
        Column::new()
            .push(
                Row::new()
                    .push(
                        Column::new()
                            .push(Space::with_height(Length::Fill))
                            .push(Text::new("Bitcoin").size(network_size))
                            .push(
                                Text::new(bitcoin_version).style(theme::Text::Color(version_color)),
                            )
                            .push(Space::with_height(Length::Fill))
                            .width(vertical_rule_position)
                            .align_items(Alignment::Center),
                    )
                    .push(
                        Column::new()
                            .push(Space::with_height(5))
                            .push(Rule::vertical(1).style(theme::Rule::Light))
                            .push(Space::with_height(10)),
                    )
                    .push(Space::with_width(Length::Fill))
                    .push(
                        Column::new()
                            .push(Space::with_height(Length::Fill))
                            .push(bitcoin_button)
                            .push(Space::with_height(Length::Fill)),
                    )
                    .push(Space::with_width(Length::Fill)),
            )
            .push(
                Row::new()
                    .push(Space::with_width(30))
                    .push(Rule::horizontal(2))
                    .push(Space::with_width(30)),
            )
            .push(
                Row::new()
                    .push(
                        Column::new()
                            .push(Space::with_height(Length::Fill))
                            .push(Text::new("Bitcoin Test").size(network_size))
                            .push(Text::new(test_version).style(theme::Text::Color(version_color)))
                            .push(Space::with_height(Length::Fill))
                            .width(vertical_rule_position)
                            .align_items(Alignment::Center),
                    )
                    .push(
                        Column::new()
                            .push(Space::with_height(10))
                            .push(Rule::vertical(1).style(theme::Rule::Light))
                            .push(Space::with_height(5)),
                    )
                    .push(Space::with_width(Length::Fill))
                    .push(
                        Column::new()
                            .push(Space::with_height(Length::Fill))
                            .push(test_button)
                            .push(Space::with_height(Length::Fill)),
                    )
                    .push(Space::with_width(Length::Fill)),
            ),
    )
    .style(theme::Container::Frame)
    .padding(10)
    .height(200)
}
