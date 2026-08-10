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
use crate::state::dialog_list::DialogSummary;
use crate::state::media::{PhotoState, photo_ref};
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
        date: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
        photo: None,
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
        name: name.to_string(),
        preview: format!("last from {name}"),
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
