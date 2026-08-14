//! What one message can have done to it, and which of those a given message offers.
//!
//! The sibling of [`crate::state::dialog_actions`], and deliberately the same shape: a pure
//! function over a kind and a flag, with the presentation hanging off the enum. The table below is
//! the thing a reader wants to check, so it is testable without an `App`.
//!
//! One difference from the chat menu is worth knowing before reading `App::run_message_action`:
//! there, every entry becomes a `TgCommand`. Here only the two deletes do. Reply and Edit prime the
//! compose box and send nothing until the user presses Enter, and Forward opens a second modal to
//! ask where. So these are not all "the account changes when you pick this".

use crate::state::dialog_actions::DialogKind;

/// One entry in the message action menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageAction {
    Reply,
    Edit,
    Forward,
    /// Drop our copy and leave everyone else's alone. `messages.deleteMessages` with `revoke`
    /// unset — which is *not* what grammers' `delete_messages` sends, so this one is raw.
    DeleteForMe,
    /// Unsend: the message goes for both sides. Telegram's own time limit applies and is the
    /// server's to enforce.
    DeleteForEveryone,
}

impl MessageAction {
    pub fn label(self) -> &'static str {
        match self {
            MessageAction::Reply => "Reply",
            MessageAction::Edit => "Edit",
            MessageAction::Forward => "Forward to…",
            MessageAction::DeleteForMe => "Delete for me",
            MessageAction::DeleteForEveryone => "Delete for everyone",
        }
    }

    /// Whether the action needs a yes/no before it goes out.
    ///
    /// Same test as the chat menu's: not "does it touch the server" but "can the user put it back
    /// from this screen". An edit is undone by editing again; a deleted message is gone.
    pub fn is_destructive(self) -> bool {
        matches!(
            self,
            MessageAction::DeleteForMe | MessageAction::DeleteForEveryone
        )
    }

    /// What the status banner says while the request is in flight, for the two entries that make
    /// one. Reply and Edit have nothing in flight yet, and Forward is still asking where.
    pub fn in_progress(self) -> Option<&'static str> {
        match self {
            MessageAction::DeleteForMe | MessageAction::DeleteForEveryone => Some("deleting…"),
            _ => None,
        }
    }

    /// The question the confirmation prompt asks.
    pub fn confirm_prompt(self) -> String {
        match self {
            MessageAction::DeleteForMe => "Delete this message from your copy of the chat?".into(),
            MessageAction::DeleteForEveryone => {
                "Delete this message for everyone in the chat?".into()
            }
            // Never reached — nothing else is destructive — but a panic here would be a crash in
            // the middle of the menu, and the generic question is still a true one.
            other => format!("{}?", other.label()),
        }
    }
}

/// The menu a message offers, in the order it is drawn.
///
/// Same ordering rule as the chat menu: the harmless entries first and the ones that ask a question
/// last, so a mistyped `Enter` lands on "Reply" rather than on a delete.
pub fn actions_for(kind: DialogKind, outgoing: bool) -> Vec<MessageAction> {
    let mut actions = vec![MessageAction::Reply];

    // Ownership is the part that can be known here. The 48-hour window is Telegram's and is left
    // to the server, which answers `MESSAGE_EDIT_TIME_EXPIRED` into the status banner — hiding the
    // entry on a timer would mean keeping one, and getting the boundary wrong in the safe
    // direction still means an entry that lies.
    if outgoing {
        actions.push(MessageAction::Edit);
    }

    // Offered everywhere. A chat with `noforwards` set refuses server-side, and there is no flag
    // on the dialog row that says so in advance.
    actions.push(MessageAction::Forward);

    // Channels and megagroups delete through `channels.deleteMessages`, which has no `revoke` flag
    // at all: every delete there is for everyone. An entry promising a local-only delete would be
    // describing something Telegram does not have. Same reasoning as `ClearHistory`'s omission in
    // `dialog_actions`.
    if matches!(
        kind,
        DialogKind::User { .. } | DialogKind::Group { megagroup: false }
    ) {
        actions.push(MessageAction::DeleteForMe);
    }

    // Your own message is always yours to unsend. Someone else's is an admin's to remove, and
    // whether this account is an admin is not on the dialog row — so it is offered in the places
    // where it is possible and a `MESSAGE_DELETE_FORBIDDEN` becomes a banner.
    if outgoing || matches!(kind, DialogKind::Group { .. } | DialogKind::Channel) {
        actions.push(MessageAction::DeleteForEveryone);
    }

    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(kind: DialogKind, outgoing: bool) -> Vec<&'static str> {
        actions_for(kind, outgoing)
            .into_iter()
            .map(MessageAction::label)
            .collect()
    }

    const PERSON: DialogKind = DialogKind::User { is_self: false };
    const BASIC_GROUP: DialogKind = DialogKind::Group { megagroup: false };
    const MEGAGROUP: DialogKind = DialogKind::Group { megagroup: true };

    #[test]
    fn your_own_message_in_a_private_chat_offers_the_full_set() {
        assert_eq!(
            labels(PERSON, true),
            [
                "Reply",
                "Edit",
                "Forward to…",
                "Delete for me",
                "Delete for everyone",
            ]
        );
    }

    #[test]
    fn only_your_own_messages_can_be_edited() {
        assert!(labels(PERSON, true).contains(&"Edit"));
        assert!(
            !labels(PERSON, false).contains(&"Edit"),
            "Telegram has no way to edit somebody else's message, admin or not"
        );
    }

    #[test]
    fn someone_elses_message_in_a_private_chat_can_only_go_from_your_own_copy() {
        let theirs = labels(PERSON, false);
        assert!(theirs.contains(&"Delete for me"));
        assert!(
            !theirs.contains(&"Delete for everyone"),
            "unsending is for messages you sent; there is nobody whose message this is but theirs"
        );
    }

    /// `channels.deleteMessages` takes no `revoke` flag — a delete in a channel or megagroup is
    /// always for everyone. Offering a local-only one would name an operation that does not exist.
    #[test]
    fn a_channel_offers_no_delete_for_me_because_there_is_no_such_thing_there() {
        for kind in [DialogKind::Channel, MEGAGROUP] {
            assert!(
                !labels(kind, true).contains(&"Delete for me"),
                "{kind:?} deletes through channels.deleteMessages, which has no revoke flag"
            );
        }
        assert!(
            labels(BASIC_GROUP, true).contains(&"Delete for me"),
            "a basic group goes through messages.deleteMessages, which does"
        );
    }

    /// Whether this account can moderate is not on the dialog row, so the entry is offered where
    /// moderation exists and the server is left to refuse.
    #[test]
    fn a_moderator_is_offered_the_delete_the_server_may_still_refuse() {
        assert!(labels(MEGAGROUP, false).contains(&"Delete for everyone"));
        assert!(labels(DialogKind::Channel, false).contains(&"Delete for everyone"));
    }

    #[test]
    fn every_message_can_be_replied_to_and_forwarded() {
        for kind in [PERSON, BASIC_GROUP, MEGAGROUP, DialogKind::Channel] {
            for outgoing in [true, false] {
                let menu = labels(kind, outgoing);
                assert!(menu.contains(&"Reply"), "{kind:?} outgoing={outgoing}");
                assert!(
                    menu.contains(&"Forward to…"),
                    "{kind:?} outgoing={outgoing}"
                );
            }
        }
    }

    #[test]
    fn only_the_actions_that_cannot_be_undone_ask_first() {
        for action in [MessageAction::DeleteForMe, MessageAction::DeleteForEveryone] {
            assert!(action.is_destructive(), "{action:?} must be confirmed");
            assert!(
                action.in_progress().is_some(),
                "{action:?} goes straight to the network, so the banner has to narrate it"
            );
        }
        for action in [
            MessageAction::Reply,
            MessageAction::Edit,
            MessageAction::Forward,
        ] {
            assert!(
                !action.is_destructive(),
                "{action:?} changes nothing on its own — it only readies the next keystroke"
            );
            assert!(
                action.in_progress().is_none(),
                "{action:?} has nothing in flight to narrate"
            );
        }
    }

    #[test]
    fn a_menu_never_offers_the_same_entry_twice() {
        for kind in [PERSON, BASIC_GROUP, MEGAGROUP, DialogKind::Channel] {
            for outgoing in [true, false] {
                let menu = actions_for(kind, outgoing);
                let mut seen = menu.clone();
                seen.sort_by_key(|action| action.label());
                seen.dedup();
                assert_eq!(seen.len(), menu.len(), "duplicate entry in {menu:?}");
            }
        }
    }

    #[test]
    fn the_two_delete_prompts_say_which_copies_go() {
        assert!(
            MessageAction::DeleteForMe.confirm_prompt().contains("your"),
            "the difference between the two is whose copy goes, so the question has to say"
        );
        assert!(
            MessageAction::DeleteForEveryone
                .confirm_prompt()
                .contains("everyone")
        );
    }
}
