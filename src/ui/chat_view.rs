//! Right pane: the transcript of the open chat plus the compose box.
//!
//! The transcript is anchored to the bottom, and `ChatBuffer::scroll` counts lines *up* from
//! there. That is what lets older messages be prepended without the viewport jumping.

use std::ops::Range;

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_image::sliced::{SignedPosition, SlicedImage};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, ChatViewMetrics, Focus};
use crate::state::chat_buffer::{ChatBuffer, ChatMessage, ReplyPreview};
use crate::ui::images::{ImageKey, ImageStore, inline_rows};
use crate::ui::text::wrap;
use crate::ui::widgets::pane;

/// A pause at least this long starts a new sender header even within one sender's run.
const GROUP_GAP_MINUTES: i64 = 5;

/// Width of the `HH:MM ` stamp that precedes a sender name. Message bodies are indented by
/// exactly this much so they line up with the name rather than with the timestamp.
const TIME_WIDTH: usize = 6;

/// Columns a read receipt claims at the right edge. `✓` is padded out to `✓✓`'s width so a message
/// doesn't shift sideways the moment it is read.
const TICK_WIDTH: usize = 2;

/// The receipt column plus the space before it. Outgoing bodies wrap short by this much whether or
/// not this particular message has been read yet — that is what keeps a message's line count, and
/// so `scroll`, independent of its read state.
const TICK_GUTTER: usize = TICK_WIDTH + 1;

/// The cursor's highlight. A background rather than a gutter glyph: the six columns to the left of
/// a body are not free — a sender header spends five of them on `HH:MM`.
const SELECTION: Style = Style::new().bg(Color::Indexed(238));

/// What a reply quotes its parent behind.
const QUOTE_MARK: &str = "┌ ";

/// A reply whose parent has not arrived yet, and one whose parent is gone for good. Both take the
/// row a resolved quote would, so the transcript's height does not change when a fetch lands.
const QUOTE_PENDING: &str = "…";
const QUOTE_MISSING: &str = "message unavailable";

/// The single line a reply's parent is quoted on.
///
/// Exactly one line, whatever state the lookup is in — that is the whole contract. `scroll` and
/// `metrics.total_lines` count lines, so a quote that grew from a placeholder to a resolved parent
/// would shift the viewport under the reader the moment the fetch came back. Same discipline as
/// `ImageStore::reserve` and `prepare` sharing `fit`.
fn quote_line(preview: Option<ReplyPreview>, width: usize) -> Line<'static> {
    let body = match preview {
        Some(preview) => {
            let text = preview.text.replace('\n', " ");
            let text = if text.trim().is_empty() {
                QUOTE_MISSING
            } else {
                text.trim()
            };
            match preview.sender {
                Some(sender) => format!("{sender}: {text}"),
                None => text.to_string(),
            }
        }
        None => QUOTE_PENDING.to_string(),
    };

    // Truncated by display width rather than by chars, so a quote full of wide glyphs still stops
    // at the pane edge instead of wrapping onto a second row.
    let room = width.saturating_sub(TIME_WIDTH + QUOTE_MARK.width());
    let mut quoted = String::new();
    for ch in body.chars() {
        if quoted.width() + ch.to_string().width() > room {
            break;
        }
        quoted.push(ch);
    }

    Line::from(Span::styled(
        format!("{:TIME_WIDTH$}{QUOTE_MARK}{quoted}", ""),
        Style::default().fg(Color::DarkGray),
    ))
}

/// `✓` for delivered, `✓✓` for read.
///
/// U+2713 rather than the heavier U+2714: the latter carries the Emoji property, so a terminal
/// that gives it emoji presentation paints two cells while `unicode_width` still reports one, and
/// the receipt column would drift.
fn tick_span(read: bool) -> Span<'static> {
    let (glyph, colour) = if read {
        ("✓✓", Color::Cyan)
    } else {
        ("✓", Color::DarkGray)
    };
    Span::styled(format!("{glyph:>TICK_WIDTH$}"), Style::default().fg(colour))
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, images: &mut ImageStore) {
    let [transcript_area, compose_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(area);

    render_transcript(frame, transcript_area, app, images);
    render_compose(frame, compose_area, app);
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &mut App, images: &mut ImageStore) {
    let focused = app.focus == Focus::Messages;
    // The read watermark rides along on the lookup the pane title already does, which is what
    // keeps `ChatBuffer` — and so the whole message cache — ignorant of read state.
    let (title, read_up_to) = app
        .dialogs
        .items
        .iter()
        .find(|item| Some(item.peer.id) == app.open_chat)
        .map(|item| (item.name.clone(), item.read_outbox_max_id))
        .unwrap_or_else(|| ("Messages".to_string(), None));

    let block = pane(&title, focused);
    let inner = block.inner(area);
    let viewport = inner.height as usize;

    let Some(buffer) = app.open_buffer() else {
        frame.render_widget(
            Paragraph::new("Select a chat on the left.")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        app.metrics = ChatViewMetrics::default();
        return;
    };

    // A picture is sized against the transcript it sits in, so it grows with the terminal.
    let max_rows = inline_rows(viewport, app.image_rows);

    let transcript = if buffer.loaded {
        build_transcript(buffer, read_up_to, inner.width as usize, max_rows, images)
    } else {
        Transcript::from_lines(vec![Line::from(Span::styled(
            "loading messages...",
            Style::default().fg(Color::Yellow),
        ))])
    };

    let total = transcript.lines.len();
    let max_scroll = total.saturating_sub(viewport);
    let mut scroll = buffer.scroll.min(max_scroll);

    // The cursor steps in messages; the viewport moves in lines. Only the transcript just built
    // knows which rows a message covers, so bringing the selection back into view happens here
    // rather than in the key handler — the same direction `metrics` and the clamped `scroll`
    // already flow, and `event::run` draws after handling keys, so it lands in the frame the user
    // sees rather than the one after.
    if std::mem::take(&mut app.scroll_to_selection)
        && let Some(range) = &transcript.selection
    {
        scroll = scroll_onto(range, total, viewport, scroll);
    }

    app.metrics = ChatViewMetrics {
        total_lines: total,
        viewport,
    };
    // A resize can leave the stored offset past the end, so write the clamped value back.
    if let Some(buffer) = app.open_chat.and_then(|id| app.chats.get_mut(&id)) {
        buffer.scroll = scroll;
    }

    let start = max_scroll - scroll;
    let end = (start + viewport).min(total);
    let visible = transcript.lines[start..end].to_vec();

    frame.render_widget(Paragraph::new(visible).block(block), area);

    // Pictures go on last: the paragraph above has just painted blank cells over the rows they
    // reserved, and the image widget marks its own cells skipped so the backend diff leaves
    // them alone afterwards.
    for placement in &transcript.images {
        let Some(protocol) =
            images.protocol(ImageKey::Message(placement.message_id), placement.size)
        else {
            continue;
        };
        let Some(position) = placement.position(start, viewport) else {
            continue;
        };
        frame.render_widget(SlicedImage::new(protocol, position), inner);
    }

    app.request_visible_photos(&transcript.photos_on_screen(start, end));
    app.request_reply_targets(&transcript.replies_on_screen(start, end));
}

/// The transcript as rendered: wrapped lines, plus where the pictures sit among them.
#[derive(Debug, Default)]
struct Transcript {
    lines: Vec<Line<'static>>,
    /// Pictures ready to draw, with the rows they cover.
    images: Vec<Placement>,
    /// Every message carrying a photo and the line its body starts on, drawn or not. This is
    /// what decides which downloads to ask for.
    photos: Vec<(i32, usize)>,
    /// Parents quoted on screen whose text is not in the buffer, and the line the quote sits on.
    /// This is what decides which parents to fetch, exactly as `photos` does for downloads.
    unresolved_replies: Vec<(i32, usize)>,
    /// The rows the cursor's message covers, when select mode is on and it is still in the buffer.
    /// Only the selected one is recorded: nothing else needs a message's line range, and keeping a
    /// range per message would be a second thing to hold in step with `lines`.
    selection: Option<Range<usize>>,
}

/// The scroll offset that brings `range` into view, or the current one when it already is.
///
/// Offsets count *up from the bottom*, so the arithmetic runs the other way round from the usual:
/// a larger `scroll` shows earlier lines. A message taller than the viewport shows its top, where
/// the sender header is, rather than its tail.
fn scroll_onto(range: &Range<usize>, total: usize, viewport: usize, current: usize) -> usize {
    let max_scroll = total.saturating_sub(viewport);
    let start = max_scroll.saturating_sub(current);

    let wanted = if range.start < start {
        max_scroll.saturating_sub(range.start)
    } else if range.end > start + viewport {
        total.saturating_sub(range.end)
    } else {
        current
    };
    wanted.min(max_scroll)
}

/// A picture's slot in the transcript. `line` is an index into `Transcript::lines`, and exactly
/// `size.height` blank lines were pushed for it — the two must stay in step or `scroll` and the
/// infinite-scroll prefetch, which are both denominated in lines, quietly drift.
#[derive(Debug, Clone, Copy)]
struct Placement {
    message_id: i32,
    line: usize,
    size: Size,
}

impl Placement {
    /// Where to draw, relative to the top of the viewport, or `None` when entirely off screen.
    ///
    /// The y may be negative: a picture scrolled half off the top renders the remainder rather
    /// than blinking out, which is the whole reason for using a sliced protocol.
    fn position(&self, start: usize, viewport: usize) -> Option<SignedPosition> {
        let y = self.line as isize - start as isize;
        if y >= viewport as isize || y + self.size.height as isize <= 0 {
            return None;
        }
        Some(SignedPosition::from((TIME_WIDTH as i16, y as i16)))
    }
}

impl Transcript {
    fn from_lines(lines: Vec<Line<'static>>) -> Self {
        Self {
            lines,
            ..Self::default()
        }
    }

    /// Ids of the photo messages showing in `start..end`, whether or not their picture has
    /// arrived — an undownloaded one is exactly what needs requesting.
    fn photos_on_screen(&self, start: usize, end: usize) -> Vec<i32> {
        on_screen(&self.photos, start, end)
    }

    /// The same, for reply parents still waiting to be looked up.
    fn replies_on_screen(&self, start: usize, end: usize) -> Vec<i32> {
        on_screen(&self.unresolved_replies, start, end)
    }
}

fn on_screen(rows: &[(i32, usize)], start: usize, end: usize) -> Vec<i32> {
    rows.iter()
        .filter(|(_, line)| (start..end).contains(line))
        .map(|(id, _)| *id)
        .collect()
}

fn build_transcript(
    buffer: &ChatBuffer,
    read_up_to: Option<i32>,
    width: usize,
    max_rows: u16,
    images: &mut ImageStore,
) -> Transcript {
    // Bodies hang under the sender name, past the timestamp column.
    let body_width = width.saturating_sub(TIME_WIDTH);
    // Receipts need a column of their own. A pane too narrow to spare one drops them rather than
    // letting them eat the text.
    let read_up_to = read_up_to.filter(|_| body_width > TICK_GUTTER);
    let outgoing_width = match read_up_to {
        Some(_) => body_width - TICK_GUTTER,
        None => body_width,
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut placements: Vec<Placement> = Vec::new();
    let mut photos: Vec<(i32, usize)> = Vec::new();

    if buffer.loading_older {
        lines.push(Line::from(Span::styled(
            "loading older messages...",
            Style::default().fg(Color::Yellow),
        )));
    } else if !buffer.has_more_older {
        lines.push(Line::from(Span::styled(
            "— beginning of conversation —",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let mut previous: Option<&ChatMessage> = None;
    let mut selection: Option<Range<usize>> = None;
    let mut unresolved_replies: Vec<(i32, usize)> = Vec::new();
    for message in &buffer.messages {
        let from = lines.len();
        if starts_new_group(previous, message) {
            let (who, who_style) = if message.outgoing {
                ("you".to_string(), Style::default().fg(Color::Green).bold())
            } else {
                (
                    message.sender.clone().unwrap_or_else(|| "—".to_string()),
                    Style::default().fg(Color::Cyan).bold(),
                )
            };

            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<TIME_WIDTH$}", message.local_time().format("%H:%M")),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(who, who_style),
            ]));
        }

        // A reply carries its parent on one line above it, whether or not the parent has been
        // found. Above the picture as well as the text, because the reply is to the whole message.
        if let Some(parent) = message.reply_to {
            let preview = buffer.reply_preview(parent);
            if preview.is_none() {
                unresolved_replies.push((parent, lines.len()));
            }
            lines.push(quote_line(preview, width));
        }

        // A blank body is a service message we don't spell out, and a receipt hanging off an empty
        // line is just a tick floating in space.
        let receipted = message.outgoing && (message.photo.is_some() || !message.text.is_empty());
        let tick = read_up_to
            .filter(|_| receipted)
            // Telegram never marks a single message read; there is only this per-chat watermark.
            .map(|max_id| tick_span(message.id <= max_id));

        // A photo holds its rows open from the moment the message appears, so the transcript
        // doesn't shift when the download lands. The label sits on the top row until it does,
        // and stays there for good if the picture can't be drawn at all.
        let reserved = message.photo.as_ref().and_then(|photo| {
            let drawn = photo.image().and_then(|image| {
                images.prepare(
                    ImageKey::Message(message.id),
                    image,
                    body_width as u16,
                    max_rows,
                )
            });
            // No way to show it is no reason to fetch it: with images off, or in a terminal
            // that reported no graphics, media stays a label and costs no bandwidth.
            let size =
                drawn.or_else(|| images.reserve(photo.pixels, body_width as u16, max_rows))?;

            photos.push((message.id, lines.len()));

            // Exactly `size.height` rows. The scroll offset counts lines, so a picture claiming
            // more or fewer rows than it covers would drift the viewport under the reader.
            let waiting = if drawn.is_some() { "" } else { photo.label };
            lines.push(Line::from(format!("{:TIME_WIDTH$}{waiting}", "")));
            lines.extend((1..size.height).map(|_| Line::default()));

            if drawn.is_some() {
                placements.push(Placement {
                    message_id: message.id,
                    line: lines.len() - size.height as usize,
                    size,
                });
            }
            Some(())
        });

        let body = match (&message.photo, reserved) {
            // The rows above already carry the label, so only the caption is left — and an
            // empty one adds nothing but a stray blank line. A picture of our own still needs a
            // line to hang its receipt from, though: the rows above belong to the picture and are
            // painted over by the image widget, so the tick can't live on them.
            (Some(_), Some(())) if message.text.is_empty() => tick.is_some().then(String::new),
            (Some(_), Some(())) => Some(message.text.clone()),
            // Nothing can be drawn here: fall back to the flattened label the client has
            // always shown for media.
            (Some(photo), None) if message.text.is_empty() => Some(photo.label.to_string()),
            (Some(photo), None) => Some(format!("{} {}", photo.label, message.text)),
            (None, _) => Some(message.text.clone()),
        };

        if let Some(body) = body {
            let wrap_width = if tick.is_some() {
                outgoing_width
            } else {
                body_width
            };
            // `wrap` always yields at least one line, even for an empty body.
            let wrapped = wrap(&body, wrap_width);
            let last = wrapped.len() - 1;
            for (n, line) in wrapped.into_iter().enumerate() {
                match (&tick, n == last) {
                    (Some(tick), true) => {
                        // Padded by display width, not by char count, so a body holding wide
                        // glyphs still lands its receipt on the pane edge.
                        let pad = wrap_width.saturating_sub(line.width());
                        lines.push(Line::from(vec![
                            Span::raw(format!("{:TIME_WIDTH$}{line}{:pad$} ", "", "")),
                            tick.clone(),
                        ]));
                    }
                    _ => lines.push(Line::from(format!("{:TIME_WIDTH$}{line}", ""))),
                }
            }
        }

        // The highlight is painted on afterwards rather than woven into the pushes above, which is
        // what makes it provably free of line accounting: it can only restyle and pad rows that
        // already exist. `scroll` and `metrics.total_lines` are denominated in lines, so a cursor
        // that added or dropped one would drift the viewport every time it moved — the same
        // discipline as `ImageStore::reserve` and the reserved tick column.
        //
        // A photo's reserved rows get styled too and are then painted over by `SlicedImage`, so on
        // a picture the highlight only shows on the caption or receipt line below it.
        if buffer.selected == Some(message.id) {
            for line in &mut lines[from..] {
                let pad = width.saturating_sub(line.width());
                if pad > 0 {
                    line.spans.push(Span::raw(" ".repeat(pad)));
                }
                line.style = SELECTION;
            }
            selection = Some(from..lines.len());
        }

        previous = Some(message);
    }

    Transcript {
        lines,
        images: placements,
        photos,
        unresolved_replies,
        selection,
    }
}

/// A run of messages from one sender shares a single header, the way chat clients group them.
/// A long enough pause starts a new group so the timestamp stays useful.
fn starts_new_group(previous: Option<&ChatMessage>, message: &ChatMessage) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    previous.outgoing != message.outgoing
        || previous.sender != message.sender
        || (message.date - previous.date).num_minutes().abs() >= GROUP_GAP_MINUTES
}

fn render_compose(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Messages;
    let has_chat = app.open_chat.is_some();

    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }));

    // What Enter will do, as a title rather than an extra row: the box is a fixed three lines, so
    // there is nowhere to put a line without taking one off the transcript. The two are mutually
    // exclusive — `start_reply` cancels an edit, and an edit cannot change what it replies to.
    if app.editing.is_some() {
        block = block.title(Span::styled(
            " Editing — Esc to abandon ",
            Style::default().fg(Color::Yellow),
        ));
    } else if let Some(parent) = app.replying_to {
        let who = app
            .open_buffer()
            .and_then(|buffer| buffer.reply_preview(parent))
            .and_then(|preview| preview.sender)
            .unwrap_or_else(|| "message".to_string());
        block = block.title(Span::styled(
            format!(" Replying to {who} — Esc to cancel "),
            Style::default().fg(Color::Cyan),
        ));
    }

    let content = if !has_chat {
        Span::styled("", Style::default())
    } else if app.compose.is_empty() && !focused {
        Span::styled(
            "Tab to write a message",
            Style::default().fg(Color::DarkGray),
        )
    } else {
        Span::raw(app.compose.as_str())
    };

    frame.render_widget(Paragraph::new(Line::from(content)).block(block), area);

    if focused && has_chat {
        frame.set_cursor_position(Position::new(
            area.x + 1 + app.compose.chars().count() as u16,
            area.y + 1,
        ));
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;
    use crate::test_support::{loaded_photo_message, message, page, peer, photo_message};

    fn at(id: i32, sender: Option<&str>, outgoing: bool, minute: i64) -> ChatMessage {
        ChatMessage {
            date: message(id, "").date + TimeDelta::minutes(minute),
            outgoing,
            sender: sender.map(str::to_string),
            ..message(id, &format!("message {id}"))
        }
    }

    /// Rows a picture may use in these tests, standing in for a generous transcript.
    const ROWS: u16 = 32;

    /// A buffer holding exactly `messages`, with no history markers to shift the line numbers.
    fn buffer_of(messages: Vec<ChatMessage>) -> ChatBuffer {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(Vec::new());
        buffer.has_more_older = true;
        buffer.messages.extend(messages);
        buffer
    }

    /// The invariant the whole cursor design hangs on. `scroll` and `metrics.total_lines` count
    /// lines, so a highlight that cost or saved a row would shift the viewport every time the
    /// cursor moved — and it moves on every keystroke.
    #[test]
    fn highlighting_a_message_does_not_change_the_line_count() {
        let mut buffer = buffer_of(page(10, 5).into_iter().rev().collect());
        let plain = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled());

        for id in 6..=10 {
            buffer.selected = Some(id);
            let marked = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled());
            assert_eq!(
                marked.lines.len(),
                plain.lines.len(),
                "selecting message {id} changed the transcript's height"
            );
        }
    }

    #[test]
    fn the_highlight_covers_the_selected_message_and_nothing_else() {
        let mut buffer = buffer_of(page(10, 5).into_iter().rev().collect());
        buffer.selected = Some(8);

        let transcript = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled());
        let range = transcript
            .selection
            .clone()
            .expect("a selected message that is in the buffer must report its rows");

        for (n, line) in transcript.lines.iter().enumerate() {
            assert_eq!(
                line.style == SELECTION,
                range.contains(&n),
                "line {n} is styled the wrong way round for a selection of {range:?}"
            );
        }
    }

    #[test]
    fn a_cursor_on_a_message_that_is_gone_reports_no_rows() {
        let mut buffer = buffer_of(page(10, 5).into_iter().rev().collect());
        buffer.selected = Some(999);

        let transcript = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled());

        assert!(
            transcript.selection.is_none(),
            "nothing to scroll onto means nothing must be scrolled"
        );
    }

    #[test]
    fn a_selection_already_on_screen_is_left_where_it_is() {
        // Rows 4..6 of a 10-line transcript, with a 10-line viewport showing all of it.
        assert_eq!(scroll_onto(&(4..6), 10, 10, 0), 0);
    }

    #[test]
    fn a_selection_above_the_viewport_scrolls_up_to_show_its_first_row() {
        // 30 lines, a 10-row viewport pinned to the bottom: rows 20..30 are showing.
        // Offsets count up from the bottom, so putting row 2 at the top means scrolling 18.
        assert_eq!(scroll_onto(&(2..4), 30, 10, 0), 18);
    }

    #[test]
    fn a_selection_below_the_viewport_scrolls_down_just_far_enough() {
        // Scrolled 18 up, rows 2..12 are showing; row 25 is well below.
        // Landing its last row on the bottom edge means an offset of 30 - 26.
        assert_eq!(scroll_onto(&(24..26), 30, 10, 18), 4);
    }

    /// A picture plus its caption can be taller than the pane. Showing the tail would put the
    /// sender header off screen, which is the part that says whose message it is.
    #[test]
    fn a_message_taller_than_the_viewport_shows_its_top() {
        assert_eq!(
            scroll_onto(&(0..25), 30, 10, 0),
            20,
            "an oversized message must be scrolled to its first row, not its last"
        );
    }

    /// A reply whose parent is in the buffer.
    fn reply(id: i32, text: &str, to: i32) -> ChatMessage {
        ChatMessage {
            reply_to: Some(to),
            ..message(id, text)
        }
    }

    /// The reply counterpart of the reserve/prepare invariant. The quote starts as a placeholder
    /// and becomes a real one when the fetch lands; if that changed the height, the viewport would
    /// jump under the reader at an arbitrary moment.
    #[test]
    fn an_unresolved_reply_quote_claims_the_same_row_as_a_resolved_one() {
        let mut buffer = buffer_of(vec![reply(11, "the usual place", 7)]);
        let pending = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled());

        buffer.reply_previews.insert(
            7,
            ReplyPreview {
                sender: Some("Bob".to_string()),
                text: "where should we meet?".to_string(),
            },
        );
        let resolved = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled());

        assert_eq!(
            pending.lines.len(),
            resolved.lines.len(),
            "the quote must claim its row before the parent arrives, not after"
        );
    }

    #[test]
    fn a_reply_quotes_its_parent_above_itself() {
        let buffer = buffer_of(vec![
            message(7, "where should we meet?"),
            reply(11, "the usual place", 7),
        ]);

        let lines = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled()).lines;
        let quote = lines
            .iter()
            .position(|line| line.spans.iter().any(|s| s.content.contains(QUOTE_MARK)))
            .expect("the reply should carry a quote line");
        let body = lines
            .iter()
            .position(|line| {
                line.spans
                    .iter()
                    .any(|s| s.content.contains("the usual place"))
            })
            .expect("the reply's own text should be there too");

        assert!(
            quote < body,
            "the quote introduces the reply, so it goes above it"
        );
        assert!(
            lines[quote]
                .spans
                .iter()
                .any(|s| s.content.contains("where should we meet?")),
            "a parent already in the buffer needs no fetch to be quoted"
        );
    }

    #[test]
    fn a_parent_that_is_not_loaded_is_asked_for_once_it_is_on_screen() {
        let buffer = buffer_of(vec![reply(11, "the usual place", 7)]);

        let transcript = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled());

        assert_eq!(
            transcript.replies_on_screen(0, transcript.lines.len()),
            [7],
            "a quote with nothing to quote is exactly what needs fetching"
        );
    }

    #[test]
    fn a_parent_already_in_the_buffer_is_never_asked_for() {
        let buffer = buffer_of(vec![
            message(7, "where should we meet?"),
            reply(11, "yes", 7),
        ]);

        let transcript = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled());

        assert!(
            transcript
                .replies_on_screen(0, transcript.lines.len())
                .is_empty()
        );
    }

    #[test]
    fn a_quote_stops_at_the_pane_edge_rather_than_wrapping() {
        let long = "a".repeat(200);
        let line = quote_line(
            Some(ReplyPreview {
                sender: Some("Alice".to_string()),
                text: long,
            }),
            40,
        );

        assert!(
            line.width() <= 40,
            "a quote wider than the pane would wrap and cost a second row: {}",
            line.width()
        );
    }

    #[test]
    fn a_multi_line_parent_is_quoted_on_one_line() {
        let line = quote_line(
            Some(ReplyPreview {
                sender: None,
                text: "first\nsecond\nthird".to_string(),
            }),
            60,
        );

        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains('\n'),
            "a newline in a quote would be a second row"
        );
        assert!(text.contains("first second third"));
    }

    #[test]
    fn the_first_message_always_gets_a_header() {
        assert!(starts_new_group(None, &at(1, Some("Alice"), false, 0)));
    }

    #[test]
    fn a_run_from_one_sender_shares_a_single_header() {
        let first = at(1, Some("Alice"), false, 0);
        let second = at(2, Some("Alice"), false, 1);
        assert!(!starts_new_group(Some(&first), &second));
    }

    #[test]
    fn a_different_sender_starts_a_new_group() {
        let alice = at(1, Some("Alice"), false, 0);
        let bob = at(2, Some("Bob"), false, 1);
        assert!(starts_new_group(Some(&alice), &bob));
    }

    #[test]
    fn your_own_reply_starts_a_new_group() {
        // Same display name would otherwise merge an incoming message with your reply.
        let theirs = at(1, Some("Alice"), false, 0);
        let mine = at(2, Some("Alice"), true, 1);
        assert!(starts_new_group(Some(&theirs), &mine));
    }

    #[test]
    fn a_long_pause_starts_a_new_group_so_the_time_stays_useful() {
        let earlier = at(1, Some("Alice"), false, 0);
        let later = at(2, Some("Alice"), false, GROUP_GAP_MINUTES);
        assert!(starts_new_group(Some(&earlier), &later));
    }

    #[test]
    fn grouping_removes_the_repeated_headers() {
        let mut buffer = ChatBuffer::new(peer(1));
        // The fixture's messages all share a sender and timestamp, so they form one group.
        buffer.set_initial(page(10, 5));

        let lines = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled()).lines;
        let headers = lines
            .iter()
            .filter(|line| line.spans.iter().any(|s| s.content.contains("Alice")))
            .count();

        assert_eq!(
            headers, 1,
            "5 messages from one sender need only one header"
        );
        // 1 header + 5 bodies. There is no start-of-conversation marker because more
        // history remains, so the ungrouped rendering would have cost 4 extra lines.
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn a_body_starts_in_the_same_column_as_the_sender_name() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(vec![]);
        buffer.messages.push_back(at(1, Some("Alice"), false, 0));

        let lines = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled()).lines;
        let header = &lines[lines.len() - 2];
        let body = lines.last().unwrap().to_string();

        let name_column: usize = header.spans[..1].iter().map(|s| s.content.len()).sum();
        assert_eq!(
            body.len() - body.trim_start().len(),
            name_column,
            "the body indent must match the width of the timestamp, or wrapped text \
             hangs under the clock instead of under the name"
        );
    }

    // -- pictures ------------------------------------------------------------
    //
    // Graphics escape sequences never land in a `TestBackend` cell grid, so these cover the
    // layout and the arithmetic instead; the pixels themselves are checked by eye.

    #[test]
    fn a_photo_reserves_exactly_the_rows_its_picture_covers() {
        // 100x200 pixels is 10 columns by 10 rows at the half-block font size.
        let buffer = buffer_of(vec![loaded_photo_message(1, "", 100, 200)]);

        let transcript = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::for_tests());

        let placement = transcript
            .images
            .first()
            .expect("the picture must be drawn");
        assert_eq!((placement.size.width, placement.size.height), (10, 10));
        assert_eq!(
            transcript.lines.len() - placement.line,
            placement.size.height as usize,
            "the blank rows and the picture must agree, or `scroll` drifts by the difference"
        );
    }

    #[test]
    fn a_photo_holds_its_rows_open_before_the_download_lands() {
        let waiting = buffer_of(vec![photo_message(1, "", 100, 200)]);
        let arrived = buffer_of(vec![loaded_photo_message(1, "", 100, 200)]);
        let mut images = ImageStore::for_tests();

        let before = build_transcript(&waiting, None, 40, ROWS, &mut images)
            .lines
            .len();
        let after = build_transcript(&arrived, None, 40, ROWS, &mut images)
            .lines
            .len();

        assert_eq!(
            before, after,
            "the transcript must not shift under the reader when a picture arrives"
        );
    }

    #[test]
    fn a_photo_still_waiting_says_what_it_is() {
        let buffer = buffer_of(vec![photo_message(1, "look at this", 100, 200)]);

        let transcript = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::for_tests());
        let text = transcript.lines[1].to_string();

        assert!(transcript.images.is_empty(), "there is nothing to draw yet");
        assert!(text.contains("[photo]"), "expected a label, got {text:?}");
    }

    #[test]
    fn a_caption_survives_the_picture_replacing_its_label() {
        let buffer = buffer_of(vec![loaded_photo_message(1, "look at this", 100, 200)]);

        let transcript = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::for_tests());
        let rendered = transcript
            .lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();

        assert!(
            rendered.iter().any(|line| line.contains("look at this")),
            "the caption must outlive the label:\n{rendered:#?}"
        );
        assert!(
            !rendered.iter().any(|line| line.contains("[photo]")),
            "the label is redundant once the picture is on screen:\n{rendered:#?}"
        );
    }

    #[test]
    fn a_terminal_that_cannot_draw_renders_the_label_it_always_did() {
        let buffer = buffer_of(vec![loaded_photo_message(1, "look at this", 100, 200)]);

        let transcript = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled());

        assert!(transcript.images.is_empty());
        assert!(
            transcript.photos_on_screen(0, 2).is_empty(),
            "a picture that can never be shown must not be downloaded either"
        );
        assert_eq!(
            transcript.lines.len(),
            2,
            "one header and one label line, exactly as before images existed"
        );
        assert_eq!(
            transcript.lines[1].to_string().trim(),
            "[photo] look at this"
        );
    }

    #[test]
    fn a_photo_scrolled_half_off_the_top_keeps_drawing_its_remainder() {
        let placement = Placement {
            message_id: 1,
            line: 4,
            size: Size::new(10, 10),
        };

        // The viewport starts six lines below the picture's top row.
        let position = placement
            .position(10, 20)
            .expect("four rows are still visible");

        assert_eq!(
            (position.x, position.y),
            (TIME_WIDTH as i16, -6),
            "a negative offset is what lets a sliced protocol draw only the visible rows"
        );
    }

    #[test]
    fn a_photo_scrolled_clear_of_the_viewport_is_not_drawn() {
        let placement = Placement {
            message_id: 1,
            line: 4,
            size: Size::new(10, 10),
        };

        assert!(
            placement.position(14, 20).is_none(),
            "its last row sits exactly on the line above the viewport"
        );
        assert!(
            placement.position(0, 4).is_none(),
            "its first row sits exactly on the line below the viewport"
        );
        assert!(placement.position(13, 20).is_some());
        assert!(placement.position(0, 5).is_some());
    }

    #[test]
    fn only_the_photos_on_screen_are_asked_for() {
        let buffer = buffer_of(vec![
            photo_message(1, "", 100, 200),
            message(2, "just words"),
            photo_message(3, "", 100, 200),
        ]);

        let transcript = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::for_tests());
        let (_, second) = transcript.photos[1];

        assert_eq!(
            transcript.photos_on_screen(0, second),
            vec![1],
            "a photo below the fold costs bandwidth nobody asked for"
        );
        assert_eq!(
            transcript.photos_on_screen(0, transcript.lines.len()),
            vec![1, 3]
        );
    }

    #[test]
    fn a_message_sent_much_later_gets_its_own_header() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(vec![]);
        buffer.messages.push_back(at(1, Some("Alice"), false, 0));
        buffer.messages.push_back(at(2, Some("Alice"), false, 60));

        let headers = build_transcript(&buffer, None, 40, ROWS, &mut ImageStore::disabled())
            .lines
            .iter()
            .filter(|line| line.spans.iter().any(|s| s.content.contains("Alice")))
            .count();

        assert_eq!(headers, 2);
    }

    // -- read receipts -------------------------------------------------------

    /// The rendered lines of a chat holding one message of our own, read up to `read_up_to`.
    fn mine(text: &str, read_up_to: Option<i32>, width: usize) -> Vec<String> {
        let buffer = buffer_of(vec![ChatMessage {
            outgoing: true,
            sender: None,
            ..message(5, text)
        }]);
        build_transcript(
            &buffer,
            read_up_to,
            width,
            ROWS,
            &mut ImageStore::disabled(),
        )
        .lines
        .iter()
        .map(ToString::to_string)
        .collect()
    }

    #[test]
    fn an_outgoing_message_gets_one_tick_until_it_is_read() {
        let lines = mine("on my way", Some(4), 40);
        assert!(
            lines.last().unwrap().ends_with('✓'),
            "a sent message must say so: {lines:?}"
        );
        assert!(!lines.last().unwrap().ends_with("✓✓"));
    }

    #[test]
    fn a_message_the_other_side_has_read_gets_two_ticks() {
        let lines = mine("on my way", Some(5), 40);
        assert!(
            lines.last().unwrap().ends_with("✓✓"),
            "the watermark includes this id, so it has been read: {lines:?}"
        );
    }

    #[test]
    fn an_incoming_message_never_gets_a_tick() {
        let buffer = buffer_of(vec![at(5, Some("Alice"), false, 0)]);

        let lines = build_transcript(&buffer, Some(99), 40, ROWS, &mut ImageStore::disabled())
            .lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        assert!(
            !lines.iter().any(|line| line.contains('✓')),
            "a receipt on someone else's message would claim to know what they saw: {lines:?}"
        );
    }

    #[test]
    fn a_chat_where_a_read_receipt_means_nothing_shows_no_ticks() {
        let lines = mine("posted", None, 40);
        assert!(
            !lines.iter().any(|line| line.contains('✓')),
            "a broadcast channel has readers, not a recipient: {lines:?}"
        );
    }

    #[test]
    fn reserving_the_tick_column_keeps_the_line_count_independent_of_the_read_state() {
        // A body long enough to wrap, so a width change would show up as a line count change.
        let text = "the quick brown fox jumps over the lazy dog and keeps on going for a while";
        for width in 12..60 {
            assert_eq!(
                mine(text, Some(0), width).len(),
                mine(text, Some(99), width).len(),
                "a message changing from ✓ to ✓✓ must not change how many lines it occupies, or \
                 `scroll` moves under the reader the moment the other side opens the chat"
            );
        }
    }

    #[test]
    fn a_tick_sits_at_the_pane_edge_without_overflowing_it() {
        let text = "the quick brown fox jumps over the lazy dog";
        for width in 12..60 {
            for read_up_to in [Some(0), Some(99)] {
                for line in mine(text, read_up_to, width) {
                    assert!(
                        line.width() <= width,
                        "{line:?} is wider than the {width}-column pane"
                    );
                }
            }
        }
    }

    #[test]
    fn a_pane_too_narrow_for_a_tick_drops_it_rather_than_the_text() {
        // TIME_WIDTH + TICK_GUTTER leaves nothing for the body at this width.
        let lines = mine("hi", Some(99), TIME_WIDTH + TICK_GUTTER);
        assert!(
            !lines.iter().any(|line| line.contains('✓')),
            "the text has to win the last few columns: {lines:?}"
        );
    }

    #[test]
    fn a_picture_of_your_own_with_no_caption_still_gets_a_line_for_its_receipt() {
        let buffer = buffer_of(vec![ChatMessage {
            outgoing: true,
            sender: None,
            ..loaded_photo_message(5, "", 100, 200)
        }]);

        let transcript = build_transcript(&buffer, Some(5), 40, ROWS, &mut ImageStore::for_tests());
        let placement = transcript
            .images
            .first()
            .expect("the picture must be drawn");

        assert!(
            transcript.lines.last().unwrap().to_string().contains("✓✓"),
            "the image rows are painted over by the widget, so the tick needs a line below them"
        );
        assert_eq!(
            transcript.lines.len() - placement.line,
            placement.size.height as usize + 1,
            "exactly one line more than the picture's own rows — the receipt sits where a \
             caption would, and the picture must still claim exactly the rows it covers"
        );
    }
}
