mod bitbox;
mod device_service;
mod gui;
mod ledger;
mod logger;
mod service;
mod theme;

use crate::{
    device_service::DeviceService,
    gui::{Bacca, Flags},
    service::ServiceFn,
};
use iced::{window::icon, Application, Settings, Size};

#[tokio::main]
async fn main() {
    logger::set_logger(true);

    let (device_sender, gui_device_receiver) = async_channel::unbounded();
    let (gui_device_sender, device_receiver) = async_channel::unbounded();

    let flags = Flags {
        device_sender: gui_device_sender.clone(),
        device_receiver: gui_device_receiver,
    };

    let device = DeviceService::new(device_sender, device_receiver, gui_device_sender);
    device.start();

    const ICON: &[u8] = include_bytes!("./sardine.png");
    let icon = icon::from_file_data(ICON, None).unwrap();

    let mut settings = Settings::with_flags(flags);
    settings.window.size = Size::new(560.0, 640.0);
    settings.window.resizable = false;
    settings.window.icon = Some(icon);

    Bacca::run(settings).expect("Fail to launch application!")
}
