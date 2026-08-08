//! Rendering. Every frame is drawn from scratch out of [`App`].

pub mod chat_list;
pub mod chat_view;
pub mod login;
pub mod text;
pub mod widgets;

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::{App, Focus, Screen, StatusKind};

/// Width of the chat list pane.
const CHAT_LIST_WIDTH: u16 = 32;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());

    match app.screen {
        Screen::Main => {
            let [list_area, chat_area] =
                Layout::horizontal([Constraint::Length(CHAT_LIST_WIDTH), Constraint::Min(20)])
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
        let color = match status.kind {
            StatusKind::Error => Color::Red,
            StatusKind::Info => Color::Green,
        };
        Line::from(Span::styled(
            format!(" {}", status.text),
            Style::default().fg(color),
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

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::App;
    use crate::state::chat_buffer::PAGE_SIZE;
    use crate::telegram::TgEvent;
    use crate::test_support::{app, dialog, page, peer};

    /// Render an app to a fixed-size test terminal and return the screen as text.
    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        format!("{}", terminal.backend())
    }

    fn loaded_app() -> App {
        let (mut app, _rx) = app();
        app.handle_event(TgEvent::Authorized(true));
        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![dialog(1, "Alice"), dialog(2, "Bob")],
            exhausted: true,
        });
        app.handle_event(TgEvent::MessagesLoaded {
            peer: peer(1).id,
            messages: page(100, PAGE_SIZE as i32),
        });
        app
    }

    #[test]
    fn the_login_screen_prompts_for_a_phone_number() {
        let (mut app, _rx) = app();
        app.handle_event(TgEvent::Authorized(false));

        let screen = screen(&mut app);
        assert!(screen.contains("Sign in to Telegram"), "{screen}");
        assert!(screen.contains("Phone number"), "{screen}");
    }

    #[test]
    fn the_main_screen_shows_both_panes() {
        let mut app = loaded_app();

        let screen = screen(&mut app);
        assert!(
            screen.contains("Chats"),
            "chat list pane missing:\n{screen}"
        );
        assert!(screen.contains("Alice"), "chat name missing:\n{screen}");
        assert!(screen.contains("Bob"), "second chat missing:\n{screen}");
        // The newest messages sit at the bottom of the transcript.
        assert!(
            screen.contains("message 100"),
            "newest message missing:\n{screen}"
        );
    }

    #[test]
    fn scrolling_up_reveals_older_messages() {
        let mut app = loaded_app();
        app.focus = Focus::Messages;
        screen(&mut app); // one frame so the metrics reflect the real viewport

        for _ in 0..3 {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::PageUp,
                crossterm::event::KeyModifiers::empty(),
            ));
        }

        let screen = screen(&mut app);
        assert!(
            !screen.contains("message 100"),
            "the newest message should have scrolled off:\n{screen}"
        );
    }

    #[test]
    fn the_status_banner_replaces_the_key_hints() {
        let mut app = loaded_app();
        assert!(screen(&mut app).contains("Enter open"));

        app.handle_event(TgEvent::Error("could not send message".to_string()));

        let screen = screen(&mut app);
        assert!(screen.contains("could not send message"), "{screen}");
        assert!(!screen.contains("Enter open"), "{screen}");
    }

    #[test]
    fn a_narrow_terminal_does_not_panic() {
        let mut app = loaded_app();
        for (width, height) in [(20, 5), (40, 3), (200, 60), (10, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| draw(frame, &mut app)).unwrap();
        }
    }
}
