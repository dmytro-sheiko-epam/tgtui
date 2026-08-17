//! The peer info screen: a profile, full screen.
//!
//! Takes the whole body rather than floating over the panes, the way the picture viewer does and
//! for the same reason — there is a picture and a paragraph of text to fit, and neither reads well
//! in a popup sized to a menu. Nothing behind it is context for what it is about to do, because it
//! is about to do nothing: this screen is read-only.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use ratatui_image::sliced::{SignedPosition, SlicedImage};

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

    // The header scrolls off first, a row at a time, so what is left of it is what it may claim —
    // holding the full height open would open a gap between it and the fields as they came up.
    let showing = header_rows.saturating_sub(scroll);
    let [header_area, body_area] =
        Layout::vertical([Constraint::Length(showing), Constraint::Min(0)]).areas(inner);

    if showing > 0 {
        draw_header(frame, header_area, app, images, avatar_size, scroll);
    }

    let skipped = scroll.saturating_sub(header_rows) as usize;
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

    let size = avatar
        .and_then(|avatar| images.reserve(avatar.pixels, AVATAR_COLS.min(width), AVATAR_ROWS));

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
            let [left, right] =
                Layout::horizontal([Constraint::Length(size.width + GUTTER), Constraint::Min(1)])
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
            && let Some(protocol) = images.protocol(ImageKey::Avatar(peer), size)
        {
            // Sliced from above, so the rows of the picture that have scrolled past the top are
            // the ones dropped rather than the ones at the bottom.
            let position = SignedPosition::from((0, -(scroll as i16)));
            frame.render_widget(SlicedImage::new(protocol, position), area);
            return;
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
                lines.push(Line::from(Span::styled(
                    line,
                    Style::default().fg(Color::Red),
                )));
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
