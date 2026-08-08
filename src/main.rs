//! tgtui — a terminal Telegram client: chat list, message history, and plain text replies.

mod app;
mod config;
mod event;
mod state;
mod telegram;
mod ui;

use color_eyre::eyre::Result;
use tracing_subscriber::EnvFilter;

use crate::app::App;
use crate::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let config = Config::load()?;
    // Keep the guard alive for the whole run or buffered log lines are lost on exit.
    let _log_guard = init_logging(&config);

    let telegram = telegram::spawn(&config).await?;
    let mut app = App::new(telegram.commands);

    // `ratatui::init` enables raw mode, switches to the alternate screen, and chains a panic
    // hook that restores both before color-eyre prints its report.
    let mut terminal = ratatui::init();
    let result = event::run(&mut terminal, &mut app, telegram.events).await;
    ratatui::restore();

    result
}

/// Send logs to a file: stdout belongs to the TUI while it is running.
fn init_logging(config: &Config) -> tracing_appender::non_blocking::WorkerGuard {
    let appender = tracing_appender::rolling::daily(&config.log_dir, "tgtui.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);

    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_env("TGTUI_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .init();

    guard
}
