mod bitbox;
mod gui;
mod jade;
mod ledger;
mod logger;
mod theme;
mod worker;

use iced::{window, Application, Settings, Size};

fn main() -> iced::Result {
    logger::set_logger();
    gui::Bacca::run(Settings {
        fonts: vec![include_bytes!("iconex-icons.ttf").as_slice().into()],
        window: window::Settings {
            size: Size::new(560.0, 640.0),
            resizable: false,
            icon: window::icon::from_file_data(include_bytes!("sardine.png"), None).ok(),
            ..Default::default()
        },
        ..Default::default()
    })
}
