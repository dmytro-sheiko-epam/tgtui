//! Responses and pushes sent from the Telegram actor back to the UI loop.

use std::sync::Arc;

use grammers_session::types::{PeerId, PeerRef};
use image::DynamicImage;

use crate::state::chat_buffer::ChatMessage;
use crate::state::dialog_list::DialogSummary;
use crate::state::folders::Folder;

#[derive(Debug)]
pub enum TgEvent {
    /// Result of the startup session check.
    Authorized(bool),
    /// The login code was sent; ask the user for it.
    CodeSent,
    /// The account has 2FA enabled, so the password is needed to finish signing in.
    PasswordNeeded {
        hint: Option<String>,
    },
    SignedIn {
        name: String,
    },
    /// A login step failed. The user stays on the current screen and can retry.
    LoginFailed(String),
    DialogsLoaded {
        items: Vec<DialogSummary>,
        exhausted: bool,
        /// Which folder's cursor this page advanced.
        archived: bool,
    },
    /// The account's own chat folders, as one answer for the whole tab strip.
    FoldersLoaded {
        folders: Vec<Folder>,
    },
    /// A chat moved between the main list and the archive — here or on another device.
    FolderChanged {
        peer: PeerId,
        archived: bool,
    },
    /// The first (newest) page of a chat's history.
    MessagesLoaded {
        peer: PeerId,
        messages: Vec<ChatMessage>,
    },
    /// A page of older messages fetched during scroll-up. Empty means history is exhausted.
    OlderMessagesLoaded {
        peer: PeerId,
        messages: Vec<ChatMessage>,
    },
    /// A photo finished downloading. `image` is `None` when it failed — the failure is still
    /// reported in the success shape, so the in-flight guard clears the way
    /// `OlderMessagesLoaded` does, and the transcript falls back to the label.
    PhotoLoaded {
        peer: PeerId,
        message_id: i32,
        image: Option<Arc<DynamicImage>>,
    },
    MessageSent {
        peer: PeerId,
        message: ChatMessage,
    },
    /// Messages were deleted. `channel` is `Some` only for channel deletions — Telegram sends
    /// bare ids for users and small groups.
    MessagesDeleted {
        channel: Option<PeerId>,
        ids: Vec<i32>,
    },
    /// A message arrived (or was edited) live over the update stream.
    IncomingMessage {
        peer: PeerRef,
        message: ChatMessage,
        edited: bool,
    },
    /// The other side read our messages up to and including `max_id`, so their ticks rise from
    /// ✓ to ✓✓.
    ///
    /// Telegram has no per-message read flag — there is only this one watermark per chat, and
    /// resolving an update gap can replay an older one after a newer one, so it is applied as a
    /// maximum rather than an assignment.
    OutgoingRead {
        peer: PeerId,
        max_id: i32,
    },
    /// *We* read incoming messages in this chat somewhere — almost always another device, because
    /// tgtui never acknowledges a read itself.
    ///
    /// Only the server's `still_unread_count` is carried: the update's `max_id` can't be turned
    /// into a count here (service messages and our own messages don't count as unread), and the
    /// count is the only thing the badge needs.
    IncomingRead {
        peer: PeerId,
        still_unread: i32,
    },
    // -- chat actions --------------------------------------------------------
    //
    // Applied when the server confirms, never optimistically: a mute that silently failed but
    // showed as muted would be a lie about the account's real state. The wait is a round trip, and
    // the status banner narrates it.
    //
    // Each also arrives unprompted when the change was made on another device, which is why they
    // carry the new value rather than meaning "the thing you asked for happened".
    MuteChanged {
        peer: PeerId,
        muted: bool,
    },
    PinChanged {
        peer: PeerId,
        pinned: bool,
    },
    BlockedChanged {
        peer: PeerId,
        blocked: bool,
    },
    /// The account's blocked list, as one answer for every dialog at once.
    BlockedPeersLoaded {
        peers: Vec<PeerId>,
    },
    /// The chat's own history is gone but the conversation remains.
    HistoryCleared {
        peer: PeerId,
    },
    /// The conversation is gone for good — deleted or left. Archiving is *not* this: it moves the
    /// row to another tab and reports `FolderChanged`.
    DialogGone {
        peer: PeerId,
        /// What to tell the user, since the three reasons read very differently.
        reason: &'static str,
    },

    /// A non-fatal problem worth surfacing in the status banner.
    Error(String),
}
