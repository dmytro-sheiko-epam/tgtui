//! What a conversation can have done to it, and which of those a given conversation offers.
//!
//! Kept apart from [`crate::state::dialog_list`] and from `App` because it is the one part of the
//! feature with no state at all: given a kind and four flags it answers with a menu. That makes
//! the table below — which is the thing a reader actually wants to check — testable on its own.

use grammers_client::peer::Peer;

/// What kind of conversation a row stands for.
///
/// [`grammers_session::types::PeerKind`] cannot answer this: it collapses broadcast channels and
/// megagroups into one `Channel` variant, and those two want different menus (you *leave* both,
/// but only a group is a group). grammers' `Peer` does split them, so the distinction is captured
/// once, when the dialog is built, and never re-derived from the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogKind {
    /// Another person. `is_self` marks Saved Messages, which is you at both ends.
    User { is_self: bool },
    /// A basic group or a megagroup. The distinction is not cosmetic: the two empty their history
    /// through different calls, and only the basic group's is available to an ordinary member.
    Group { megagroup: bool },
    /// A broadcast channel.
    Channel,
}

impl DialogKind {
    pub fn of(peer: &Peer) -> Self {
        match peer {
            Peer::User(user) => DialogKind::User {
                is_self: user.is_self(),
            },
            Peer::Group(group) => DialogKind::Group {
                megagroup: group.is_megagroup(),
            },
            Peer::Channel(_) => DialogKind::Channel,
        }
    }

    /// Whether "the other side read this" is a statement worth making.
    ///
    /// A broadcast channel has readers, not a recipient — Telegram reports view counts there and
    /// never moves an outbox watermark worth believing. Saved Messages is you at both ends.
    /// Megagroups keep their ticks, which is right: they do get `updateReadChannelOutbox`.
    pub fn receipts_make_sense(self) -> bool {
        match self {
            DialogKind::Channel => false,
            DialogKind::User { is_self } => !is_self,
            DialogKind::Group { .. } => true,
        }
    }
}

/// One entry in the chat action menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogAction {
    /// Show what Telegram will say about this peer. Alone among these entries it asks nothing of
    /// the account and changes nothing on any other device — it only puts a screen up.
    Info,
    Mute,
    Unmute,
    Pin,
    Unpin,
    /// Move to (or back from) the archive, which the tab strip shows as a folder of its own.
    Archive,
    Unarchive,
    ClearHistory,
    Block,
    Unblock,
    /// Delete for a user, leave for a group or channel. One Telegram call — `delete_dialog`
    /// dispatches on peer kind itself — under three names, because "delete" would be a lie about
    /// a channel that carries on existing without you.
    DeleteOrLeave,
}

impl DialogAction {
    pub fn label(self, kind: DialogKind) -> &'static str {
        match self {
            DialogAction::Info => "Info",
            DialogAction::Mute => "Mute",
            DialogAction::Unmute => "Unmute",
            DialogAction::Pin => "Pin to top",
            DialogAction::Unpin => "Unpin",
            DialogAction::Archive => "Archive",
            DialogAction::Unarchive => "Unarchive",
            DialogAction::ClearHistory => "Clear history",
            DialogAction::Block => "Block user",
            DialogAction::Unblock => "Unblock user",
            DialogAction::DeleteOrLeave => match kind {
                DialogKind::User { .. } => "Delete chat",
                DialogKind::Group { .. } => "Leave group",
                DialogKind::Channel => "Leave channel",
            },
        }
    }

    /// Whether the action needs a yes/no before it goes out.
    ///
    /// The test is not "does it touch the server" — every action here does — but "can the user put
    /// it back from this screen". Muting, pinning and blocking are all one keystroke to reverse;
    /// leaving a private channel is not reversible at all.
    pub fn is_destructive(self) -> bool {
        matches!(
            self,
            DialogAction::ClearHistory | DialogAction::Block | DialogAction::DeleteOrLeave
        )
    }

    /// What the status banner says while the request is in flight.
    ///
    /// `None` means the entry aims the UI rather than issuing a request — the same shape
    /// `MessageAction::in_progress` already has for Reply, Edit and Forward.
    pub fn in_progress(self) -> Option<&'static str> {
        match self {
            DialogAction::Info => None,
            DialogAction::Mute => Some("muting…"),
            DialogAction::Unmute => Some("unmuting…"),
            DialogAction::Pin => Some("pinning…"),
            DialogAction::Unpin => Some("unpinning…"),
            DialogAction::Archive => Some("archiving…"),
            DialogAction::Unarchive => Some("unarchiving…"),
            DialogAction::ClearHistory => Some("clearing history…"),
            DialogAction::Block => Some("blocking…"),
            DialogAction::Unblock => Some("unblocking…"),
            DialogAction::DeleteOrLeave => Some("leaving…"),
        }
    }

    /// The question the confirmation prompt asks.
    pub fn confirm_prompt(self, kind: DialogKind, name: &str) -> String {
        match self {
            // `revoke: false` — this empties the transcript for us and leaves theirs alone.
            DialogAction::ClearHistory => format!("Clear your copy of the history with {name}?"),
            DialogAction::Block => format!("Block {name}?"),
            DialogAction::DeleteOrLeave => match kind {
                DialogKind::User { .. } => format!("Delete the chat with {name}?"),
                DialogKind::Group { .. } => format!("Leave {name}?"),
                DialogKind::Channel => format!("Leave {name}? Rejoining may not be possible."),
            },
            // Never reached — nothing else is destructive — but a panic here would be a crash in
            // the middle of the menu, and the generic question is still a true one.
            _ => format!("{} {name}?", self.label(kind)),
        }
    }
}

/// The menu a conversation offers, in the order it is drawn.
///
/// The order mirrors the official clients: the reversible things first, the ones that need a
/// confirmation last, so a mistyped `Enter` lands on "Mute" rather than "Leave channel".
pub fn actions_for(
    kind: DialogKind,
    muted: bool,
    pinned: bool,
    blocked: bool,
    archived: bool,
) -> Vec<DialogAction> {
    let mut actions = vec![
        DialogAction::Info,
        if muted {
            DialogAction::Unmute
        } else {
            DialogAction::Mute
        },
        if pinned {
            DialogAction::Unpin
        } else {
            DialogAction::Pin
        },
        if archived {
            DialogAction::Unarchive
        } else {
            DialogAction::Archive
        },
    ];

    // A broadcast channel's history is the channel's, not yours, and there is no copy of your own
    // to empty. A megagroup has one, but it is cleared through `channels.deleteHistory`, which
    // wants admin rights and deletes for everyone — a different and much larger operation than the
    // one this entry promises, so it is not offered either.
    if matches!(
        kind,
        DialogKind::User { .. } | DialogKind::Group { megagroup: false }
    ) {
        actions.push(DialogAction::ClearHistory);
    }

    // Blocking is about a person. There is nobody to block in a group or channel, and blocking
    // yourself is not a thing Saved Messages supports.
    if matches!(kind, DialogKind::User { is_self: false }) {
        actions.push(if blocked {
            DialogAction::Unblock
        } else {
            DialogAction::Block
        });
    }

    actions.push(DialogAction::DeleteOrLeave);
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(kind: DialogKind, muted: bool, pinned: bool, blocked: bool) -> Vec<&'static str> {
        actions_for(kind, muted, pinned, blocked, false)
            .into_iter()
            .map(|action| action.label(kind))
            .collect()
    }

    const PERSON: DialogKind = DialogKind::User { is_self: false };
    const SAVED: DialogKind = DialogKind::User { is_self: true };
    const BASIC_GROUP: DialogKind = DialogKind::Group { megagroup: false };
    const MEGAGROUP: DialogKind = DialogKind::Group { megagroup: true };

    #[test]
    fn a_private_chat_offers_the_full_set_including_blocking() {
        assert_eq!(
            labels(PERSON, false, false, false),
            [
                "Info",
                "Mute",
                "Pin to top",
                "Archive",
                "Clear history",
                "Block user",
                "Delete chat",
            ]
        );
    }

    #[test]
    fn a_group_is_left_rather_than_deleted_and_has_nobody_to_block() {
        assert_eq!(
            labels(BASIC_GROUP, false, false, false),
            [
                "Info",
                "Mute",
                "Pin to top",
                "Archive",
                "Clear history",
                "Leave group",
            ],
            "a group has no single counterpart, so a Block entry would have no target"
        );
    }

    /// A megagroup looks like a group and is left like one, but "clear history" there is
    /// `channels.deleteHistory` — admin-only and destructive for everybody — so it is not the same
    /// entry and is not offered under the same name.
    #[test]
    fn a_megagroup_is_left_like_a_group_but_offers_no_clear_history() {
        assert_eq!(
            labels(MEGAGROUP, false, false, false),
            ["Info", "Mute", "Pin to top", "Archive", "Leave group",]
        );
    }

    #[test]
    fn a_channel_is_left_and_has_no_history_of_yours_to_clear() {
        assert_eq!(
            labels(DialogKind::Channel, false, false, false),
            ["Info", "Mute", "Pin to top", "Archive", "Leave channel",],
            "the history in a broadcast channel belongs to the channel; there is no copy of it \
             that clearing could empty"
        );
    }

    #[test]
    fn saved_messages_cannot_be_blocked_because_it_is_you_at_both_ends() {
        assert!(
            !labels(SAVED, false, false, false).contains(&"Block user"),
            "blocking yourself is not an operation Telegram has"
        );
    }

    #[test]
    fn every_toggle_shows_the_way_out_of_the_state_it_is_in() {
        let on = labels(PERSON, true, true, true);
        assert!(on.contains(&"Unmute") && on.contains(&"Unpin") && on.contains(&"Unblock user"));

        let off = labels(PERSON, false, false, false);
        assert!(
            off.contains(&"Mute") && off.contains(&"Pin to top") && off.contains(&"Block user")
        );
    }

    #[test]
    fn a_toggle_never_offers_both_of_its_faces_at_once() {
        for (muted, pinned, blocked) in [(false, false, false), (true, true, true)] {
            let menu = actions_for(PERSON, muted, pinned, blocked, false);
            let mut seen = menu.clone();
            seen.dedup();
            assert_eq!(seen.len(), menu.len(), "duplicate entry in {menu:?}");
            assert_eq!(
                menu.len(),
                7,
                "the private-chat menu is seven entries whichever way the toggles sit: {menu:?}"
            );
        }
    }

    #[test]
    fn only_the_actions_that_cannot_be_undone_from_this_screen_ask_first() {
        for action in [
            DialogAction::ClearHistory,
            DialogAction::Block,
            DialogAction::DeleteOrLeave,
        ] {
            assert!(action.is_destructive(), "{action:?} must be confirmed");
        }
        for action in [
            DialogAction::Mute,
            DialogAction::Unmute,
            DialogAction::Pin,
            DialogAction::Unpin,
            DialogAction::Unblock,
        ] {
            assert!(
                !action.is_destructive(),
                "{action:?} is one keystroke to reverse, so a prompt would only be in the way"
            );
        }
    }

    /// Archiving used to be the one action with no way back, and its label said so. Now that the
    /// tab strip has an Archive folder it is a toggle like the others, and gating it would be in
    /// the way of a keystroke the very next menu undoes.
    #[test]
    fn archiving_is_a_toggle_that_the_archive_tab_can_undo() {
        assert!(!DialogAction::Archive.is_destructive());
        assert!(!DialogAction::Unarchive.is_destructive());

        let filed = actions_for(PERSON, false, false, false, true);
        assert!(filed.contains(&DialogAction::Unarchive));
        assert!(
            !filed.contains(&DialogAction::Archive),
            "a chat already in the archive has nowhere to be archived to"
        );
    }

    #[test]
    fn a_leave_prompt_names_the_chat_and_the_channel_one_warns_about_coming_back() {
        assert_eq!(
            DialogAction::DeleteOrLeave.confirm_prompt(BASIC_GROUP, "Rust Users"),
            "Leave Rust Users?"
        );
        assert!(
            DialogAction::DeleteOrLeave
                .confirm_prompt(DialogKind::Channel, "Rust Blog")
                .contains("Rejoining")
        );
        assert_eq!(
            DialogAction::DeleteOrLeave.confirm_prompt(PERSON, "Alice"),
            "Delete the chat with Alice?"
        );
    }

    #[test]
    fn receipts_are_suppressed_where_they_would_mean_nothing() {
        assert!(PERSON.receipts_make_sense());
        assert!(BASIC_GROUP.receipts_make_sense());
        assert!(MEGAGROUP.receipts_make_sense());
        assert!(
            !SAVED.receipts_make_sense(),
            "Saved Messages is you at both ends"
        );
        assert!(
            !DialogKind::Channel.receipts_make_sense(),
            "a broadcast channel has readers, not a recipient"
        );
    }

    #[test]
    fn every_conversation_offers_info_and_offers_it_first() {
        for kind in [PERSON, SAVED, BASIC_GROUP, MEGAGROUP, DialogKind::Channel] {
            let menu = actions_for(kind, false, false, false, false);
            assert_eq!(
                menu.first(),
                Some(&DialogAction::Info),
                "the order runs reversible-first and destructive-last so a mistyped Enter lands \
                 somewhere harmless; Info changes nothing at all, so it belongs at the top"
            );
        }
    }

    #[test]
    fn info_is_the_one_entry_that_asks_nothing_of_the_server() {
        assert_eq!(
            DialogAction::Info.in_progress(),
            None,
            "every other entry is a request whose progress the banner narrates; Info only puts a \
             screen up, so a banner would be reporting work that is not happening"
        );
        assert!(!DialogAction::Info.is_destructive());

        for action in [DialogAction::Mute, DialogAction::Block, DialogAction::Pin] {
            assert!(action.in_progress().is_some());
        }
    }
}
