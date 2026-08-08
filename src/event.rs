//! The main loop: terminal input, Telegram events and a redraw tick, merged.

use std::time::Duration;

use color_eyre::eyre::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::app::App;
use crate::telegram::TgEvent;
use crate::ui;

/// Forces a redraw often enough for spinners and the status banner to feel live.
const TICK: Duration = Duration::from_millis(250);

pub async fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    mut events: mpsc::UnboundedReceiver<TgEvent>,
) -> Result<()> {
    let mut input = EventStream::new();
    let mut ticker = tokio::time::interval(TICK);

    loop {
        terminal.draw(|frame| ui::draw(frame, app))?;
        if app.should_quit {
            return Ok(());
        }

        tokio::select! {
            terminal_event = input.next() => match terminal_event {
                // Windows reports both press and release; only act on the press.
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => app.handle_key(key),
                Some(Ok(_)) => {}
                Some(Err(err)) => return Err(err.into()),
                None => return Ok(()),
            },
            telegram_event = events.recv() => match telegram_event {
                Some(event) => app.handle_event(event),
                // The actor is gone, so nothing more will ever arrive.
                None => return Ok(()),
            },
            _ = ticker.tick() => app.tick(),
        }
    }
}
