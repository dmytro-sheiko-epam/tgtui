//! Which media a message carries, and whether any of it can be shown as a picture.
//!
//! Resolving happens once, at ingest, because a `Message` is not kept around: by the time the
//! transcript wants to draw a photo the only thing left is what landed in `ChatMessage`.

use std::sync::Arc;

use grammers_client::media::{Document, Media, Photo, PhotoSize};
use image::DynamicImage;

/// Largest edge worth downloading. The transcript renders into a handful of character cells, so
/// fetching the original of a 4000px photo would spend bandwidth on pixels no terminal can show.
/// Telegram's `x` and `y` thumbnail types sit at or below this.
const MAX_SOURCE_EDGE: u32 = 1280;

/// Refuse anything larger, both when choosing a source here and while downloading in the actor.
/// A document whose declared mime type lies would otherwise be pulled into memory in full.
pub const MAX_PHOTO_BYTES: usize = 4 * 1024 * 1024;

/// The only sticker format that is a still image. Lottie (`application/x-tgsticker`) and WebM
/// stickers would both need an animation decoder.
const STILL_STICKER_MIME: &str = "image/webp";

/// A message's media, resolved into something the actor can download and the transcript can size
/// before a single byte has arrived.
#[derive(Debug, Clone)]
pub struct PhotoRef {
    pub source: PhotoSource,
    /// Pixel dimensions, taken from the message itself. Knowing them up front is what lets the
    /// transcript reserve the right number of rows, so the view doesn't jump when the image lands.
    pub pixels: (u32, u32),
    /// What to print when the picture can't be shown: the terminal has no graphics protocol, the
    /// download is still in flight, or it failed.
    pub label: &'static str,
    pub state: PhotoState,
}

/// What to hand to `Client::iter_download`. Both variants implement `Downloadable`, but they are
/// distinct types, so the actor matches rather than storing a trait object.
#[derive(Debug, Clone, PartialEq)]
pub enum PhotoSource {
    /// A server-side thumbnail — already sized for a terminal, and all a video can offer.
    Thumb(PhotoSize),
    /// The file itself, for image documents and stickers whose thumbnails are too small to bother.
    File(Document),
}

#[derive(Debug, Clone, Default)]
pub enum PhotoState {
    #[default]
    Pending,
    Loading,
    Ready(Arc<DynamicImage>),
    /// Terminal. Retrying would re-request on every frame, because the trigger is visibility.
    Failed,
}

impl PhotoRef {
    /// Whether there is a picture to draw right now, as opposed to a label to print.
    pub fn image(&self) -> Option<&Arc<DynamicImage>> {
        match &self.state {
            PhotoState::Ready(image) => Some(image),
            _ => None,
        }
    }
}

/// Resolve a message's media into a picture, or `None` when it is only worth labelling.
pub fn photo_ref(media: &Media) -> Option<PhotoRef> {
    let (source, pixels) = match media {
        Media::Photo(photo) => pick_thumb(photo.thumbs())?,
        Media::Sticker(sticker) => {
            if sticker.is_animated() || sticker.document.mime_type() != Some(STILL_STICKER_MIME) {
                return None;
            }
            // Stickers are small by construction, so the file itself is the right source; their
            // thumbnails are 100px placeholders.
            let pixels = resolution(&sticker.document)?;
            (PhotoSource::File(sticker.document.clone()), pixels)
        }
        Media::Document(document) => document_source(document)?,
        _ => return None,
    };

    Some(PhotoRef {
        source,
        pixels,
        label: media_label(media),
        state: PhotoState::Pending,
    })
}

/// Images sent as files show themselves; videos and GIFs show their cover frame.
fn document_source(document: &Document) -> Option<(PhotoSource, (u32, u32))> {
    let mime = document.mime_type()?;

    // An animated GIF arrives as a video document, so it goes down the cover-frame path with
    // everything else that moves.
    let still_image = mime.starts_with("image/") && !mime.eq_ignore_ascii_case("image/gif");
    if still_image
        && let Some(pixels) = resolution(document)
        && document.size().is_some_and(|size| size <= MAX_PHOTO_BYTES)
    {
        return Some((PhotoSource::File(document.clone()), pixels));
    }

    if mime.starts_with("image/") || mime.starts_with("video/") {
        return pick_thumb(document.thumbs());
    }
    None
}

/// A peer's current profile picture, carried with the id of the picture itself.
#[derive(Debug, Clone)]
pub struct Avatar {
    /// `tl::types::Photo.id`, which is globally unique and immutable: it names *this* picture
    /// forever, the way a message id does. Carried because the encoding cache has to be keyed by
    /// it — a peer id names whatever the peer is wearing today, so a peer who changes their photo
    /// while tgtui is running would be drawn from the entry the old one left behind.
    pub id: i64,
    pub picture: PhotoRef,
}

/// A peer's current profile picture, as a downloadable reference.
///
/// Deliberately shares `pick_thumb` with [`photo_ref`], the way `dialog_list::is_muted` is shared
/// by the dialog seed and the live update: two functions choosing a source independently would
/// eventually disagree about what "sized for a terminal" means.
///
/// `None` for a peer with no picture, and for `photoEmpty`, which has no sizes at all.
pub fn avatar_ref(photo: &Photo) -> Option<Avatar> {
    let (source, pixels) = pick_thumb(photo.thumbs())?;
    Some(Avatar {
        // Read after `pick_thumb` has answered, which it only does for a `photo` — `Photo::id`
        // unwraps the raw enum and a `photoEmpty` has no picture to name.
        id: photo.id(),
        picture: PhotoRef {
            source,
            pixels,
            // Never printed: the info screen falls back to the peer's initials rather than a
            // label, because a bordered box with `[photo]` in it reads as a broken picture.
            label: "[photo]",
            state: PhotoState::Pending,
        },
    })
}

/// Choose which thumbnail to download: the largest that still fits `MAX_SOURCE_EDGE`, falling
/// back to the smallest available so an unusually sized photo still shows something.
fn pick_thumb(thumbs: Vec<PhotoSize>) -> Option<(PhotoSource, (u32, u32))> {
    let mut sized: Vec<(PhotoSize, (u32, u32))> = thumbs
        .into_iter()
        .filter_map(|thumb| {
            let pixels = thumb_pixels(&thumb)?;
            Some((thumb, pixels))
        })
        .collect();
    if sized.is_empty() {
        return None;
    }

    sized.sort_by_key(|(_, (width, height))| (*width).max(*height));
    let index = sized
        .iter()
        .rposition(|(_, (width, height))| (*width).max(*height) <= MAX_SOURCE_EDGE)
        .unwrap_or(0);

    let (thumb, pixels) = sized.swap_remove(index);
    Some((PhotoSource::Thumb(thumb), pixels))
}

fn thumb_pixels(thumb: &PhotoSize) -> Option<(u32, u32)> {
    match thumb {
        PhotoSize::Size(size) => dimensions(size.width, size.height),
        PhotoSize::Cached(size) => dimensions(size.width, size.height),
        PhotoSize::Progressive(size) => dimensions(size.width, size.height),
        // `Stripped` and `Path` are blurred placeholders that carry no dimensions of their own,
        // and `Empty` has nothing behind it at all.
        PhotoSize::Stripped(_) | PhotoSize::Path(_) | PhotoSize::Empty(_) => None,
    }
}

fn resolution(document: &Document) -> Option<(u32, u32)> {
    let (width, height) = document.resolution()?;
    dimensions(width, height)
}

fn dimensions(width: i32, height: i32) -> Option<(u32, u32)> {
    let (width, height) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    (width > 0 && height > 0).then_some((width, height))
}

pub fn media_label(media: &Media) -> &'static str {
    match media {
        Media::Photo(_) => "[photo]",
        Media::Document(_) => "[file]",
        Media::Sticker(_) => "[sticker]",
        Media::Contact(_) => "[contact]",
        Media::Poll(_) => "[poll]",
        Media::Geo(_) | Media::GeoLive(_) => "[location]",
        Media::Dice(_) => "[dice]",
        Media::Venue(_) => "[venue]",
        Media::WebPage(_) => "[link]",
        // `Media` is non-exhaustive; new kinds get a neutral label rather than breaking the build.
        _ => "[media]",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{document_media, photo_media, sticker_media, stripped_thumb, thumb};
    use grammers_client::tl;

    fn source_pixels(media: &Media) -> Option<(u32, u32)> {
        photo_ref(media).map(|photo| photo.pixels)
    }

    #[test]
    fn a_photo_offers_the_largest_thumbnail_a_terminal_can_use() {
        let media = photo_media(&[
            thumb("m", 320, 240),
            thumb("x", 800, 600),
            thumb("w", 2560, 1920),
        ]);

        assert_eq!(
            source_pixels(&media),
            Some((800, 600)),
            "the 2560px original would spend bandwidth on pixels no terminal can show"
        );
    }

    #[test]
    fn an_oversized_photo_still_offers_its_smallest_thumbnail() {
        let media = photo_media(&[thumb("w", 2560, 1920), thumb("y", 1600, 1200)]);

        assert_eq!(
            source_pixels(&media),
            Some((1600, 1200)),
            "when nothing fits the cap, the smallest is better than showing nothing"
        );
    }

    #[test]
    fn a_photo_with_only_a_blurred_placeholder_is_left_as_a_label() {
        // A stripped size is a 30px blur with no dimensions of its own, so there is nothing to
        // reserve rows for and nothing worth showing.
        let media = photo_media(&[stripped_thumb()]);

        assert!(photo_ref(&media).is_none());
        assert_eq!(media_label(&media), "[photo]");
    }

    #[test]
    fn a_still_sticker_is_shown_but_an_animated_one_is_only_labelled() {
        let still = sticker_media("image/webp", false);
        let lottie = sticker_media("application/x-tgsticker", true);

        assert_eq!(source_pixels(&still), Some((512, 512)));
        assert!(
            photo_ref(&lottie).is_none(),
            "a Lottie sticker would need an animation decoder"
        );
        assert_eq!(media_label(&lottie), "[sticker]");
    }

    #[test]
    fn an_image_document_is_shown_as_itself() {
        let media = document_media("image/png", Some((640, 480)), 200_000, &[]);

        assert_eq!(source_pixels(&media), Some((640, 480)));
        assert!(
            matches!(photo_ref(&media).unwrap().source, PhotoSource::File(_)),
            "a small image file is worth pulling in full rather than as a thumbnail"
        );
    }

    #[test]
    fn a_huge_image_document_falls_back_to_its_thumbnail() {
        let media = document_media(
            "image/png",
            Some((8000, 6000)),
            MAX_PHOTO_BYTES + 1,
            &[thumb("m", 320, 240)],
        );

        assert_eq!(source_pixels(&media), Some((320, 240)));
    }

    #[test]
    fn a_video_offers_its_cover_frame_rather_than_the_video() {
        let media = document_media(
            "video/mp4",
            Some((1920, 1080)),
            50_000_000,
            &[thumb("m", 320, 180)],
        );

        let photo = photo_ref(&media).expect("a video with a thumbnail can show its cover frame");
        assert_eq!(photo.pixels, (320, 180));
        assert!(matches!(photo.source, PhotoSource::Thumb(_)));
    }

    #[test]
    fn a_video_with_no_thumbnail_is_left_as_a_label() {
        let media = document_media("video/mp4", Some((1920, 1080)), 50_000_000, &[]);

        assert!(photo_ref(&media).is_none());
    }

    #[test]
    fn a_plain_file_is_never_mistaken_for_a_picture() {
        let media = document_media("application/pdf", None, 10_000, &[thumb("m", 320, 240)]);

        assert!(
            photo_ref(&media).is_none(),
            "a PDF has a page thumbnail, but showing it as the message would be misleading"
        );
        assert_eq!(media_label(&media), "[file]");
    }

    #[test]
    fn an_avatar_picks_a_thumbnail_the_same_way_a_message_photo_does() {
        let avatar = avatar_ref(&crate::test_support::profile_photo(&[
            crate::test_support::thumb("a", 160, 160),
            crate::test_support::thumb("x", 640, 640),
        ]))
        .expect("a profile photo with sizes is downloadable");

        assert_eq!(
            avatar.picture.pixels,
            (640, 640),
            "sharing `pick_thumb` with `photo_ref` is what keeps the two from disagreeing about \
             what a terminal-sized source is"
        );
        assert!(matches!(avatar.picture.state, PhotoState::Pending));
    }

    #[test]
    fn an_avatar_is_named_by_the_picture_rather_than_by_the_peer() {
        let old = avatar_ref(&crate::test_support::profile_photo_with_id(
            11,
            &[crate::test_support::thumb("x", 160, 160)],
        ))
        .expect("a profile photo with sizes is downloadable");
        let new = avatar_ref(&crate::test_support::profile_photo_with_id(
            22,
            &[crate::test_support::thumb("x", 160, 160)],
        ))
        .expect("a profile photo with sizes is downloadable");

        assert_ne!(
            old.id, new.id,
            "the id is what keys the encoding cache; if the same peer's new picture came back \
             under the old id, changing an avatar mid-session would keep drawing the old one"
        );
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
}
