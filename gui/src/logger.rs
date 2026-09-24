use chrono::Local;
use colored::Colorize;

pub fn set_logger() {
    fern::Dispatch::new()
        .format(|out, message, record| {
            let color = match record.level() {
                log::Level::Error => "red",
                log::Level::Warn => "yellow",
                log::Level::Info => "green",
                log::Level::Debug => "blue",
                log::Level::Trace => "magenta",
            };
            let formatted = format!(
                "[{}][{}][{}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.target(),
                record.level(),
                message
            );
            out.finish(format_args!("{}", formatted.color(color)))
        })
        .level(log::LevelFilter::Info)
        .level_for("ledger_transport_hidapi", log::LevelFilter::Error)
        .chain(std::io::stdout())
        .apply()
        .expect("the logger is only set once");
}
