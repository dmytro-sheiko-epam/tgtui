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
use grammers_client::media::Photo;
use grammers_client::tl;

use crate::state::media::{Avatar, avatar_ref};

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
    pub avatar: Option<Avatar>,
    /// Whether we have blocked this user. `None` for a chat or channel, which have no such flag
    /// about us — `channelFull.blocked` means the channel is restricted, a different fact, and
    /// must never be carried under this name.
    pub blocked: Option<bool>,
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
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(ch);
    }
    if count < 0 { format!("-{out}") } else { out }
}

/// A private chat's profile.
///
/// Two arguments because `users.getFullUser` splits what a person is across two objects: the
/// handle, phone and badges live on the `User` in the response's `users`, and only the bio,
/// blocked flag and common-chat count live on the `UserFull`.
pub fn user(full: &tl::types::UserFull, user: Option<&tl::types::User>) -> PeerInfo {
    let mut info = PeerInfo {
        about: full.about.clone().filter(|about| !about.is_empty()),
        blocked: Some(full.blocked),
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
    info.subtitle_line(user.and_then(last_seen).into_iter().collect());

    info.push("Phone", user.and_then(|user| user.phone.clone()));
    info.push("Birthday", full.birthday.as_ref().map(birthday));
    info.push(
        "Groups",
        (full.common_chats_count > 0).then(|| format!("{} in common", full.common_chats_count)),
    );
    info.push("Peer id", Some(full.id.to_string()));
    info.avatar = full
        .profile_photo
        .clone()
        .and_then(|photo| avatar_ref(&Photo::from_raw(photo)));
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
    let utc: DateTime<Utc> = Utc
        .timestamp_opt(unix as i64, 0)
        .single()
        .unwrap_or_default();
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
    info.avatar = full
        .chat_photo
        .clone()
        .and_then(|photo| avatar_ref(&Photo::from_raw(photo)));
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
    info.avatar = avatar_ref(&Photo::from_raw(full.chat_photo.clone()));
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

    /// A `user` as the response's `users` list carries one, with every flag off. The badge tests
    /// switch on the handful they are about.
    fn plain_user() -> tl::types::User {
        tl::types::User {
            is_self: false,
            contact: false,
            mutual_contact: false,
            deleted: false,
            bot: false,
            bot_chat_history: false,
            bot_nochats: false,
            verified: false,
            restricted: false,
            min: false,
            bot_inline_geo: false,
            support: false,
            scam: false,
            apply_min_photo: false,
            fake: false,
            bot_attach_menu: false,
            premium: false,
            attach_menu_enabled: false,
            bot_can_edit: false,
            close_friend: false,
            stories_hidden: false,
            stories_unavailable: false,
            contact_require_premium: false,
            bot_business: false,
            bot_has_main_app: false,
            bot_forum_view: false,
            bot_forum_can_manage_topics: false,
            bot_can_manage_bots: false,
            bot_guestchat: false,
            bot_guard: false,
            id: 1,
            access_hash: None,
            first_name: None,
            last_name: None,
            username: None,
            phone: None,
            photo: None,
            status: None,
            bot_info_version: None,
            restriction_reason: None,
            bot_inline_placeholder: None,
            lang_code: None,
            emoji_status: None,
            usernames: None,
            stories_max_id: None,
            color: None,
            profile_color: None,
            bot_active_users: None,
            bot_verification_icon: None,
            send_paid_messages_stars: None,
        }
    }

    #[test]
    fn the_badge_line_carries_the_handle_and_every_flag_the_user_wears() {
        let info = user(
            &empty_user_full(),
            Some(&tl::types::User {
                username: Some("alice".to_string()),
                bot: true,
                verified: true,
                scam: true,
                fake: true,
                premium: true,
                ..plain_user()
            }),
        );

        assert_eq!(
            info.subtitle.first().map(String::as_str),
            Some("@alice · bot · verified · SCAM · FAKE · premium"),
            "`SCAM` and `FAKE` are Telegram's own warning labels about who the reader is talking \
             to; dropping one, or letting the handle and the badges land on separate lines, would \
             quietly hide a warning the account is entitled to see"
        );
    }

    #[test]
    fn a_user_wearing_no_badges_gets_no_line_rather_than_an_empty_one() {
        let info = user(&empty_user_full(), Some(&plain_user()));

        assert!(
            info.subtitle.is_empty(),
            "an empty subtitle line is a blank row under the name that reads as a failed render, \
             which is why `subtitle_line` refuses to push one: {:?}",
            info.subtitle
        );
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
                        rank: None,
                    }
                    .into(),
                    tl::types::ChatParticipant {
                        user_id: 2,
                        inviter_id: 1,
                        date: 0,
                        rank: None,
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
}
