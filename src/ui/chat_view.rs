//! Right pane: the transcript of the open chat plus the compose box.
//!
//! The transcript is anchored to the bottom, and `ChatBuffer::scroll` counts lines *up* from
//! there. That is what lets older messages be prepended without the viewport jumping.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui_image::sliced::{SignedPosition, SlicedImage};

use crate::app::{App, ChatViewMetrics, Focus};
use crate::state::chat_buffer::{ChatBuffer, ChatMessage};
use crate::ui::images::{ImageStore, inline_rows};
use crate::ui::text::wrap;
use crate::ui::widgets::pane;

/// A pause at least this long starts a new sender header even within one sender's run.
const GROUP_GAP_MINUTES: i64 = 5;

/// Width of the `HH:MM ` stamp that precedes a sender name. Message bodies are indented by
/// exactly this much so they line up with the name rather than with the timestamp.
const TIME_WIDTH: usize = 6;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App, images: &mut ImageStore) {
    let [transcript_area, compose_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(area);

    render_transcript(frame, transcript_area, app, images);
    render_compose(frame, compose_area, app);
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &mut App, images: &mut ImageStore) {
    let focused = app.focus == Focus::Messages;
    let title = app
        .dialogs
        .items
        .iter()
        .find(|item| Some(item.peer.id) == app.open_chat)
        .map(|item| item.name.clone())
        .unwrap_or_else(|| "Messages".to_string());

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
        build_transcript(buffer, inner.width as usize, max_rows, images)
    } else {
        Transcript::from_lines(vec![Line::from(Span::styled(
            "loading messages...",
            Style::default().fg(Color::Yellow),
        ))])
    };

    let total = transcript.lines.len();
    let max_scroll = total.saturating_sub(viewport);
    let scroll = buffer.scroll.min(max_scroll);

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
        let Some(protocol) = images.protocol(placement.message_id, placement.size) else {
            continue;
        };
        let Some(position) = placement.position(start, viewport) else {
            continue;
        };
        frame.render_widget(SlicedImage::new(protocol, position), inner);
    }

    app.request_visible_photos(&transcript.photos_on_screen(start, end));
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
        self.photos
            .iter()
            .filter(|(_, line)| (start..end).contains(line))
            .map(|(id, _)| *id)
            .collect()
    }
}

fn build_transcript(
    buffer: &ChatBuffer,
    width: usize,
    max_rows: u16,
    images: &mut ImageStore,
) -> Transcript {
    // Bodies hang under the sender name, past the timestamp column.
    let body_width = width.saturating_sub(TIME_WIDTH);
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
    for message in &buffer.messages {
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

        // A photo holds its rows open from the moment the message appears, so the transcript
        // doesn't shift when the download lands. The label sits on the top row until it does,
        // and stays there for good if the picture can't be drawn at all.
        let reserved = message.photo.as_ref().and_then(|photo| {
            let drawn = photo
                .image()
                .and_then(|image| images.prepare(message.id, image, body_width as u16, max_rows));
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
            // empty one adds nothing but a stray blank line.
            (Some(_), Some(())) if message.text.is_empty() => None,
            (Some(_), Some(())) => Some(message.text.clone()),
            // Nothing can be drawn here: fall back to the flattened label the client has
            // always shown for media.
            (Some(photo), None) if message.text.is_empty() => Some(photo.label.to_string()),
            (Some(photo), None) => Some(format!("{} {}", photo.label, message.text)),
            (None, _) => Some(message.text.clone()),
        };

        if let Some(body) = body {
            for line in wrap(&body, body_width) {
                lines.push(Line::from(format!("{:TIME_WIDTH$}{line}", "")));
            }
        }
        previous = Some(message);
    }

    Transcript {
        lines,
        images: placements,
        photos,
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

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if focused {
            Color::Cyan
        } else {
            Color::DarkGray
        }));

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

        let lines = build_transcript(&buffer, 40, ROWS, &mut ImageStore::disabled()).lines;
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

        let lines = build_transcript(&buffer, 40, ROWS, &mut ImageStore::disabled()).lines;
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

        let transcript = build_transcript(&buffer, 40, ROWS, &mut ImageStore::for_tests());

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

        let before = build_transcript(&waiting, 40, ROWS, &mut images)
            .lines
            .len();
        let after = build_transcript(&arrived, 40, ROWS, &mut images)
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

        let transcript = build_transcript(&buffer, 40, ROWS, &mut ImageStore::for_tests());
        let text = transcript.lines[1].to_string();

        assert!(transcript.images.is_empty(), "there is nothing to draw yet");
        assert!(text.contains("[photo]"), "expected a label, got {text:?}");
    }

    #[test]
    fn a_caption_survives_the_picture_replacing_its_label() {
        let buffer = buffer_of(vec![loaded_photo_message(1, "look at this", 100, 200)]);

        let transcript = build_transcript(&buffer, 40, ROWS, &mut ImageStore::for_tests());
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

        let transcript = build_transcript(&buffer, 40, ROWS, &mut ImageStore::disabled());

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

        let transcript = build_transcript(&buffer, 40, ROWS, &mut ImageStore::for_tests());
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

        let headers = build_transcript(&buffer, 40, ROWS, &mut ImageStore::disabled())
            .lines
            .iter()
            .filter(|line| line.spans.iter().any(|s| s.content.contains("Alice")))
            .count();

        assert_eq!(headers, 2);
    }
}
