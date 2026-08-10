//! Application state and the reducers driving it.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use grammers_session::types::{PeerId, PeerKind};
use tokio::sync::mpsc;

use crate::state::chat_buffer::ChatBuffer;
use crate::state::dialog_list::DialogListState;
use crate::state::media::{PhotoRef, PhotoSource, PhotoState};
use crate::telegram::{TgCommand, TgEvent};

/// How long a status banner stays on screen before it fades away.
const STATUS_TTL: Duration = Duration::from_secs(6);

/// Load more history once the view is within this many lines of the top of the buffer.
const SCROLL_PREFETCH_LINES: usize = 10;

/// Photo downloads allowed in flight at once. Opening a photo-heavy chat would otherwise fire a
/// whole viewport of requests in a single frame.
const MAX_PHOTO_DOWNLOADS: usize = 4;

/// Decoded pictures held in memory at once. Each costs a few hundred kilobytes, so scrolling an
/// image-heavy chat would otherwise grow the process without bound. Comfortably more than one
/// viewport holds, so an image on screen is never evicted only to be fetched again.
const MAX_DECODED_PHOTOS: usize = 48;

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
    /// The picture being examined full screen. Modal: while it is set, keys go to the viewer and
    /// the transcript is not drawn at all.
    pub viewer: Option<i32>,
    /// `TGTUI_IMAGE_ROWS`, if set: an absolute cap on how tall an inline picture may be.
    pub image_rows: Option<u16>,
    /// Photo messages the last frame actually drew, in transcript order so the newest is last.
    visible_photos: Vec<i32>,
    pub should_quit: bool,
    /// Photo downloads issued but not yet answered, capped by `MAX_PHOTO_DOWNLOADS`.
    downloading: usize,
    /// Decoded pictures in the order they arrived, so the oldest can be dropped first.
    decoded: VecDeque<(PeerId, i32)>,
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
            viewer: None,
            image_rows: None,
            visible_photos: Vec::new(),
            should_quit: false,
            downloading: 0,
            decoded: VecDeque::new(),
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
            TgEvent::PhotoLoaded {
                peer,
                message_id,
                image,
            } => {
                self.downloading = self.downloading.saturating_sub(1);
                let stored = self
                    .photo_mut(peer, message_id)
                    .map(|photo| {
                        photo.state = match image {
                            Some(image) => PhotoState::Ready(image),
                            None => PhotoState::Failed,
                        };
                        matches!(photo.state, PhotoState::Ready(_))
                    })
                    .unwrap_or(false);
                if stored {
                    self.remember_decoded(peer, message_id);
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
                            let mut replacement = message.clone();
                            // An edit usually only touches the caption. Carrying a picture we
                            // already hold across the replacement keeps it from flickering back
                            // to a label and downloading itself again.
                            if let (Some(old), Some(new)) =
                                (&existing.photo, &mut replacement.photo)
                                && old.source == new.source
                            {
                                new.state = old.state.clone();
                            }
                            *existing = replacement;
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

        // Whoever was being examined may have just been deleted out from under the viewer.
        if self
            .viewer
            .is_some_and(|id| !self.photo_ids().contains(&id))
        {
            self.viewer = None;
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
        // Ctrl+P rather than a letter: with the message pane focused every plain character goes
        // into the compose box.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('p') {
            return self.toggle_viewer();
        }

        // The viewer is modal — nothing behind it is reachable, and nothing it swallows can
        // leak into the compose box.
        if self.viewer.is_some() {
            return self.handle_viewer_key(key);
        }

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

    /// Ask for the pictures of the photo messages currently on screen.
    ///
    /// The render pass supplies the ids because only it knows where the wrapped transcript
    /// actually landed — the same reason it writes `metrics` back.
    pub fn request_visible_photos(&mut self, visible: &[i32]) {
        // Also what `Ctrl+P` opens: the render pass is the only thing that knows which pictures
        // actually made it onto the screen. The viewer calls this too, for the one picture it is
        // showing, and must not overwrite what the transcript found.
        if self.viewer.is_none() && self.visible_photos != visible {
            self.visible_photos = visible.to_vec();
        }

        let Some(peer_id) = self.open_chat else {
            return;
        };
        let Some(buffer) = self.chats.get(&peer_id) else {
            return;
        };
        let peer = buffer.peer;

        let budget = MAX_PHOTO_DOWNLOADS.saturating_sub(self.downloading);
        let wanted: Vec<(i32, PhotoSource)> = buffer
            .messages
            .iter()
            .filter(|message| visible.contains(&message.id))
            .filter_map(|message| {
                let photo = message.photo.as_ref()?;
                // Only `Pending` is ever requested. `Loading` is already in flight, and
                // `Failed` is terminal — the trigger here is visibility, so a retry would fire
                // again on the very next frame.
                matches!(photo.state, PhotoState::Pending)
                    .then(|| (message.id, photo.source.clone()))
            })
            .take(budget)
            .collect();

        for (message_id, source) in wanted {
            if let Some(photo) = self.photo_mut(peer_id, message_id) {
                photo.state = PhotoState::Loading;
            }
            self.downloading += 1;
            self.send(TgCommand::DownloadPhoto {
                peer,
                message_id,
                source: Box::new(source),
            });
        }
    }

    // -- the full-screen viewer ----------------------------------------------

    fn handle_viewer_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.viewer = None,
            KeyCode::Left | KeyCode::Char('h') => self.step_viewer(-1),
            KeyCode::Right | KeyCode::Char('l') => self.step_viewer(1),
            _ => {}
        }
    }

    fn toggle_viewer(&mut self) {
        if self.viewer.is_some() {
            self.viewer = None;
            return;
        }
        // The newest picture on screen. `visible_photos` holds only what the last frame could
        // actually draw, so this never opens onto an empty frame.
        match self.visible_photos.last() {
            Some(&id) => self.viewer = Some(id),
            None => self.set_status("no picture in view", StatusKind::Info),
        }
    }

    /// Move to the neighbouring picture in the chat, stopping at either end.
    ///
    /// Wrapping from the newest picture to one a thousand messages back would lose your place,
    /// so the ends are walls.
    fn step_viewer(&mut self, delta: isize) {
        let Some(current) = self.viewer else {
            return;
        };
        let photos = self.photo_ids();
        let Some(at) = photos.iter().position(|&id| id == current) else {
            return;
        };

        let next = (at as isize + delta).clamp(0, photos.len() as isize - 1) as usize;
        self.viewer = Some(photos[next]);
    }

    /// Every picture in the open chat, oldest first — including ones never scrolled into view,
    /// which the viewer downloads on arrival.
    pub fn photo_ids(&self) -> Vec<i32> {
        self.open_buffer()
            .map(|buffer| {
                buffer
                    .messages
                    .iter()
                    .filter(|message| message.photo.is_some())
                    .map(|message| message.id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Where the open picture sits among the chat's pictures, as a 1-based `(nth, total)`.
    pub fn viewer_position(&self) -> Option<(usize, usize)> {
        let current = self.viewer?;
        let photos = self.photo_ids();
        let at = photos.iter().position(|&id| id == current)?;
        Some((at + 1, photos.len()))
    }

    /// Track a newly decoded picture, dropping the oldest once too many are held.
    fn remember_decoded(&mut self, peer: PeerId, message_id: i32) {
        self.decoded.push_back((peer, message_id));
        while self.decoded.len() > MAX_DECODED_PHOTOS
            && let Some((peer, message_id)) = self.decoded.pop_front()
        {
            // Back to `Pending` rather than `Failed`: scrolling it into view again should
            // fetch it once more.
            if let Some(photo) = self.photo_mut(peer, message_id) {
                photo.state = PhotoState::Pending;
            }
        }
    }

    fn photo_mut(&mut self, peer: PeerId, message_id: i32) -> Option<&mut PhotoRef> {
        self.chats
            .get_mut(&peer)?
            .messages
            .iter_mut()
            .find(|message| message.id == message_id)?
            .photo
            .as_mut()
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
    use crate::test_support::{
        app, channel, dialog, drain, gradient, message, page, peer, photo_message,
    };

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

    // -- pictures ------------------------------------------------------------

    /// An open chat holding `count` photo messages with ids 1..=count.
    fn chat_of_photos(count: i32) -> (App, mpsc::UnboundedReceiver<TgCommand>) {
        let (mut app, mut rx) = app();
        app.handle_event(TgEvent::Authorized(true));
        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![dialog(1, "Alice")],
            exhausted: true,
        });
        app.handle_event(TgEvent::MessagesLoaded {
            peer: peer(1).id,
            messages: (1..=count)
                .rev()
                .map(|id| photo_message(id, "", 100, 200))
                .collect(),
        });
        drain(&mut rx);
        (app, rx)
    }

    fn photo_state(app: &App, id: i32) -> PhotoState {
        app.open_buffer()
            .unwrap()
            .messages
            .iter()
            .find(|m| m.id == id)
            .unwrap()
            .photo
            .clone()
            .unwrap()
            .state
    }

    #[test]
    fn a_visible_photo_is_requested_exactly_once() {
        let (mut app, mut rx) = chat_of_photos(1);

        app.request_visible_photos(&[1]);
        let commands = drain(&mut rx);
        assert!(
            matches!(
                commands.as_slice(),
                [TgCommand::DownloadPhoto { message_id: 1, .. }]
            ),
            "expected one download, got {commands:?}"
        );

        // Every frame calls this again while the message stays on screen.
        app.request_visible_photos(&[1]);
        assert!(
            drain(&mut rx).is_empty(),
            "the in-flight guard must hold across redraws"
        );
    }

    #[test]
    fn a_photo_off_screen_costs_no_bandwidth() {
        let (mut app, mut rx) = chat_of_photos(3);

        app.request_visible_photos(&[2]);

        let commands = drain(&mut rx);
        assert!(
            matches!(
                commands.as_slice(),
                [TgCommand::DownloadPhoto { message_id: 2, .. }]
            ),
            "only the message on screen is worth fetching, got {commands:?}"
        );
    }

    #[test]
    fn a_failed_photo_is_never_requested_again() {
        let (mut app, mut rx) = chat_of_photos(1);
        app.request_visible_photos(&[1]);
        drain(&mut rx);

        app.handle_event(TgEvent::PhotoLoaded {
            peer: peer(1).id,
            message_id: 1,
            image: None,
        });
        app.request_visible_photos(&[1]);

        assert!(matches!(photo_state(&app, 1), PhotoState::Failed));
        assert!(
            drain(&mut rx).is_empty(),
            "visibility is the trigger, so a retry would fire again on the very next frame"
        );
    }

    #[test]
    fn only_a_few_photos_download_at_a_time() {
        let (mut app, mut rx) = chat_of_photos(20);
        let visible: Vec<i32> = (1..=20).collect();

        app.request_visible_photos(&visible);

        assert_eq!(
            drain(&mut rx).len(),
            MAX_PHOTO_DOWNLOADS,
            "opening a photo-heavy chat must not fire a whole viewport of requests at once"
        );
    }

    #[test]
    fn a_finished_download_frees_a_slot_for_the_next_photo() {
        let (mut app, mut rx) = chat_of_photos(20);
        let visible: Vec<i32> = (1..=20).collect();
        app.request_visible_photos(&visible);
        drain(&mut rx);

        app.handle_event(TgEvent::PhotoLoaded {
            peer: peer(1).id,
            message_id: 1,
            image: Some(gradient(8, 8)),
        });
        app.request_visible_photos(&visible);

        assert!(matches!(photo_state(&app, 1), PhotoState::Ready(_)));
        assert_eq!(
            drain(&mut rx).len(),
            1,
            "one download came back, so exactly one more may start"
        );
    }

    #[test]
    fn the_oldest_pictures_are_dropped_once_too_many_are_held() {
        let count = MAX_DECODED_PHOTOS as i32 + 1;
        let (mut app, _rx) = chat_of_photos(count);

        for id in 1..=count {
            app.handle_event(TgEvent::PhotoLoaded {
                peer: peer(1).id,
                message_id: id,
                image: Some(gradient(8, 8)),
            });
        }

        assert!(
            matches!(photo_state(&app, 1), PhotoState::Pending),
            "scrolling an image-heavy chat would otherwise grow the process without bound"
        );
        assert!(
            matches!(photo_state(&app, count), PhotoState::Ready(_)),
            "and the newest must survive"
        );
    }

    // -- the full-screen viewer ----------------------------------------------

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    /// A chat of `count` pictures, all downloaded, with the render pass having reported them all
    /// on screen.
    fn viewable_chat(count: i32) -> (App, mpsc::UnboundedReceiver<TgCommand>) {
        let (mut app, mut rx) = chat_of_photos(count);
        for id in 1..=count {
            app.handle_event(TgEvent::PhotoLoaded {
                peer: peer(1).id,
                message_id: id,
                image: Some(gradient(8, 8)),
            });
        }
        let visible: Vec<i32> = (1..=count).collect();
        app.request_visible_photos(&visible);
        drain(&mut rx);
        (app, rx)
    }

    #[test]
    fn ctrl_p_opens_the_newest_picture_on_screen() {
        let (mut app, _rx) = viewable_chat(3);
        app.focus = Focus::Messages;

        app.handle_key(ctrl(KeyCode::Char('p')));

        assert_eq!(
            app.viewer,
            Some(3),
            "the newest in view is the one you meant"
        );
    }

    #[test]
    fn plain_p_still_types_into_the_compose_box() {
        let (mut app, _rx) = viewable_chat(1);
        app.focus = Focus::Messages;

        app.handle_key(key(KeyCode::Char('p')));

        assert_eq!(app.compose, "p");
        assert!(
            app.viewer.is_none(),
            "the viewer key must not steal an ordinary letter"
        );
    }

    #[test]
    fn ctrl_p_with_nothing_drawable_says_so_instead_of_opening_an_empty_frame() {
        // A chat of photos that were never rendered, so nothing is known to be on screen.
        let (mut app, _rx) = chat_of_photos(2);

        app.handle_key(ctrl(KeyCode::Char('p')));

        assert!(app.viewer.is_none());
        assert_eq!(app.status.as_ref().unwrap().text, "no picture in view");
    }

    #[test]
    fn arrows_step_between_the_chats_pictures() {
        let (mut app, _rx) = viewable_chat(3);
        app.handle_key(ctrl(KeyCode::Char('p')));

        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.viewer, Some(2));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.viewer, Some(3));
    }

    #[test]
    fn stepping_past_either_end_stays_put() {
        let (mut app, _rx) = viewable_chat(2);
        app.handle_key(ctrl(KeyCode::Char('p')));

        for _ in 0..5 {
            app.handle_key(key(KeyCode::Left));
        }
        assert_eq!(app.viewer, Some(1), "the oldest picture is a wall");

        for _ in 0..5 {
            app.handle_key(key(KeyCode::Right));
        }
        assert_eq!(
            app.viewer,
            Some(2),
            "wrapping to the far end of a long chat would lose your place"
        );
    }

    #[test]
    fn the_viewer_reaches_pictures_that_were_never_on_screen() {
        let (mut app, mut rx) = chat_of_photos(3);
        // Only the newest was ever rendered and downloaded.
        app.request_visible_photos(&[3]);
        app.handle_event(TgEvent::PhotoLoaded {
            peer: peer(1).id,
            message_id: 3,
            image: Some(gradient(8, 8)),
        });
        drain(&mut rx);

        app.handle_key(ctrl(KeyCode::Char('p')));
        app.handle_key(key(KeyCode::Left));
        // Standing in for the viewer's own render, which asks for whatever it is showing.
        app.request_visible_photos(&[2]);

        assert_eq!(app.viewer, Some(2));
        let commands = drain(&mut rx);
        assert!(
            matches!(
                commands.as_slice(),
                [TgCommand::DownloadPhoto { message_id: 2, .. }]
            ),
            "stepping onto an undownloaded picture must fetch it, got {commands:?}"
        );
    }

    #[test]
    fn the_viewer_does_not_forget_what_the_transcript_had_on_screen() {
        let (mut app, _rx) = viewable_chat(3);
        app.handle_key(ctrl(KeyCode::Char('p')));

        // The viewer's render, reporting only the picture it shows.
        app.handle_key(key(KeyCode::Left));
        app.request_visible_photos(&[2]);
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(ctrl(KeyCode::Char('p')));

        assert_eq!(
            app.viewer,
            Some(3),
            "reopening must go back to what the transcript shows, not the last one examined"
        );
    }

    #[test]
    fn esc_closes_the_viewer_and_leaves_the_chat_open() {
        let (mut app, _rx) = viewable_chat(2);
        app.focus = Focus::Messages;
        app.handle_key(ctrl(KeyCode::Char('p')));

        app.handle_key(key(KeyCode::Esc));

        assert!(app.viewer.is_none());
        assert_eq!(app.open_chat, Some(peer(1).id));
        assert_eq!(
            app.focus,
            Focus::Messages,
            "Esc closed the viewer, so it must not also have gone back to the chat list"
        );
    }

    #[test]
    fn ctrl_p_closes_the_viewer_it_opened() {
        let (mut app, _rx) = viewable_chat(1);

        app.handle_key(ctrl(KeyCode::Char('p')));
        app.handle_key(ctrl(KeyCode::Char('p')));

        assert!(app.viewer.is_none());
    }

    #[test]
    fn typing_cannot_leak_through_the_viewer() {
        let (mut app, _rx) = viewable_chat(1);
        app.focus = Focus::Messages;
        app.handle_key(ctrl(KeyCode::Char('p')));

        app.handle_key(key(KeyCode::Char('x')));
        app.handle_key(key(KeyCode::Enter));

        assert!(
            app.compose.is_empty(),
            "the viewer is modal; keys behind it must not reach the compose box"
        );
    }

    #[test]
    fn deleting_the_picture_being_examined_closes_the_viewer() {
        let (mut app, _rx) = viewable_chat(2);
        app.handle_key(ctrl(KeyCode::Char('p')));

        app.handle_event(TgEvent::MessagesDeleted {
            channel: None,
            ids: vec![2],
        });

        assert!(
            app.viewer.is_none(),
            "the viewer must not be left pointing at a message that is gone"
        );
    }

    #[test]
    fn an_edit_keeps_the_picture_it_already_downloaded() {
        let (mut app, _rx) = chat_of_photos(1);
        app.handle_event(TgEvent::PhotoLoaded {
            peer: peer(1).id,
            message_id: 1,
            image: Some(gradient(8, 8)),
        });

        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(1),
            message: photo_message(1, "fixed the typo", 100, 200),
            edited: true,
        });

        assert!(
            matches!(photo_state(&app, 1), PhotoState::Ready(_)),
            "an edited caption must not make the picture flicker back to a label"
        );
    }
}
