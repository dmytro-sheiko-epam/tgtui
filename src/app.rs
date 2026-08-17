//! Application state and the reducers driving it.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use grammers_session::types::{PeerId, PeerKind, PeerRef};
use tokio::sync::mpsc;

use crate::state::chat_buffer::ChatBuffer;
use crate::state::dialog_actions::{DialogAction, DialogKind, actions_for};
use crate::state::dialog_list::DialogListState;
use crate::state::media::{PhotoRef, PhotoSource, PhotoState};
use crate::state::message_actions::{self, MessageAction};
use crate::state::peer_info::PeerInfo;
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

/// The chat action menu, open over the chat list.
///
/// Carries the peer it was opened on rather than reading the selection when a key is pressed: the
/// list reorders itself under live updates, and an action must land on the conversation the user
/// was looking at when they opened the menu.
#[derive(Debug)]
pub struct ChatMenu {
    pub peer: PeerRef,
    pub kind: DialogKind,
    pub name: String,
    pub actions: Vec<DialogAction>,
    pub selected: usize,
    /// The action waiting on a yes or no. Set only for the ones that cannot be undone from here.
    pub confirming: Option<DialogAction>,
}

impl ChatMenu {
    pub fn action(&self) -> Option<DialogAction> {
        self.actions.get(self.selected).copied()
    }

    /// The question on screen, if one is pending.
    pub fn prompt(&self) -> Option<String> {
        self.confirming
            .map(|action| action.confirm_prompt(self.kind, &self.name))
    }
}

/// The action menu for one message, open over the transcript.
///
/// Snapshots the message id for the same reason [`ChatMenu`] snapshots the peer: live updates move
/// the transcript under the popup, and the action must land on the message the user was pointing at
/// when they opened it.
#[derive(Debug)]
pub struct MessageMenu {
    pub peer: PeerRef,
    pub message_id: i32,
    pub actions: Vec<MessageAction>,
    pub selected: usize,
    pub confirming: Option<MessageAction>,
}

impl MessageMenu {
    pub fn action(&self) -> Option<MessageAction> {
        self.actions.get(self.selected).copied()
    }

    pub fn prompt(&self) -> Option<String> {
        self.confirming.map(MessageAction::confirm_prompt)
    }
}

/// Choosing where a message is going.
///
/// Carries the messages rather than looking them up when Enter lands: the menu that opened this has
/// closed and the cursor may have moved on, but what is being forwarded was decided back then.
#[derive(Debug)]
pub struct ForwardPicker {
    pub source: PeerRef,
    pub ids: Vec<i32>,
    /// Typed into rather than navigated with letters, so this modal takes plain characters where
    /// the two menus take `j` and `k`.
    pub filter: String,
    /// An index into the *filtered* rows, so it has to be clamped whenever the filter narrows.
    pub selected: usize,
}

/// A profile being read.
///
/// Fetched fresh on every open and dropped on close: there is no cache of profiles anywhere, so
/// there is no second staleness problem to reason about. A bio edited on another device simply
/// arrives the next time the screen is opened.
pub struct PeerInfoView {
    /// Kept for the avatar download, which needs the access hash.
    pub peer: PeerRef,
    /// From the dialog row, so the title is right before the fetch lands.
    pub name: String,
    pub kind: DialogKind,
    pub state: InfoState,
    /// Lines scrolled *past the top*. The opposite of `ChatBuffer.scroll`, which counts up from
    /// the bottom — deliberately, because a profile is a fixed-length document read top-down,
    /// while a transcript grows at the end and must not move when older history is prepended.
    pub scroll: u16,
}

#[derive(Debug)]
pub enum InfoState {
    Loading,
    Ready(Box<PeerInfo>),
    Failed(String),
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
    /// The message the compose box is rewriting, if it is rewriting one rather than writing a new
    /// one. Set by the Edit entry, cleared when the edit is sent or abandoned.
    pub editing: Option<i32>,
    /// The message the next send will be threaded to. Mutually exclusive with `editing` — an edit
    /// replaces text and cannot also change what a message replies to.
    pub replying_to: Option<i32>,
    pub focus: Focus,
    pub metrics: ChatViewMetrics,
    /// The picture being examined full screen. Modal: while it is set, keys go to the viewer and
    /// the transcript is not drawn at all.
    pub viewer: Option<i32>,
    /// The chat action menu. Modal too, but a popup: the panes stay drawn behind it.
    pub menu: Option<ChatMenu>,
    /// The action menu for the message under the cursor. Modal in the same way.
    pub message_menu: Option<MessageMenu>,
    /// Picking a destination for a forward. Modal, and outranks both menus — by the time it is up
    /// the menu that opened it has closed.
    pub forward: Option<ForwardPicker>,
    /// The profile being read. Modal and full screen, like `viewer`: while it is set the two
    /// panes are not drawn at all.
    pub peer_info: Option<PeerInfoView>,
    /// The cursor moved and the transcript has not been redrawn yet, so it may be off screen.
    /// Consumed by `chat_view::render_transcript`, which is the only code that knows where a
    /// message sits in lines.
    pub scroll_to_selection: bool,
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
            editing: None,
            replying_to: None,
            focus: Focus::Chats,
            metrics: ChatViewMetrics::default(),
            viewer: None,
            menu: None,
            message_menu: None,
            forward: None,
            peer_info: None,
            scroll_to_selection: false,
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
            TgEvent::DialogsLoaded {
                items,
                exhausted,
                archived,
            } => {
                self.dialogs.extend(items, exhausted, archived);
                // Show something as soon as the first page lands.
                if self.open_chat.is_none() {
                    self.open_selected_chat();
                }
                // A custom folder is a filter over the main list, so a page that added nothing to
                // it has to pull the next one straight away — without this the strip would sit on
                // an empty folder until the user pressed `j`, which there is nothing to press on.
                self.load_more_dialogs_if_needed();
            }
            TgEvent::FoldersLoaded { folders } => self.dialogs.set_folders(folders),
            TgEvent::FolderChanged { peer, archived } => self.refile_dialog(peer, archived),
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
            // The new text is not here: it arrives as `updateEditMessage` and is applied by the
            // `IncomingMessage { edited: true }` arm below, which also keeps the decoded picture.
            TgEvent::MessageEdited => self.set_status("edited", StatusKind::Info),
            TgEvent::MessagesForwarded { destination } => {
                let where_to = self
                    .dialogs
                    .find(destination)
                    .map(|item| item.name.clone())
                    .unwrap_or_else(|| "another chat".to_string());
                self.set_status(format!("forwarded to {where_to}"), StatusKind::Info);
            }
            TgEvent::ReplyTargetsLoaded {
                peer,
                asked,
                targets,
            } => {
                if let Some(buffer) = self.chats.get_mut(&peer) {
                    // `asked` rather than `targets`: a parent that has been deleted comes back as
                    // nothing, and the guard has to hold for it too or the next frame asks again.
                    buffer.reply_requested.extend(asked);
                    buffer.reply_previews.extend(targets);
                }
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
                    let counts = !message.outgoing;
                    self.dialogs.bump(peer.id, message.text);
                    // An edit is not a new message; our own message echoed back from another
                    // device is not unread; and a message in the chat on screen has already been
                    // read, by definition, by the person reading it.
                    if counts && Some(peer.id) != self.open_chat {
                        self.dialogs.mark_unread(peer.id);
                    }
                }
            }
            // Chat actions. Each also arrives unprompted when the change was made on another
            // device, which is why they carry the new value rather than confirming a request.
            TgEvent::MuteChanged { peer, muted } => {
                self.dialogs.set_muted(peer, muted);
                self.set_status(if muted { "muted" } else { "unmuted" }, StatusKind::Info);
            }
            TgEvent::PinChanged { peer, pinned } => {
                self.dialogs.set_pinned(peer, pinned);
                self.set_status(if pinned { "pinned" } else { "unpinned" }, StatusKind::Info);
            }
            TgEvent::BlockedChanged { peer, blocked } => {
                self.dialogs.set_blocked(peer, blocked);
                // Unlike mute and pin this leaves no mark on the row, so the banner is the only
                // evidence the user gets that anything happened.
                self.set_status(
                    if blocked {
                        "user blocked"
                    } else {
                        "user unblocked"
                    },
                    StatusKind::Info,
                );
            }
            TgEvent::BlockedPeersLoaded { peers } => self.dialogs.set_blocked_list(&peers),
            TgEvent::HistoryCleared { peer } => {
                if let Some(buffer) = self.chats.get_mut(&peer) {
                    buffer.clear();
                }
                // `clear` has taken the cursor with the messages, so a menu still open over them
                // would be pointing at a message that no longer exists.
                self.close_message_menu(peer);
                // The row keeps its place in the list but has nothing left to preview, and a chat
                // with no messages has nothing left unread either.
                self.dialogs.clear_preview(peer);
                self.dialogs.clear_unread(peer);
                self.set_status("history cleared", StatusKind::Info);
            }
            TgEvent::DialogGone { peer, reason } => self.forget_dialog(peer, reason),

            TgEvent::PeerInfoLoaded { peer, info } => {
                // A late answer must not land on the wrong profile: the screen may have been
                // closed and reopened on another chat entirely while this was in flight.
                if self.peer_info.as_ref().map(|view| view.peer.id) != Some(peer) {
                    return;
                }

                let state = match info {
                    Ok(info) => {
                        // A server answer for this one peer, and a fresher one than the single
                        // page of `contacts.getBlocked` the flag is otherwise seeded from. Not
                        // optimism — the same rule the chat actions follow.
                        if let Some(blocked) = info.blocked {
                            self.dialogs.set_blocked(peer, blocked);
                        }
                        InfoState::Ready(info)
                    }
                    Err(why) => InfoState::Failed(why),
                };

                if let Some(view) = self.peer_info.as_mut() {
                    view.state = state;
                }
            }

            TgEvent::OutgoingRead { peer, max_id } => self.dialogs.mark_outbox_read(peer, max_id),
            TgEvent::IncomingRead { peer, still_unread } => self
                .dialogs
                .reconcile_unread(peer, still_unread.max(0) as usize),
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

    /// Drop a conversation that has left the main list — deleted, left, or archived.
    ///
    /// All three are the same event as far as the list is concerned; only the wording differs.
    fn forget_dialog(&mut self, peer: PeerId, reason: &str) {
        let name = self.dialogs.find(peer).map(|item| item.name.clone());
        if !self.dialogs.remove(peer) {
            return;
        }
        self.chats.remove(&peer);

        // A profile of a conversation that no longer exists is a screen with nothing behind it.
        if self
            .peer_info
            .as_ref()
            .is_some_and(|view| view.peer.id == peer)
        {
            self.peer_info = None;
        }

        // The chat pane may be showing a conversation that is no longer in the list. `remove` has
        // already left the selection somewhere valid, so following it is enough — and the compose
        // box has to be emptied, or a half-typed line would be sent into the next chat.
        if self.open_chat == Some(peer) {
            self.open_chat = None;
            self.viewer = None;
            self.compose.clear();
            // A half-rewritten message must not be carried into whichever chat replaces this one —
            // and its id, like the reply target's, would name a different message there anyway.
            self.editing = None;
            self.replying_to = None;
            self.focus = Focus::Chats;
            self.open_selected_chat();
        }
        // The buffer went with the conversation, and the cursor with it.
        self.close_message_menu(peer);

        match name {
            Some(name) => self.set_status(format!("{name} — {reason}"), StatusKind::Info),
            None => self.set_status(reason.to_string(), StatusKind::Info),
        }
    }

    /// Move a conversation between the main list and the archive.
    ///
    /// Deliberately *not* [`App::forget_dialog`]: the conversation still exists and its transcript
    /// is still worth keeping, so the `ChatBuffer` stays and only the row's folder changes. The
    /// chat pane is left showing it too — a chat you just archived is one you were reading a
    /// second ago, and closing it would be a surprise. Only the selection has to be caught, and
    /// `set_archived` has already put that somewhere valid in the tab now on screen.
    fn refile_dialog(&mut self, peer: PeerId, archived: bool) {
        let Some(name) = self.dialogs.find(peer).map(|item| item.name.clone()) else {
            return;
        };
        self.dialogs.set_archived(peer, archived);

        let reason = if archived { "archived" } else { "unarchived" };
        self.set_status(format!("{name} — {reason}"), StatusKind::Info);
    }

    fn enter_main(&mut self) {
        self.submitting = false;
        self.login_error = None;
        self.input.clear();
        self.screen = Screen::Main;
        self.dialogs.main.loading = true;
        self.send(TgCommand::LoadMoreDialogs { archived: false });
        // Nothing on a dialog row says whether a user is blocked, so the action menu would have no
        // way to offer "Unblock" without this.
        self.send(TgCommand::LoadBlockedPeers);
        // The tab strip needs its folders before it can draw anything but "All" and "Archive".
        self.send(TgCommand::LoadFolders);
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
        // Chords rather than letters: with the message pane focused every plain character goes
        // into the compose box.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // The viewer is modal and full screen — nothing behind it is reachable, and nothing it
        // swallows can leak into the compose box.
        if self.viewer.is_some() {
            if ctrl && key.code == KeyCode::Char('p') {
                return self.toggle_viewer();
            }
            return self.handle_viewer_key(key);
        }

        // Full screen like the viewer, and claimed right behind it for the same reason: while it
        // is up neither pane is drawn, so nothing behind it is reachable and nothing it swallows
        // can leak into the compose box.
        //
        // The two can never both be open. With a picture up the chat list is not drawn, so
        // `Ctrl+A` is unreachable and no `Info` entry can be chosen; with a profile up, the
        // handler below swallows `Ctrl+P`.
        if self.peer_info.is_some() {
            return self.handle_peer_info_key(key);
        }

        // The picker takes plain characters into its filter, so it has to be claimed before either
        // menu — and before the compose box, for the same reason they are.
        if self.forward.is_some() {
            return self.handle_forward_key(key);
        }

        // The menu is modal too. Its keys are claimed before anything else, or `j`, `k`, `y` and
        // `n` would fall through into the compose box behind the popup.
        if self.menu.is_some() {
            return self.handle_menu_key(key);
        }
        if self.message_menu.is_some() {
            return self.handle_message_menu_key(key);
        }

        if ctrl && key.code == KeyCode::Char('p') {
            return self.toggle_viewer();
        }
        // Deliberately after the viewer check: with a picture open the chat list is not drawn at
        // all, and a menu over it would be acting on something the user cannot see.
        if ctrl && key.code == KeyCode::Char('a') {
            return self.open_menu();
        }
        // After the viewer for the same reason as the menu: with a picture open the chat list is
        // not drawn, so changing which folder it shows would be a change nobody can see.
        if ctrl && matches!(key.code, KeyCode::Char('o')) {
            return self.step_folder(true);
        }
        if ctrl && matches!(key.code, KeyCode::Char('e')) {
            return self.step_folder(false);
        }
        // After the viewer for the same reason again: with a picture open the transcript is not
        // drawn, so a cursor in it would be moving over something nobody can see. `Ctrl+S` is safe
        // to claim because `ratatui::init` puts the terminal in raw mode, which clears `IXON` —
        // nothing upstream is still reading it as XOFF.
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            return self.toggle_select_mode();
        }

        // The message cursor is modal in the same way the menu is: while it is on, `j`, `k` and
        // `q` belong to it rather than to the compose box behind it.
        if self.selecting() {
            return self.handle_select_key(key);
        }

        match key.code {
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Chats => Focus::Messages,
                    Focus::Messages => Focus::Chats,
                };
                return;
            }
            // An abandoned edit or reply takes one `Esc` of its own before the next one leaves the
            // pane, or backing out of either would also throw away the focus.
            KeyCode::Esc if self.editing.is_some() => {
                self.cancel_edit();
                return;
            }
            KeyCode::Esc if self.replying_to.is_some() => {
                self.cancel_reply();
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

    /// Show the next or previous folder in the tab strip.
    ///
    /// Opening whatever the new tab starts on keeps the two panes agreeing with each other, and it
    /// is what makes the first `Ctrl+O` into the archive fetch its first page: the archive has a
    /// cursor of its own that nothing has asked for yet.
    fn step_folder(&mut self, forward: bool) {
        self.dialogs.step_tab(forward);
        self.open_selected_chat();
        self.load_more_dialogs_if_needed();
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

    // -- the profile screen ---------------------------------------------------

    /// Open the selected conversation's profile and ask for it.
    ///
    /// Nothing is applied locally and nothing is assumed: the screen goes up in its loading state
    /// and the fields arrive when the server answers.
    pub fn open_peer_info(&mut self) {
        let Some(summary) = self.dialogs.selected_summary() else {
            return self.set_status("no chat selected", StatusKind::Info);
        };

        let peer = summary.peer;
        self.peer_info = Some(PeerInfoView {
            peer,
            name: summary.name.clone(),
            kind: summary.kind,
            state: InfoState::Loading,
            scroll: 0,
        });
        self.send(TgCommand::LoadPeerInfo { peer });
    }

    // -- the chat action menu ------------------------------------------------

    fn open_menu(&mut self) {
        let Some(summary) = self.dialogs.selected_summary() else {
            return self.set_status("no chat selected", StatusKind::Info);
        };

        self.menu = Some(ChatMenu {
            peer: summary.peer,
            kind: summary.kind,
            name: summary.name.clone(),
            actions: actions_for(
                summary.kind,
                summary.muted,
                summary.pinned,
                summary.blocked,
                summary.archived,
            ),
            selected: 0,
            confirming: None,
        });
    }

    fn handle_menu_key(&mut self, key: KeyEvent) {
        let Some(menu) = self.menu.as_mut() else {
            return;
        };

        // A pending confirmation takes the keyboard entirely. An unanswered "Leave channel?" must
        // not be dismissed by an arrow key, and `Esc` here means "no", not "close the menu".
        if let Some(pending) = menu.confirming {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    menu.confirming = None;
                    self.run_action(pending);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => menu.confirming = None,
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.menu = None,
            KeyCode::Up | KeyCode::Char('k') => menu.selected = menu.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                menu.selected = (menu.selected + 1).min(menu.actions.len().saturating_sub(1));
            }
            KeyCode::Enter => match menu.action() {
                Some(action) if action.is_destructive() => menu.confirming = Some(action),
                Some(action) => self.run_action(action),
                None => {}
            },
            _ => {}
        }
    }

    /// Send the command an action stands for, and close the menu.
    ///
    /// Nothing is applied to the list here. The reducers do that when the server confirms — a mute
    /// that quietly failed but showed as muted would be a lie about the account's real state, and
    /// this is the one part of the app whose state is shared with the user's other devices.
    fn run_action(&mut self, action: DialogAction) {
        let Some(menu) = self.menu.take() else {
            return;
        };
        let peer = menu.peer;

        // Not every entry is a request. Info only puts a screen up — the same way the message
        // menu's Reply and Edit only aim the compose box — which is why `in_progress` is an
        // `Option` and why this returns before reaching the channel.
        if action == DialogAction::Info {
            return self.open_peer_info();
        }

        self.send(match action {
            DialogAction::Info => unreachable!("handled above, before the menu was consumed"),
            DialogAction::Mute => TgCommand::SetMuted { peer, muted: true },
            DialogAction::Unmute => TgCommand::SetMuted { peer, muted: false },
            DialogAction::Pin => TgCommand::SetPinned { peer, pinned: true },
            DialogAction::Unpin => TgCommand::SetPinned {
                peer,
                pinned: false,
            },
            DialogAction::Archive => TgCommand::SetArchived {
                peer,
                archived: true,
            },
            DialogAction::Unarchive => TgCommand::SetArchived {
                peer,
                archived: false,
            },
            DialogAction::ClearHistory => TgCommand::ClearHistory { peer },
            DialogAction::Block => TgCommand::SetBlocked {
                peer,
                blocked: true,
            },
            DialogAction::Unblock => TgCommand::SetBlocked {
                peer,
                blocked: false,
            },
            DialogAction::DeleteOrLeave => TgCommand::DeleteDialog { peer },
        });

        if let Some(progress) = action.in_progress() {
            self.set_status(progress, StatusKind::Info);
        }
    }

    // -- the message cursor --------------------------------------------------

    /// Whether the open chat has a cursor on it. There is no separate mode flag: `selected` being
    /// `Some` *is* the mode, so the two can never disagree.
    pub fn selecting(&self) -> bool {
        self.open_buffer()
            .is_some_and(|buffer| buffer.selected.is_some())
    }

    fn toggle_select_mode(&mut self) {
        if self.selecting() {
            return self.leave_select_mode();
        }
        let Some(buffer) = self.open_buffer_mut() else {
            return;
        };
        if !buffer.select_newest() {
            return self.set_status("no messages here", StatusKind::Info);
        }
        // The cursor lives in the transcript, so the transcript is what the keyboard is aimed at.
        self.focus = Focus::Messages;
        self.scroll_to_selection = true;
    }

    fn leave_select_mode(&mut self) {
        if let Some(buffer) = self.open_buffer_mut() {
            buffer.selected = None;
        }
    }

    fn handle_select_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.leave_select_mode(),
            KeyCode::Up | KeyCode::Char('k') => self.step_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.step_selection(1),
            KeyCode::Enter => self.open_message_menu(),
            KeyCode::Tab => {
                self.leave_select_mode();
                self.focus = Focus::Chats;
            }
            _ => {}
        }
    }

    /// Open the action menu on the message under the cursor.
    ///
    /// Which entries it offers depends on the conversation as well as the message, so the kind
    /// comes off the dialog row rather than being re-derived from the peer id — `PeerKind` cannot
    /// tell a broadcast channel from a megagroup, and those two delete differently.
    fn open_message_menu(&mut self) {
        let Some(buffer) = self.open_buffer() else {
            return;
        };
        let (peer, Some(message_id)) = (buffer.peer, buffer.selected) else {
            return;
        };
        let Some(outgoing) = buffer
            .messages
            .iter()
            .find(|message| message.id == message_id)
            .map(|message| message.outgoing)
        else {
            return;
        };
        let Some(kind) = self.dialogs.find(peer.id).map(|item| item.kind) else {
            return;
        };

        self.message_menu = Some(MessageMenu {
            peer,
            message_id,
            actions: message_actions::actions_for(kind, outgoing),
            selected: 0,
            confirming: None,
        });
    }

    /// Shut a message menu that is open on `peer`, whose messages have just gone.
    fn close_message_menu(&mut self, peer: PeerId) {
        if self
            .message_menu
            .as_ref()
            .is_some_and(|menu| menu.peer.id == peer)
        {
            self.message_menu = None;
        }
    }

    /// The same modal shape as [`App::handle_menu_key`], down to `Esc` meaning "no" rather than
    /// "close" while a question is up.
    fn handle_message_menu_key(&mut self, key: KeyEvent) {
        let Some(menu) = self.message_menu.as_mut() else {
            return;
        };

        if let Some(pending) = menu.confirming {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    menu.confirming = None;
                    self.run_message_action(pending);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => menu.confirming = None,
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.message_menu = None,
            KeyCode::Up | KeyCode::Char('k') => menu.selected = menu.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                menu.selected = (menu.selected + 1).min(menu.actions.len().saturating_sub(1));
            }
            KeyCode::Enter => match menu.action() {
                Some(action) if action.is_destructive() => menu.confirming = Some(action),
                Some(action) => self.run_message_action(action),
                None => {}
            },
            _ => {}
        }
    }

    /// Carry out an action and close the menu.
    ///
    /// Unlike [`App::run_action`], not every entry here is a request: Reply and Edit only aim the
    /// compose box at something, and Forward opens a second modal to ask where. Only the deletes
    /// leave for the network, and those apply nothing locally — the reducer runs when the server
    /// confirms, exactly as the chat actions do.
    fn run_message_action(&mut self, action: MessageAction) {
        let Some(menu) = self.message_menu.take() else {
            return;
        };
        let (peer, id) = (menu.peer, menu.message_id);

        match action {
            MessageAction::DeleteForMe => self.send(TgCommand::DeleteMessages {
                peer,
                ids: vec![id],
                revoke: false,
            }),
            MessageAction::DeleteForEveryone => self.send(TgCommand::DeleteMessages {
                peer,
                ids: vec![id],
                revoke: true,
            }),
            MessageAction::Edit => self.start_edit(id),
            MessageAction::Reply => self.start_reply(id),
            MessageAction::Forward => {
                self.forward = Some(ForwardPicker {
                    source: peer,
                    ids: vec![id],
                    filter: String::new(),
                    selected: 0,
                });
            }
        }

        if let Some(progress) = action.in_progress() {
            self.set_status(progress, StatusKind::Info);
        }
    }

    // -- the forward picker --------------------------------------------------

    /// Rows the picker is offering, as indices into `dialogs.items`.
    pub fn forward_matches(&self) -> Vec<usize> {
        self.forward
            .as_ref()
            .map(|picker| self.dialogs.matching(&picker.filter))
            .unwrap_or_default()
    }

    fn handle_forward_key(&mut self, key: KeyEvent) {
        let matches = self.forward_matches();
        let Some(picker) = self.forward.as_mut() else {
            return;
        };

        match key.code {
            KeyCode::Esc => self.forward = None,
            KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down => {
                picker.selected = (picker.selected + 1).min(matches.len().saturating_sub(1));
            }
            KeyCode::Enter => self.send_forward(),
            KeyCode::Backspace => {
                picker.filter.pop();
                // Widening the filter cannot invalidate the selection, but it does change what it
                // points at, so start again from the top rather than somewhere arbitrary.
                picker.selected = 0;
            }
            KeyCode::Char(ch) => {
                picker.filter.push(ch);
                // Narrowing can leave `selected` past the end of a shorter list, and it indexes
                // the filtered rows rather than the pool.
                picker.selected = 0;
            }
            _ => {}
        }
    }

    fn send_forward(&mut self) {
        let Some(&index) = self.forward_matches().get(
            self.forward
                .as_ref()
                .map(|picker| picker.selected)
                .unwrap_or(0),
        ) else {
            return;
        };
        let Some(destination) = self.dialogs.items.get(index).map(|item| item.peer) else {
            return;
        };
        let name = self.dialogs.items[index].name.clone();
        let Some(picker) = self.forward.take() else {
            return;
        };

        self.send(TgCommand::ForwardMessages {
            source: picker.source,
            ids: picker.ids,
            destination,
        });
        self.set_status(format!("forwarding to {name}…"), StatusKind::Info);
    }

    /// Load a message back into the compose box so it can be rewritten.
    ///
    /// `raw_text` rather than `text`: the transcript's version of a media message has the label
    /// folded into it, and sending that back would write `[photo]` into the caption.
    ///
    /// The cursor goes away, because the keyboard is now aimed at the compose box and leaving a
    /// highlight up would say otherwise.
    fn start_edit(&mut self, id: i32) {
        let Some(text) = self.open_buffer().and_then(|buffer| {
            buffer
                .messages
                .iter()
                .find(|message| message.id == id)
                .map(|message| message.raw_text.clone())
        }) else {
            return;
        };

        self.compose = text;
        self.editing = Some(id);
        self.leave_select_mode();
        self.focus = Focus::Messages;
    }

    /// Put the compose box back to writing a new message, keeping whatever is typed in it.
    fn cancel_edit(&mut self) {
        if self.editing.take().is_some() {
            self.compose.clear();
        }
    }

    /// Aim the next send at a message, so it threads under it.
    ///
    /// Whatever is half-typed stays: unlike an edit, a reply adds to what the box already holds
    /// rather than replacing it, so throwing the text away would be losing work for nothing. An
    /// edit in progress does go, because a message cannot be both rewritten and replied to.
    fn start_reply(&mut self, id: i32) {
        self.cancel_edit();
        self.replying_to = Some(id);
        self.leave_select_mode();
        self.focus = Focus::Messages;
    }

    fn cancel_reply(&mut self) {
        self.replying_to = None;
    }

    fn step_selection(&mut self, delta: isize) {
        let Some(buffer) = self.open_buffer_mut() else {
            return;
        };
        let at_oldest = buffer.select_step(delta);
        self.scroll_to_selection = true;
        // Walking off the top of what we hold is the same signal scrolling there is, and the
        // guard in `load_older_if_needed` is what stops it queueing a second page.
        if at_oldest {
            self.load_older_if_needed();
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

    /// The profile screen's keys: scroll, and close.
    ///
    /// Everything else is swallowed rather than falling through, which is what makes this modal.
    /// The renderer clamps `scroll` against the profile it just measured, the same way
    /// `render_transcript` clamps `ChatBuffer.scroll` — only the frame just built knows how many
    /// lines the fields came to.
    fn handle_peer_info_key(&mut self, key: KeyEvent) {
        let Some(view) = self.peer_info.as_mut() else {
            return;
        };

        match key.code {
            KeyCode::Esc => self.peer_info = None,
            KeyCode::Down | KeyCode::Char('j') => view.scroll = view.scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
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
            let archived = self.dialogs.showing_archive();
            self.dialogs.cursor_mut().loading = true;
            self.send(TgCommand::LoadMoreDialogs { archived });
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
        // Both of these are message ids, and a message id means something different in every
        // conversation. Carried across, they would aim the next Enter at whatever message happens
        // to hold that id here — an edit or a reply landing on a message nobody pointed at. The
        // compose *text* deliberately stays: that is the user's, and it follows them.
        self.editing = None;
        self.replying_to = None;
        self.message_menu = None;
        // Local only, and deliberately so: tgtui never sends a read acknowledgement, so opening a
        // chat here must not change what the account's other clients — or the sender — see. The
        // badge is a note to ourselves about this session and nothing more. It has to be cleared
        // before the cache check below, or a revisit would leave it standing.
        self.dialogs.clear_unread(peer.id);

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
        let reply_to = self.replying_to.take();
        // Nothing is applied locally either way. A sent message comes back as `MessageSent`; an
        // edit comes back over the update stream and is replaced in place there.
        match self.editing.take() {
            Some(message_id) => {
                self.send(TgCommand::EditMessage {
                    peer,
                    message_id,
                    text,
                });
                self.set_status("editing…", StatusKind::Info);
            }
            None => self.send(TgCommand::SendMessage {
                peer,
                text,
                reply_to,
            }),
        }
    }

    /// Ask for the parents of replies on screen whose parent is not in the buffer.
    ///
    /// Called from the renderer every frame, like [`App::request_visible_photos`], so the filtering
    /// has to be the thing that stops it: `reply_requested` is set here and never cleared.
    pub fn request_reply_targets(&mut self, ids: &[i32]) {
        let Some(buffer) = self.open_buffer_mut() else {
            return;
        };
        let wanted = buffer.unfetched_replies(ids);
        if wanted.is_empty() {
            return;
        }
        buffer.reply_requested.extend(wanted.iter().copied());
        let peer = buffer.peer;

        self.send(TgCommand::LoadReplyTargets { peer, ids: wanted });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::chat_buffer::{PAGE_SIZE, ReplyPreview};
    use crate::state::dialog_list::FolderTab;
    use crate::state::peer_info::PeerInfo;
    use crate::telegram::TgEvent;
    use crate::test_support::{
        app, archived_dialog, channel, channel_dialog, dialog, drain, folder, gradient,
        group_dialog, message, outgoing, page, peer, photo_message, user_full,
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
            archived: false,
        });
        app.handle_event(TgEvent::MessagesLoaded {
            peer: peer(1).id,
            messages: page(100, PAGE_SIZE as i32),
        });
        drain(&mut rx);
        (app, rx)
    }

    /// The selected chat's profile, opened and answered.
    fn opened_profile() -> (App, mpsc::UnboundedReceiver<TgCommand>) {
        let (mut app, rx) = opened_chat();
        app.open_peer_info();
        (app, rx)
    }

    #[test]
    fn opening_a_profile_asks_for_it_and_shows_the_name_while_it_waits() {
        let (mut app, mut rx) = opened_chat();
        app.open_peer_info();

        let view = app.peer_info.as_ref().expect("the screen is open");
        assert_eq!(
            view.name, "Alice",
            "the name comes off the dialog row, so the title is right before the fetch lands"
        );
        assert!(matches!(view.state, InfoState::Loading));
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::LoadPeerInfo { peer: asked }] if asked.id == peer(1).id
        ));
    }

    #[test]
    fn a_profile_that_arrives_replaces_the_loading_state() {
        let (mut app, _rx) = opened_profile();
        app.handle_event(TgEvent::PeerInfoLoaded {
            peer: peer(1).id,
            info: Ok(Box::new(user_full("Alice"))),
        });

        let InfoState::Ready(info) = &app.peer_info.as_ref().unwrap().state else {
            panic!("the profile should be ready");
        };
        assert_eq!(info.about.as_deref(), Some("This is Alice."));
    }

    #[test]
    fn a_failed_profile_fetch_leaves_the_loading_state_and_says_why() {
        let (mut app, _rx) = opened_profile();
        app.handle_event(TgEvent::PeerInfoLoaded {
            peer: peer(1).id,
            info: Err("could not read this profile: CHANNEL_PRIVATE".to_string()),
        });

        match &app.peer_info.as_ref().unwrap().state {
            InfoState::Failed(why) => assert!(why.contains("CHANNEL_PRIVATE")),
            other => panic!(
                "a screen stuck on `Loading` forever is worse than one that says what went \
                 wrong, but got {other:?}"
            ),
        }
    }

    #[test]
    fn an_answer_for_another_peer_is_dropped() {
        let (mut app, _rx) = opened_profile();
        app.handle_event(TgEvent::PeerInfoLoaded {
            peer: peer(2).id,
            info: Ok(Box::new(user_full("Bob"))),
        });

        assert!(
            matches!(app.peer_info.as_ref().unwrap().state, InfoState::Loading),
            "a slow answer for a profile the user has already closed and reopened elsewhere must \
             not land on whichever one happens to be on screen"
        );
    }

    #[test]
    fn a_profile_correcting_the_blocked_flag_updates_the_dialog_row() {
        let (mut app, _rx) = opened_profile();
        assert!(!app.dialogs.find(peer(1).id).unwrap().blocked);

        app.handle_event(TgEvent::PeerInfoLoaded {
            peer: peer(1).id,
            info: Ok(Box::new(PeerInfo {
                blocked: Some(true),
                ..user_full("Alice")
            })),
        });

        assert!(
            app.dialogs.find(peer(1).id).unwrap().blocked,
            "the seed is one page of `contacts.getBlocked`, so past it a blocked user shows \
             `Block`; the profile is a fresher server answer for this one peer"
        );
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
            [
                TgCommand::LoadMoreDialogs { archived: false },
                TgCommand::LoadBlockedPeers,
                TgCommand::LoadFolders,
            ]
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
                TgCommand::LoadMoreDialogs { archived: false },
                TgCommand::LoadBlockedPeers,
                TgCommand::LoadFolders,
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
            archived: false,
        });

        assert_eq!(app.open_chat, Some(peer(1).id));
        // The page also leaves the list one row long and the server not yet drained, so the
        // prefetch fires without waiting for a keypress.
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [
                TgCommand::OpenChat { .. },
                TgCommand::LoadMoreDialogs { archived: false }
            ]
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
            matches!(commands.as_slice(), [TgCommand::SendMessage { text, peer: p, reply_to: None }]
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
            archived: false,
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

    // -- read state ----------------------------------------------------------

    fn summary(app: &App, peer_id: PeerId) -> &crate::state::dialog_list::DialogSummary {
        app.dialogs
            .items
            .iter()
            .find(|item| item.peer.id == peer_id)
            .expect("the fixture loaded this dialog")
    }

    #[test]
    fn a_message_arriving_in_a_closed_chat_raises_its_unread_badge() {
        // `opened_chat` leaves chat 1 open, so chat 2 is the one nobody is looking at.
        let (mut app, _rx) = opened_chat();

        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(2),
            message: message(1, "you around?"),
            edited: false,
        });

        assert_eq!(summary(&app, peer(2).id).unread, 1);
    }

    #[test]
    fn a_message_arriving_in_the_open_chat_is_already_read() {
        let (mut app, _rx) = opened_chat();

        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(1),
            message: message(101, "you around?"),
            edited: false,
        });

        assert_eq!(
            summary(&app, peer(1).id).unread,
            0,
            "a message on screen has been read, by definition, by the person reading it"
        );
    }

    #[test]
    fn your_own_message_from_another_device_never_counts_as_unread() {
        let (mut app, _rx) = opened_chat();

        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(2),
            message: outgoing(1, "sent from the phone"),
            edited: false,
        });

        assert_eq!(summary(&app, peer(2).id).unread, 0);
    }

    #[test]
    fn an_edit_does_not_count_as_a_new_unread() {
        let (mut app, _rx) = opened_chat();

        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(2),
            message: message(1, "fixed the typo"),
            edited: true,
        });

        assert_eq!(summary(&app, peer(2).id).unread, 0);
    }

    #[test]
    fn opening_a_chat_clears_its_badge_without_telling_telegram() {
        let (mut app, mut rx) = opened_chat();
        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(2),
            message: message(1, "you around?"),
            edited: false,
        });
        drain(&mut rx);

        app.dialogs.selected = app
            .dialogs
            .items
            .iter()
            .position(|item| item.peer.id == peer(2).id)
            .unwrap();
        app.open_selected_chat();

        assert_eq!(summary(&app, peer(2).id).unread, 0);
        assert!(
            matches!(drain(&mut rx).as_slice(), [TgCommand::OpenChat { .. }]),
            "reading here must stay local: an acknowledgement would mark the conversation read \
             on the user's phone too, which is not what opening a terminal client means"
        );
    }

    #[test]
    fn reopening_a_cached_chat_still_clears_its_badge() {
        let (mut app, mut rx) = opened_chat();
        // Leave and come back, so the buffer is already cached on the second visit.
        app.dialogs.selected = 1;
        app.open_selected_chat();
        app.dialogs.selected = 0;
        app.open_selected_chat();
        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(2),
            message: message(1, "you around?"),
            edited: false,
        });
        drain(&mut rx);

        app.dialogs.selected = app
            .dialogs
            .items
            .iter()
            .position(|item| item.peer.id == peer(2).id)
            .unwrap();
        app.open_selected_chat();

        assert_eq!(
            summary(&app, peer(2).id).unread,
            0,
            "the badge is cleared before the cached-buffer early return, or a revisit leaves it \
             standing"
        );
    }

    #[test]
    fn a_message_read_by_the_other_side_raises_the_watermark() {
        let (mut app, _rx) = opened_chat();

        app.handle_event(TgEvent::OutgoingRead {
            peer: peer(1).id,
            max_id: 97,
        });

        assert_eq!(summary(&app, peer(1).id).read_outbox_max_id, Some(97));
    }

    #[test]
    fn an_outbox_read_names_the_channel_it_belongs_to() {
        let (mut app, _rx) = app();
        app.handle_event(TgEvent::Authorized(true));
        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![dialog(7, "Alice"), channel_dialog(7, "Announcements")],
            exhausted: true,
            archived: false,
        });

        app.handle_event(TgEvent::OutgoingRead {
            peer: channel(7).id,
            max_id: 9,
        });

        assert_eq!(summary(&app, channel(7).id).read_outbox_max_id, Some(9));
        assert_eq!(
            summary(&app, peer(7).id).read_outbox_max_id,
            Some(0),
            "channel ids restart at 1 per channel and collide with user ids, so the two must \
             stay distinct peers all the way from the update"
        );
    }

    #[test]
    fn a_read_watermark_for_a_chat_never_opened_is_still_remembered() {
        let (mut app, _rx) = opened_chat();

        // Chat 2 has no `ChatBuffer` at all — this is the case that rules the buffer out as the
        // home for read state.
        app.handle_event(TgEvent::OutgoingRead {
            peer: peer(2).id,
            max_id: 12,
        });

        assert!(!app.chats.contains_key(&peer(2).id));
        assert_eq!(summary(&app, peer(2).id).read_outbox_max_id, Some(12));
    }

    #[test]
    fn reading_elsewhere_reconciles_a_badge_downwards_only() {
        let (mut app, _rx) = opened_chat();
        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(2),
            message: message(1, "you around?"),
            edited: false,
        });

        app.handle_event(TgEvent::IncomingRead {
            peer: peer(2).id,
            still_unread: 4,
        });

        assert_eq!(
            summary(&app, peer(2).id).unread,
            1,
            "the server counts from its own read pointer, which tgtui never moves, so its number \
             can only ever be believed when it is the smaller one"
        );
    }

    // -- the message cursor --------------------------------------------------

    /// The message the cursor is on in the open chat.
    fn cursor(app: &App) -> Option<i32> {
        app.open_buffer().and_then(|buffer| buffer.selected)
    }

    #[test]
    fn ctrl_s_puts_the_cursor_on_the_newest_message_and_aims_the_keyboard_at_it() {
        let (mut app, _rx) = opened_chat();

        app.handle_key(ctrl(KeyCode::Char('s')));

        assert_eq!(cursor(&app), Some(100));
        assert_eq!(app.focus, Focus::Messages);
        assert!(
            app.scroll_to_selection,
            "the newest message is at the bottom, but the renderer still has to be told to check"
        );
    }

    #[test]
    fn ctrl_s_again_puts_the_cursor_away() {
        let (mut app, _rx) = opened_chat();

        app.handle_key(ctrl(KeyCode::Char('s')));
        app.handle_key(ctrl(KeyCode::Char('s')));

        assert_eq!(cursor(&app), None);
        assert!(!app.selecting());
    }

    /// The whole reason the cursor is modal. With the transcript focused every plain character is
    /// typed into the compose box, so `j` and `k` have to be claimed before they get there.
    #[test]
    fn select_mode_swallows_j_and_k_instead_of_typing_them() {
        let (mut app, _rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('s')));

        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('k')));
        app.handle_key(key(KeyCode::Char('j')));

        assert_eq!(cursor(&app), Some(99));
        assert!(
            app.compose.is_empty(),
            "a cursor keystroke that reached the compose box would be typing into a message"
        );
    }

    #[test]
    fn arrow_keys_move_the_cursor_rather_than_scrolling_while_it_is_up() {
        let (mut app, _rx) = opened_chat();
        let before = app.open_buffer().unwrap().scroll;
        app.handle_key(ctrl(KeyCode::Char('s')));

        app.handle_key(key(KeyCode::Up));

        assert_eq!(cursor(&app), Some(99));
        assert_eq!(
            app.open_buffer().unwrap().scroll,
            before,
            "the renderer moves the viewport to follow the cursor; the key handler must not also \
             move it, or the two would fight"
        );
    }

    #[test]
    fn esc_leaves_select_mode_and_gives_the_keyboard_back_to_the_compose_box() {
        let (mut app, _rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('s')));

        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Char('j')));

        assert_eq!(cursor(&app), None);
        assert_eq!(app.compose, "j");
    }

    /// Same ranking as the action menu, and for the same reason: with a picture open the
    /// transcript is not drawn at all, so a cursor moving through it would be invisible.
    #[test]
    fn the_viewer_outranks_the_message_cursor() {
        let (mut app, _rx) = opened_chat();
        app.open_buffer_mut().unwrap().messages[0] = photo_message(60, "look", 400, 300);
        app.handle_key(ctrl(KeyCode::Char('s')));
        app.viewer = Some(60);

        app.handle_key(key(KeyCode::Char('j')));

        assert_eq!(
            cursor(&app),
            Some(100),
            "the cursor must not have moved under a picture that is covering the transcript"
        );
    }

    #[test]
    fn walking_off_the_top_of_what_we_hold_asks_for_more_history() {
        let (mut app, mut rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('s')));

        for _ in 0..PAGE_SIZE {
            app.handle_key(key(KeyCode::Up));
        }

        assert_eq!(cursor(&app), Some(51), "the oldest message we hold");
        assert!(
            drain(&mut rx)
                .iter()
                .any(|command| matches!(command, TgCommand::LoadOlderMessages { .. })),
            "reaching the top with the cursor is the same signal as scrolling there"
        );
    }

    #[test]
    fn a_page_of_older_history_leaves_the_cursor_on_the_same_message() {
        let (mut app, _rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('s')));
        app.handle_key(key(KeyCode::Up));

        app.handle_event(TgEvent::OlderMessagesLoaded {
            peer: peer(1).id,
            messages: page(50, PAGE_SIZE as i32),
        });

        assert_eq!(cursor(&app), Some(99));
    }

    #[test]
    fn a_chat_with_nothing_in_it_says_so_rather_than_turning_the_mode_on() {
        let (mut app, _rx) = opened_chat();
        app.handle_event(TgEvent::HistoryCleared { peer: peer(1).id });

        app.handle_key(ctrl(KeyCode::Char('s')));

        assert!(!app.selecting());
        assert!(app.status.is_some());
    }

    // -- the message action menu ---------------------------------------------

    /// Put the cursor on `id` and open its menu. The cursor is placed directly rather than walked
    /// there with `Ctrl+S` and arrows — that path has tests of its own above, and `Ctrl+S` toggles,
    /// so a helper that pressed it would turn select mode *off* on its second call.
    fn message_menu_on(app: &mut App, id: i32) {
        app.open_buffer_mut()
            .expect("a chat should be open")
            .selected = Some(id);
        app.handle_key(key(KeyCode::Enter));
    }

    fn message_menu_labels(app: &App) -> Vec<&'static str> {
        app.message_menu
            .as_ref()
            .expect("the message menu should be open")
            .actions
            .iter()
            .map(|action| action.label())
            .collect()
    }

    /// Walk the message menu down to the entry with this label and press Enter.
    fn pick_message_action(app: &mut App, label: &str) {
        let menu = app.message_menu.as_ref().expect("the menu should be open");
        let at = menu
            .actions
            .iter()
            .position(|action| action.label() == label)
            .unwrap_or_else(|| panic!("no {label:?} in {:?}", message_menu_labels(app)));
        for _ in 0..at {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Enter));
    }

    #[test]
    fn enter_opens_the_menu_on_the_message_under_the_cursor() {
        let (mut app, _rx) = opened_chat();

        message_menu_on(&mut app, 99);

        let menu = app.message_menu.as_ref().unwrap();
        assert_eq!(menu.message_id, 99);
        assert_eq!(menu.peer.id, peer(1).id);
    }

    /// The menu is built from the message *and* the conversation, so the fixture's incoming
    /// messages in a private chat offer no Edit and no unsend.
    #[test]
    fn the_menu_reflects_whose_message_it_is() {
        let (mut app, _rx) = opened_chat();
        message_menu_on(&mut app, 100);
        assert_eq!(
            message_menu_labels(&app),
            ["Reply", "Forward to…", "Delete for me"]
        );

        app.handle_key(key(KeyCode::Esc));
        app.open_buffer_mut()
            .unwrap()
            .messages
            .push_back(outgoing(101, "mine"));
        message_menu_on(&mut app, 101);
        assert_eq!(
            message_menu_labels(&app),
            [
                "Reply",
                "Edit",
                "Forward to…",
                "Delete for me",
                "Delete for everyone",
            ]
        );
    }

    /// The same hazard the chat menu has: `j`, `k`, `y` and `n` are plain characters, and the
    /// compose box is right behind the popup.
    #[test]
    fn the_message_menu_swallows_the_keys_it_uses() {
        let (mut app, _rx) = opened_chat();
        message_menu_on(&mut app, 100);

        app.handle_key(key(KeyCode::Char('j')));
        app.handle_key(key(KeyCode::Char('y')));
        app.handle_key(key(KeyCode::Char('n')));

        assert!(app.compose.is_empty());
        assert_eq!(app.message_menu.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn a_delete_asks_before_it_goes_out() {
        let (mut app, mut rx) = opened_chat();
        message_menu_on(&mut app, 100);

        pick_message_action(&mut app, "Delete for me");

        assert!(
            drain(&mut rx).is_empty(),
            "nothing may leave for the network before the question is answered"
        );
        assert_eq!(
            app.message_menu.as_ref().unwrap().confirming,
            Some(MessageAction::DeleteForMe)
        );
    }

    #[test]
    fn answering_yes_sends_the_delete_and_no_sends_nothing() {
        let (mut app, mut rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Delete for me");

        app.handle_key(key(KeyCode::Char('n')));
        assert!(drain(&mut rx).is_empty());
        assert!(
            app.message_menu.is_some(),
            "no closes the question, not the menu"
        );

        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('y')));

        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::DeleteMessages { ids, revoke: false, .. }] if ids == &[100]
        ));
        assert!(app.message_menu.is_none(), "yes closes the menu");
    }

    #[test]
    fn unsending_asks_the_server_to_revoke() {
        let (mut app, mut rx) = opened_chat();
        app.open_buffer_mut()
            .unwrap()
            .messages
            .push_back(outgoing(101, "oops"));
        message_menu_on(&mut app, 101);

        pick_message_action(&mut app, "Delete for everyone");
        app.handle_key(key(KeyCode::Char('y')));

        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::DeleteMessages { ids, revoke: true, .. }] if ids == &[101]
        ));
    }

    /// Nothing is applied here — the transcript closes up when the server confirms, exactly as the
    /// chat actions do. A delete that quietly failed but showed as deleted would be a lie about
    /// what the account still holds.
    #[test]
    fn a_delete_leaves_the_transcript_alone_until_the_server_answers() {
        let (mut app, mut rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Delete for me");
        app.handle_key(key(KeyCode::Char('y')));
        drain(&mut rx);

        assert!(
            app.open_buffer()
                .unwrap()
                .messages
                .iter()
                .any(|m| m.id == 100),
            "the message must still be there while the request is in flight"
        );

        app.handle_event(TgEvent::MessagesDeleted {
            channel: None,
            ids: vec![100],
        });

        assert!(
            !app.open_buffer()
                .unwrap()
                .messages
                .iter()
                .any(|m| m.id == 100)
        );
        assert_eq!(
            cursor(&app),
            None,
            "the cursor was on the message that went, so it goes too"
        );
    }

    // -- forwarding ----------------------------------------------------------

    fn type_into(app: &mut App, text: &str) {
        for ch in text.chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
    }

    /// Names the picker is currently offering.
    fn forward_names(app: &App) -> Vec<String> {
        app.forward_matches()
            .into_iter()
            .map(|index| app.dialogs.items[index].name.clone())
            .collect()
    }

    #[test]
    fn forwarding_opens_a_picker_over_every_chat() {
        let (mut app, mut rx) = opened_chat();
        message_menu_on(&mut app, 100);

        pick_message_action(&mut app, "Forward to…");

        assert!(
            app.message_menu.is_none(),
            "the menu hands over to the picker"
        );
        assert_eq!(forward_names(&app), ["Alice", "Bob"]);
        assert!(
            drain(&mut rx).is_empty(),
            "nothing goes out until a destination is chosen"
        );
    }

    /// The picker takes plain characters, so it has to be claimed ahead of the compose box and of
    /// the menus that navigate with `j` and `k`.
    #[test]
    fn typing_in_the_picker_filters_rather_than_composing() {
        let (mut app, _rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Forward to…");

        type_into(&mut app, "bo");

        assert_eq!(forward_names(&app), ["Bob"]);
        assert!(app.compose.is_empty());
    }

    #[test]
    fn the_filter_ignores_case_and_backspace_widens_it_again() {
        let (mut app, _rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Forward to…");

        type_into(&mut app, "ALIC");
        assert_eq!(forward_names(&app), ["Alice"]);

        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Backspace));
        assert_eq!(forward_names(&app), ["Alice", "Bob"]);
    }

    /// `selected` indexes the filtered rows, so narrowing the list can leave it past the end.
    #[test]
    fn narrowing_the_filter_cannot_strand_the_selection() {
        let (mut app, mut rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Forward to…");

        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.forward.as_ref().unwrap().selected, 1);

        type_into(&mut app, "alice");
        assert_eq!(
            app.forward.as_ref().unwrap().selected,
            0,
            "row 1 no longer exists in a list of one"
        );

        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::ForwardMessages { destination, .. }] if destination.id == peer(1).id
        ));
    }

    #[test]
    fn enter_forwards_to_the_highlighted_chat() {
        let (mut app, mut rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Forward to…");

        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::ForwardMessages { source, ids, destination }]
                if source.id == peer(1).id && ids == &[100] && destination.id == peer(2).id
        ));
        assert!(app.forward.is_none());
    }

    #[test]
    fn esc_cancels_a_forward_without_sending_anything() {
        let (mut app, mut rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Forward to…");

        app.handle_key(key(KeyCode::Esc));

        assert!(app.forward.is_none());
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_filter_matching_nothing_forwards_nothing() {
        let (mut app, mut rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Forward to…");

        type_into(&mut app, "nobody");
        app.handle_key(key(KeyCode::Enter));

        assert!(drain(&mut rx).is_empty());
        assert!(
            app.forward.is_some(),
            "Enter with nothing to pick must leave the picker up rather than closing silently"
        );
    }

    /// The picker searches the whole pool, not the folder on screen: where you want to send
    /// something has nothing to do with which tab you happen to be reading.
    #[test]
    fn the_picker_reaches_chats_the_current_folder_does_not_show() {
        let (mut app, _rx) = opened_chat();
        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![archived_dialog(3, "Carol")],
            exhausted: true,
            archived: true,
        });
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Forward to…");

        type_into(&mut app, "carol");

        assert_eq!(
            forward_names(&app),
            ["Carol"],
            "an archived chat is still somewhere you can forward to"
        );
    }

    // -- replying ------------------------------------------------------------

    #[test]
    fn replying_threads_the_next_message_under_the_one_picked() {
        let (mut app, mut rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Reply");

        assert_eq!(app.replying_to, Some(100));
        assert!(!app.selecting());

        for ch in "on my way".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::SendMessage { text, reply_to: Some(100), .. }] if text == "on my way"
        ));
        assert_eq!(
            app.replying_to, None,
            "the thread ends with the message that used it"
        );
    }

    /// Unlike an edit, a reply adds to what the box already holds rather than replacing it.
    #[test]
    fn starting_a_reply_keeps_what_is_already_typed() {
        let (mut app, _rx) = opened_chat();
        app.handle_key(key(KeyCode::Tab));
        for ch in "yes".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }

        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Reply");

        assert_eq!(app.compose, "yes");
    }

    /// A message cannot be both rewritten and replied to, so picking one ends the other.
    #[test]
    fn replying_abandons_an_edit_in_progress() {
        let (mut app, _rx) = editing("teh typo");

        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Reply");

        assert_eq!(app.editing, None);
        assert_eq!(app.replying_to, Some(100));
        assert!(
            app.compose.is_empty(),
            "the edit's text was the message's, not the user's"
        );
    }

    #[test]
    fn esc_cancels_a_reply_before_it_leaves_the_pane() {
        let (mut app, mut rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Reply");

        app.handle_key(key(KeyCode::Esc));

        assert_eq!(app.replying_to, None);
        assert_eq!(app.focus, Focus::Messages);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_parent_on_screen_is_fetched_once_and_not_again() {
        let (mut app, mut rx) = opened_chat();

        app.request_reply_targets(&[7, 7, 8]);
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::LoadReplyTargets { ids, .. }] if ids == &[7, 8]
        ));

        app.request_reply_targets(&[7, 8]);
        assert!(
            drain(&mut rx).is_empty(),
            "the renderer asks again every frame; the guard is what stops it"
        );
    }

    /// A parent that has been deleted comes back as nothing at all. The guard must hold for it
    /// too, or the very next frame asks for it again — the same terminal shape as a failed photo.
    #[test]
    fn a_parent_the_server_cannot_find_stops_being_asked_for() {
        let (mut app, mut rx) = opened_chat();
        app.request_reply_targets(&[7]);
        drain(&mut rx);

        app.handle_event(TgEvent::ReplyTargetsLoaded {
            peer: peer(1).id,
            asked: vec![7],
            targets: Vec::new(),
        });

        app.request_reply_targets(&[7]);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_fetched_parent_becomes_the_quote_for_every_reply_to_it() {
        let (mut app, _rx) = opened_chat();

        app.handle_event(TgEvent::ReplyTargetsLoaded {
            peer: peer(1).id,
            asked: vec![7],
            targets: vec![(
                7,
                ReplyPreview {
                    sender: Some("Bob".to_string()),
                    text: "where should we meet?".to_string(),
                },
            )],
        });

        let preview = app.open_buffer().unwrap().reply_preview(7).unwrap();
        assert_eq!(preview.text, "where should we meet?");
        assert_eq!(preview.sender.as_deref(), Some("Bob"));
    }

    /// A message already in the buffer needs no round trip to be quoted.
    #[test]
    fn a_parent_in_the_buffer_is_read_straight_out_of_it() {
        let (app, _rx) = opened_chat();

        let preview = app.open_buffer().unwrap().reply_preview(100).unwrap();
        assert_eq!(preview.text, "message 100");
    }

    // -- editing -------------------------------------------------------------

    /// Put an outgoing message in the buffer and start editing it.
    fn editing(text: &str) -> (App, mpsc::UnboundedReceiver<TgCommand>) {
        let (mut app, mut rx) = opened_chat();
        app.open_buffer_mut()
            .unwrap()
            .messages
            .push_back(outgoing(101, text));
        message_menu_on(&mut app, 101);
        pick_message_action(&mut app, "Edit");
        drain(&mut rx);
        (app, rx)
    }

    #[test]
    fn editing_loads_the_message_back_into_the_compose_box() {
        let (app, _rx) = editing("teh typo");

        assert_eq!(app.compose, "teh typo");
        assert_eq!(app.editing, Some(101));
        assert!(
            !app.selecting(),
            "the keyboard is aimed at the compose box now, so a highlight would say otherwise"
        );
    }

    /// The transcript's copy of a media message has the label folded into it. Sending that back
    /// would write `[file]` into the caption.
    #[test]
    fn editing_a_media_message_offers_its_caption_rather_than_the_label() {
        let (mut app, _rx) = opened_chat();
        let mut media = outgoing(101, "here you go");
        media.text = "[file] here you go".to_string();
        app.open_buffer_mut().unwrap().messages.push_back(media);

        message_menu_on(&mut app, 101);
        pick_message_action(&mut app, "Edit");

        assert_eq!(
            app.compose, "here you go",
            "the label is the transcript's, not the message's"
        );
    }

    #[test]
    fn enter_sends_an_edit_rather_than_a_new_message_while_one_is_loaded() {
        let (mut app, mut rx) = editing("teh typo");

        app.handle_key(key(KeyCode::Backspace));
        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::EditMessage { message_id: 101, text, .. }] if text == "teh typ"
        ));
        assert_eq!(
            app.editing, None,
            "the box goes back to writing new messages"
        );
        assert!(app.compose.is_empty());
    }

    #[test]
    fn esc_abandons_an_edit_before_it_leaves_the_pane() {
        let (mut app, mut rx) = editing("teh typo");

        app.handle_key(key(KeyCode::Esc));

        assert_eq!(app.editing, None);
        assert!(app.compose.is_empty());
        assert_eq!(
            app.focus,
            Focus::Messages,
            "the first Esc cancels the edit; leaving the pane takes a second one"
        );
        assert!(drain(&mut rx).is_empty());

        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.focus, Focus::Chats);
    }

    /// Nothing is applied locally. The new text arrives over the update stream, through the arm
    /// that already existed for edits made on another device.
    #[test]
    fn an_edit_reaches_the_transcript_through_the_update_stream() {
        let (mut app, mut rx) = editing("teh typo");
        app.handle_key(key(KeyCode::Enter));
        drain(&mut rx);

        assert_eq!(
            app.open_buffer().unwrap().messages.back().unwrap().text,
            "teh typo",
            "the transcript must not change on the strength of a request that may still fail"
        );

        app.handle_event(TgEvent::IncomingMessage {
            peer: peer(1),
            message: outgoing(101, "the typo"),
            edited: true,
        });

        assert_eq!(
            app.open_buffer().unwrap().messages.back().unwrap().text,
            "the typo"
        );
    }

    /// Walk from Alice to Bob with the chat list.
    fn walk_to_the_next_chat(app: &mut App) {
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Down));
    }

    /// A message id means something different in every conversation, so carrying one across would
    /// aim the next Enter at whatever message happens to hold that id in the new chat.
    #[test]
    fn walking_to_another_chat_abandons_a_pending_reply() {
        let (mut app, _rx) = opened_chat();
        message_menu_on(&mut app, 100);
        pick_message_action(&mut app, "Reply");

        walk_to_the_next_chat(&mut app);

        assert_eq!(app.open_chat, Some(peer(2).id));
        assert_eq!(
            app.replying_to, None,
            "id 100 in Bob's chat is a different message entirely"
        );
    }

    #[test]
    fn walking_to_another_chat_abandons_a_pending_edit() {
        let (mut app, _rx) = editing("teh typo");

        walk_to_the_next_chat(&mut app);

        assert_eq!(app.open_chat, Some(peer(2).id));
        assert_eq!(app.editing, None);
    }

    /// The text is the user's, unlike the ids beside it.
    #[test]
    fn walking_to_another_chat_keeps_what_is_typed() {
        let (mut app, _rx) = opened_chat();
        app.handle_key(key(KeyCode::Tab));
        type_into(&mut app, "half a thought");

        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Down));

        assert_eq!(app.compose, "half a thought");
    }

    #[test]
    fn switching_chats_abandons_a_pending_edit() {
        let (mut app, _rx) = editing("half rewritten");

        app.handle_event(TgEvent::DialogGone {
            peer: peer(1).id,
            reason: "deleted",
        });

        assert_eq!(
            app.editing, None,
            "the id names a message in a chat that is gone; in the next one it would name another"
        );
        assert!(app.compose.is_empty());
    }

    #[test]
    fn losing_the_conversation_shuts_a_menu_open_over_it() {
        let (mut app, _rx) = opened_chat();
        message_menu_on(&mut app, 100);

        app.handle_event(TgEvent::DialogGone {
            peer: peer(1).id,
            reason: "deleted",
        });

        assert!(app.message_menu.is_none());
    }

    #[test]
    fn clearing_the_history_shuts_a_menu_open_over_it() {
        let (mut app, _rx) = opened_chat();
        message_menu_on(&mut app, 100);

        app.handle_event(TgEvent::HistoryCleared { peer: peer(1).id });

        assert!(app.message_menu.is_none());
    }

    // -- the chat action menu ------------------------------------------------

    /// Open the menu on the selected chat and return the labels it offers.
    fn menu_labels(app: &App) -> Vec<&'static str> {
        let menu = app.menu.as_ref().expect("the menu should be open");
        menu.actions
            .iter()
            .map(|action| action.label(menu.kind))
            .collect()
    }

    /// Walk the menu selection down to the entry with this label.
    fn select_action(app: &mut App, label: &str) {
        let menu = app.menu.as_ref().expect("the menu should be open");
        let at = menu
            .actions
            .iter()
            .position(|action| action.label(menu.kind) == label)
            .unwrap_or_else(|| panic!("no {label:?} in {:?}", menu_labels(app)));
        for _ in 0..at {
            app.handle_key(key(KeyCode::Down));
        }
    }

    #[test]
    fn ctrl_a_opens_the_menu_on_the_selected_chat() {
        let (mut app, _rx) = opened_chat();

        app.handle_key(ctrl(KeyCode::Char('a')));

        assert_eq!(app.menu.as_ref().unwrap().name, "Alice");
        assert!(menu_labels(&app).contains(&"Delete chat"));
    }

    #[test]
    fn choosing_info_from_the_chat_menu_opens_the_profile() {
        let (mut app, mut rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('a')));
        // Info is the first entry, so the menu opens on it.
        app.handle_key(key(KeyCode::Enter));

        assert!(
            app.menu.is_none(),
            "the menu closes behind the screen it opened"
        );
        assert!(app.peer_info.is_some());
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::LoadPeerInfo { .. }]
        ));
    }

    #[test]
    fn the_info_screen_swallows_ctrl_p_so_the_viewer_cannot_open_behind_it() {
        let (mut app, _rx) = opened_profile();
        app.handle_key(ctrl(KeyCode::Char('p')));

        assert!(
            app.viewer.is_none(),
            "with the profile full screen the transcript is not drawn, so a picture opened from \
             it would be chosen from something nobody can see"
        );
        assert!(app.peer_info.is_some());
    }

    #[test]
    fn typing_while_a_profile_is_open_does_not_reach_the_compose_box() {
        let (mut app, _rx) = opened_profile();
        app.handle_key(key(KeyCode::Char('x')));

        assert!(
            app.compose.is_empty(),
            "every modal is claimed before the compose box, which otherwise swallows every letter"
        );
    }

    #[test]
    fn escape_closes_the_profile_and_forgets_it() {
        let (mut app, _rx) = opened_profile();
        app.handle_key(key(KeyCode::Esc));

        assert!(app.peer_info.is_none());
    }

    #[test]
    fn deleting_the_chat_closes_its_info_screen() {
        let (mut app, _rx) = opened_profile();
        app.handle_event(TgEvent::DialogGone {
            peer: peer(1).id,
            reason: "deleted",
        });

        assert!(
            app.peer_info.is_none(),
            "a profile of a conversation that no longer exists is a screen with nothing behind it"
        );
    }

    #[test]
    fn deleting_another_chat_leaves_an_open_profile_alone() {
        let (mut app, _rx) = opened_profile();
        app.handle_event(TgEvent::DialogGone {
            peer: peer(2).id,
            reason: "deleted",
        });

        assert!(app.peer_info.is_some());
    }

    #[test]
    fn a_group_offers_leave_rather_than_delete() {
        let (mut app, _rx) = app();
        app.handle_event(TgEvent::Authorized(true));
        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![group_dialog(5, "Rust Users")],
            exhausted: true,
            archived: false,
        });

        app.handle_key(ctrl(KeyCode::Char('a')));

        let labels = menu_labels(&app);
        assert!(labels.contains(&"Leave group"), "{labels:?}");
        assert!(!labels.contains(&"Delete chat"), "{labels:?}");
    }

    /// Without the modal check in `handle_main_key`, every one of these would end up in the
    /// compose box behind the popup.
    #[test]
    fn the_menu_swallows_the_keys_the_compose_box_would_otherwise_take() {
        let (mut app, _rx) = opened_chat();
        app.focus = Focus::Messages;
        app.handle_key(ctrl(KeyCode::Char('a')));

        for ch in ['j', 'k', 'y', 'n', 'q'] {
            app.handle_key(key(KeyCode::Char(ch)));
        }

        assert!(
            app.compose.is_empty(),
            "menu keys leaked into the compose box: {:?}",
            app.compose
        );
    }

    #[test]
    fn a_reversible_action_goes_out_as_soon_as_it_is_picked() {
        let (mut app, mut rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('a')));
        select_action(&mut app, "Mute");

        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::SetMuted { muted: true, .. }]
        ));
        assert!(
            app.menu.is_none(),
            "the menu closes once the action is sent"
        );
    }

    #[test]
    fn a_destructive_action_asks_before_anything_leaves_the_app() {
        let (mut app, mut rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('a')));
        select_action(&mut app, "Delete chat");

        app.handle_key(key(KeyCode::Enter));

        assert!(
            drain(&mut rx).is_empty(),
            "nothing may go out until the question is answered"
        );
        assert!(app.menu.as_ref().unwrap().prompt().is_some());
    }

    #[test]
    fn answering_no_leaves_the_menu_open_and_sends_nothing() {
        let (mut app, mut rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('a')));
        select_action(&mut app, "Delete chat");
        app.handle_key(key(KeyCode::Enter));

        app.handle_key(key(KeyCode::Char('n')));

        assert!(drain(&mut rx).is_empty());
        let menu = app
            .menu
            .as_ref()
            .expect("cancelling a question is not cancelling the menu");
        assert!(menu.confirming.is_none());
    }

    /// `Esc` closes the menu, but with a question up it has to mean "no" instead — otherwise the
    /// habit of pressing it to back out would be indistinguishable from answering.
    #[test]
    fn escape_answers_the_question_rather_than_closing_the_menu() {
        let (mut app, mut rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('a')));
        select_action(&mut app, "Delete chat");
        app.handle_key(key(KeyCode::Enter));

        app.handle_key(key(KeyCode::Esc));

        assert!(drain(&mut rx).is_empty());
        assert!(app.menu.is_some());
        assert!(app.menu.as_ref().unwrap().confirming.is_none());
    }

    #[test]
    fn answering_yes_sends_the_command_and_closes_the_menu() {
        let (mut app, mut rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('a')));
        select_action(&mut app, "Delete chat");
        app.handle_key(key(KeyCode::Enter));

        app.handle_key(key(KeyCode::Char('y')));

        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::DeleteDialog { .. }]
        ));
        assert!(app.menu.is_none());
    }

    /// The account's real state is shared with every other device, so the list must never claim a
    /// change the server has not made.
    #[test]
    fn nothing_shows_as_changed_until_the_server_confirms_it() {
        let (mut app, _rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('a')));
        select_action(&mut app, "Mute");
        app.handle_key(key(KeyCode::Enter));

        assert!(
            !summary(&app, peer(1).id).muted,
            "a mute that quietly failed but showed as muted would be a lie about the account"
        );

        app.handle_event(TgEvent::MuteChanged {
            peer: peer(1).id,
            muted: true,
        });

        assert!(summary(&app, peer(1).id).muted);
    }

    /// The list reorders itself under live updates, so reading the selection when the key is
    /// pressed would let an action land on a conversation that merely moved under the highlight.
    #[test]
    fn the_menu_acts_on_the_chat_it_was_opened_on() {
        let (mut app, mut rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('a')));

        // Whatever the list does now, the pending action still belongs to Alice.
        app.dialogs.selected = 1;
        select_action(&mut app, "Mute");
        app.handle_key(key(KeyCode::Enter));

        match drain(&mut rx).as_slice() {
            [TgCommand::SetMuted { peer: target, .. }] => assert_eq!(target.id, peer(1).id),
            other => panic!("expected one mute, got {other:?}"),
        }
    }

    #[test]
    fn clearing_the_history_empties_the_transcript_but_keeps_the_chat() {
        let (mut app, _rx) = opened_chat();

        app.handle_event(TgEvent::HistoryCleared { peer: peer(1).id });

        assert!(app.chats[&peer(1).id].messages.is_empty());
        assert!(
            !app.chats[&peer(1).id].has_more_older,
            "there is provably nothing behind an emptied history, so scrolling up must not start \
             paginating it"
        );
        assert_eq!(app.open_chat, Some(peer(1).id), "the chat is still open");
        assert!(
            app.dialogs.find(peer(1).id).is_some(),
            "and still in the list"
        );
    }

    #[test]
    fn leaving_the_open_chat_closes_it_and_moves_to_another() {
        let (mut app, _rx) = opened_chat();
        app.focus = Focus::Messages;
        app.compose = "half a thought".to_string();

        app.handle_event(TgEvent::DialogGone {
            peer: peer(1).id,
            reason: "left",
        });

        assert!(app.dialogs.find(peer(1).id).is_none());
        assert!(!app.chats.contains_key(&peer(1).id));
        assert_eq!(
            app.open_chat,
            Some(peer(2).id),
            "the pane must follow the selection rather than keep showing a chat that is gone"
        );
        assert!(
            app.compose.is_empty(),
            "a half-typed line must not be carried into the chat that replaced it"
        );
    }

    #[test]
    fn leaving_the_last_chat_leaves_nothing_open_and_does_not_panic() {
        let (mut app, _rx) = app();
        app.handle_event(TgEvent::Authorized(true));
        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![dialog(1, "Alice")],
            exhausted: true,
            archived: false,
        });

        app.handle_event(TgEvent::DialogGone {
            peer: peer(1).id,
            reason: "deleted",
        });

        assert!(app.dialogs.items.is_empty());
        assert_eq!(app.open_chat, None);
        assert_eq!(app.dialogs.selected, 0);
    }

    /// With a picture open the chat list is not drawn at all, so a menu over it would be acting
    /// on something the user cannot see.
    #[test]
    fn the_viewer_keeps_the_menu_shut() {
        let (mut app, _rx) = viewable_chat(2);
        app.handle_key(ctrl(KeyCode::Char('p')));
        assert!(app.viewer.is_some());

        app.handle_key(ctrl(KeyCode::Char('a')));

        assert!(app.menu.is_none());
    }

    #[test]
    fn the_blocked_list_decides_which_face_the_block_entry_shows() {
        let (mut app, _rx) = opened_chat();

        app.handle_event(TgEvent::BlockedPeersLoaded {
            peers: vec![peer(1).id],
        });
        app.handle_key(ctrl(KeyCode::Char('a')));

        assert!(menu_labels(&app).contains(&"Unblock user"));
    }

    // -- folder tabs ---------------------------------------------------------

    #[test]
    fn ctrl_o_and_ctrl_e_step_through_the_folders_in_opposite_directions() {
        let (mut app, _rx) = opened_chat();
        app.handle_event(TgEvent::FoldersLoaded {
            folders: vec![folder("Work", &[peer(2).id])],
        });

        app.handle_key(ctrl(KeyCode::Char('o')));
        assert_eq!(app.dialogs.tab, FolderTab::Custom(0));

        app.handle_key(ctrl(KeyCode::Char('o')));
        assert_eq!(app.dialogs.tab, FolderTab::Archive);

        app.handle_key(ctrl(KeyCode::Char('e')));
        assert_eq!(app.dialogs.tab, FolderTab::Custom(0));
    }

    /// The archive has a cursor of its own that nothing has asked for yet, so arriving on the tab
    /// is what has to fetch its first page — there is no row there to scroll to the end of.
    #[test]
    fn arriving_at_the_archive_fetches_it_once() {
        let (mut app, mut rx) = opened_chat();

        app.handle_key(ctrl(KeyCode::Char('o')));

        assert_eq!(app.dialogs.tab, FolderTab::Archive);
        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::LoadMoreDialogs { archived: true }]
        ));

        // The in-flight guard is the archive's own, and it stops the very next frame asking again.
        assert!(app.dialogs.archive.loading);
        app.handle_key(ctrl(KeyCode::Char('e')));
        app.handle_key(ctrl(KeyCode::Char('o')));
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_page_of_archived_chats_lands_in_the_archive_tab_and_nowhere_else() {
        let (mut app, _rx) = opened_chat();
        app.dialogs.main.exhausted = false;

        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![archived_dialog(7, "Old group")],
            exhausted: true,
            archived: true,
        });

        assert_eq!(
            app.dialogs.visible().len(),
            2,
            "the main list must not grow by an archived chat"
        );
        assert!(
            !app.dialogs.main.exhausted,
            "an archive page says nothing about how much of the main list is left"
        );
        assert!(app.dialogs.archive.exhausted);

        app.dialogs.tab = FolderTab::Archive;
        assert_eq!(app.dialogs.selected_summary().unwrap().name, "Old group");
    }

    #[test]
    fn the_menu_on_an_archived_chat_offers_the_way_back_out() {
        let (mut app, mut rx) = opened_chat();
        app.handle_event(TgEvent::FolderChanged {
            peer: peer(1).id,
            archived: true,
        });
        app.dialogs.tab = FolderTab::Archive;
        drain(&mut rx);

        app.handle_key(ctrl(KeyCode::Char('a')));
        assert!(menu_labels(&app).contains(&"Unarchive"));
        assert!(!menu_labels(&app).contains(&"Archive"));

        select_action(&mut app, "Unarchive");
        app.handle_key(key(KeyCode::Enter));

        assert!(matches!(
            drain(&mut rx).as_slice(),
            [TgCommand::SetArchived {
                archived: false,
                ..
            }]
        ));
    }

    /// Archiving is not deleting: the conversation carries on existing in another tab, so throwing
    /// its transcript away would mean fetching the whole thing again to read it there.
    #[test]
    fn archiving_moves_the_chat_without_closing_it_or_dropping_its_history() {
        let (mut app, _rx) = opened_chat();
        assert!(app.chats.contains_key(&peer(1).id));

        app.handle_event(TgEvent::FolderChanged {
            peer: peer(1).id,
            archived: true,
        });

        assert!(app.dialogs.find(peer(1).id).unwrap().archived);
        assert!(app.chats.contains_key(&peer(1).id));
        assert_eq!(app.open_chat, Some(peer(1).id));
        assert_eq!(app.status.as_ref().unwrap().text, "Alice — archived");
    }

    /// A folder is a filter over the main list, so an empty one has to keep pulling pages by
    /// itself — there is no row in it to scroll to the end of.
    #[test]
    fn an_empty_folder_keeps_paging_the_main_list_without_a_keypress() {
        let (mut app, mut rx) = opened_chat();
        app.handle_event(TgEvent::FoldersLoaded {
            folders: vec![folder("Work", &[peer(99).id])],
        });
        app.dialogs.main.exhausted = false;
        app.handle_key(ctrl(KeyCode::Char('o')));
        drain(&mut rx);

        app.handle_event(TgEvent::DialogsLoaded {
            items: vec![dialog(3, "Carol")],
            exhausted: false,
            archived: false,
        });

        assert!(app.dialogs.visible().is_empty());
        assert!(
            matches!(
                drain(&mut rx).as_slice(),
                [TgCommand::LoadMoreDialogs { archived: false }]
            ),
            "a page that added nothing to the folder must fetch the next one on its own"
        );
    }
}
