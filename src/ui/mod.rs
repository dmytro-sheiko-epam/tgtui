//! Rendering. Every frame is drawn from scratch out of [`App`].

pub mod chat_list;
pub mod chat_view;
pub mod forward_picker;
pub mod images;
pub mod login;
pub mod menu;
pub mod peer_view;
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
        // Full screen like the viewer, and for the same reason: there is a picture and a
        // paragraph to fit, and neither reads well in a popup sized to a menu.
        Screen::Main if app.peer_info.is_some() => peer_view::render(frame, body, app, images),
        Screen::Main => {
            let [list_area, chat_area] =
                Layout::horizontal([Constraint::Length(CHAT_LIST_WIDTH), Constraint::Min(20)])
                    .areas(body);

            chat_list::render(frame, list_area, app);
            chat_view::render(frame, chat_area, app, images);

            // Last, and over the whole body: a menu floats, and the pane underneath is the
            // context for what it is about to do. Only one can be open at a time — `handle_main_key`
            // routes every key to whichever it is — so the order between them never shows.
            if let Some(menu) = &app.menu {
                menu::render(frame, body, &menu::chat(menu));
            }
            if let Some(menu) = &app.message_menu {
                menu::render(frame, body, &menu::message(menu));
            }
            // Over both: by the time it is up, the menu that opened it has closed, and it is the
            // one the keyboard is aimed at.
            forward_picker::render(frame, body, app);
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
        } else if app.peer_info.is_some() {
            // `handle_peer_info_key` swallows everything else, so offering the compose box's keys
            // here would be a lie about what the keyboard does — the same reason the menu drops
            // its own keys while a confirmation is up.
            " ↑/↓ scroll · Esc close"
        } else if app.forward.is_some() {
            " type to search · ↑/↓ select · Enter forward · Esc cancel"
        } else if let Some(menu) = &app.menu {
            // While a question is up the only two keys that do anything are `y` and `n`, so
            // offering the menu's own keys here would be a lie.
            if menu.confirming.is_some() {
                " y confirm · n cancel"
            } else {
                " ↑/↓ select · Enter do it · Esc close"
            }
        } else if let Some(menu) = &app.message_menu {
            if menu.confirming.is_some() {
                " y confirm · n cancel"
            } else {
                " ↑/↓ select · Enter do it · Esc close"
            }
        } else if app.selecting() {
            " ↑/↓ message · Enter actions · Esc back to writing"
        } else {
            match app.focus {
                Focus::Chats => {
                    " ↑/↓ select · Enter open · Ctrl+O/E folder · Ctrl+A actions · Tab compose · Ctrl+C quit"
                }
                Focus::Messages => {
                    " type to write · Enter send · Ctrl+S pick a message · Ctrl+P picture · Ctrl+A actions · Tab/Esc back"
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
    use crate::state::dialog_list::FolderTab;
    use crate::telegram::TgEvent;
    use crate::test_support::{
        app, channel_dialog, dialog, folder, gradient, loaded_photo_message, message, page, peer,
        profile_with_avatar,
    };

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
            archived: false,
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
    fn the_strip_names_every_folder_and_the_pane_shows_the_one_it_is_on() {
        let mut app = loaded_app();
        app.handle_event(TgEvent::FoldersLoaded {
            folders: vec![folder("Work", &[peer(2).id])],
        });

        let strip = screen(&mut app);
        for tab in ["All", "Work", "Archive"] {
            assert!(strip.contains(tab), "tab {tab} missing:\n{strip}");
        }

        // Through the key rather than the field, so the chat pane follows the new selection too —
        // leaving it on a chat the list no longer offers would be the bug worth catching here.
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        assert_eq!(app.dialogs.tab, FolderTab::Custom(0));

        let filtered = screen(&mut app);
        assert!(
            filtered.contains("Bob") && !filtered.contains("Alice"),
            "the list must show the folder's members and only those:\n{filtered}"
        );
    }

    /// A folder still filling as the main list pages in looks exactly like an account with no
    /// chats, and "no chats" would be the wrong thing to conclude from it.
    #[test]
    fn an_empty_tab_says_which_kind_of_empty_it_is() {
        let mut app = loaded_app();
        app.handle_event(TgEvent::FoldersLoaded {
            folders: vec![folder("Work", &[])],
        });

        app.dialogs.tab = FolderTab::Archive;
        assert!(screen(&mut app).contains("no archived chats"));

        app.dialogs.tab = FolderTab::Custom(0);
        assert!(screen(&mut app).contains("nothing in this folder"));
    }

    /// A wide strip has to be windowed rather than truncated: the tab you are on is the one that
    /// must stay legible, whatever else gets cut.
    #[test]
    fn a_strip_too_wide_for_the_pane_keeps_the_active_tab_on_screen() {
        let mut app = loaded_app();
        app.handle_event(TgEvent::FoldersLoaded {
            folders: (1..=8)
                .map(|n| folder(&format!("Folder {n}"), &[]))
                .collect(),
        });
        app.dialogs.tab = FolderTab::Custom(7);

        let screen = screen(&mut app);
        assert!(screen.contains("Folder 8"), "{screen}");
        assert!(
            screen.contains('‹'),
            "the strip must say that it has been cut:\n{screen}"
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

    #[test]
    fn an_unread_chat_wears_its_count_in_the_list() {
        let mut app = loaded_app();
        // Chat 1 is the open one, so chat 2 is the one that can carry a badge.
        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(2),
            message: message(1, "you around?"),
            edited: false,
        });
        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(2),
            message: message(2, "still there?"),
            edited: false,
        });

        let screen = screen(&mut app);
        assert!(
            screen.contains("Bob") && screen.contains(" 2 "),
            "the badge must show how many are waiting:\n{screen}"
        );
    }

    // -- the chat action menu ------------------------------------------------

    fn with_menu(app: &mut App) -> String {
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        screen(app)
    }

    #[test]
    fn the_menu_floats_over_the_panes_rather_than_replacing_them() {
        let mut app = loaded_app();

        let screen = with_menu(&mut app);

        assert!(screen.contains("Mute"), "the menu is missing:\n{screen}");
        assert!(screen.contains("Delete chat"), "{screen}");
        assert!(
            screen.contains("Bob"),
            "the list underneath is the context for the action, so it must stay visible:\n{screen}"
        );
    }

    #[test]
    fn the_menu_names_the_chat_it_will_act_on() {
        let mut app = loaded_app();
        assert!(with_menu(&mut app).contains("Alice"));
    }

    #[test]
    fn a_channel_menu_says_leave_channel() {
        let (mut app, _rx) = app();
        app.handle_event(TgEvent::Authorized(true));
        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![channel_dialog(9, "Rust Blog")],
            exhausted: true,
            archived: false,
        });

        let screen = with_menu(&mut app);

        assert!(screen.contains("Leave channel"), "{screen}");
        assert!(
            !screen.contains("Clear history"),
            "a broadcast channel has no copy of its history that is yours to clear:\n{screen}"
        );
    }

    #[test]
    fn the_confirmation_replaces_the_menu_and_says_which_keys_answer_it() {
        let mut app = loaded_app();
        with_menu(&mut app);
        // Walk to "Delete chat", the last entry, and pick it.
        for _ in 0..6 {
            app.handle_key(crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Down,
                crossterm::event::KeyModifiers::empty(),
            ));
        }
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::empty(),
        ));

        let screen = screen(&mut app);
        assert!(screen.contains("Confirm"), "{screen}");
        assert!(screen.contains("Delete the chat with Alice?"), "{screen}");
        assert!(
            screen.contains("y confirm"),
            "the footer must say how to answer:\n{screen}"
        );
        assert!(
            // A menu entry rather than "Archive", which is also the name of a folder tab.
            !screen.contains("Pin to top"),
            "an unanswered question must not leave the menu behind it clickable:\n{screen}"
        );
    }

    #[test]
    fn a_muted_or_pinned_chat_is_marked_in_the_list() {
        let mut app = loaded_app();
        app.handle_event(TgEvent::MuteChanged {
            peer: peer(2).id,
            muted: true,
        });
        app.handle_event(TgEvent::PinChanged {
            peer: peer(2).id,
            pinned: true,
        });
        // Clear the status banner so the row markers are what the assertion is reading.
        app.status = None;

        let screen = screen(&mut app);
        let row = screen
            .lines()
            .find(|line| line.contains("Bob"))
            .unwrap_or_default();
        assert!(row.contains('~'), "no mute marker on the row: {row:?}");
        assert!(row.contains('^'), "no pin marker on the row: {row:?}");
    }

    /// Open the message cursor's menu on the newest message and pick an entry by label.
    fn with_message_action(app: &mut App, label: &str) -> String {
        painted(app, 80, 24);
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('s'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        app.handle_key(key(crossterm::event::KeyCode::Enter));

        let menu = app.message_menu.as_ref().expect("the menu should be open");
        let at = menu
            .actions
            .iter()
            .position(|action| action.label() == label)
            .expect("no such entry");
        for _ in 0..at {
            app.handle_key(key(crossterm::event::KeyCode::Down));
        }
        app.handle_key(key(crossterm::event::KeyCode::Enter));

        painted(app, 80, 24)
    }

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::empty())
    }

    #[test]
    fn the_message_menu_floats_over_the_transcript() {
        let mut app = loaded_app();
        painted(&mut app, 80, 24);
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('s'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        app.handle_key(key(crossterm::event::KeyCode::Enter));

        let screen = painted(&mut app, 80, 24);

        assert!(screen.contains("Reply"), "the menu is missing:\n{screen}");
        assert!(screen.contains("Forward"), "{screen}");
        assert!(
            screen.contains("Alice"),
            "the pane underneath is the context for the action:\n{screen}"
        );
    }

    #[test]
    fn the_forward_picker_lists_the_chats_it_can_send_to() {
        let mut app = loaded_app();

        let screen = with_message_action(&mut app, "Forward to…");

        assert!(
            screen.contains("Forward to"),
            "the picker's title is missing:\n{screen}"
        );
        assert!(screen.contains("type to search"), "{screen}");
        assert!(screen.contains("Bob"), "{screen}");
        assert!(
            screen.contains("Enter forward"),
            "the footer must say how to send:\n{screen}"
        );
    }

    #[test]
    fn the_forward_picker_says_when_nothing_matches() {
        let mut app = loaded_app();
        with_message_action(&mut app, "Forward to…");
        for ch in "zzz".chars() {
            app.handle_key(key(crossterm::event::KeyCode::Char(ch)));
        }

        let screen = painted(&mut app, 80, 24);

        assert!(screen.contains("no chat by that name"), "{screen}");
    }

    #[test]
    fn the_forward_picker_does_not_panic_in_a_terminal_with_no_room() {
        let mut app = loaded_app();
        with_message_action(&mut app, "Forward to…");

        for (width, height) in [(20, 5), (40, 3), (200, 60), (10, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut images = ImageStore::disabled();
            terminal
                .draw(|frame| draw(frame, &mut app, &mut images))
                .unwrap();
        }
    }

    #[test]
    fn the_message_menu_does_not_panic_in_a_terminal_with_no_room() {
        let mut app = loaded_app();
        painted(&mut app, 80, 24);
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('s'),
            crossterm::event::KeyModifiers::CONTROL,
        ));
        app.handle_key(key(crossterm::event::KeyCode::Enter));

        for (width, height) in [(20, 5), (40, 3), (200, 60), (10, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut images = ImageStore::disabled();
            terminal
                .draw(|frame| draw(frame, &mut app, &mut images))
                .unwrap();
        }
    }

    // -- the profile screen --------------------------------------------------

    /// The selected chat's profile, open and answered, with a picture not yet downloaded.
    fn with_profile(app: &mut App) {
        app.open_peer_info();
        app.handle_event(TgEvent::PeerInfoLoaded {
            peer: peer(1).id,
            info: Ok(Box::new(profile_with_avatar())),
        });
    }

    /// Which row a piece of text landed on, so two frames can be compared for movement.
    fn row_of(screen: &str, needle: &str) -> Option<usize> {
        screen.lines().position(|line| line.contains(needle))
    }

    #[test]
    fn the_profile_fills_the_screen_and_hides_the_chat_list() {
        let mut app = loaded_app();
        with_profile(&mut app);

        let screen = screen(&mut app);

        assert!(
            screen.contains("This is Alice."),
            "the bio belongs on the profile:\n{screen}"
        );
        assert!(
            screen.contains("Peer id"),
            "the field table is missing:\n{screen}"
        );
        assert!(
            !screen.contains("Chats"),
            "the profile is full screen, so the chat list must be gone:\n{screen}"
        );
        assert!(
            screen.contains("Esc close"),
            "the footer must say how to get out, and must not offer keys the screen swallows:\n\
             {screen}"
        );
        assert!(
            !screen.contains("Tab compose"),
            "`handle_peer_info_key` swallows the compose keys, so advertising them would be a \
             lie about what the keyboard does:\n{screen}"
        );
    }

    /// The invariant the whole `reserve`/`prepare` design exists for, on this screen: the header
    /// claims the avatar's box from its *stated* pixel size, so the picture landing must not move
    /// a single field.
    #[test]
    fn the_fields_do_not_move_when_the_profile_picture_arrives() {
        let mut app = loaded_app();
        with_profile(&mut app);

        let before = painted(&mut app, 80, 24);
        app.handle_event(TgEvent::AvatarLoaded {
            peer: peer(1).id,
            image: Some(gradient(160, 160)),
        });
        let after = painted(&mut app, 80, 24);

        assert!(
            after.contains('▀') || after.contains('▄'),
            "the picture never reached the buffer, so this proves nothing:\n{after}"
        );
        for field in ["This is Alice.", "Peer id"] {
            assert_eq!(
                row_of(&before, field),
                row_of(&after, field),
                "{field:?} moved when the picture arrived; the box is claimed before the \
                 download precisely so the reader's eyes stay put:\n{before}\n{after}"
            );
        }
    }

    /// `AVATAR_ROWS` is an absolute cap that knows nothing about how tall the terminal is, so a
    /// short one has to clamp the picture to the rows the layout actually granted.
    #[test]
    fn a_short_terminal_keeps_the_profile_picture_inside_the_pane() {
        let mut app = loaded_app();
        with_profile(&mut app);
        app.handle_event(TgEvent::AvatarLoaded {
            peer: peer(1).id,
            image: Some(gradient(160, 160)),
        });

        let screen = painted(&mut app, 60, 10);

        let border = screen.lines().nth(8).unwrap_or_default().to_string();
        assert!(
            !border.contains('▀') && !border.contains('▄'),
            "the picture smeared over the pane border: {border:?}\n{screen}"
        );
    }

    #[test]
    fn the_profile_does_not_panic_in_a_terminal_with_no_room() {
        let mut app = loaded_app();
        with_profile(&mut app);
        app.handle_event(TgEvent::AvatarLoaded {
            peer: peer(1).id,
            image: Some(gradient(160, 160)),
        });

        for (width, height) in [(20, 5), (40, 3), (200, 60), (10, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut images = ImageStore::for_tests();
            terminal
                .draw(|frame| draw(frame, &mut app, &mut images))
                .unwrap();
        }
    }

    /// Scrolling is clamped against the profile the frame just measured, so walking off the end
    /// must settle rather than empty the pane.
    #[test]
    fn scrolling_past_the_end_of_a_profile_settles_rather_than_blanking_it() {
        let mut app = loaded_app();
        with_profile(&mut app);
        painted(&mut app, 80, 24);

        for _ in 0..40 {
            app.handle_key(key(crossterm::event::KeyCode::Down));
            painted(&mut app, 80, 24);
        }

        let screen = painted(&mut app, 80, 24);
        assert!(
            screen.contains("Peer id"),
            "the last field must stay on screen; blank rows would be indistinguishable from a \
             failed render:\n{screen}"
        );
    }

    #[test]
    fn the_menu_does_not_panic_in_a_terminal_with_no_room() {
        let mut app = loaded_app();
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('a'),
            crossterm::event::KeyModifiers::CONTROL,
        ));

        for (width, height) in [(20, 5), (40, 3), (200, 60), (10, 2)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let mut images = ImageStore::disabled();
            terminal
                .draw(|frame| draw(frame, &mut app, &mut images))
                .unwrap();
        }
    }
}
