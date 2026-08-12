//! What a conversation can have done to it, and which of those a given conversation offers.
//!
//! Kept apart from [`crate::state::dialog_list`] and from `App` because it is the one part of the
//! feature with no state at all: given a kind and three flags it answers with a menu. That makes
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
    Mute,
    Unmute,
    Pin,
    Unpin,
    /// One-way from here: `iter_dialogs` asks for folder 0 only, so an archived chat is not in the
    /// list to be unarchived. See `CLAUDE.md`.
    Archive,
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
            DialogAction::Mute => "Mute",
            DialogAction::Unmute => "Unmute",
            DialogAction::Pin => "Pin to top",
            DialogAction::Unpin => "Unpin",
            // The parenthetical is not decoration: there is no archive view to bring it back from.
            DialogAction::Archive => "Archive (hides the chat)",
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
    /// Not the label: "Archive (hides the chat)…" would be a strange thing to watch happen, and
    /// the parenthetical has already done its job by the time the user picks it.
    pub fn in_progress(self) -> &'static str {
        match self {
            DialogAction::Mute => "muting…",
            DialogAction::Unmute => "unmuting…",
            DialogAction::Pin => "pinning…",
            DialogAction::Unpin => "unpinning…",
            DialogAction::Archive => "archiving…",
            DialogAction::ClearHistory => "clearing history…",
            DialogAction::Block => "blocking…",
            DialogAction::Unblock => "unblocking…",
            DialogAction::DeleteOrLeave => "leaving…",
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
) -> Vec<DialogAction> {
    let mut actions = vec![
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
        DialogAction::Archive,
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
        actions_for(kind, muted, pinned, blocked)
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
                "Mute",
                "Pin to top",
                "Archive (hides the chat)",
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
                "Mute",
                "Pin to top",
                "Archive (hides the chat)",
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
            [
                "Mute",
                "Pin to top",
                "Archive (hides the chat)",
                "Leave group",
            ]
        );
    }

    #[test]
    fn a_channel_is_left_and_has_no_history_of_yours_to_clear() {
        assert_eq!(
            labels(DialogKind::Channel, false, false, false),
            [
                "Mute",
                "Pin to top",
                "Archive (hides the chat)",
                "Leave channel",
            ],
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
            let menu = actions_for(PERSON, muted, pinned, blocked);
            let mut seen = menu.clone();
            seen.dedup();
            assert_eq!(seen.len(), menu.len(), "duplicate entry in {menu:?}");
            assert_eq!(
                menu.len(),
                6,
                "the private-chat menu is six entries whichever way the toggles sit: {menu:?}"
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

    /// Archiving is not reversible from this UI — there is no archive view — so despite being
    /// harmless on the server it is the one non-undoable action that is *not* gated. Pinned here
    /// so the decision is deliberate rather than an oversight.
    #[test]
    fn archiving_is_not_gated_but_says_what_it_does_in_its_label() {
        assert!(!DialogAction::Archive.is_destructive());
        assert!(
            DialogAction::Archive.label(PERSON).contains("hides"),
            "with no confirmation and no way back, the label is the only warning there is"
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
}
