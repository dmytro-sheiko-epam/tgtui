//! Rendering. Every frame is drawn from scratch out of [`App`].

pub mod chat_list;
pub mod chat_view;
pub mod images;
pub mod login;
pub mod photo_view;
pub mod text;
pub mod widgets;

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::app::{App, Focus, Screen, StatusKind};
use crate::ui::images::ImageStore;

/// Width of the chat list pane.
const CHAT_LIST_WIDTH: u16 = 32;

pub fn draw(frame: &mut Frame, app: &mut App, images: &mut ImageStore) {
    let [body, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());

    match app.screen {
        // The viewer takes the whole body rather than floating over the panes: a picture worth
        // opening full screen is worth every column.
        Screen::Main if app.viewer.is_some() => photo_view::render(frame, body, app, images),
        Screen::Main => {
            let [list_area, chat_area] =
                Layout::horizontal([Constraint::Length(CHAT_LIST_WIDTH), Constraint::Min(20)])
                    .areas(body);

            chat_list::render(frame, list_area, app);
            chat_view::render(frame, chat_area, app, images);
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
        let hints = if app.viewer.is_some() {
            " ←/→ previous/next picture · Esc close"
        } else {
            match app.focus {
                Focus::Chats => " ↑/↓ select · Enter open · Tab compose · Ctrl+C quit",
                Focus::Messages => {
                    " type to write · Enter send · ↑/↓ scroll · Ctrl+P picture · Tab/Esc back"
                }
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
    use crate::test_support::{app, dialog, loaded_photo_message, page, peer};

    /// Render an app to a fixed-size test terminal and return the screen as text.
    fn screen(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        // Graphics escape sequences never reach a `TestBackend` cell grid, so these assertions
        // are about text either way; a disabled store keeps them honest about that.
        let mut images = ImageStore::disabled();
        terminal
            .draw(|frame| draw(frame, app, &mut images))
            .unwrap();
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

    /// Render with a real half-block picker, which is the only protocol a cell grid can see.
    fn painted(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut images = ImageStore::for_tests();
        terminal
            .draw(|frame| draw(frame, app, &mut images))
            .unwrap();
        format!("{}", terminal.backend())
    }

    fn with_photo(id: i32, caption: &str) -> App {
        let mut app = loaded_app();
        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(1),
            message: loaded_photo_message(id, caption, 100, 200),
            edited: false,
        });
        app
    }

    /// Half-blocks are ordinary cells, so unlike Kitty or Sixel escape sequences they show up in
    /// a `TestBackend`. That makes them the one protocol that can prove the whole path — decoded
    /// image, encoded protocol, widget placed at the reserved rows — actually paints something.
    #[test]
    fn a_photo_paints_over_the_rows_it_reserved() {
        let mut app = with_photo(101, "");

        let screen = painted(&mut app, 80, 20);

        assert!(
            screen.contains('▀') || screen.contains('▄'),
            "the picture never reached the buffer:\n{screen}"
        );
        assert!(
            !screen.contains("[photo]"),
            "the label must give way once the picture is drawn:\n{screen}"
        );
    }

    #[test]
    fn the_viewer_fills_the_screen_and_hides_the_chat_list() {
        let mut app = with_photo(101, "look at this");
        painted(&mut app, 80, 30); // a frame, so the render pass reports what is on screen
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('p'),
            crossterm::event::KeyModifiers::CONTROL,
        ));

        let screen = painted(&mut app, 80, 30);

        assert!(
            screen.contains('▀') || screen.contains('▄'),
            "the picture never reached the buffer:\n{screen}"
        );
        assert!(
            screen.contains("photo · Alice"),
            "the viewer's title is missing:\n{screen}"
        );
        assert!(
            screen.contains("look at this"),
            "the caption belongs under the picture:\n{screen}"
        );
        assert!(
            !screen.contains("Chats"),
            "the viewer is full screen, so the chat list must be gone:\n{screen}"
        );
        assert!(
            screen.contains("Esc close"),
            "the footer must say how to get out:\n{screen}"
        );
    }

    #[test]
    fn a_bigger_terminal_draws_a_bigger_picture() {
        fn rows_painted(width: u16, height: u16) -> usize {
            let mut app = with_photo(101, "");
            painted(&mut app, width, height)
                .lines()
                .filter(|line| line.contains('▀') || line.contains('▄'))
                .count()
        }

        assert!(
            rows_painted(80, 40) > rows_painted(80, 16),
            "a picture is sized against the transcript, so more room must mean a bigger picture"
        );
    }

    #[test]
    fn a_narrow_terminal_does_not_panic() {
        let mut app = with_photo(101, "a caption");
        for (width, height) in [(20, 5), (40, 3), (200, 60), (10, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut images = ImageStore::for_tests();
            terminal
                .draw(|frame| draw(frame, &mut app, &mut images))
                .unwrap();
        }
    }

    #[test]
    fn the_viewer_does_not_panic_in_a_terminal_with_no_room() {
        let mut app = with_photo(101, "a caption");
        painted(&mut app, 80, 30);
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('p'),
            crossterm::event::KeyModifiers::CONTROL,
        ));

        for (width, height) in [(20, 5), (40, 3), (200, 60), (10, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut images = ImageStore::for_tests();
            terminal
                .draw(|frame| draw(frame, &mut app, &mut images))
                .unwrap();
        }
    }
}
