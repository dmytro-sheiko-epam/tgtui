//! Requests sent from the UI loop into the Telegram actor.

use grammers_session::types::PeerRef;

#[derive(Debug)]
pub enum TgCommand {
    /// Ask whether the persisted session is still logged in.
    CheckAuthorized,
    RequestLoginCode {
        phone: String,
    },
    SignIn {
        code: String,
    },
    CheckPassword {
        password: String,
    },
    /// Fetch the next page of dialogs for the chat list.
    LoadMoreDialogs,
    /// Load the most recent page of messages for a chat that has not been opened yet.
    OpenChat {
        peer: PeerRef,
    },
    /// Infinite scroll: fetch the page of messages older than `before_id`.
    LoadOlderMessages {
        peer: PeerRef,
        before_id: i32,
    },
    SendMessage {
        peer: PeerRef,
        text: String,
    },
    /// Close the connection and let the sender pool wind down.
    Shutdown,
}
