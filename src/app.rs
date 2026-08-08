//! Application state and the reducers driving it.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use grammers_session::types::{PeerId, PeerKind};
use tokio::sync::mpsc;

use crate::state::chat_buffer::ChatBuffer;
use crate::state::dialog_list::DialogListState;
use crate::telegram::{TgCommand, TgEvent};

/// How long a status banner stays on screen before it fades away.
const STATUS_TTL: Duration = Duration::from_secs(6);

/// Load more history once the view is within this many lines of the top of the buffer.
const SCROLL_PREFETCH_LINES: usize = 10;

#[derive(Debug)]
pub enum Screen {
    /// Checking whether the persisted session is still valid.
    Connecting,
    Phone,
    Code,
    Password {
        hint: Option<String>,
    },
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Chats,
    Messages,
}

/// Whether a status line reports a problem or just narrates progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Error,
}

#[derive(Debug)]
pub struct Status {
    pub text: String,
    pub kind: StatusKind,
    shown_at: Instant,
}

/// What the last render measured, so key handling knows when the view is at the top.
#[derive(Debug, Default, Clone, Copy)]
pub struct ChatViewMetrics {
    pub total_lines: usize,
    pub viewport: usize,
}

impl ChatViewMetrics {
    pub fn max_scroll(&self) -> usize {
        self.total_lines.saturating_sub(self.viewport)
    }
}

pub struct App {
    pub screen: Screen,
    /// Text being typed on whichever login screen is active.
    pub input: String,
    /// A login request is in flight; the screen shows progress and ignores further submits.
    pub submitting: bool,
    pub login_error: Option<String>,
    pub status: Option<Status>,
    pub dialogs: DialogListState,
    pub chats: HashMap<PeerId, ChatBuffer>,
    pub open_chat: Option<PeerId>,
    pub compose: String,
    pub focus: Focus,
    pub metrics: ChatViewMetrics,
    pub should_quit: bool,
    commands: mpsc::UnboundedSender<TgCommand>,
}

impl App {
    pub fn new(commands: mpsc::UnboundedSender<TgCommand>) -> Self {
        let app = Self {
            screen: Screen::Connecting,
            input: String::new(),
            submitting: false,
            login_error: None,
            status: None,
            dialogs: DialogListState::default(),
            chats: HashMap::new(),
            open_chat: None,
            compose: String::new(),
            focus: Focus::Chats,
            metrics: ChatViewMetrics::default(),
            should_quit: false,
            commands,
        };
        app.send(TgCommand::CheckAuthorized);
        app
    }

    fn send(&self, command: TgCommand) {
        if self.commands.send(command).is_err() {
            tracing::warn!("telegram actor is gone; command dropped");
        }
    }

    fn set_status(&mut self, text: impl Into<String>, kind: StatusKind) {
        self.status = Some(Status {
            text: text.into(),
            kind,
            shown_at: Instant::now(),
        });
    }

    /// The buffer for the chat currently on screen, if any.
    pub fn open_buffer(&self) -> Option<&ChatBuffer> {
        self.open_chat.and_then(|id| self.chats.get(&id))
    }

    fn open_buffer_mut(&mut self) -> Option<&mut ChatBuffer> {
        let id = self.open_chat?;
        self.chats.get_mut(&id)
    }

    pub fn quit(&mut self) {
        self.send(TgCommand::Shutdown);
        self.should_quit = true;
    }

    /// Periodic housekeeping, driven by the redraw tick.
    pub fn tick(&mut self) {
        if self
            .status
            .as_ref()
            .is_some_and(|status| status.shown_at.elapsed() > STATUS_TTL)
        {
            self.status = None;
        }
    }

    // -- events from Telegram ------------------------------------------------

    pub fn handle_event(&mut self, event: TgEvent) {
        match event {
            TgEvent::Authorized(true) => self.enter_main(),
            TgEvent::Authorized(false) => {
                self.submitting = false;
                self.screen = Screen::Phone;
            }
            TgEvent::CodeSent => {
                self.submitting = false;
                self.input.clear();
                self.screen = Screen::Code;
            }
            TgEvent::PasswordNeeded { hint } => {
                self.submitting = false;
                self.input.clear();
                self.screen = Screen::Password { hint };
            }
            TgEvent::SignedIn { name } => {
                self.set_status(format!("signed in as {name}"), StatusKind::Info);
                self.enter_main();
            }
            TgEvent::LoginFailed(error) => {
                self.submitting = false;
                self.login_error = Some(error);
            }
            TgEvent::DialogsLoaded { items, exhausted } => {
                self.dialogs.extend(items, exhausted);
                // Show something as soon as the first page lands.
                if self.open_chat.is_none() {
                    self.open_selected_chat();
                }
            }
            TgEvent::MessagesLoaded { peer, messages } => {
                if let Some(buffer) = self.chats.get_mut(&peer) {
                    buffer.set_initial(messages);
                }
            }
            TgEvent::OlderMessagesLoaded { peer, messages } => {
                if let Some(buffer) = self.chats.get_mut(&peer) {
                    buffer.prepend_older(messages);
                }
            }
            TgEvent::MessageSent { peer, message } => {
                if let Some(buffer) = self.chats.get_mut(&peer) {
                    buffer.push_newest(message.clone());
                }
                self.dialogs.bump(peer, message.text);
            }
            TgEvent::MessagesDeleted { channel, ids } => self.remove_messages(channel, &ids),
            TgEvent::IncomingMessage {
                peer,
                message,
                edited,
            } => {
                if let Some(buffer) = self.chats.get_mut(&peer.id) {
                    if edited {
                        if let Some(existing) =
                            buffer.messages.iter_mut().find(|m| m.id == message.id)
                        {
                            *existing = message.clone();
                        }
                    } else {
                        buffer.push_newest(message.clone());
                    }
                }
                if !edited {
                    self.dialogs.bump(peer.id, message.text);
                }
            }
            TgEvent::Error(error) => {
                self.submitting = false;
                // Failing the startup check would otherwise strand us on the connecting screen,
                // so fall through to the phone prompt, where retrying reconnects.
                if matches!(self.screen, Screen::Connecting) {
                    self.screen = Screen::Phone;
                    self.login_error = Some(error);
                } else {
                    self.set_status(error, StatusKind::Error);
                }
            }
        }
    }

    /// Drop deleted messages from whichever buffer holds them.
    ///
    /// Telegram only names the chat for channel deletions. Everywhere else it sends bare
    /// message ids, which is still unambiguous because users and small groups draw from one
    /// per-account sequence — but channel ids restart at 1 per channel, so a peer-less
    /// deletion must skip channel buffers or it would delete an unrelated message that
    /// happens to share an id.
    fn remove_messages(&mut self, channel: Option<PeerId>, ids: &[i32]) {
        match channel {
            Some(channel) => {
                if let Some(buffer) = self.chats.get_mut(&channel) {
                    buffer.remove(ids);
                }
            }
            None => {
                for (peer, buffer) in self.chats.iter_mut() {
                    if peer.kind() != PeerKind::Channel {
                        buffer.remove(ids);
                    }
                }
            }
        }
    }

    fn enter_main(&mut self) {
        self.submitting = false;
        self.login_error = None;
        self.input.clear();
        self.screen = Screen::Main;
        self.dialogs.loading = true;
        self.send(TgCommand::LoadMoreDialogs);
    }

    // -- keyboard ------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        // Ctrl+C always quits, whatever is on screen.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q'))
        {
            self.quit();
            return;
        }

        match self.screen {
            Screen::Connecting => {}
            Screen::Phone | Screen::Code | Screen::Password { .. } => self.handle_login_key(key),
            Screen::Main => self.handle_main_key(key),
        }
    }

    fn handle_login_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(ch) => self.input.push(ch),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.submit_login(),
            _ => {}
        }
    }

    fn submit_login(&mut self) {
        if self.submitting || self.input.trim().is_empty() {
            return;
        }
        let value = self.input.trim().to_string();
        self.login_error = None;
        self.submitting = true;

        match self.screen {
            Screen::Phone => self.send(TgCommand::RequestLoginCode { phone: value }),
            Screen::Code => self.send(TgCommand::SignIn { code: value }),
            Screen::Password { .. } => self.send(TgCommand::CheckPassword { password: value }),
            _ => self.submitting = false,
        }
    }

    fn handle_main_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Chats => Focus::Messages,
                    Focus::Messages => Focus::Chats,
                };
                return;
            }
            KeyCode::Esc if self.focus == Focus::Messages => {
                self.focus = Focus::Chats;
                return;
            }
            KeyCode::PageUp => return self.scroll_messages_up(self.page_step()),
            KeyCode::PageDown => return self.scroll_messages_down(self.page_step()),
            _ => {}
        }

        match self.focus {
            Focus::Chats => self.handle_chats_key(key),
            Focus::Messages => self.handle_messages_key(key),
        }
    }

    fn handle_chats_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.dialogs.select_prev();
                self.open_selected_chat();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.dialogs.select_next();
                self.open_selected_chat();
                self.load_more_dialogs_if_needed();
            }
            KeyCode::Enter => {
                self.open_selected_chat();
                self.focus = Focus::Messages;
            }
            KeyCode::Char('q') => self.quit(),
            _ => {}
        }
    }

    fn handle_messages_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.scroll_messages_up(1),
            KeyCode::Down => self.scroll_messages_down(1),
            KeyCode::Char(ch) => self.compose.push(ch),
            KeyCode::Backspace => {
                self.compose.pop();
            }
            KeyCode::Enter => self.send_composed(),
            _ => {}
        }
    }

    fn page_step(&self) -> usize {
        self.metrics.viewport.max(1)
    }

    fn scroll_messages_up(&mut self, lines: usize) {
        let max_scroll = self.metrics.max_scroll();
        if let Some(buffer) = self.open_buffer_mut() {
            buffer.scroll = (buffer.scroll + lines).min(max_scroll);
        }
        self.load_older_if_needed();
    }

    fn scroll_messages_down(&mut self, lines: usize) {
        if let Some(buffer) = self.open_buffer_mut() {
            buffer.scroll = buffer.scroll.saturating_sub(lines);
        }
    }

    /// The heart of infinite scroll: once the view nears the top of what we hold, ask for more.
    fn load_older_if_needed(&mut self) {
        let max_scroll = self.metrics.max_scroll();
        let Some(buffer) = self.open_buffer_mut() else {
            return;
        };
        if !buffer.loaded
            || buffer.loading_older
            || !buffer.has_more_older
            || buffer.scroll + SCROLL_PREFETCH_LINES < max_scroll
        {
            return;
        }
        let Some(before_id) = buffer.oldest_id() else {
            return;
        };

        buffer.loading_older = true;
        let peer = buffer.peer;
        self.send(TgCommand::LoadOlderMessages { peer, before_id });
    }

    fn load_more_dialogs_if_needed(&mut self) {
        if self.dialogs.wants_more() {
            self.dialogs.loading = true;
            self.send(TgCommand::LoadMoreDialogs);
        }
    }

    fn open_selected_chat(&mut self) {
        let Some(peer) = self.dialogs.selected_peer() else {
            return;
        };
        if self.open_chat == Some(peer.id) {
            return;
        }
        self.open_chat = Some(peer.id);

        // Only the first visit hits the network; revisits reuse the cached buffer.
        match self.chats.entry(peer.id) {
            Entry::Occupied(_) => return,
            Entry::Vacant(entry) => entry.insert(ChatBuffer::new(peer)),
        };
        self.send(TgCommand::OpenChat { peer });
    }

    fn send_composed(&mut self) {
        let text = self.compose.trim().to_string();
        if text.is_empty() {
            return;
        }
        let Some(buffer) = self.open_buffer() else {
            return;
        };
        let peer = buffer.peer;

        self.compose.clear();
        self.send(TgCommand::SendMessage { peer, text });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::chat_buffer::PAGE_SIZE;
    use crate::telegram::TgEvent;
    use crate::test_support::{app, channel, dialog, drain, message, page, peer};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    /// Drive a fresh app to the point where chat 1 is open with a full page loaded.
    fn opened_chat() -> (App, mpsc::UnboundedReceiver<TgCommand>) {
        let (mut app, mut rx) = app();
        app.handle_event(TgEvent::Authorized(true));
        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![dialog(1, "Alice"), dialog(2, "Bob")],
            exhausted: true,
        });
        app.handle_event(TgEvent::MessagesLoaded {
            peer: peer(1).id,
            messages: page(100, PAGE_SIZE as i32),
        });
        drain(&mut rx);
        (app, rx)
    }

    #[test]
    fn startup_asks_whether_the_saved_session_still_works() {
        let (_app, mut rx) = app();
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::CheckAuthorized]
        ));
    }

    #[test]
    fn a_valid_session_skips_login_and_loads_chats() {
        let (mut app, mut rx) = app();
        drain(&mut rx);

        app.handle_event(TgEvent::Authorized(true));

        assert!(matches!(app.screen, Screen::Main));
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::LoadMoreDialogs]
        ));
    }

    #[test]
    fn login_walks_phone_then_code_then_password() {
        let (mut app, mut rx) = app();
        app.handle_event(TgEvent::Authorized(false));
        assert!(matches!(app.screen, Screen::Phone));

        app.input = "+15551234567".to_string();
        app.handle_key(key(KeyCode::Enter));
        app.handle_event(TgEvent::CodeSent);
        assert!(matches!(app.screen, Screen::Code));

        app.input = "12345".to_string();
        app.handle_key(key(KeyCode::Enter));
        app.handle_event(TgEvent::PasswordNeeded {
            hint: Some("pet".to_string()),
        });
        assert!(matches!(app.screen, Screen::Password { .. }));

        app.input = "hunter2".to_string();
        app.handle_key(key(KeyCode::Enter));
        app.handle_event(TgEvent::SignedIn {
            name: "Alice".to_string(),
        });
        assert!(matches!(app.screen, Screen::Main));

        let commands = drain(&mut rx);
        assert!(matches!(
            commands.as_slice(),
            [
                TgCommand::CheckAuthorized,
                TgCommand::RequestLoginCode { .. },
                TgCommand::SignIn { .. },
                TgCommand::CheckPassword { .. },
                TgCommand::LoadMoreDialogs,
            ]
        ));
    }

    #[test]
    fn a_rejected_code_keeps_the_screen_so_it_can_be_retyped() {
        let (mut app, _rx) = app();
        app.handle_event(TgEvent::Authorized(false));
        app.input = "+15551234567".to_string();
        app.handle_key(key(KeyCode::Enter));
        app.handle_event(TgEvent::CodeSent);

        app.input = "00000".to_string();
        app.handle_key(key(KeyCode::Enter));
        app.handle_event(TgEvent::LoginFailed("that code was not valid".to_string()));

        assert!(matches!(app.screen, Screen::Code));
        assert_eq!(app.login_error.as_deref(), Some("that code was not valid"));
        assert!(!app.submitting, "the screen must accept another attempt");
    }

    #[test]
    fn a_failed_startup_check_falls_through_to_the_phone_prompt() {
        let (mut app, _rx) = app();
        assert!(matches!(app.screen, Screen::Connecting));

        app.handle_event(TgEvent::Error("could not reach Telegram".to_string()));

        assert!(
            matches!(app.screen, Screen::Phone),
            "a network failure must not strand the user on the connecting screen"
        );
        assert!(app.login_error.is_some());
    }

    #[test]
    fn the_first_chat_opens_as_soon_as_the_list_arrives() {
        let (mut app, mut rx) = app();
        app.handle_event(TgEvent::Authorized(true));
        drain(&mut rx);

        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![dialog(1, "Alice")],
            exhausted: false,
        });

        assert_eq!(app.open_chat, Some(peer(1).id));
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::OpenChat { .. }]
        ));
    }

    #[test]
    fn revisiting_a_chat_reuses_the_cached_buffer() {
        let (mut app, mut rx) = opened_chat();

        app.handle_key(key(KeyCode::Down)); // to Bob
        app.handle_event(TgEvent::MessagesLoaded {
            peer: peer(2).id,
            messages: page(10, 2),
        });
        drain(&mut rx);

        app.handle_key(key(KeyCode::Up)); // back to Alice

        assert_eq!(app.open_chat, Some(peer(1).id));
        assert!(
            drain(&mut rx).is_empty(),
            "a cached chat must not be re-fetched"
        );
    }

    #[test]
    fn scrolling_near_the_top_asks_for_older_messages_exactly_once() {
        let (mut app, mut rx) = opened_chat();
        app.focus = Focus::Messages;
        // Stand in for a render: 100 lines of transcript in a 20 line viewport.
        app.metrics = ChatViewMetrics {
            total_lines: 100,
            viewport: 20,
        };

        // One page up is still far from the top (scroll 20, top is 80).
        app.handle_key(key(KeyCode::PageUp));
        assert!(
            drain(&mut rx).is_empty(),
            "must not prefetch from the middle of the transcript"
        );

        // Keep going until the top is within the prefetch margin.
        for _ in 0..3 {
            app.handle_key(key(KeyCode::PageUp));
        }
        let commands = drain(&mut rx);
        assert!(
            matches!(commands.as_slice(), [TgCommand::LoadOlderMessages { before_id, .. }] if *before_id == 51),
            "expected one request offset at the oldest held message, got {commands:?}"
        );

        // Scrolling again while that request is in flight must not queue a duplicate.
        app.handle_key(key(KeyCode::PageUp));
        assert!(drain(&mut rx).is_empty(), "the in-flight guard must hold");
    }

    #[test]
    fn older_messages_extend_the_buffer_without_moving_the_viewport() {
        let (mut app, _rx) = opened_chat();
        app.focus = Focus::Messages;
        app.metrics = ChatViewMetrics {
            total_lines: 100,
            viewport: 20,
        };
        for _ in 0..4 {
            app.handle_key(key(KeyCode::PageUp));
        }
        let scroll_before = app.open_buffer().unwrap().scroll;

        app.handle_event(TgEvent::OlderMessagesLoaded {
            peer: peer(1).id,
            messages: page(50, 10),
        });

        let buffer = app.open_buffer().unwrap();
        assert_eq!(buffer.messages.len(), PAGE_SIZE + 10);
        assert_eq!(buffer.oldest_id(), Some(41));
        assert!(!buffer.loading_older);
        assert_eq!(
            buffer.scroll, scroll_before,
            "the offset counts from the bottom, so prepending must not move the view"
        );
    }

    #[test]
    fn reaching_the_start_of_history_stops_further_requests() {
        let (mut app, mut rx) = opened_chat();
        app.focus = Focus::Messages;
        app.metrics = ChatViewMetrics {
            total_lines: 100,
            viewport: 20,
        };
        for _ in 0..4 {
            app.handle_key(key(KeyCode::PageUp));
        }
        drain(&mut rx);

        app.handle_event(TgEvent::OlderMessagesLoaded {
            peer: peer(1).id,
            messages: Vec::new(),
        });
        app.handle_key(key(KeyCode::PageUp));

        assert!(!app.open_buffer().unwrap().has_more_older);
        assert!(
            drain(&mut rx).is_empty(),
            "must stop asking once history is exhausted"
        );
    }

    #[test]
    fn typing_and_pressing_enter_sends_the_message() {
        let (mut app, mut rx) = opened_chat();
        app.focus = Focus::Messages;

        for ch in "hi there".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Enter));

        let commands = drain(&mut rx);
        assert!(
            matches!(commands.as_slice(), [TgCommand::SendMessage { text, peer: p }]
                if text == "hi ther" && p.id == peer(1).id),
            "got {commands:?}"
        );
        assert!(app.compose.is_empty(), "the box clears optimistically");
    }

    #[test]
    fn an_empty_compose_box_sends_nothing() {
        let (mut app, mut rx) = opened_chat();
        app.focus = Focus::Messages;

        app.handle_key(key(KeyCode::Char(' ')));
        app.handle_key(key(KeyCode::Enter));

        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_sent_message_appears_once_despite_the_update_echo() {
        let (mut app, _rx) = opened_chat();
        let sent = message(101, "hi there");

        app.handle_event(TgEvent::MessageSent {
            peer: peer(1).id,
            message: sent.clone(),
        });
        // The update stream reports the same message moments later.
        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(1),
            message: sent,
            edited: false,
        });

        let buffer = app.open_buffer().unwrap();
        assert_eq!(buffer.messages.iter().filter(|m| m.id == 101).count(), 1);
        assert_eq!(app.dialogs.items[0].preview, "hi there");
    }

    #[test]
    fn an_edit_replaces_the_message_in_place() {
        let (mut app, _rx) = opened_chat();
        let before = app.open_buffer().unwrap().messages.len();

        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(1),
            message: message(100, "edited text"),
            edited: true,
        });

        let buffer = app.open_buffer().unwrap();
        assert_eq!(buffer.messages.len(), before, "an edit must not add a row");
        assert_eq!(
            buffer.messages.iter().find(|m| m.id == 100).unwrap().text,
            "edited text"
        );
    }

    #[test]
    fn a_deleted_message_disappears_from_the_transcript() {
        let (mut app, _rx) = opened_chat();
        let before = app.open_buffer().unwrap().messages.len();

        app.handle_event(TgEvent::MessagesDeleted {
            channel: None,
            ids: vec![100, 99],
        });

        let buffer = app.open_buffer().unwrap();
        assert_eq!(buffer.messages.len(), before - 2);
        assert!(!buffer.messages.iter().any(|m| m.id == 100 || m.id == 99));
    }

    #[test]
    fn a_channel_deletion_only_touches_that_channel() {
        let (mut app, _rx) = opened_chat();
        // A channel whose ids restart at 1 and so overlap the private chat's ids.
        app.chats
            .insert(channel(500).id, ChatBuffer::new(channel(500)));
        app.chats
            .get_mut(&channel(500).id)
            .unwrap()
            .set_initial(page(100, 3));

        app.handle_event(TgEvent::MessagesDeleted {
            channel: Some(channel(500).id),
            ids: vec![100],
        });

        assert!(
            !app.chats[&channel(500).id]
                .messages
                .iter()
                .any(|m| m.id == 100)
        );
        assert!(
            app.chats[&peer(1).id].messages.iter().any(|m| m.id == 100),
            "the private chat's message 100 is unrelated and must survive"
        );
    }

    #[test]
    fn a_peerless_deletion_leaves_channels_alone() {
        let (mut app, _rx) = opened_chat();
        app.chats
            .insert(channel(500).id, ChatBuffer::new(channel(500)));
        app.chats
            .get_mut(&channel(500).id)
            .unwrap()
            .set_initial(page(100, 3));

        // Telegram omits the chat for users and small groups; channel ids would collide.
        app.handle_event(TgEvent::MessagesDeleted {
            channel: None,
            ids: vec![100],
        });

        assert!(
            app.chats[&channel(500).id]
                .messages
                .iter()
                .any(|m| m.id == 100),
            "a bare id must never delete from a channel"
        );
        assert!(!app.chats[&peer(1).id].messages.iter().any(|m| m.id == 100));
    }

    #[test]
    fn deleting_a_message_we_do_not_hold_is_harmless() {
        let (mut app, _rx) = opened_chat();
        let before = app.open_buffer().unwrap().messages.len();

        app.handle_event(TgEvent::MessagesDeleted {
            channel: None,
            ids: vec![999_999],
        });

        assert_eq!(app.open_buffer().unwrap().messages.len(), before);
    }

    #[test]
    fn errors_and_progress_are_distinguishable_in_the_banner() {
        let (mut app, _rx) = app();
        app.handle_event(TgEvent::Authorized(true));

        app.handle_event(TgEvent::Error("could not send message".to_string()));
        let status = app.status.as_ref().unwrap();
        assert_eq!(status.kind, StatusKind::Error);
        assert_eq!(status.text, "could not send message");

        app.handle_event(TgEvent::SignedIn {
            name: "Alice".to_string(),
        });
        assert_eq!(app.status.as_ref().unwrap().kind, StatusKind::Info);
    }

    #[test]
    fn scrolling_down_never_runs_past_the_newest_message() {
        let (mut app, _rx) = opened_chat();
        app.focus = Focus::Messages;
        app.metrics = ChatViewMetrics {
            total_lines: 100,
            viewport: 20,
        };

        app.handle_key(key(KeyCode::PageUp));
        for _ in 0..10 {
            app.handle_key(key(KeyCode::PageDown));
        }

        assert_eq!(app.open_buffer().unwrap().scroll, 0);
    }
}
