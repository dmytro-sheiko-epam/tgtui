//! Application state and the reducers driving it.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use grammers_session::types::PeerId;
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
    Password { hint: Option<String> },
    Main,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Chats,
    Messages,
}

#[derive(Debug)]
pub struct Status {
    pub text: String,
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

    fn set_status(&mut self, text: impl Into<String>) {
        self.status = Some(Status {
            text: text.into(),
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
                self.set_status(format!("signed in as {name}"));
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
                    self.set_status(error);
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
