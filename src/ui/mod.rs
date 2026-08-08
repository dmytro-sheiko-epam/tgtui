//! Rendering. Every frame is drawn from scratch out of [`App`].

pub mod chat_list;
pub mod chat_view;
pub mod login;
pub mod text;
pub mod widgets;

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::{App, Focus, Screen};

/// Width of the chat list pane.
const CHAT_LIST_WIDTH: u16 = 32;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());

    match app.screen {
        Screen::Main => {
            let [list_area, chat_area] = Layout::horizontal([
                Constraint::Length(CHAT_LIST_WIDTH),
                Constraint::Min(20),
            ])
            .areas(body);

            chat_list::render(frame, list_area, app);
            chat_view::render(frame, chat_area, app);
        }
        _ => login::render(frame, body, app),
    }

    render_footer(frame, footer, app);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    // A status message takes precedence over the key hints while it is alive.
    let line = if let Some(status) = &app.status {
        Line::from(Span::styled(
            format!(" {}", status.text),
            Style::default().fg(Color::Yellow),
        ))
    } else if matches!(app.screen, Screen::Main) {
        let hints = match app.focus {
            Focus::Chats => " ↑/↓ select · Enter open · Tab compose · Ctrl+C quit",
            Focus::Messages => {
                " type to write · Enter send · ↑/↓ & PgUp/PgDn scroll · Tab/Esc back"
            }
        };
        Line::from(Span::styled(hints, Style::default().fg(Color::DarkGray)))
    } else {
        Line::default()
    };

    frame.render_widget(Paragraph::new(line), area);
}
