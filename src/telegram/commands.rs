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
    /// Fetch the next page of dialogs for the chat list, from folder 1 rather than folder 0 when
    /// `archived`. The two folders page independently and have a cursor each.
    LoadMoreDialogs {
        archived: bool,
    },
    /// Read the account's own chat folders. They are filter rules rather than a collection, so
    /// this answers once for the whole tab strip and nothing pages.
    LoadFolders,
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
        /// The message being replied to, if this is a reply. Telegram threads on a bare id.
        reply_to: Option<i32>,
    },
    /// Fetch the parents of replies on screen whose parent is not in the buffer. Issued by
    /// visibility, the same way photos are.
    LoadReplyTargets {
        peer: PeerRef,
        ids: Vec<i32>,
    },

    // -- message actions -----------------------------------------------------
    //
    // Like the chat actions below, these change what the account really holds and are visible on
    // every other device. They are only ever issued from an explicit menu choice, and the two
    // deletes pass a confirmation first.
    /// Remove messages from this chat. `revoke` decides whose copies go: ours only, or everyone's.
    DeleteMessages {
        peer: PeerRef,
        ids: Vec<i32>,
        revoke: bool,
    },
    /// Replace the text of one of our own messages. Telegram's edit window is the server's to
    /// enforce; past it the request comes back as an error and the banner says so.
    EditMessage {
        peer: PeerRef,
        message_id: i32,
        text: String,
    },
    /// Copy messages from one conversation into another. A chat with forwarding restricted
    /// refuses server-side — there is no flag on the dialog row that says so in advance.
    ForwardMessages {
        source: PeerRef,
        ids: Vec<i32>,
        destination: PeerRef,
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
    /// Move a chat into the archive folder, or back out of it. Both are the same call — folders
    /// are the mechanism and folder 0 is the main list.
    SetArchived {
        peer: PeerRef,
        archived: bool,
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
