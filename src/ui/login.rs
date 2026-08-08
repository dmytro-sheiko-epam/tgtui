//! The phone / code / 2FA-password screens.
//!
//! All three ask for a single line of text, so they share one prompt and differ only in
//! their labels and whether the input is masked.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, Screen};
use crate::ui::widgets::centered;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let (title, help, masked) = match &app.screen {
        Screen::Connecting => (
            "Connecting",
            "checking your saved session...".to_string(),
            false,
        ),
        Screen::Phone => (
            "Sign in to Telegram",
            "Phone number, with country code (e.g. +15551234567)".to_string(),
            false,
        ),
        Screen::Code => (
            "Confirmation code",
            "Telegram sent a code to your other devices".to_string(),
            false,
        ),
        Screen::Password { hint } => (
            "Two-factor password",
            match hint {
                Some(hint) if !hint.is_empty() => format!("Password hint: {hint}"),
                _ => "This account is protected by a cloud password".to_string(),
            },
            true,
        ),
        Screen::Main => return,
    };

    let box_area = centered(area, 64, 9);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(Color::Cyan).bold(),
        ));
    let inner = block.inner(box_area);
    frame.render_widget(block, box_area);

    let [help_area, input_area, status_area] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Min(1),
    ])
    .areas(inner);

    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::Gray)),
        help_area,
    );

    let connecting = matches!(app.screen, Screen::Connecting);
    if !connecting {
        let shown = if masked {
            "*".repeat(app.input.chars().count())
        } else {
            app.input.clone()
        };
        frame.render_widget(
            Paragraph::new(shown.as_str()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::DarkGray)),
            ),
            input_area,
        );
        // Put the real terminal cursor where the next character will land.
        frame.set_cursor_position(Position::new(
            input_area.x + 1 + shown.chars().count() as u16,
            input_area.y + 1,
        ));
    }

    let status = if let Some(error) = &app.login_error {
        Span::styled(error.as_str(), Style::default().fg(Color::Red))
    } else if app.submitting || connecting {
        Span::styled("working...", Style::default().fg(Color::Yellow))
    } else {
        Span::styled(
            "Enter to continue · Ctrl+C to quit",
            Style::default().fg(Color::DarkGray),
        )
    };
    frame.render_widget(Paragraph::new(Line::from(status)), status_area);
}
