//! The user's own chat folders, and the rule that decides what falls into one.
//!
//! Telegram folders are not a server-side collection. `messages.getDialogFilters` hands back
//! *rules* — a few type flags plus explicit include and exclude lists — and every client evaluates
//! them over the dialog list it already has. There is no request that answers "the chats in Work",
//! which is why this module is a predicate rather than another cursor.
//!
//! Kept apart from [`crate::state::dialog_list`] for the same reason
//! [`crate::state::dialog_actions`] is: given a rule and a row it answers yes or no with no state
//! at all, so the table below — which is the thing a reader actually wants to check — is testable
//! on its own.

use grammers_client::tl;
use grammers_session::types::{PeerId, PeerRef};

use crate::state::dialog_actions::DialogKind;
use crate::state::dialog_list::DialogSummary;

/// One of the account's folders, as a tab and the rule behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    pub title: String,
    pub rule: FolderRule,
}

/// Which conversations a folder gathers.
///
/// The five type flags are additive — a folder with `groups` and `broadcasts` set takes both — and
/// the three `exclude_*` flags then subtract from whatever they let through. `include` and
/// `exclude` name individual chats and outrank the flags entirely; see [`matches`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderRule {
    pub contacts: bool,
    pub non_contacts: bool,
    pub groups: bool,
    pub broadcasts: bool,
    pub bots: bool,
    pub exclude_muted: bool,
    pub exclude_read: bool,
    pub exclude_archived: bool,
    /// `pinned_peers` and `include_peers` together: both name a chat that belongs here whatever
    /// the flags say, and tgtui does not reorder a folder by its pins.
    pub include: Vec<PeerId>,
    pub exclude: Vec<PeerId>,
}

impl Folder {
    /// Translate one filter from `messages.getDialogFilters`, or `None` for a row that is not a
    /// folder of the user's own.
    pub fn from_raw(filter: &tl::enums::DialogFilter) -> Option<Self> {
        match filter {
            tl::enums::DialogFilter::Filter(filter) => Some(Self {
                title: title_text(&filter.title),
                rule: FolderRule {
                    contacts: filter.contacts,
                    non_contacts: filter.non_contacts,
                    groups: filter.groups,
                    broadcasts: filter.broadcasts,
                    bots: filter.bots,
                    exclude_muted: filter.exclude_muted,
                    exclude_read: filter.exclude_read,
                    exclude_archived: filter.exclude_archived,
                    include: peer_ids(&filter.pinned_peers, &filter.include_peers),
                    exclude: peer_ids(&filter.exclude_peers, &[]),
                },
            }),
            // A folder joined through a shared link. It carries no type flags and no exclusions at
            // all: its membership is exactly the list of chats the link named.
            tl::enums::DialogFilter::Chatlist(filter) => Some(Self {
                title: title_text(&filter.title),
                rule: FolderRule {
                    include: peer_ids(&filter.pinned_peers, &filter.include_peers),
                    ..FolderRule::default()
                },
            }),
            // Not a folder: it marks where "All Chats" sits in the account's tab order, and that
            // tab already exists as `FolderTab::Main`.
            tl::enums::DialogFilter::Default => None,
        }
    }
}

/// Whether a conversation belongs in a folder.
///
/// Order matters, and mirrors what the official clients do: a chat named in `exclude` is out no
/// matter what, a chat named in `include` is in no matter what else, and only then do the flags
/// get a say. Getting those first two the other way round would make a folder that says "no muted
/// chats" quietly drop a chat the user added to it by hand.
pub fn matches(rule: &FolderRule, item: &DialogSummary) -> bool {
    if rule.exclude.contains(&item.peer.id) {
        return false;
    }
    if rule.include.contains(&item.peer.id) {
        return true;
    }
    if rule.exclude_archived && item.archived {
        return false;
    }
    if rule.exclude_muted && item.muted {
        return false;
    }
    // "Unread" here is tgtui's own badge, which counts from the last time this client opened the
    // chat — the same number the row shows. Nothing else would agree with what is on screen.
    if rule.exclude_read && item.unread == 0 {
        return false;
    }

    match item.kind {
        // A bot is a user, so this has to come first or a `contacts`-only folder would collect
        // every bot the account has ever talked to.
        DialogKind::User { .. } if item.bot => rule.bots,
        DialogKind::User { .. } if item.contact => rule.contacts,
        DialogKind::User { .. } => rule.non_contacts,
        DialogKind::Group { .. } => rule.groups,
        DialogKind::Channel => rule.broadcasts,
    }
}

/// A folder's name. Custom emoji in the title are entities over this text, and a terminal has
/// nothing to draw them with, so only the text survives.
fn title_text(title: &tl::enums::TextWithEntities) -> String {
    let tl::enums::TextWithEntities::Entities(title) = title;
    title.text.clone()
}

/// The identities behind two lists of `InputPeer`, which is the only form a filter names them in.
fn peer_ids(first: &[tl::enums::InputPeer], second: &[tl::enums::InputPeer]) -> Vec<PeerId> {
    first
        .iter()
        .chain(second)
        .map(|peer| PeerRef::from(peer).id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{channel_dialog, dialog, group_dialog};

    fn rule() -> FolderRule {
        FolderRule::default()
    }

    #[test]
    fn a_folder_of_groups_takes_groups_and_nothing_else() {
        let rule = FolderRule {
            groups: true,
            ..rule()
        };
        assert!(matches(&rule, &group_dialog(1, "Rust Users")));
        assert!(!matches(&rule, &dialog(2, "Alice")));
        assert!(!matches(&rule, &channel_dialog(3, "Rust Blog")));
    }

    #[test]
    fn contacts_and_non_contacts_split_the_same_users_between_them() {
        let alice = DialogSummary {
            contact: true,
            ..dialog(1, "Alice")
        };
        let stranger = dialog(2, "Unknown");

        let contacts = FolderRule {
            contacts: true,
            ..rule()
        };
        assert!(matches(&contacts, &alice));
        assert!(!matches(&contacts, &stranger));

        let others = FolderRule {
            non_contacts: true,
            ..rule()
        };
        assert!(!matches(&others, &alice));
        assert!(matches(&others, &stranger));
    }

    /// A bot is a `user` on the wire, so without the bot arm taking precedence every folder of
    /// contacts or non-contacts would silently collect bots too.
    #[test]
    fn a_bot_answers_to_the_bot_flag_rather_than_the_user_ones() {
        let bot = DialogSummary {
            bot: true,
            ..dialog(1, "GitHub")
        };

        assert!(matches(
            &FolderRule {
                bots: true,
                ..rule()
            },
            &bot
        ));
        assert!(
            !matches(
                &FolderRule {
                    non_contacts: true,
                    ..rule()
                },
                &bot
            ),
            "a bot must not fall into a folder that only asked for people"
        );
    }

    #[test]
    fn a_named_chat_is_in_the_folder_whatever_its_type_flags_say() {
        let alice = dialog(1, "Alice");
        let rule = FolderRule {
            groups: true,
            include: vec![alice.peer.id],
            ..rule()
        };
        assert!(
            matches(&rule, &alice),
            "a chat added to a folder by hand belongs to it even though it is not a group"
        );
    }

    #[test]
    fn an_exclusion_beats_an_inclusion() {
        let alice = dialog(1, "Alice");
        let rule = FolderRule {
            contacts: true,
            include: vec![alice.peer.id],
            exclude: vec![alice.peer.id],
            ..rule()
        };
        assert!(
            !matches(&rule, &alice),
            "the exclude list is the last word; nothing may put a chat back"
        );
    }

    #[test]
    fn a_named_chat_survives_the_exclude_flags_that_would_otherwise_drop_it() {
        let alice = DialogSummary {
            muted: true,
            unread: 0,
            archived: true,
            ..dialog(1, "Alice")
        };
        let rule = FolderRule {
            exclude_muted: true,
            exclude_read: true,
            exclude_archived: true,
            include: vec![alice.peer.id],
            ..rule()
        };
        assert!(
            matches(&rule, &alice),
            "a folder the user put this chat in must not drop it for being quiet"
        );
    }

    #[test]
    fn the_exclude_flags_subtract_from_what_the_type_flags_let_through() {
        let base = FolderRule {
            contacts: true,
            non_contacts: true,
            ..rule()
        };
        let alice = dialog(1, "Alice");

        let muted = DialogSummary {
            muted: true,
            ..alice.clone()
        };
        assert!(matches(&base, &muted));
        assert!(!matches(
            &FolderRule {
                exclude_muted: true,
                ..base.clone()
            },
            &muted
        ));

        let read = DialogSummary { unread: 0, ..alice };
        assert!(matches(&base, &read));
        assert!(!matches(
            &FolderRule {
                exclude_read: true,
                ..base.clone()
            },
            &read
        ));
    }

    /// Archived chats are in the same pool as everything else, so a folder that says nothing about
    /// the archive collects them — which is what the official clients do too.
    #[test]
    fn an_archived_chat_is_only_kept_out_of_a_folder_that_asks_for_that() {
        let archived = DialogSummary {
            archived: true,
            ..dialog(1, "Old friend")
        };
        let base = FolderRule {
            non_contacts: true,
            ..rule()
        };

        assert!(matches(&base, &archived));
        assert!(!matches(
            &FolderRule {
                exclude_archived: true,
                ..base
            },
            &archived
        ));
    }

    #[test]
    fn an_empty_rule_gathers_nothing() {
        assert!(
            !matches(&rule(), &dialog(1, "Alice")),
            "a folder that named no types and no chats has no members"
        );
    }

    // -- reading the filters off the wire ------------------------------------

    fn title(text: &str) -> tl::enums::TextWithEntities {
        tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities {
            text: text.to_string(),
            entities: Vec::new(),
        })
    }

    /// The `InputPeer` form a filter names a chat in, which is also the only form carrying the
    /// access hash that makes the id resolvable.
    fn input_user(id: i64) -> tl::enums::InputPeer {
        tl::enums::InputPeer::User(tl::types::InputPeerUser {
            user_id: id,
            access_hash: id,
        })
    }

    fn raw_filter() -> tl::types::DialogFilter {
        tl::types::DialogFilter {
            contacts: false,
            non_contacts: false,
            groups: false,
            broadcasts: false,
            bots: false,
            exclude_muted: false,
            exclude_read: false,
            exclude_archived: false,
            title_noanimate: false,
            id: 2,
            title: title("Work"),
            emoticon: None,
            color: None,
            pinned_peers: Vec::new(),
            include_peers: Vec::new(),
            exclude_peers: Vec::new(),
        }
    }

    #[test]
    fn a_filter_becomes_a_tab_with_its_rule_intact() {
        let folder = Folder::from_raw(&tl::enums::DialogFilter::Filter(tl::types::DialogFilter {
            groups: true,
            exclude_muted: true,
            pinned_peers: vec![input_user(1)],
            include_peers: vec![input_user(2)],
            exclude_peers: vec![input_user(3)],
            ..raw_filter()
        }))
        .expect("a filter of the user's own is a tab");

        assert_eq!(folder.title, "Work");
        assert!(folder.rule.groups && folder.rule.exclude_muted);
        assert_eq!(
            folder.rule.include,
            vec![dialog(1, "").peer.id, dialog(2, "").peer.id],
            "pinned and included chats are both simply members; tgtui does not reorder by pin"
        );
        assert_eq!(folder.rule.exclude, vec![dialog(3, "").peer.id]);
    }

    /// A folder joined through a shared link carries members and nothing else, so reading it as a
    /// full filter would give it every flag unset — which is the same thing, but only by accident.
    #[test]
    fn a_shared_folder_is_read_as_the_list_of_chats_the_link_named() {
        let folder = Folder::from_raw(&tl::enums::DialogFilter::Chatlist(
            tl::types::DialogFilterChatlist {
                has_my_invites: false,
                title_noanimate: false,
                id: 3,
                title: title("Rustaceans"),
                emoticon: None,
                color: None,
                pinned_peers: vec![input_user(1)],
                include_peers: vec![input_user(2)],
            },
        ))
        .expect("a chatlist is a tab too");

        assert_eq!(folder.title, "Rustaceans");
        assert_eq!(folder.rule.include.len(), 2);
        assert!(folder.rule.exclude.is_empty());
        assert_eq!(
            folder.rule,
            FolderRule {
                include: folder.rule.include.clone(),
                ..rule()
            }
        );
    }

    #[test]
    fn the_all_chats_marker_is_not_a_folder() {
        assert!(
            Folder::from_raw(&tl::enums::DialogFilter::Default).is_none(),
            "`dialogFilterDefault` marks where All Chats sits in the order, and that tab already \
             exists as `FolderTab::Main`"
        );
    }
}
