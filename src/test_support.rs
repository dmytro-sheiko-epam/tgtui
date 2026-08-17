//! Fixtures for tests. Lets the state machine be exercised without a Telegram connection.

use std::sync::Arc;

use chrono::{TimeZone, Utc};
use grammers_client::media::Media;
use grammers_client::tl;
use grammers_session::types::{PeerAuth, PeerId, PeerRef};
use image::DynamicImage;
use tokio::sync::mpsc;

use crate::app::App;
use crate::state::chat_buffer::ChatMessage;
use crate::state::dialog_actions::DialogKind;
use crate::state::dialog_list::DialogSummary;
use crate::state::folders::{Folder, FolderRule};
use crate::state::media::{PhotoState, photo_ref};
use crate::state::peer_info::{InfoRow, PeerInfo};
use crate::telegram::TgCommand;

pub fn peer(id: i64) -> PeerRef {
    PeerRef {
        id: PeerId::user_unchecked(id),
        auth: PeerAuth::default(),
    }
}

/// A broadcast channel or supergroup, whose message ids restart at 1 per channel.
pub fn channel(id: i64) -> PeerRef {
    PeerRef {
        id: PeerId::channel_unchecked(id),
        auth: PeerAuth::default(),
    }
}

pub fn message(id: i32, text: &str) -> ChatMessage {
    ChatMessage {
        id,
        outgoing: false,
        sender: Some("Alice".to_string()),
        text: text.to_string(),
        raw_text: text.to_string(),
        date: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        photo: None,
        reply_to: None,
    }
}

/// A message carrying a photo of the given pixel size, still waiting to be downloaded.
pub fn photo_message(id: i32, caption: &str, width: u32, height: u32) -> ChatMessage {
    let media = photo_media(&[thumb("x", width as i32, height as i32)]);
    ChatMessage {
        photo: Some(photo_ref(&media).expect("the fixture thumbnail is downloadable")),
        ..message(id, caption)
    }
}

/// The same, with the picture already downloaded and decoded.
pub fn loaded_photo_message(id: i32, caption: &str, width: u32, height: u32) -> ChatMessage {
    let mut msg = photo_message(id, caption, width, height);
    msg.photo.as_mut().unwrap().state = PhotoState::Ready(gradient(width, height));
    msg
}

/// A decoded picture, as the actor would deliver one.
///
/// A vertical gradient rather than a flat colour: the half-blocks encoder collapses a cell whose
/// two halves match into a plain space, so a uniform image renders as nothing at all.
pub fn gradient(width: u32, height: u32) -> Arc<DynamicImage> {
    let image = image::RgbImage::from_fn(width.max(1), height.max(1), |_, y| {
        let shade = (y * 255 / height.max(1)) as u8;
        image::Rgb([shade, shade, shade])
    });
    Arc::new(DynamicImage::ImageRgb8(image))
}

/// A downloadable thumbnail of a given resolution.
pub fn thumb(photo_type: &str, width: i32, height: i32) -> tl::enums::PhotoSize {
    tl::enums::PhotoSize::Size(tl::types::PhotoSize {
        r#type: photo_type.to_string(),
        w: width,
        h: height,
        size: width * height / 8,
    })
}

/// The blurred inline placeholder Telegram attaches to every photo. Carries no dimensions.
pub fn stripped_thumb() -> tl::enums::PhotoSize {
    tl::enums::PhotoSize::PhotoStrippedSize(tl::types::PhotoStrippedSize {
        r#type: "i".to_string(),
        bytes: vec![0x01, 0x20, 0x20],
    })
}

pub fn photo_media(sizes: &[tl::enums::PhotoSize]) -> Media {
    let raw = tl::enums::MessageMedia::Photo(tl::types::MessageMediaPhoto {
        spoiler: false,
        live_photo: false,
        photo: Some(tl::enums::Photo::Photo(tl::types::Photo {
            has_stickers: false,
            id: 1,
            access_hash: 1,
            file_reference: Vec::new(),
            date: 0,
            sizes: sizes.to_vec(),
            video_sizes: None,
            dc_id: 2,
        })),
        ttl_seconds: None,
        video: None,
    });
    Media::from_raw(raw).expect("a photo is always renderable as media")
}

/// A profile photo as `users.getFullUser` delivers one: a bare `tl::enums::Photo` rather than the
/// `MessageMediaPhoto` a message carries.
pub fn profile_photo(sizes: &[tl::enums::PhotoSize]) -> grammers_client::media::Photo {
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

pub fn document_media(
    mime: &str,
    resolution: Option<(i32, i32)>,
    size: usize,
    thumbs: &[tl::enums::PhotoSize],
) -> Media {
    document(mime, size, thumbs, resolution.map(image_size_attr), None)
}

/// A sticker document. `animated` sets the Lottie attribute that marks a `.tgs` sticker.
pub fn sticker_media(mime: &str, animated: bool) -> Media {
    document(
        mime,
        30_000,
        &[],
        Some(image_size_attr((512, 512))),
        Some(animated),
    )
}

fn image_size_attr((w, h): (i32, i32)) -> tl::enums::DocumentAttribute {
    tl::enums::DocumentAttribute::ImageSize(tl::types::DocumentAttributeImageSize { w, h })
}

fn document(
    mime: &str,
    size: usize,
    thumbs: &[tl::enums::PhotoSize],
    resolution: Option<tl::enums::DocumentAttribute>,
    sticker: Option<bool>,
) -> Media {
    let mut attributes: Vec<tl::enums::DocumentAttribute> = resolution.into_iter().collect();
    if let Some(animated) = sticker {
        attributes.push(tl::enums::DocumentAttribute::Sticker(
            tl::types::DocumentAttributeSticker {
                mask: false,
                alt: "🙂".to_string(),
                stickerset: tl::enums::InputStickerSet::Empty,
                mask_coords: None,
            },
        ));
        if animated {
            attributes.push(tl::enums::DocumentAttribute::Animated);
        }
    }

    let raw = tl::enums::MessageMedia::Document(tl::types::MessageMediaDocument {
        nopremium: false,
        spoiler: false,
        video: false,
        round: false,
        voice: false,
        document: Some(tl::enums::Document::Document(tl::types::Document {
            id: 1,
            access_hash: 1,
            file_reference: Vec::new(),
            date: 0,
            mime_type: mime.to_string(),
            size: size as i64,
            thumbs: (!thumbs.is_empty()).then(|| thumbs.to_vec()),
            video_thumbs: None,
            dc_id: 2,
            attributes,
        })),
        alt_documents: None,
        video_cover: None,
        video_timestamp: None,
        ttl_seconds: None,
    });
    Media::from_raw(raw).expect("a document is always renderable as media")
}

/// A page as the actor delivers it: newest first.
pub fn page(newest_id: i32, count: i32) -> Vec<ChatMessage> {
    (0..count)
        .map(|offset| {
            let id = newest_id - offset;
            message(id, &format!("message {id}"))
        })
        .collect()
}

pub fn dialog(id: i64, name: &str) -> DialogSummary {
    DialogSummary {
        peer: peer(id),
        kind: DialogKind::User { is_self: false },
        name: name.to_string(),
        preview: format!("last from {name}"),
        read_outbox_max_id: Some(0),
        unread: 0,
        muted: false,
        pinned: false,
        blocked: false,
        archived: false,
        contact: false,
        bot: false,
    }
}

/// The same chat, sitting in folder 1 rather than folder 0.
pub fn archived_dialog(id: i64, name: &str) -> DialogSummary {
    DialogSummary {
        archived: true,
        ..dialog(id, name)
    }
}

/// A folder that gathers exactly the chats it names, which is the shape that makes a test about
/// tabs be about tabs rather than about the rule engine.
pub fn folder(title: &str, members: &[PeerId]) -> Folder {
    Folder {
        title: title.to_string(),
        rule: FolderRule {
            include: members.to_vec(),
            ..FolderRule::default()
        },
    }
}

/// The same, for a channel — whose bare id shares a number space with a user's.
///
/// The watermark is left in place. A real broadcast channel would have none, but these fields are
/// set independently here: the tests that use this fixture are about telling two peers with the
/// same bare id apart, and they need something to tell apart.
pub fn channel_dialog(id: i64, name: &str) -> DialogSummary {
    DialogSummary {
        peer: channel(id),
        kind: DialogKind::Channel,
        ..dialog(id, name)
    }
}

/// A megagroup: a channel-shaped peer that is still a group to the action menu.
pub fn group_dialog(id: i64, name: &str) -> DialogSummary {
    DialogSummary {
        peer: channel(id),
        kind: DialogKind::Group { megagroup: true },
        ..dialog(id, name)
    }
}

/// A message of our own, as the echo of a send or an update from another device delivers it.
pub fn outgoing(id: i32, text: &str) -> ChatMessage {
    ChatMessage {
        outgoing: true,
        sender: None,
        ..message(id, text)
    }
}

/// A dialog row as the server sends it, carrying the read state the summary is seeded from.
pub fn raw_dialog(read_outbox_max_id: i32, unread_count: i32) -> tl::enums::Dialog {
    raw_dialog_with(read_outbox_max_id, unread_count, false, None)
}

/// The same, with the pin flag and the mute deadline the notify state is seeded from.
pub fn raw_dialog_with(
    read_outbox_max_id: i32,
    unread_count: i32,
    pinned: bool,
    mute_until: Option<i32>,
) -> tl::enums::Dialog {
    tl::enums::Dialog::Dialog(tl::types::Dialog {
        pinned,
        unread_mark: false,
        view_forum_as_messages: false,
        peer: tl::enums::Peer::User(tl::types::PeerUser { user_id: 1 }),
        top_message: read_outbox_max_id.max(1),
        read_inbox_max_id: 0,
        read_outbox_max_id,
        unread_count,
        unread_mentions_count: 0,
        unread_reactions_count: 0,
        unread_poll_votes_count: 0,
        notify_settings: tl::enums::PeerNotifySettings::Settings(tl::types::PeerNotifySettings {
            show_previews: None,
            silent: None,
            mute_until,
            ios_sound: None,
            android_sound: None,
            other_sound: None,
            stories_muted: None,
            stories_hide_sender: None,
            stories_ios_sound: None,
            stories_android_sound: None,
            stories_other_sound: None,
        }),
        pts: None,
        draft: None,
        folder_id: None,
        ttl_period: None,
    })
}

/// A user as the archive fetch receives it, undressed, with the flags the folder rules read.
pub fn raw_user(id: i64, name: &str, contact: bool, bot: bool) -> tl::enums::User {
    tl::enums::User::User(tl::types::User {
        is_self: false,
        contact,
        mutual_contact: false,
        deleted: false,
        bot,
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
        id,
        access_hash: Some(id),
        first_name: Some(name.to_string()),
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
    })
}

/// A channel or megagroup, the two the same row tells apart by one flag.
pub fn raw_channel(id: i64, title: &str, megagroup: bool) -> tl::enums::Chat {
    tl::enums::Chat::Channel(tl::types::Channel {
        creator: false,
        left: false,
        broadcast: !megagroup,
        verified: false,
        megagroup,
        restricted: false,
        signatures: false,
        min: false,
        scam: false,
        has_link: false,
        has_geo: false,
        slowmode_enabled: false,
        call_active: false,
        call_not_empty: false,
        fake: false,
        gigagroup: false,
        noforwards: false,
        join_to_send: false,
        join_request: false,
        forum: false,
        stories_hidden: false,
        stories_hidden_min: false,
        stories_unavailable: false,
        signature_profiles: false,
        autotranslation: false,
        broadcast_messages_allowed: false,
        monoforum: false,
        forum_tabs: false,
        id,
        access_hash: Some(id),
        title: title.to_string(),
        username: None,
        photo: tl::enums::ChatPhoto::Empty,
        date: 0,
        restriction_reason: None,
        admin_rights: None,
        banned_rights: None,
        default_banned_rights: None,
        participants_count: None,
        usernames: None,
        stories_max_id: None,
        color: None,
        profile_color: None,
        emoji_status: None,
        level: None,
        subscription_until_date: None,
        bot_verification_icon: None,
        send_paid_messages_stars: None,
        linked_monoforum_id: None,
    })
}

/// The newest message in a chat, which is where an archived row's preview line comes from.
pub fn raw_message(peer: tl::enums::Peer, id: i32, text: &str) -> tl::enums::Message {
    tl::enums::Message::Message(tl::types::Message {
        out: false,
        mentioned: false,
        media_unread: false,
        silent: false,
        post: false,
        from_scheduled: false,
        legacy: false,
        edit_hide: false,
        pinned: false,
        noforwards: false,
        invert_media: false,
        offline: false,
        video_processing_pending: false,
        paid_suggested_post_stars: false,
        paid_suggested_post_ton: false,
        id,
        from_id: None,
        from_boosts_applied: None,
        from_rank: None,
        peer_id: peer,
        saved_peer_id: None,
        fwd_from: None,
        via_bot_id: None,
        via_business_bot_id: None,
        guestchat_via_from: None,
        reply_to: None,
        date: 1_700_000_000,
        message: text.to_string(),
        media: None,
        reply_markup: None,
        entities: None,
        views: None,
        forwards: None,
        replies: None,
        edit_date: None,
        post_author: None,
        grouped_id: None,
        reactions: None,
        restriction_reason: None,
        ttl_period: None,
        quick_reply_shortcut_id: None,
        effect: None,
        factcheck: None,
        report_delivery_until_date: None,
        paid_message_stars: None,
        suggested_post: None,
        schedule_repeat_period: None,
        summary_from_language: None,
        rich_message: None,
    })
}

/// The archive row, which stands for a group of chats and carries no read state of its own.
pub fn raw_folder() -> tl::enums::Dialog {
    tl::enums::Dialog::Folder(tl::types::DialogFolder {
        pinned: false,
        folder: tl::enums::Folder::Folder(tl::types::Folder {
            autofill_new_broadcasts: false,
            autofill_public_groups: false,
            autofill_new_correspondents: false,
            id: 1,
            title: "Archived".to_string(),
            photo: None,
        }),
        peer: tl::enums::Peer::User(tl::types::PeerUser { user_id: 1 }),
        top_message: 1,
        unread_muted_peers_count: 0,
        unread_unmuted_peers_count: 3,
        unread_muted_messages_count: 0,
        unread_unmuted_messages_count: 7,
    })
}

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
        blocked: None,
    }
}

/// An app plus the receiving end of the commands it issues.
pub fn app() -> (App, mpsc::UnboundedReceiver<TgCommand>) {
    let (tx, rx) = mpsc::unbounded_channel();
    (App::new(tx), rx)
}

/// Drain every command queued so far.
pub fn drain(rx: &mut mpsc::UnboundedReceiver<TgCommand>) -> Vec<TgCommand> {
    let mut commands = Vec::new();
    while let Ok(command) = rx.try_recv() {
        commands.push(command);
    }
    commands
}
