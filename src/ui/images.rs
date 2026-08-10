//! The terminal's graphics capability, plus a memo of already-encoded pictures.
//!
//! Encoding lives on this side of the app rather than in `App` because it is terminal-specific:
//! mapping pixels onto character cells needs the font's size in pixels, which only a capability
//! query knows. `App` holds the decoded image and nothing more.

use std::collections::HashMap;
use std::sync::Arc;

use image::DynamicImage;
use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use ratatui_image::sliced::SlicedProtocol;
use ratatui_image::{FontSize, Resize};

use crate::config::ImageMode;

/// Encoded pictures kept at once. A picture on screen is held at two sizes — inline and, once
/// the viewer has shown it, full screen — so this leaves room for both without re-encoding as
/// you toggle between them.
const MAX_CACHED_PROTOCOLS: usize = 48;

/// Rows a picture may occupy inline: two thirds of the transcript, so it dominates the view
/// without pushing the conversation off screen entirely. A fixed cap looked the same on a 20-row
/// terminal and an 80-row one, which is what made pictures feel too small.
///
/// `cap` is the `TGTUI_IMAGE_ROWS` override, for anyone who wants them smaller or larger.
pub fn inline_rows(viewport: usize, cap: Option<u16>) -> u16 {
    let rows = (viewport as u16).saturating_mul(2) / 3;
    cap.unwrap_or(u16::MAX).min(rows).max(1)
}

pub struct ImageStore {
    /// `None` when images are switched off or the terminal could not be queried; everything
    /// below then reports "no picture" and the transcript falls back to labels.
    picker: Option<Picker>,
    /// Keyed by size as well as message, because the same picture is held inline *and* full
    /// screen; one entry per message would re-encode on every trip into the viewer and back.
    cache: HashMap<(i32, Size), Cached>,
    /// Ticks on every lookup, so the least recently drawn entry is the one evicted.
    clock: u64,
}

struct Cached {
    protocol: SlicedProtocol,
    used: u64,
}

impl ImageStore {
    /// Query the terminal for a graphics protocol and font size.
    ///
    /// Must be called *after* the terminal is in raw mode: the query writes control sequences to
    /// stdout and reads the reply from stdin, which cooked mode would swallow.
    pub fn new(mode: ImageMode) -> Self {
        let picker = match mode {
            ImageMode::Off => None,
            ImageMode::Halfblocks => Some(Picker::halfblocks()),
            ImageMode::Auto => match Picker::from_query_stdio() {
                Ok(picker) => {
                    tracing::debug!(protocol = ?picker.protocol_type(), font = ?picker.font_size(), "graphics enabled");
                    Some(picker)
                }
                Err(err) => {
                    tracing::warn!(%err, "could not query the terminal; media stays labelled");
                    None
                }
            },
        };

        Self {
            picker,
            cache: HashMap::new(),
            clock: 0,
        }
    }

    /// A store that never draws anything, standing in for a terminal without graphics.
    /// `TGTUI_IMAGES=off` takes the same path through `new`.
    #[cfg(test)]
    pub fn disabled() -> Self {
        Self {
            picker: None,
            cache: HashMap::new(),
            clock: 0,
        }
    }

    /// A store with the library's fixed half-block font size, so cell arithmetic in tests does
    /// not depend on whatever terminal happens to run them.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            picker: Some(Picker::halfblocks()),
            cache: HashMap::new(),
            clock: 0,
        }
    }

    /// The cell box a picture of `pixels` will occupy, without needing the picture itself.
    ///
    /// Telegram states a photo's dimensions in the message, so the transcript can hold the rows
    /// open from the moment the message appears and not jump when the download lands.
    pub fn reserve(&self, pixels: (u32, u32), max_cols: u16, max_rows: u16) -> Option<Size> {
        fit(
            pixels,
            self.picker.as_ref()?.font_size(),
            max_cols,
            max_rows,
        )
    }

    /// Encode `image` if needed and report the cell box it occupies, or `None` when there is no
    /// way to draw it.
    ///
    /// Deliberately the same `fit` as `reserve`: a picture that changed size on arrival would
    /// shift the rows underneath it.
    pub fn prepare(
        &mut self,
        id: i32,
        image: &Arc<DynamicImage>,
        max_cols: u16,
        max_rows: u16,
    ) -> Option<Size> {
        let picker = self.picker.as_ref()?;
        let requested = fit(
            (image.width(), image.height()),
            picker.font_size(),
            max_cols,
            max_rows,
        )?;

        self.clock += 1;
        let clock = self.clock;

        if let Some(cached) = self.cache.get_mut(&(id, requested)) {
            cached.used = clock;
            return Some(requested);
        }

        // Encoding blocks, but it runs once per image and size. A full-screen picture is several
        // times the pixels of an inline one, so this is the cost to watch if a frame ever
        // stutters; `ratatui_image::thread` is the escape hatch.
        let protocol = match SlicedProtocol::new_with_resize(
            picker,
            image.as_ref().clone(),
            requested,
            Resize::Fit(None),
        ) {
            Ok(protocol) => protocol,
            Err(err) => {
                tracing::debug!(%err, id, "could not encode image for this terminal");
                return None;
            }
        };

        self.cache.insert(
            (id, requested),
            Cached {
                protocol,
                used: clock,
            },
        );
        self.evict();
        Some(requested)
    }

    /// The encoded picture for a message at a given size, once `prepare` has built it.
    pub fn protocol(&self, id: i32, size: Size) -> Option<&SlicedProtocol> {
        self.cache.get(&(id, size)).map(|cached| &cached.protocol)
    }

    fn evict(&mut self) {
        while self.cache.len() > MAX_CACHED_PROTOCOLS {
            let Some(oldest) = self
                .cache
                .iter()
                .min_by_key(|(_, cached)| cached.used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            self.cache.remove(&oldest);
        }
    }
}

/// Fit `pixels` into `max_cols` columns and `max_rows` rows, preserving proportions.
///
/// Never scales up, matching `Resize::Fit`, so a small sticker stays small instead of being
/// blown up into a blurry banner.
fn fit((width, height): (u32, u32), font: FontSize, max_cols: u16, max_rows: u16) -> Option<Size> {
    if width == 0 || height == 0 || max_cols == 0 || max_rows == 0 {
        return None;
    }

    let bound_width = max_cols as u32 * font.width as u32;
    let bound_height = max_rows as u32 * font.height as u32;

    let (width, height) = if width <= bound_width && height <= bound_height {
        (width, height)
    } else {
        let scale = f64::min(
            bound_width as f64 / width as f64,
            bound_height as f64 / height as f64,
        );
        (
            ((width as f64 * scale) as u32).max(1),
            ((height as f64 * scale) as u32).max(1),
        )
    };

    // A partly filled cell still needs a whole row, hence the rounding up.
    Some(Size::new(
        width.div_ceil(font.width as u32).max(1) as u16,
        height.div_ceil(font.height as u32).max(1) as u16,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The half-block picker's font size, which every test below is measured against.
    const FONT: FontSize = FontSize::new(10, 20);

    /// Rows a picture may use inline in a generously sized terminal.
    const ROWS: u16 = 32;

    fn image(width: u32, height: u32) -> Arc<DynamicImage> {
        Arc::new(DynamicImage::new_rgb8(width, height))
    }

    #[test]
    fn an_image_that_already_fits_keeps_its_natural_size() {
        let mut store = ImageStore::for_tests();

        let size = store.prepare(1, &image(100, 200), 80, ROWS).unwrap();

        assert_eq!(
            (size.width, size.height),
            (10, 10),
            "100x200 pixels at {FONT:?} per cell is 10 columns by 10 rows"
        );
    }

    #[test]
    fn a_tall_image_is_clamped_to_the_rows_it_was_offered() {
        let mut store = ImageStore::for_tests();

        // 40 rows tall at its natural size, past the cap.
        let size = store.prepare(1, &image(100, 800), 80, ROWS).unwrap();

        assert!(
            size.height <= ROWS,
            "a picture must leave room for the conversation, got {} rows",
            size.height
        );
    }

    #[test]
    fn a_taller_viewport_earns_a_taller_picture() {
        let mut store = ImageStore::for_tests();

        let small = store.prepare(1, &image(100, 800), 80, 8).unwrap();
        let large = store.prepare(1, &image(100, 800), 80, 32).unwrap();

        assert!(
            large.height > small.height,
            "the whole point of a relative cap is that a big terminal shows a big picture: \
             {small:?} vs {large:?}"
        );
    }

    #[test]
    fn a_picture_never_takes_more_than_two_thirds_of_the_transcript() {
        assert_eq!(inline_rows(45, None), 30);
        assert_eq!(inline_rows(20, None), 13);
        assert_eq!(
            inline_rows(1, None),
            1,
            "even a one-line transcript must ask for a drawable size"
        );
        assert_eq!(
            inline_rows(45, Some(8)),
            8,
            "TGTUI_IMAGE_ROWS must be able to make them smaller"
        );
        assert_eq!(
            inline_rows(6, Some(40)),
            4,
            "and must never push a picture past the space that exists"
        );
    }

    #[test]
    fn a_wide_image_is_clamped_to_the_columns_it_was_offered() {
        let mut store = ImageStore::for_tests();

        let size = store.prepare(1, &image(2000, 100), 30, ROWS).unwrap();

        assert!(
            size.width <= 30,
            "a picture must not spill past the transcript, got {} columns",
            size.width
        );
    }

    #[test]
    fn the_rows_held_open_are_the_rows_the_picture_ends_up_using() {
        let mut store = ImageStore::for_tests();

        // The dimensions Telegram states before the download, and the decoded image after.
        let held = store.reserve((900, 1600), 40, ROWS).unwrap();
        let used = store.prepare(1, &image(900, 1600), 40, ROWS).unwrap();

        assert_eq!(
            held, used,
            "a picture that changed size on arrival would shift the transcript under the reader"
        );
    }

    #[test]
    fn a_disabled_store_reports_no_picture_at_all() {
        let mut store = ImageStore::disabled();

        assert!(store.reserve((100, 200), 80, ROWS).is_none());
        assert!(store.prepare(1, &image(100, 200), 80, ROWS).is_none());
        assert!(store.protocol(1, Size::new(10, 10)).is_none());
    }

    #[test]
    fn a_transcript_with_no_room_left_draws_nothing() {
        let store = ImageStore::for_tests();

        assert!(
            store.reserve((100, 200), 0, ROWS).is_none(),
            "a pane too narrow for a single column has nowhere to put a picture"
        );
        assert!(
            store.reserve((100, 200), 80, 0).is_none(),
            "and neither has one with no rows to spare"
        );
    }

    #[test]
    fn encoding_happens_once_per_image_and_size() {
        let mut store = ImageStore::for_tests();
        let image = image(100, 200);

        let size = store.prepare(1, &image, 80, ROWS).unwrap();
        let first = store.cache[&(1, size)].used;
        store.prepare(1, &image, 80, ROWS);

        assert_eq!(store.cache.len(), 1);
        assert!(
            store.cache[&(1, size)].used > first,
            "a second lookup must refresh the entry rather than re-encode it"
        );
    }

    #[test]
    fn the_same_picture_can_be_held_at_two_sizes_at_once() {
        let mut store = ImageStore::for_tests();
        let image = image(400, 800);

        // How the transcript and then the full-screen viewer ask for the same photo.
        let inline = store.prepare(1, &image, 40, 12).unwrap();
        let full = store.prepare(1, &image, 120, 40).unwrap();

        assert_ne!(inline, full);
        assert!(
            store.protocol(1, inline).is_some() && store.protocol(1, full).is_some(),
            "both must survive, or every trip in and out of the viewer re-encodes"
        );
    }

    #[test]
    fn a_narrower_terminal_re_encodes_the_image() {
        let mut store = ImageStore::for_tests();
        let image = image(100, 200);

        let wide = store.prepare(1, &image, 80, ROWS).unwrap();
        let narrow = store.prepare(1, &image, 5, ROWS).unwrap();

        assert_eq!(wide.width, 10);
        assert_eq!(
            narrow.width, 5,
            "a resize must rebuild the protocol, not reuse the old one"
        );
    }

    #[test]
    fn the_cache_stays_bounded_as_a_photo_heavy_chat_scrolls() {
        let mut store = ImageStore::for_tests();
        let mut sizes = Vec::new();

        for id in 0..(MAX_CACHED_PROTOCOLS as i32 * 2) {
            sizes.push(store.prepare(id, &image(100, 200), 80, ROWS).unwrap());
        }

        assert_eq!(store.cache.len(), MAX_CACHED_PROTOCOLS);
        assert!(
            store.protocol(0, sizes[0]).is_none(),
            "the least recently drawn picture is the one that goes"
        );
        let last = MAX_CACHED_PROTOCOLS * 2 - 1;
        assert!(store.protocol(last as i32, sizes[last]).is_some());
    }
}
