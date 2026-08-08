//! Responses and pushes sent from the Telegram actor back to the UI loop.

use grammers_session::types::{PeerId, PeerRef};

use crate::state::chat_buffer::ChatMessage;
use crate::state::dialog_list::DialogSummary;

#[derive(Debug)]
pub enum TgEvent {
    /// Result of the startup session check.
    Authorized(bool),
    /// The login code was sent; ask the user for it.
    CodeSent,
    /// The account has 2FA enabled, so the password is needed to finish signing in.
    PasswordNeeded { hint: Option<String> },
    SignedIn { name: String },
    /// A login step failed. The user stays on the current screen and can retry.
    LoginFailed(String),
    DialogsLoaded {
        items: Vec<DialogSummary>,
        exhausted: bool,
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
    MessageSent {
        peer: PeerId,
        message: ChatMessage,
    },
    /// A message arrived (or was edited) live over the update stream.
    IncomingMessage {
        peer: PeerRef,
        message: ChatMessage,
        edited: bool,
    },
    /// A non-fatal problem worth surfacing in the status banner.
    Error(String),
}
