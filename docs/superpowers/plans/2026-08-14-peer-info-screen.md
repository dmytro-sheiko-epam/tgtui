# Peer Info Screen Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a full-screen, read-only profile of the selected conversation, opened from an `Info` entry in the existing `Ctrl+A` chat action menu.

**Architecture:** The standard four-step shape this app uses for anything that talks to Telegram — new `TgCommand` → actor arm → new `TgEvent` → `App::handle_event` arm → render. The fetched profile lives in one `Option` on `App`, is fetched fresh on every open, and is dropped on close; there is no profile cache. Flattening the three TL responses into something drawable is a pure function in `state::peer_info`, so the table (and the fields deliberately left out) is testable with no app and no network.

**Tech Stack:** Rust, `grammers-client` / `grammers-session` / `grammers-tl-types` 0.10 (pinned with `=`), `ratatui`, `ratatui-image`, `tokio`, `chrono`.

**Spec:** `docs/superpowers/specs/2026-08-14-peer-info-design.md`

## Global Constraints

- Every test lives in a `#[cfg(test)] mod tests` in the same file as the code it covers. There is no `tests/` directory and none may be created.
- Tests are named as full sentences describing the behaviour they protect (`a_peerless_deletion_leaves_channels_alone`), and assert with a message explaining what would break.
- Comments explain the *why* — a non-obvious constraint, ordering requirement, or protocol quirk. Never the *what*.
- `App` is entirely synchronous: it never awaits and never does I/O. Anything needing the network is a `TgCommand` pushed down a channel.
- Only `src/telegram/` may touch `grammers_client::Client`.
- `ui::` is a pure function of `App`, rebuilt from scratch every frame.
- grammers 0.10 wraps none of the three full-info calls, so all three are raw `client.invoke`s. This is the established path here (`messages.getDialogFilters`, `contacts.block`, the hand-rolled archive `messages.getDialogs`).
- `PeerKind`'s variants are `User`, `Chat` (a basic group), `Channel` (both broadcast channels and megagroups). It is *not* `Group`.
- Run `cargo fmt` and `cargo clippy --all-targets` before every commit. Clippy must be clean.
- The full suite is `cargo test`; it needs no network.

## File Structure

| File | Responsibility |
| --- | --- |
| `src/state/peer_info.rs` | **New.** `PeerInfo` / `InfoRow`, and the three pure flatteners from TL. Holds no state. |
| `src/state/media.rs` | Gains `avatar_ref`, sharing `pick_thumb` with `photo_ref`. |
| `src/state/mod.rs` | Declares the new module. |
| `src/ui/images.rs` | Cache key widens from a bare message id to `ImageKey`. |
| `src/ui/chat_view.rs`, `src/ui/photo_view.rs` | Call sites updated for `ImageKey`. |
| `src/telegram/commands.rs` | `LoadPeerInfo`, `DownloadAvatar`. |
| `src/telegram/events.rs` | `PeerInfoLoaded`, `AvatarLoaded`. |
| `src/telegram/mod.rs` | Actor arms: three-way dispatch on `PeerKind`, and the avatar download. |
| `src/state/dialog_actions.rs` | `DialogAction::Info`; `in_progress` becomes `Option`. |
| `src/app.rs` | `PeerInfoView` / `InfoState`, the reducers, key routing, `forget_dialog`. |
| `src/ui/peer_view.rs` | **New.** Draws the profile. |
| `src/ui/mod.rs` | Declares the module and adds the `draw` arm. |
| `src/test_support.rs` | Fixtures for a `UserFull` and a decoded avatar. |
| `CLAUDE.md` | Scope, Keys, modal ordering, invariants. |

---

### Task 1: The info table

Pure flatteners with no state and no wiring. Nothing else in the app references this yet — it compiles and tests on its own.

**Files:**
- Create: `src/state/peer_info.rs`
- Modify: `src/state/mod.rs`

**Interfaces:**
- Consumes: `crate::state::media::{PhotoRef, avatar_ref}` — `avatar_ref` does not exist yet, so **Task 1 must not call it**. The `avatar` field is populated as `None` here and filled in by Task 2.
- Produces:
  - `pub struct PeerInfo { pub subtitle: Vec<String>, pub about: Option<String>, pub rows: Vec<InfoRow>, pub avatar: Option<PhotoRef> }`
  - `pub struct InfoRow { pub label: &'static str, pub value: String }`
  - `pub fn user(full: &tl::types::UserFull, user: Option<&tl::types::User>) -> PeerInfo`
  - `pub fn chat(full: &tl::types::ChatFull) -> PeerInfo`
  - `pub fn channel(full: &tl::types::ChannelFull) -> PeerInfo`

- [ ] **Step 1: Create the module with its types and helpers**

Create `src/state/peer_info.rs`:

```rust
//! What Telegram will say about one peer, flattened into something a screen can draw.
//!
//! Pure, like [`crate::state::dialog_actions`] and for the same reason: the interesting part of
//! this feature is *which* fields a kind shows and which it does not, and keeping it out of `App`
//! is what makes that table checkable on its own.
//!
//! Three TL shapes arrive here — `userFull`, `chatFull` and `channelFull` — and one shape leaves.
//! A row whose field the server did not send is never pushed, so no screen shows a label with
//! nothing beside it.

use chrono::{DateTime, Local, TimeZone, Utc};
use grammers_client::tl;

use crate::state::media::PhotoRef;

/// A profile, ready to draw.
#[derive(Debug, Default)]
pub struct PeerInfo {
    /// Lines under the name: the handle and badges, then presence or member counts.
    pub subtitle: Vec<String>,
    /// Bio or channel description. Kept apart from `rows` because it wraps as a paragraph rather
    /// than sitting in a label column.
    pub about: Option<String>,
    pub rows: Vec<InfoRow>,
    /// The current profile picture. `None` when the peer has none, or has `photoEmpty`.
    pub avatar: Option<PhotoRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoRow {
    pub label: &'static str,
    pub value: String,
}

impl PeerInfo {
    /// Push a row, or nothing at all when the server did not send the field.
    ///
    /// Every optional field goes through this rather than being pushed conditionally at the call
    /// site: a label with an empty value beside it reads as data the peer does not have, which is
    /// a different and wrong statement.
    fn push(&mut self, label: &'static str, value: Option<String>) {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            self.rows.push(InfoRow { label, value });
        }
    }

    fn subtitle_line(&mut self, parts: Vec<String>) {
        if !parts.is_empty() {
            self.subtitle.push(parts.join(" · "));
        }
    }
}

/// Digits grouped in threes, so a channel with six figures of members is readable at a glance.
///
/// A thin space rather than a comma or a full stop: both of those mean a decimal separator
/// somewhere, and this number is never a decimal.
fn thousands(count: i32) -> String {
    let digits = count.abs().to_string();
    let mut out = String::new();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push('\u{202f}');
        }
        out.push(ch);
    }
    if count < 0 { format!("-{out}") } else { out }
}
```

Register it in `src/state/mod.rs` alongside the existing modules (`pub mod peer_info;`, in alphabetical order with the others).

- [ ] **Step 2: Write the failing tests for the user profile**

Append to `src/state/peer_info.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The two settings blobs every full-info response carries and no test here reads.
    ///
    /// Spelled out once because the generated TL types derive only `Debug`, `Clone` and
    /// `PartialEq` — there is no `Default` to fall back on.
    fn no_settings() -> tl::enums::PeerSettings {
        tl::types::PeerSettings {
            report_spam: false,
            add_contact: false,
            block_contact: false,
            share_contact: false,
            need_contacts_exception: false,
            report_geo: false,
            autoarchived: false,
            invite_members: false,
            request_chat_broadcast: false,
            business_bot_paused: false,
            business_bot_can_reply: false,
            geo_distance: None,
            request_chat_title: None,
            request_chat_date: None,
            business_bot_id: None,
            business_bot_manage_url: None,
            charge_paid_message_stars: None,
            registration_month: None,
            phone_country: None,
            name_change_date: None,
            photo_change_date: None,
        }
        .into()
    }

    fn no_notify_settings() -> tl::enums::PeerNotifySettings {
        tl::types::PeerNotifySettings {
            show_previews: None,
            silent: None,
            mute_until: None,
            ios_sound: None,
            android_sound: None,
            other_sound: None,
            stories_muted: None,
            stories_hide_sender: None,
            stories_ios_sound: None,
            stories_android_sound: None,
            stories_other_sound: None,
        }
        .into()
    }

    fn empty_user_full() -> tl::types::UserFull {
        tl::types::UserFull {
            blocked: false,
            phone_calls_available: false,
            phone_calls_private: false,
            can_pin_message: false,
            has_scheduled: false,
            video_calls_available: false,
            voice_messages_forbidden: false,
            translations_disabled: false,
            stories_pinned_available: false,
            blocked_my_stories_from: false,
            wallpaper_overridden: false,
            contact_require_premium: false,
            read_dates_private: false,
            sponsored_enabled: false,
            can_view_revenue: false,
            bot_can_manage_emoji_status: false,
            display_gifts_button: false,
            noforwards_my_enabled: false,
            noforwards_peer_enabled: false,
            unofficial_security_risk: false,
            id: 1,
            about: None,
            settings: no_settings(),
            personal_photo: None,
            profile_photo: None,
            fallback_photo: None,
            notify_settings: no_notify_settings(),
            bot_info: None,
            pinned_msg_id: None,
            common_chats_count: 0,
            folder_id: None,
            ttl_period: None,
            theme: None,
            private_forward_name: None,
            bot_group_admin_rights: None,
            bot_broadcast_admin_rights: None,
            wallpaper: None,
            stories: None,
            business_work_hours: None,
            business_location: None,
            business_greeting_message: None,
            business_away_message: None,
            business_intro: None,
            birthday: None,
            personal_channel_id: None,
            personal_channel_message: None,
            stargifts_count: None,
            starref_program: None,
            bot_verification: None,
            send_paid_messages_stars: None,
            disallowed_gifts: None,
            stars_rating: None,
            stars_my_pending_rating: None,
            stars_my_pending_rating_date: None,
            main_tab: None,
            saved_music: None,
            note: None,
            bot_manager_id: None,
        }
    }

    #[test]
    fn a_profile_never_shows_a_row_the_server_did_not_send() {
        let info = user(&empty_user_full(), None);

        assert!(
            info.rows.iter().all(|row| !row.value.is_empty()),
            "a label with nothing beside it reads as data the peer does not have, which is a \
             different and wrong statement: {:?}",
            info.rows
        );
        assert!(
            !info.rows.iter().any(|row| row.label == "Phone"),
            "a user who has not shared a phone number must show no Phone row at all"
        );
    }

    #[test]
    fn groups_in_common_are_only_worth_a_row_when_there_are_some() {
        let none = user(&empty_user_full(), None);
        assert!(!none.rows.iter().any(|row| row.label == "Groups"));

        let shared = user(
            &tl::types::UserFull {
                common_chats_count: 4,
                ..empty_user_full()
            },
            None,
        );
        assert_eq!(
            shared
                .rows
                .iter()
                .find(|row| row.label == "Groups")
                .map(|row| row.value.as_str()),
            Some("4 in common")
        );
    }

    #[test]
    fn a_bio_is_a_paragraph_rather_than_a_row() {
        let info = user(
            &tl::types::UserFull {
                about: Some("Rust, coffee, long walks.".to_string()),
                ..empty_user_full()
            },
            None,
        );

        assert_eq!(info.about.as_deref(), Some("Rust, coffee, long walks."));
        assert!(
            !info.rows.iter().any(|row| row.label == "Bio"),
            "a bio runs to several lines and wraps as a paragraph; in the label column it would \
             either be truncated or push every other value off the screen"
        );
    }

    #[test]
    fn large_counts_are_grouped_so_they_can_be_read_at_a_glance() {
        assert_eq!(thousands(7), "7");
        assert_eq!(thousands(1234), "1\u{202f}234");
        assert_eq!(thousands(1234567), "1\u{202f}234\u{202f}567");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

```sh
cargo test state::peer_info
```

Expected: FAIL to compile — `cannot find function 'user' in this scope`.

- [ ] **Step 4: Implement the user flattener**

Add to `src/state/peer_info.rs`, above the test module:

```rust
/// A private chat's profile.
///
/// Two arguments because `users.getFullUser` splits what a person is across two objects: the
/// handle, phone and badges live on the `User` in the response's `users`, and only the bio,
/// blocked flag and common-chat count live on the `UserFull`.
pub fn user(full: &tl::types::UserFull, user: Option<&tl::types::User>) -> PeerInfo {
    let mut info = PeerInfo {
        about: full.about.clone().filter(|about| !about.is_empty()),
        ..PeerInfo::default()
    };

    let mut badges: Vec<String> = Vec::new();
    if let Some(user) = user {
        if let Some(username) = handle(user) {
            badges.push(format!("@{username}"));
        }
        if user.bot {
            badges.push("bot".to_string());
        }
        if user.verified {
            badges.push("verified".to_string());
        }
        // Telegram's own warning labels. Passing them on is the whole point of having them.
        if user.scam {
            badges.push("SCAM".to_string());
        }
        if user.fake {
            badges.push("FAKE".to_string());
        }
        if user.premium {
            badges.push("premium".to_string());
        }
    }
    info.subtitle_line(badges);
    info.subtitle_line(user.and_then(|user| last_seen(user)).into_iter().collect());

    info.push("Phone", user.and_then(|user| user.phone.clone()));
    info.push("Birthday", full.birthday.as_ref().map(birthday));
    info.push(
        "Groups",
        (full.common_chats_count > 0).then(|| format!("{} in common", full.common_chats_count)),
    );
    info.push("Peer id", Some(full.id.to_string()));
    info
}

/// The handle Telegram shows, which is the first of the `usernames` list once a user has more
/// than one. The rest are aliases and saying so would cost a row to little purpose.
fn handle(user: &tl::types::User) -> Option<String> {
    if let Some(username) = user.username.as_ref().filter(|name| !name.is_empty()) {
        return Some(username.clone());
    }
    user.usernames
        .as_ref()?
        .iter()
        .map(|username| match username {
            tl::enums::Username::Username(username) => username,
        })
        .find(|username| username.active)
        .map(|username| username.username.clone())
}

/// Presence, in the resolution Telegram is willing to state it.
///
/// The three vague variants exist because the user chose to be vague; they are reported as they
/// come rather than being turned into a timestamp that would be a guess.
fn last_seen(user: &tl::types::User) -> Option<String> {
    Some(match user.status.as_ref()? {
        tl::enums::UserStatus::Online(_) => "online".to_string(),
        tl::enums::UserStatus::Offline(status) => {
            format!("last seen {}", timestamp(status.was_online))
        }
        tl::enums::UserStatus::Recently(_) => "last seen recently".to_string(),
        tl::enums::UserStatus::LastWeek(_) => "last seen within a week".to_string(),
        tl::enums::UserStatus::LastMonth(_) => "last seen within a month".to_string(),
        // A user who has hidden their presence entirely. Saying nothing is more honest than
        // saying "unknown", which reads as a failure rather than a choice.
        tl::enums::UserStatus::Empty => return None,
    })
}

fn timestamp(unix: i32) -> String {
    let utc: DateTime<Utc> = Utc.timestamp_opt(unix as i64, 0).single().unwrap_or_default();
    utc.with_timezone(&Local)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// A birthday without a year is the common case: Telegram makes the year optional on purpose.
fn birthday(birthday: &tl::enums::Birthday) -> String {
    let tl::enums::Birthday::Birthday(birthday) = birthday;
    match birthday.year {
        Some(year) => format!("{:04}-{:02}-{:02}", year, birthday.month, birthday.day),
        None => format!("{:02}-{:02}", birthday.month, birthday.day),
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

```sh
cargo test state::peer_info
```

Expected: PASS, 4 tests.

If a fixture field name is rejected, the layer has moved: read the definition in
`~/.cargo/registry/src/*/grammers-tl-types-0.10.0/tl/api.tl` (`userFull#6cbe645`,
`peerSettings#f47741f7`, `peerNotifySettings#99622c0c`) and match it. Do not reach for `..Default::default()` — the generated types derive only `Debug`, `Clone` and `PartialEq`.

- [ ] **Step 6: Write the failing tests for groups and channels**

Add to the test module in `src/state/peer_info.rs`:

```rust
    fn empty_channel_full() -> tl::types::ChannelFull {
        // Only the fields these tests read are set to anything interesting; the rest are the
        // "server sent nothing" values, which is exactly the case the omission tests need.
        tl::types::ChannelFull {
            can_view_participants: false,
            can_set_username: false,
            can_set_stickers: false,
            hidden_prehistory: false,
            can_set_location: false,
            has_scheduled: false,
            can_view_stats: false,
            blocked: false,
            can_delete_channel: false,
            antispam: false,
            participants_hidden: false,
            translations_disabled: false,
            stories_pinned_available: false,
            view_forum_as_messages: false,
            restricted_sponsored: false,
            can_view_revenue: false,
            paid_media_allowed: false,
            can_view_stars_revenue: false,
            paid_reactions_available: false,
            stargifts_available: false,
            paid_messages_available: false,
            id: 42,
            about: String::new(),
            participants_count: None,
            admins_count: None,
            kicked_count: None,
            banned_count: None,
            online_count: None,
            read_inbox_max_id: 0,
            read_outbox_max_id: 0,
            unread_count: 0,
            chat_photo: tl::enums::Photo::Empty(tl::types::PhotoEmpty { id: 0 }),
            notify_settings: no_notify_settings(),
            exported_invite: None,
            bot_info: Vec::new(),
            migrated_from_chat_id: None,
            migrated_from_max_id: None,
            pinned_msg_id: None,
            stickerset: None,
            available_min_id: None,
            folder_id: None,
            linked_chat_id: None,
            location: None,
            slowmode_seconds: None,
            slowmode_next_send_date: None,
            stats_dc: None,
            pts: 0,
            call: None,
            ttl_period: None,
            pending_suggestions: None,
            groupcall_default_join_as: None,
            theme_emoticon: None,
            requests_pending: None,
            recent_requesters: None,
            default_send_as: None,
            available_reactions: None,
            reactions_limit: None,
            stories: None,
            wallpaper: None,
            boosts_applied: None,
            boosts_unrestrict: None,
            emojiset: None,
            bot_verification: None,
            stargifts_count: None,
            send_paid_messages_stars: None,
            main_tab: None,
            guard_bot_id: None,
        }
    }

    #[test]
    fn a_channel_reports_the_counts_it_was_given_and_no_others() {
        let info = channel(&tl::types::ChannelFull {
            participants_count: Some(1234),
            about: "News and updates.".to_string(),
            ..empty_channel_full()
        });

        assert_eq!(info.about.as_deref(), Some("News and updates."));
        assert_eq!(
            info.rows
                .iter()
                .find(|row| row.label == "Members")
                .map(|row| row.value.as_str()),
            Some("1\u{202f}234")
        );
        assert!(
            !info.rows.iter().any(|row| row.label == "Online"),
            "a broadcast channel is not sent an online count, and inventing a zero would claim \
             nobody is reading it"
        );
    }

    #[test]
    fn an_invite_link_is_never_shown_because_it_is_a_credential() {
        let info = channel(&tl::types::ChannelFull {
            exported_invite: Some(
                tl::types::ChatInviteExported {
                    revoked: false,
                    permanent: true,
                    request_needed: false,
                    link: "https://t.me/+secret".to_string(),
                    admin_id: 1,
                    date: 0,
                    start_date: None,
                    expire_date: None,
                    usage_limit: None,
                    usage: None,
                    requested: None,
                    subscription_expired: None,
                    title: None,
                    subscription_pricing: None,
                }
                .into(),
            ),
            ..empty_channel_full()
        });

        assert!(
            !info
                .rows
                .iter()
                .any(|row| row.value.contains("t.me/+") || row.label == "Invite"),
            "anything that can read this screen could join the chat with that link"
        );
    }

    #[test]
    fn a_linked_chat_is_omitted_rather_than_shown_as_a_bare_id() {
        let info = channel(&tl::types::ChannelFull {
            linked_chat_id: Some(777),
            ..empty_channel_full()
        });

        assert!(
            !info.rows.iter().any(|row| row.value.contains("777")),
            "the linked chat arrives as a bare id; printing the number tells the reader nothing \
             and resolving it means a second request for a line nobody asked for"
        );
    }

    fn basic_group_full(participants: tl::enums::ChatParticipants) -> tl::types::ChatFull {
        tl::types::ChatFull {
            can_set_username: false,
            has_scheduled: false,
            translations_disabled: false,
            id: 7,
            about: String::new(),
            participants,
            chat_photo: None,
            notify_settings: no_notify_settings(),
            exported_invite: None,
            bot_info: None,
            pinned_msg_id: None,
            folder_id: None,
            call: None,
            ttl_period: None,
            groupcall_default_join_as: None,
            theme_emoticon: None,
            requests_pending: None,
            recent_requesters: None,
            available_reactions: None,
            reactions_limit: None,
        }
    }

    #[test]
    fn a_basic_group_counts_the_participant_list_it_was_sent() {
        let info = chat(&basic_group_full(
            tl::types::ChatParticipants {
                chat_id: 7,
                participants: vec![
                    tl::types::ChatParticipant {
                        user_id: 1,
                        inviter_id: 1,
                        date: 0,
                    }
                    .into(),
                    tl::types::ChatParticipant {
                        user_id: 2,
                        inviter_id: 1,
                        date: 0,
                    }
                    .into(),
                ],
                version: 1,
            }
            .into(),
        ));

        assert_eq!(
            info.subtitle,
            vec!["2 members".to_string()],
            "a basic group is small enough for the server to send every participant, so the \
             length of that list is the count rather than a field of its own"
        );
    }

    #[test]
    fn a_group_that_will_not_show_its_members_claims_no_count() {
        let info = chat(&basic_group_full(
            tl::types::ChatParticipantsForbidden {
                chat_id: 7,
                self_participant: None,
            }
            .into(),
        ));

        assert!(
            info.subtitle.is_empty(),
            "the server refused the list; a zero there would be inventing a fact about the group"
        );
    }

    #[test]
    fn slow_mode_is_stated_in_units_a_person_uses() {
        let info = channel(&tl::types::ChannelFull {
            slowmode_seconds: Some(3600),
            ..empty_channel_full()
        });

        assert_eq!(
            info.rows
                .iter()
                .find(|row| row.label == "Slow mode")
                .map(|row| row.value.as_str()),
            Some("1h")
        );
    }
```

- [ ] **Step 7: Run to verify they fail**

```sh
cargo test state::peer_info
```

Expected: FAIL to compile — `cannot find function 'channel' in this scope`.

- [ ] **Step 8: Implement the group and channel flatteners**

Add to `src/state/peer_info.rs`, above the test module:

```rust
/// A basic group's profile.
///
/// Thinner than the others by nature: `chatFull` carries a description and the participant list
/// and little else. The member count is the length of that list rather than a field, because a
/// basic group is small enough for the server to send all of them.
pub fn chat(full: &tl::types::ChatFull) -> PeerInfo {
    let mut info = PeerInfo {
        about: (!full.about.is_empty()).then(|| full.about.clone()),
        ..PeerInfo::default()
    };

    let members = match &full.participants {
        tl::enums::ChatParticipants::Participants(participants) => {
            Some(participants.participants.len())
        }
        // The server refuses the list to non-members. There is then no count to state, and
        // guessing one would be inventing a fact about the group.
        tl::enums::ChatParticipants::Forbidden(_) => None,
    };
    info.subtitle_line(
        members
            .map(|count| format!("{} members", thousands(count as i32)))
            .into_iter()
            .collect(),
    );

    info.push("Peer id", Some(full.id.to_string()));
    info
}

/// A megagroup's or broadcast channel's profile.
///
/// One function for both because `channels.getFullChannel` answers for both, and the fields that
/// only one of them has arrive as `None` on the other — so the omission rule already draws the
/// line without a kind flag being passed in.
pub fn channel(full: &tl::types::ChannelFull) -> PeerInfo {
    let mut info = PeerInfo {
        about: (!full.about.is_empty()).then(|| full.about.clone()),
        ..PeerInfo::default()
    };

    let mut presence: Vec<String> = Vec::new();
    if let Some(count) = full.participants_count {
        presence.push(format!("{} members", thousands(count)));
    }
    if let Some(count) = full.online_count {
        presence.push(format!("{} online", thousands(count)));
    }
    info.subtitle_line(presence);

    info.push("Members", full.participants_count.map(thousands));
    info.push("Online", full.online_count.map(thousands));
    info.push("Admins", full.admins_count.map(thousands));
    info.push("Slow mode", full.slowmode_seconds.map(duration));

    // Deliberately absent, and each for its own reason:
    //
    // - `exported_invite` is a credential: anything that can read this screen could join the chat
    //   with it.
    // - `linked_chat_id` and `migrated_from_chat_id` are bare ids. The number tells a reader
    //   nothing, and resolving one means a second request for a line nobody asked for.
    // - `wallpaper`, `stargifts_count`, `boosts_applied` and the rest of the layer's ornaments
    //   have nothing to do with reading a conversation from a terminal.
    info.push("Peer id", Some(full.id.to_string()));
    info
}

/// A span of seconds in the largest unit that divides it exactly, which is how every value
/// Telegram actually offers for slow mode is written in its own UI.
fn duration(seconds: i32) -> String {
    match seconds {
        seconds if seconds >= 3600 && seconds % 3600 == 0 => format!("{}h", seconds / 3600),
        seconds if seconds >= 60 && seconds % 60 == 0 => format!("{}m", seconds / 60),
        seconds => format!("{seconds}s"),
    }
}
```

- [ ] **Step 9: Run the full suite**

```sh
cargo fmt && cargo clippy --all-targets && cargo test
```

Expected: PASS. The pre-existing suite is unaffected; `state::peer_info` adds 10 tests.

- [ ] **Step 10: Commit**

```sh
git add src/state/peer_info.rs src/state/mod.rs
git commit -m "feat: flatten a peer's full info into a drawable table

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 2: A downloadable reference to a profile picture

**Files:**
- Modify: `src/state/media.rs`

**Interfaces:**
- Consumes: `pick_thumb` (private to `state::media`, and staying private), `PhotoRef`, `PhotoState`.
- Produces: `pub fn avatar_ref(photo: &grammers_client::media::Photo) -> Option<PhotoRef>`

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `src/state/media.rs`:

```rust
    /// A profile photo as `users.getFullUser` delivers one: a bare `tl::enums::Photo` rather than
    /// the `MessageMediaPhoto` a message carries.
    fn profile_photo(sizes: &[tl::enums::PhotoSize]) -> grammers_client::media::Photo {
        grammers_client::media::Photo::from_raw(tl::enums::Photo::Photo(tl::types::Photo {
            has_stickers: false,
            id: 1,
            access_hash: 1,
            file_reference: Vec::new(),
            date: 0,
            sizes: sizes.to_vec(),
            video_sizes: None,
            dc_id: 2,
        }))
    }

    #[test]
    fn an_avatar_picks_a_thumbnail_the_same_way_a_message_photo_does() {
        let avatar = avatar_ref(&profile_photo(&[
            crate::test_support::thumb("a", 160, 160),
            crate::test_support::thumb("x", 640, 640),
        ]))
        .expect("a profile photo with sizes is downloadable");

        assert_eq!(
            avatar.pixels,
            (640, 640),
            "sharing `pick_thumb` with `photo_ref` is what keeps the two from disagreeing about \
             what a terminal-sized source is"
        );
        assert!(matches!(avatar.state, PhotoState::Pending));
    }

    #[test]
    fn an_empty_profile_photo_yields_no_avatar_rather_than_an_empty_box() {
        let empty = grammers_client::media::Photo::from_raw(tl::enums::Photo::Empty(
            tl::types::PhotoEmpty { id: 0 },
        ));

        assert!(
            avatar_ref(&empty).is_none(),
            "a `photoEmpty` has no sizes at all; reserving a box for it would leave a hole above \
             the fields with nothing ever arriving to fill it"
        );
    }
```

- [ ] **Step 2: Run to verify they fail**

```sh
cargo test state::media
```

Expected: FAIL to compile — `cannot find function 'avatar_ref' in this scope`.

- [ ] **Step 3: Implement**

Add to `src/state/media.rs`, next to `photo_ref`:

```rust
/// A peer's current profile picture, as a downloadable reference.
///
/// Deliberately shares `pick_thumb` with [`photo_ref`], the way `dialog_list::is_muted` is shared
/// by the dialog seed and the live update: two functions choosing a source independently would
/// eventually disagree about what "sized for a terminal" means.
///
/// `None` for a peer with no picture, and for `photoEmpty`, which has no sizes at all.
pub fn avatar_ref(photo: &Photo) -> Option<PhotoRef> {
    let (source, pixels) = pick_thumb(photo.thumbs())?;
    Some(PhotoRef {
        source,
        pixels,
        // Never printed: the info screen falls back to the peer's initials rather than a label,
        // because a bordered box with `[photo]` in it reads as a broken picture.
        label: "[photo]",
        state: PhotoState::Pending,
    })
}
```

Add `Photo` to the `grammers_client::media` import at the top of the file.

- [ ] **Step 4: Run to verify they pass**

```sh
cargo test state::media
```

Expected: PASS.

- [ ] **Step 5: Populate the avatar in Task 1's flatteners**

In `src/state/peer_info.rs`, import `crate::state::media::avatar_ref` and `grammers_client::media::Photo`, then set the field in each of the three constructors:

```rust
// in `user`, after `info.push("Peer id", …)`:
info.avatar = full
    .profile_photo
    .clone()
    .and_then(|photo| avatar_ref(&Photo::from_raw(photo)));

// in `chat`:
info.avatar = full
    .chat_photo
    .clone()
    .and_then(|photo| avatar_ref(&Photo::from_raw(photo)));

// in `channel` — note `chat_photo` is not optional here, and is `photoEmpty` when there is none,
// which `avatar_ref` already answers `None` to:
info.avatar = avatar_ref(&Photo::from_raw(full.chat_photo.clone()));
```

- [ ] **Step 6: Run the full suite and commit**

```sh
cargo fmt && cargo clippy --all-targets && cargo test
```

Expected: PASS.

```sh
git add src/state/media.rs src/state/peer_info.rs
git commit -m "feat: resolve a peer's profile photo into a downloadable thumbnail

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 3: Key the image cache by what the picture belongs to

A mechanical widening with no behaviour change. The existing suite is the test: it must stay green.

**Files:**
- Modify: `src/ui/images.rs`, `src/ui/chat_view.rs`, `src/ui/photo_view.rs`

**Interfaces:**
- Produces:
  - `pub enum ImageKey { Message(i32), Avatar(PeerId) }` (in `crate::ui::images`), deriving `Debug, Clone, Copy, PartialEq, Eq, Hash`
  - `ImageStore::prepare(&mut self, id: ImageKey, image: &Arc<DynamicImage>, max_cols: u16, max_rows: u16) -> Option<Size>`
  - `ImageStore::protocol(&self, id: ImageKey, size: Size) -> Option<&SlicedProtocol>`
  - `ImageStore::reserve` is unchanged — it takes no id.

- [ ] **Step 1: Add the key type and widen the store**

In `src/ui/images.rs`, add `use grammers_session::types::PeerId;` and, above `ImageStore`:

```rust
/// What a cached encoding belongs to.
///
/// Was a bare message id until profiles arrived. An avatar has no message to be keyed by, and a
/// synthetic id would collide with a real one the moment a chat's ids reached it — channel ids
/// restart at 1 per channel, so there is no id space that is safely out of the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageKey {
    Message(i32),
    Avatar(PeerId),
}
```

Then change three things in the same file:

- The `cache` field: `cache: HashMap<(ImageKey, Size), Cached>,` and update its doc comment's first words from "Keyed by size as well as message" to "Keyed by size as well as picture".
- `prepare`'s first parameter: `id: ImageKey`.
- `protocol`'s first parameter: `id: ImageKey`.

Inside `prepare`, the `tracing::debug!` call interpolates `id`, which is no longer `Display`. Change it to:

```rust
tracing::debug!(%err, ?id, "could not encode image for this terminal");
```

- [ ] **Step 2: Update the two call sites**

In `src/ui/chat_view.rs` and `src/ui/photo_view.rs`, every `images.prepare(<id>, …)` and `images.protocol(<id>, …)` becomes `images.prepare(ImageKey::Message(<id>), …)` / `images.protocol(ImageKey::Message(<id>), …)`. Find them with:

```sh
grep -n "prepare(\|protocol(" src/ui/chat_view.rs src/ui/photo_view.rs
```

Add `ImageKey` to each file's `crate::ui::images` import.

- [ ] **Step 3: Run the full suite to verify nothing changed**

```sh
cargo fmt && cargo clippy --all-targets && cargo test
```

Expected: PASS, with the same test count as before this task. This is a refactor; a behaviour change here is a bug.

- [ ] **Step 4: Commit**

```sh
git add src/ui/images.rs src/ui/chat_view.rs src/ui/photo_view.rs
git commit -m "refactor: key encoded images by what they belong to, not by message id

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 4: Fetch a profile and hold it

The command, the event, the actor's three-way dispatch, and the `App` state it lands in. No menu entry and no rendering yet — tests call `App::open_peer_info` directly.

**Files:**
- Modify: `src/telegram/commands.rs`, `src/telegram/events.rs`, `src/telegram/mod.rs`, `src/app.rs`, `src/test_support.rs`

**Interfaces:**
- Consumes: `crate::state::peer_info::{self, PeerInfo}` from Task 1.
- Produces:
  - `TgCommand::LoadPeerInfo { peer: PeerRef }`
  - `TgEvent::PeerInfoLoaded { peer: PeerId, info: Result<Box<PeerInfo>, String> }`
  - `App::peer_info: Option<PeerInfoView>` (public field)
  - `pub struct PeerInfoView { pub peer: PeerRef, pub name: String, pub kind: DialogKind, pub state: InfoState, pub scroll: u16 }`
  - `pub enum InfoState { Loading, Ready(Box<PeerInfo>), Failed(String) }`
  - `App::open_peer_info(&mut self)` — opens on the *selected* dialog; no arguments.
  - `crate::test_support::user_full(id: i64) -> PeerInfo`

- [ ] **Step 1: Add the command and the event**

In `src/telegram/commands.rs`, add above the `// -- chat actions` block:

```rust
    /// Read everything Telegram will say about one peer.
    ///
    /// Which of the three full-info calls this becomes is decided from the peer's own kind, so
    /// nothing else has to be carried. Unlike the chat actions below this changes nothing about
    /// the account, which is why it needs no confirmation and reports no progress.
    LoadPeerInfo {
        peer: PeerRef,
    },
```

In `src/telegram/events.rs`, add `use crate::state::peer_info::PeerInfo;` and the variant:

```rust
    /// A peer's profile, for the info screen.
    ///
    /// Boxed for the reason `TgCommand::DownloadPhoto` boxes its source: inline, a whole profile
    /// would make every event in this channel as large as the largest one.
    ///
    /// `Err` carries what to print in place of the fields. A profile can be refused outright —
    /// privacy settings, a channel we have been kicked from — and the screen has to leave its
    /// loading state either way, so the failure travels in the same event rather than in the
    /// status banner, which is transient and would be gone before the user read it.
    PeerInfoLoaded {
        peer: PeerId,
        info: Result<Box<PeerInfo>, String>,
    },
```

- [ ] **Step 2: Add the actor arm**

In `src/telegram/mod.rs`, add to `actor_loop`'s match, next to `TgCommand::LoadFolders`:

```rust
            TgCommand::LoadPeerInfo { peer } => actor.load_peer_info(peer),
```

And on `impl Actor`, next to `load_blocked_peers`:

```rust
    /// Read a peer's full profile.
    ///
    /// grammers 0.10 wraps none of the three calls, so all three are raw — the same path
    /// `messages.getDialogFilters` and the archive's `messages.getDialogs` already take.
    ///
    /// `PeerKind`'s three variants line up one-to-one with the three requests, and a megagroup
    /// takes the channel one correctly because it *is* a `Channel`. That is the whole dispatch;
    /// nothing has to be carried on the command to disambiguate.
    fn load_peer_info(&mut self, peer: PeerRef) {
        let client = self.client.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            let info = fetch_peer_info(&client, peer)
                .await
                .map_err(|err| format!("could not read this profile: {err}"));

            let _ = events.send(TgEvent::PeerInfoLoaded {
                peer: peer.id,
                info,
            });
        });
    }
```

And as a free function at the end of the file, beside `fetch_image`:

```rust
async fn fetch_peer_info(
    client: &Client,
    peer: PeerRef,
) -> Result<Box<PeerInfo>, InvocationError> {
    let info = match peer.id.kind() {
        PeerKind::User => {
            let tl::enums::users::UserFull::Full(answer) = client
                .invoke(&tl::functions::users::GetFullUser {
                    id: (&peer).into(),
                })
                .await?;
            let tl::enums::UserFull::Full(full) = answer.full_user;

            // The handle, phone and badges are on the `User` rather than the `UserFull`, and the
            // response carries both. Matching on the id rather than taking the first entry
            // because `users` also carries anyone the profile mentions.
            let bare = peer.id.bare_id_unchecked();
            let user = answer.users.iter().find_map(|user| match user {
                tl::enums::User::User(user) if user.id == bare => Some(user),
                _ => None,
            });
            peer_info::user(&full, user)
        }
        PeerKind::Chat => {
            let tl::enums::messages::ChatFull::Full(answer) = client
                .invoke(&tl::functions::messages::GetFullChat {
                    chat_id: peer.id.bare_id_unchecked(),
                })
                .await?;
            match answer.full_chat {
                tl::enums::ChatFull::Full(full) => peer_info::chat(&full),
                // Cannot happen — `messages.getFullChat` answers for basic groups only — but a
                // panic here would take down the whole client for one profile.
                tl::enums::ChatFull::ChannelFull(full) => peer_info::channel(&full),
            }
        }
        PeerKind::Channel => {
            let tl::enums::messages::ChatFull::Full(answer) = client
                .invoke(&tl::functions::channels::GetFullChannel {
                    channel: (&peer).into(),
                })
                .await?;
            match answer.full_chat {
                tl::enums::ChatFull::ChannelFull(full) => peer_info::channel(&full),
                tl::enums::ChatFull::Full(full) => peer_info::chat(&full),
            }
        }
    };

    Ok(Box::new(info))
}
```

Add the imports this needs at the top of `src/telegram/mod.rs`: `use crate::state::peer_info::{self, PeerInfo};` and, if not already there, `grammers_session::types::PeerKind`.

- [ ] **Step 3: Write the failing App tests**

First add the fixture to `src/test_support.rs`:

```rust
/// A profile as the actor would deliver one, with a bio and nothing else.
pub fn user_full(name: &str) -> PeerInfo {
    PeerInfo {
        subtitle: vec![format!("@{}", name.to_lowercase())],
        about: Some(format!("This is {name}.")),
        rows: vec![InfoRow {
            label: "Peer id",
            value: "1".to_string(),
        }],
        avatar: None,
    }
}
```

with `use crate::state::peer_info::{InfoRow, PeerInfo};` added to its imports.

Then add to `src/app.rs`'s `mod tests`:

```rust
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
        
            drain(&mut rx).as_slice(),
            [TgCommand::LoadPeerInfo { peer }] if peer.id == peer(1).id
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
```

Add `InfoState`, `PeerInfo` and `user_full` to the test module's imports.

Note this requires a field the spec did not name explicitly: add `pub blocked: Option<bool>` to `PeerInfo` in `src/state/peer_info.rs` (defaulting to `None`, set to `Some(full.blocked)` in `peer_info::user` and left `None` by `chat` and `channel`, which have no such flag about *us*). Add a test for it in `state::peer_info`:

```rust
    #[test]
    fn only_a_user_profile_reports_whether_we_have_blocked_them() {
        assert_eq!(
            user(
                &tl::types::UserFull {
                    blocked: true,
                    ..empty_user_full()
                },
                None
            )
            .blocked,
            Some(true)
        );
        assert_eq!(
            channel(&empty_channel_full()).blocked,
            None,
            "`channelFull.blocked` means the channel is restricted, not that we blocked it; \
             carrying it under the same name would put a Unblock entry on a chat menu"
        );
    }
```

- [ ] **Step 4: Run to verify they fail**

```sh
cargo test app::tests
```

Expected: FAIL to compile — `no method named 'open_peer_info'`.

- [ ] **Step 5: Implement the App state and reducers**

In `src/app.rs`, add near the `ChatMenu` / `ForwardPicker` definitions:

```rust
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
```

Add the field to `App` (beside `forward`) and to `App::new`:

```rust
    /// The profile being read. Modal and full screen, like `viewer`: while it is set the two
    /// panes are not drawn at all.
    pub peer_info: Option<PeerInfoView>,
```

Add the opener:

```rust
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
```

Add the reducer to `handle_event`, next to the other chat-state arms:

```rust
            TgEvent::PeerInfoLoaded { peer, info } => {
                // A late answer must not land on the wrong profile: ids repeat across peers only
                // in the message-id sense, but the screen may have been closed and reopened on
                // another chat entirely while this was in flight.
                let Some(view) = self.peer_info.as_mut().filter(|view| view.peer.id == peer)
                else {
                    return;
                };

                view.state = match info {
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
            }
```

The borrow checker will object to touching `self.dialogs` while `view` is borrowed from `self.peer_info`. Resolve it by computing the new state first and assigning after:

```rust
            TgEvent::PeerInfoLoaded { peer, info } => {
                if self.peer_info.as_ref().map(|view| view.peer.id) != Some(peer) {
                    return;
                }

                let state = match info {
                    Ok(info) => {
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
```

Import `PeerInfo` and `peer_info` as needed.

- [ ] **Step 6: Run to verify they pass**

```sh
cargo fmt && cargo clippy --all-targets && cargo test
```

Expected: PASS.

- [ ] **Step 7: Commit**

```sh
git add src/telegram/commands.rs src/telegram/events.rs src/telegram/mod.rs src/app.rs src/state/peer_info.rs src/test_support.rs
git commit -m "feat: fetch a peer's profile and hold it while it is on screen

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: The menu entry and the keyboard

**Files:**
- Modify: `src/state/dialog_actions.rs`, `src/app.rs`

**Interfaces:**
- Consumes: `App::open_peer_info` from Task 4.
- Produces:
  - `DialogAction::Info` (first in every menu)
  - `DialogAction::in_progress(self) -> Option<&'static str>` (was `&'static str`)
  - `App::handle_peer_info_key(&mut self, key: KeyEvent)`

- [ ] **Step 1: Write the failing tests for the menu table**

Add to `src/state/dialog_actions.rs`'s `mod tests`:

```rust
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
```

The existing tests in that module assert exact label lists — `a_private_chat_offers_the_full_set_including_blocking`, `a_group_is_left_rather_than_deleted_and_has_nobody_to_block`, `a_megagroup_is_left_like_a_group_but_offers_no_clear_history`, `a_channel_is_left_and_has_no_history_of_yours_to_clear`, and `a_toggle_never_offers_both_of_its_faces_at_once` (which asserts a length of 6). Each needs `"Info"` prepended to its expected list, and the length assertion raised from 6 to 7.

- [ ] **Step 2: Run to verify they fail**

```sh
cargo test state::dialog_actions
```

Expected: FAIL to compile — `no variant named 'Info'`.

- [ ] **Step 3: Implement the action**

In `src/state/dialog_actions.rs`:

- Add `Info` as the first variant of `DialogAction`, with:

```rust
    /// Show what Telegram will say about this peer. Alone among these entries it asks nothing of
    /// the account and changes nothing on any other device — it only puts a screen up.
    Info,
```

- `label`: `DialogAction::Info => "Info",`
- `is_destructive`: unchanged; `Info` falls into the existing `matches!` as `false`.
- `in_progress`: change the return type to `Option<&'static str>`, wrap every existing arm in `Some(…)`, and add `DialogAction::Info => None,`. Update the doc comment to say that `None` means the entry aims the UI rather than issuing a request — the same shape `MessageAction::in_progress` already has for Reply, Edit and Forward.
- `actions_for`: make `Info` the first element of the initial `vec![…]`.

- [ ] **Step 4: Run to verify they pass**

```sh
cargo test state::dialog_actions
```

Expected: FAIL — `src/app.rs` does not compile yet, because `run_action` still uses `in_progress` as a `&str` and has no `Info` arm. That is the next step.

- [ ] **Step 5: Write the failing App tests for the key routing**

Add to `src/app.rs`'s `mod tests`:

```rust
    #[test]
    fn choosing_info_from_the_chat_menu_opens_the_profile() {
        let (mut app, mut rx) = opened_chat();
        app.handle_key(ctrl(KeyCode::Char('a')));
        // Info is the first entry, so the menu opens on it.
        app.handle_key(key(KeyCode::Enter));

        assert!(app.menu.is_none(), "the menu closes behind the screen it opened");
        assert!(app.peer_info.is_some());
        
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
```

`ctrl` is the existing helper in that module; if it is not present, it is `KeyEvent::new(code, KeyModifiers::CONTROL)`.

- [ ] **Step 6: Run to verify they fail**

```sh
cargo test app::tests
```

Expected: FAIL to compile.

- [ ] **Step 7: Implement the routing**

In `src/app.rs`:

Add the `Info` arm to `run_action`. It is the one entry that is not a `TgCommand`, so the `self.send(match …)` shape has to split:

```rust
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
            // … existing arms unchanged, minus the `Info` variant, which is unreachable here …
            DialogAction::Info => unreachable!("handled above, before the menu was consumed"),
        });

        if let Some(progress) = action.in_progress() {
            self.set_status(progress, StatusKind::Info);
        }
    }
```

Claim the screen in `handle_main_key`, immediately after the viewer block and before the forward picker:

```rust
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
```

Add the handler beside `handle_viewer_key`:

```rust
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
```

And in `forget_dialog`, beside the `self.viewer = None;` line — but outside the `open_chat` check, because a profile can be open on a chat that is not the one on screen:

```rust
        // A profile of a conversation that no longer exists is a screen with nothing behind it.
        if self.peer_info.as_ref().is_some_and(|view| view.peer.id == peer) {
            self.peer_info = None;
        }
```

Place it just after `self.chats.remove(&peer);`.

- [ ] **Step 8: Run the full suite**

```sh
cargo fmt && cargo clippy --all-targets && cargo test
```

Expected: PASS.

- [ ] **Step 9: Commit**

```sh
git add src/state/dialog_actions.rs src/app.rs
git commit -m "feat: open a peer's profile from the chat action menu

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 6: Download the avatar

**Files:**
- Modify: `src/telegram/commands.rs`, `src/telegram/events.rs`, `src/telegram/mod.rs`, `src/app.rs`

**Interfaces:**
- Consumes: `PeerInfo::avatar` (Task 2), `App::peer_info` (Task 4).
- Produces:
  - `TgCommand::DownloadAvatar { peer: PeerRef, source: Box<PhotoSource> }`
  - `TgEvent::AvatarLoaded { peer: PeerId, image: Option<Arc<DynamicImage>> }`

- [ ] **Step 1: Write the failing tests**

Add to `src/app.rs`'s `mod tests`:

```rust
    /// A profile carrying a picture that has not been fetched yet.
    fn profile_with_avatar() -> PeerInfo {
        PeerInfo {
            avatar: Some(
                crate::state::media::avatar_ref(&crate::test_support::profile_photo(160, 160))
                    .expect("the fixture picture is downloadable"),
            ),
            ..user_full("Alice")
        }
    }

    #[test]
    fn a_profile_with_a_picture_asks_for_it_once() {
        let (mut app, mut rx) = opened_profile();
        drain(&mut rx);

        app.handle_event(TgEvent::PeerInfoLoaded {
            peer: peer(1).id,
            info: Ok(Box::new(profile_with_avatar())),
        });

        
            drain(&mut rx).as_slice(),
            [TgCommand::DownloadAvatar { peer, .. }] if peer.id == peer(1).id
        ));
    }

    #[test]
    fn a_profile_with_no_picture_asks_for_nothing() {
        let (mut app, mut rx) = opened_profile();
        drain(&mut rx);

        app.handle_event(TgEvent::PeerInfoLoaded {
            peer: peer(1).id,
            info: Ok(Box::new(user_full("Alice"))),
        });

        assert!(
            drain(&mut rx).is_empty(),
            "there is nothing to fetch, and a request for a picture that does not exist would \
             come back as an error the user never asked to see"
        );
    }

    #[test]
    fn an_avatar_that_arrives_is_ready_to_draw() {
        let (mut app, _rx) = opened_profile();
        app.handle_event(TgEvent::PeerInfoLoaded {
            peer: peer(1).id,
            info: Ok(Box::new(profile_with_avatar())),
        });
        app.handle_event(TgEvent::AvatarLoaded {
            peer: peer(1).id,
            image: Some(gradient(160, 160)),
        });

        let InfoState::Ready(info) = &app.peer_info.as_ref().unwrap().state else {
            panic!("the profile should be ready");
        };
        assert!(info.avatar.as_ref().unwrap().image().is_some());
    }

    #[test]
    fn a_failed_avatar_download_is_terminal_rather_than_retried() {
        let (mut app, mut rx) = opened_profile();
        app.handle_event(TgEvent::PeerInfoLoaded {
            peer: peer(1).id,
            info: Ok(Box::new(profile_with_avatar())),
        });
        drain(&mut rx);

        app.handle_event(TgEvent::AvatarLoaded {
            peer: peer(1).id,
            image: None,
        });

        let InfoState::Ready(info) = &app.peer_info.as_ref().unwrap().state else {
            panic!("the profile should be ready");
        };
        
            info.avatar.as_ref().unwrap().state,
            PhotoState::Failed
        ));
        assert!(
            drain(&mut rx).is_empty(),
            "the reserved box keeps its rows and shows initials; asking again would be a request \
             per frame for a picture the server has already refused"
        );
    }
```

Add the fixture to `src/test_support.rs`:

```rust
/// A profile photo as `users.getFullUser` delivers one: a bare `tl::enums::Photo` rather than the
/// `MessageMediaPhoto` a message carries.
pub fn profile_photo(width: i32, height: i32) -> grammers_client::media::Photo {
    grammers_client::media::Photo::from_raw(tl::enums::Photo::Photo(tl::types::Photo {
        has_stickers: false,
        id: 1,
        access_hash: 1,
        file_reference: Vec::new(),
        date: 0,
        sizes: vec![thumb("x", width, height)],
        video_sizes: None,
        dc_id: 2,
    }))
}
```

(If Task 2's test module defined a local `profile_photo`, replace it with a call to this shared one so there is a single fixture.)

- [ ] **Step 2: Run to verify they fail**

```sh
cargo test app::tests
```

Expected: FAIL to compile — `no variant named 'DownloadAvatar'`.

- [ ] **Step 3: Add the command, the event, and the actor arm**

In `src/telegram/commands.rs`, beside `LoadPeerInfo`:

```rust
    /// Fetch the peer's current profile picture.
    ///
    /// Its own command rather than a widening of `DownloadPhoto`, whose path is tuned around
    /// three things an avatar does not have: a visibility trigger that fires every frame, the
    /// in-flight cap, and the decoded-image eviction queue. There is exactly one avatar, it is
    /// always on screen, and it dies with the screen.
    DownloadAvatar {
        peer: PeerRef,
        source: Box<PhotoSource>,
    },
```

In `src/telegram/events.rs`, beside `PeerInfoLoaded`:

```rust
    /// The profile picture finished downloading. `None` when it failed — reported in the success
    /// shape for the reason `PhotoLoaded` is: the in-flight guard has to clear either way, and
    /// the screen falls back to the peer's initials.
    AvatarLoaded {
        peer: PeerId,
        image: Option<Arc<DynamicImage>>,
    },
```

In `src/telegram/mod.rs`, add to `actor_loop`:

```rust
            TgCommand::DownloadAvatar { peer, source } => actor.download_avatar(peer, source),
```

and, beside `download_photo`:

```rust
    fn download_avatar(&mut self, peer: PeerRef, source: Box<PhotoSource>) {
        let client = self.client.clone();
        let events = self.events.clone();

        tokio::spawn(async move {
            let image = match fetch_image(&client, &source).await {
                Ok(image) => Some(Arc::new(image)),
                Err(err) => {
                    // No banner, like a photo download: the screen already says what happened by
                    // falling back to the peer's initials.
                    tracing::debug!(%err, "avatar download failed");
                    None
                }
            };

            let _ = events.send(TgEvent::AvatarLoaded {
                peer: peer.id,
                image,
            });
        });
    }
```

- [ ] **Step 4: Implement the App side**

In `src/app.rs`, extend the `PeerInfoLoaded` reducer to request the picture once, after the state is assigned:

```rust
                // Requested here rather than from the renderer, unlike the transcript's photos:
                // there is exactly one avatar and it is on screen the moment the profile is, so
                // there is nothing for a visibility trigger to decide. `PhotoState` is still the
                // guard, so a second answer for the same screen changes nothing.
                self.request_avatar();
```

and add:

```rust
    /// Fetch the open profile's picture, if it has one that has not been asked for yet.
    fn request_avatar(&mut self) {
        let Some(view) = self.peer_info.as_mut() else {
            return;
        };
        let InfoState::Ready(info) = &mut view.state else {
            return;
        };
        let Some(avatar) = info.avatar.as_mut() else {
            return;
        };
        if !matches!(avatar.state, PhotoState::Pending) {
            return;
        }

        avatar.state = PhotoState::Loading;
        let command = TgCommand::DownloadAvatar {
            peer: view.peer,
            source: Box::new(avatar.source.clone()),
        };
        self.send(command);
    }
```

`self.send` takes `&self`, so building the command before calling it keeps the mutable borrow of `self.peer_info` from overlapping. If the borrow checker still objects, copy `view.peer` and the source into locals, `drop` the borrow by ending the block, and send after.

Add the reducer:

```rust
            TgEvent::AvatarLoaded { peer, image } => {
                // Same stale guard as the profile itself: the screen may have moved on.
                let Some(view) = self.peer_info.as_mut().filter(|view| view.peer.id == peer)
                else {
                    return;
                };
                let InfoState::Ready(info) = &mut view.state else {
                    return;
                };
                let Some(avatar) = info.avatar.as_mut() else {
                    return;
                };

                // `Failed` is terminal. Nothing retries it, because the picture is asked for once
                // and the screen has initials to fall back to.
                avatar.state = match image {
                    Some(image) => PhotoState::Ready(image),
                    None => PhotoState::Failed,
                };
            }
```

- [ ] **Step 5: Run the full suite**

```sh
cargo fmt && cargo clippy --all-targets && cargo test
```

Expected: PASS.

- [ ] **Step 6: Commit**

```sh
git add src/telegram/commands.rs src/telegram/events.rs src/telegram/mod.rs src/app.rs src/test_support.rs src/state/media.rs
git commit -m "feat: download the profile picture for an open peer info screen

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 7: Draw the profile

**Files:**
- Create: `src/ui/peer_view.rs`
- Modify: `src/ui/mod.rs`

**Interfaces:**
- Consumes: `App::peer_info`, `InfoState`, `PeerInfo`, `InfoRow`, `ImageStore`, `ImageKey::Avatar`.
- Produces: `pub fn render(frame: &mut Frame, area: Rect, app: &mut App, images: &mut ImageStore)`; `pub fn initials(name: &str) -> String`; `pub fn avatar_box(…) -> Option<Size>` as described below.

- [ ] **Step 1: Write the failing tests**

Create `src/ui/peer_view.rs` with only the test module and the two helpers under test, so the first run is a real failure rather than a missing file:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_come_from_the_first_letters_of_the_first_two_words() {
        assert_eq!(initials("Alice Smith"), "AS");
        assert_eq!(initials("Alice"), "A");
        assert_eq!(initials("Rust Users Group"), "RU");
    }

    #[test]
    fn a_nameless_peer_still_gets_something_to_put_in_the_box() {
        assert_eq!(
            initials(""),
            "?",
            "an empty box beside the fields reads as a rendering failure rather than as a peer \
             with no name"
        );
    }

    #[test]
    fn scrolling_clamps_at_the_end_of_the_profile() {
        assert_eq!(clamp_scroll(0, 40, 10), 0);
        assert_eq!(
            clamp_scroll(500, 40, 10),
            30,
            "scrolling past the end would leave the reader looking at blank rows with no way to \
             tell the screen from a failed one"
        );
        assert_eq!(
            clamp_scroll(500, 6, 10),
            0,
            "a profile shorter than the viewport has nothing to scroll"
        );
    }
}
```

- [ ] **Step 2: Run to verify they fail**

```sh
cargo test ui::peer_view
```

Expected: FAIL — the module is not declared in `src/ui/mod.rs`, then FAIL to compile on `initials` / `clamp_scroll`.

- [ ] **Step 3: Implement the module**

Write `src/ui/peer_view.rs`:

```rust
//! The peer info screen: a profile, full screen.
//!
//! Takes the whole body rather than floating over the panes, the way the picture viewer does and
//! for the same reason — there is a picture and a paragraph of text to fit, and neither reads well
//! in a popup sized to a menu. Nothing behind it is context for what it is about to do, because it
//! is about to do nothing: this screen is read-only.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use ratatui_image::sliced::SlicedImage;

use crate::app::{App, InfoState, PeerInfoView};
use crate::state::peer_info::PeerInfo;
use crate::ui::images::{ImageKey, ImageStore};
use crate::ui::text::wrap;
use crate::ui::widgets::pane;

/// Columns and rows the avatar may claim. Wide enough to be a face rather than a smudge, short
/// enough that the fields beside it are not pushed off a laptop-sized terminal.
const AVATAR_COLS: u16 = 20;
const AVATAR_ROWS: u16 = 10;

/// Columns between the avatar and the text beside it.
const GUTTER: u16 = 2;

/// Width of the label column in the field table, so every value starts in the same place.
const LABEL_WIDTH: usize = 12;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, images: &mut ImageStore) {
    let Some(view) = app.peer_info.as_ref() else {
        return;
    };

    let block = pane(&view.name, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width < 8 || inner.height < 3 {
        // Too small to say anything truthfully. Better an empty pane than a clipped half of one
        // field, which would look like the profile is missing everything else.
        return;
    }

    let (header_rows, avatar_size) = header(view, images, inner.width);
    let lines = body(view, inner.width);

    // Clamped against the profile just measured, the same direction `metrics` and the clamped
    // `ChatBuffer.scroll` already flow: only the frame being built knows how many lines the
    // fields came to.
    let total = header_rows.saturating_add(lines.len() as u16);
    let scroll = clamp_scroll(view.scroll, total, inner.height);
    if let Some(view) = app.peer_info.as_mut() {
        view.scroll = scroll;
    }

    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(header_rows), Constraint::Min(0)]).areas(inner);

    // The header scrolls off first, so it is only drawn while any of it is still on screen.
    if scroll < header_rows {
        draw_header(frame, header_area, &*app, images, avatar_size, scroll);
    }

    let skipped = scroll.saturating_sub(header_rows) as usize;
    let body_area = if scroll < header_rows { body_area } else { inner };
    frame.render_widget(
        Paragraph::new(lines.into_iter().skip(skipped).collect::<Vec<Line>>()),
        body_area,
    );
}

/// How many rows the header claims, and the box the avatar occupies within it.
///
/// The box is decided from the picture's *stated* pixel size rather than from the decoded image,
/// so it is the same before and after the download lands — the same discipline the transcript
/// keeps with `ImageStore::reserve` and `prepare` sharing `fit`. A header that grew when the
/// picture arrived would shove every field down under the reader's eyes.
fn header(view: &PeerInfoView, images: &ImageStore, width: u16) -> (u16, Option<Size>) {
    let avatar = match &view.state {
        InfoState::Ready(info) => info.avatar.as_ref(),
        _ => None,
    };

    let size = avatar.and_then(|avatar| {
        images.reserve(avatar.pixels, AVATAR_COLS.min(width), AVATAR_ROWS)
    });

    // One line for the name, one per subtitle line, and a blank line under the lot.
    let text_rows = 1 + subtitles(view).len() as u16 + 1;
    let rows = size.map_or(text_rows, |size| size.height.max(text_rows) + 1);
    (rows, size)
}

fn subtitles(view: &PeerInfoView) -> Vec<String> {
    match &view.state {
        InfoState::Ready(info) => info.subtitle.clone(),
        InfoState::Loading => vec!["loading…".to_string()],
        InfoState::Failed(_) => Vec::new(),
    }
}

fn draw_header(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    images: &mut ImageStore,
    size: Option<Size>,
    scroll: u16,
) {
    let Some(view) = app.peer_info.as_ref() else {
        return;
    };

    let (picture_area, text_area) = match size {
        Some(size) => {
            let [left, right] = Layout::horizontal([
                Constraint::Length(size.width + GUTTER),
                Constraint::Min(1),
            ])
            .areas(area);
            (
                Some(Rect {
                    width: size.width,
                    ..left
                }),
                right,
            )
        }
        None => (None, area),
    };

    if let (Some(picture_area), Some(size)) = (picture_area, size) {
        draw_avatar(frame, picture_area, view, images, size, scroll);
    }

    let mut lines = vec![Line::from(Span::styled(
        view.name.clone(),
        Style::default().bold(),
    ))];
    lines.extend(
        subtitles(view)
            .into_iter()
            .map(|line| Line::from(Span::styled(line, Style::default().fg(Color::DarkGray)))),
    );

    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(scroll as usize)
                .collect::<Vec<Line>>(),
        ),
        text_area,
    );
}

/// The picture, or the peer's initials where it will be.
///
/// The box is claimed either way. Collapsing it when the download fails would shift every field
/// below it at an arbitrary moment, which is the whole thing `reserve`/`prepare` exists to stop.
fn draw_avatar(
    frame: &mut Frame,
    area: Rect,
    view: &PeerInfoView,
    images: &mut ImageStore,
    size: Size,
    scroll: u16,
) {
    let InfoState::Ready(info) = &view.state else {
        return;
    };
    let Some(avatar) = info.avatar.as_ref() else {
        return;
    };

    let area = Rect {
        height: size.height.saturating_sub(scroll),
        ..area
    };
    if area.height == 0 {
        return;
    }

    if let Some(image) = avatar.image() {
        let peer = view.peer.id;
        if images
            .prepare(ImageKey::Avatar(peer), image, size.width, size.height)
            .is_some()
        {
            if let Some(protocol) = images.protocol(ImageKey::Avatar(peer), size) {
                frame.render_widget(SlicedImage::new(protocol), area);
                return;
            }
        }
    }

    let placeholder = initials(&view.name);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            placeholder,
            Style::default().fg(Color::DarkGray).bold(),
        )))
        .centered(),
        area,
    );
}

/// The bio and the field table, as lines.
fn body(view: &PeerInfoView, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    match &view.state {
        InfoState::Loading => lines.push(dim("loading…")),
        InfoState::Failed(why) => {
            for line in wrap(why, width as usize) {
                lines.push(Line::from(Span::styled(line, Style::default().fg(Color::Red))));
            }
        }
        InfoState::Ready(info) => lines.extend(fields(info, width)),
    }

    lines.push(Line::default());
    lines.push(dim("esc  close    ↑↓  scroll"));
    lines
}

fn fields(info: &PeerInfo, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    if let Some(about) = &info.about {
        for line in wrap(about, width as usize) {
            lines.push(Line::from(line));
        }
        lines.push(Line::default());
    }

    for row in &info.rows {
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:LABEL_WIDTH$}", row.label),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(row.value.clone()),
        ]));
    }

    lines
}

fn dim(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(Color::DarkGray),
    ))
}

/// The peer's initials, for the box while the picture is loading or after it failed.
pub fn initials(name: &str) -> String {
    let letters: String = name
        .split_whitespace()
        .take(2)
        .filter_map(|word| word.chars().next())
        .flat_map(char::to_uppercase)
        .collect();

    // A peer with no name at all still needs something in the box: an empty one reads as a
    // rendering failure rather than as a peer with no name.
    if letters.is_empty() {
        "?".to_string()
    } else {
        letters
    }
}

/// Lines that may be scrolled past, given how long the profile is and how tall the view is.
///
/// Counts *from the top*, unlike `ChatBuffer.scroll`. A profile is a fixed-length document read
/// top-down; a transcript grows at the end and must not move when older history is prepended.
pub fn clamp_scroll(scroll: u16, total: u16, viewport: u16) -> u16 {
    scroll.min(total.saturating_sub(viewport))
}
```

Declare it in `src/ui/mod.rs` (`mod peer_view;` beside the others) and add the `draw` arm, after the viewer's and before the two-pane arm:

```rust
        // Full screen like the viewer, and for the same reason: there is a picture and a
        // paragraph to fit, and neither reads well in a popup sized to a menu.
        Screen::Main if app.peer_info.is_some() => peer_view::render(frame, body, app, images),
```

- [ ] **Step 4: Run to verify the tests pass**

```sh
cargo test ui::peer_view
```

Expected: PASS, 3 tests.

- [ ] **Step 5: Reconcile the borrow checker**

`render` holds a shared borrow of `app.peer_info` to measure the profile, then takes a mutable one to write the clamped `scroll` back, then re-borrows shared to draw. That sequence is fine under NLL as written — `header` and `body` both return owned data, so the first borrow ends at its last use — but if the borrow checker does object, restructure by pulling what drawing needs into locals *before* the mutable write, exactly as `chat_view::render_transcript` does when it writes `App.metrics` back. Do not work around it by cloning the `PeerInfo`: it holds a decoded image.

Run:

```sh
cargo fmt && cargo clippy --all-targets && cargo test
```

Expected: PASS. Clippy must be clean; `#[allow(…)]` is not an acceptable fix here.

- [ ] **Step 6: Verify it against a real account**

```sh
TGTUI_LOG=debug cargo run
```

Select a private chat, press `Ctrl+A`, press `Enter` on `Info`. Confirm: the name appears immediately, the fields arrive a moment later, the avatar appears without the fields below it jumping, `↑`/`↓` scroll, and `Esc` returns to the two panes. Repeat on a group and on a channel. Then `TGTUI_IMAGES=off cargo run` and confirm the header falls back to text with no gap where the picture would be.

- [ ] **Step 7: Commit**

```sh
git add src/ui/peer_view.rs src/ui/mod.rs
git commit -m "feat: draw the peer info screen

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 8: Document it

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update Scope**

In the `## Scope` section, after the chat-actions paragraph, add:

```markdown
`Ctrl+A` also opens `Info`, the one entry in that menu that asks nothing of the account: a
full-screen, read-only profile of the conversation. A user shows their handle and badges, presence,
bio, phone, birthday and how many groups you share; a basic group shows its description and member
count; a megagroup or channel adds online and admin counts and slow mode. `state::peer_info` is the
table, and the reasoning behind each omission is in the tests there — an invite link is a
credential, a linked chat arrives as a bare id worth neither printing nor a second request to
resolve, and the layer's ornaments (wallpapers, gift counts, star ratings) have nothing to do with
reading a conversation from a terminal. grammers 0.10 wraps none of `users.getFullUser`,
`messages.getFullChat` or `channels.getFullChannel`, so all three are raw `invoke`s; `PeerKind`'s
three variants pick between them exactly. Editing anything about a peer, listing participants, and
opening the avatar in the picture viewer are out of scope.
```

- [ ] **Step 2: Update Keys**

In the `Keys:` paragraph, after the `Ctrl+A` sentence, add:

```markdown
`Info` in that menu opens the profile full screen, where `↑`/`↓` scroll and `Esc` closes.
```

- [ ] **Step 3: Update the modal-ordering paragraph**

Replace the "Four modals stack" paragraph with:

```markdown
Five modals stack, and the order in `handle_main_key` is deliberate: viewer, peer info, forward
picker, chat menu, message menu, message cursor. The viewer is first because it is full screen and
nothing behind it is visible; the info screen is second for the same reason. The forward picker is
next because it takes plain characters into its filter, so it must be claimed ahead of the menus
that navigate with `j`/`k`. Everything modal comes before the compose box, which otherwise swallows
every letter. The viewer and the info screen can never both be open: with a picture up the chat
list is not drawn, so `Ctrl+A` is unreachable and no `Info` entry can be chosen; with a profile up,
`handle_peer_info_key` swallows `Ctrl+P`.
```

- [ ] **Step 4: Add the invariants**

Append to the `### Invariants worth knowing before editing` list:

```markdown
- **The info screen scrolls from the top; the transcript scrolls from the bottom.** A profile is a
  fixed-length document read top-down, so `PeerInfoView.scroll` counts lines scrolled *past* and
  `peer_view::clamp_scroll` bounds it against the profile the frame just measured. `ChatBuffer.scroll`
  counts *up from the newest message* instead, so prepending older history does not move the
  viewport. The two look alike and mean opposite things.
- **A profile is fetched fresh and never cached.** `App.peer_info` is dropped on close, so there is
  no second staleness problem: a bio edited on another device simply arrives next time. It also
  means a late `PeerInfoLoaded` or `AvatarLoaded` must be dropped when its peer is not the one on
  screen, and that a profile must not outlive its conversation — `forget_dialog` clears it.
- **The avatar claims its rows before it arrives, and keeps them if it never does.** Same
  `ImageStore::reserve`/`prepare` pair sharing `fit` that the transcript uses. A failed download
  falls back to the peer's initials inside the box it already reserved; collapsing the box would
  shove every field below it up at an arbitrary moment.
- **Not every chat-menu entry is a request.** `DialogAction::in_progress` returns an `Option`
  because `Info` only puts a screen up, exactly as `MessageAction::in_progress` does for Reply,
  Edit and Forward. A banner narrating work that is not happening would be a lie about the account.
```

- [ ] **Step 5: Verify and commit**

```sh
cargo test
```

Expected: PASS (documentation only; this confirms nothing was broken).

```sh
git add CLAUDE.md
git commit -m "docs: record the peer info screen and its invariants

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage** — every section maps to a task: the three TL calls and `PeerKind` dispatch (Task 4), the info table and its omissions (Task 1), `avatar_ref` and `photoEmpty` (Task 2), `ImageKey` (Task 3), `PeerInfoView`/`InfoState` and the stale guard and blocked correction (Task 4), the `Info` entry and `in_progress` becoming `Option` (Task 5), `DownloadAvatar`/`AvatarLoaded` and request-once (Task 6), the renderer, scroll direction, initials fallback, no-protocol and too-short guards (Task 7), and CLAUDE.md (Task 8). Every test named in the spec appears in a task.

**Gaps found and closed during this review:** the generated TL types derive only `Debug`, `Clone` and `PartialEq`, so two fixtures that reached for `::default()` were replaced with the spelled-out `no_settings` / `no_notify_settings` helpers; `peer_info::chat` had an implementation in Task 1 Step 8 but no test, so `a_basic_group_counts_the_participant_list_it_was_sent` and `a_group_that_will_not_show_its_members_claims_no_count` were added to Step 6; and `draw_header` / `draw_avatar` took `&mut App` while only reading, which made the borrow in `render` unresolvable — both now take what they read.

**One addition the spec did not name:** `PeerInfo.blocked: Option<bool>`. The spec calls for `userFull.blocked` to correct the dialog row, but never says how it crosses from `state::peer_info` to `App`. It is introduced in Task 4 Step 3, with a test explaining why `channelFull.blocked` — which means the *channel* is restricted, not that we blocked it — must not be carried under the same name.

**Type consistency** — `ImageKey` is `ImageKey::Message` / `ImageKey::Avatar` throughout; `in_progress` returns `Option<&'static str>` from Task 5 onward and Task 5's `run_action` is the only caller; `PeerInfo` is boxed in both the event and `InfoState::Ready`; `peer_info::user` takes two arguments and `chat`/`channel` take one, matching every call in Task 4.
