//! Requests sent from the UI loop into the Telegram actor.

use grammers_session::types::PeerRef;

use crate::state::media::PhotoSource;

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
    /// Fetch and decode the picture for one message. Issued only for messages already on screen.
    ///
    /// The source is boxed because it carries a full file location: inline, it would make every
    /// command in this channel as large as the largest one.
    DownloadPhoto {
        peer: PeerRef,
        message_id: i32,
        source: Box<PhotoSource>,
    },
    SendMessage {
        peer: PeerRef,
        text: String,
    },

    // -- chat actions --------------------------------------------------------
    //
    // Every one of these changes the account's real state and is visible on the user's other
    // devices. None is issued without the user picking it out of the action menu, and the
    // irreversible ones pass a confirmation prompt first.
    /// Silence or unsilence a chat's notifications.
    SetMuted {
        peer: PeerRef,
        muted: bool,
    },
    SetPinned {
        peer: PeerRef,
        pinned: bool,
    },
    /// Move a chat into the archive folder. One-way: `iter_dialogs` only asks for folder 0, so an
    /// archived chat is not in the list to be brought back.
    Archive {
        peer: PeerRef,
    },
    /// Empty our copy of a chat's history, leaving the conversation in the list.
    ClearHistory {
        peer: PeerRef,
    },
    /// Delete a private chat, or leave a group or channel — one call for all three.
    DeleteDialog {
        peer: PeerRef,
    },
    SetBlocked {
        peer: PeerRef,
        blocked: bool,
    },
    /// Read the account's blocked list once, so Block/Unblock knows which face to show.
    LoadBlockedPeers,

    /// Close the connection and let the sender pool wind down.
    Shutdown,
}
