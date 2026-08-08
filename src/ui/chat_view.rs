//! Right pane: the transcript of the open chat plus the compose box.
//!
//! The transcript is anchored to the bottom, and `ChatBuffer::scroll` counts lines *up* from
//! there. That is what lets older messages be prepended without the viewport jumping.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, ChatViewMetrics, Focus};
use crate::state::chat_buffer::{ChatBuffer, ChatMessage};
use crate::ui::text::wrap;
use crate::ui::widgets::pane;

/// A pause at least this long starts a new sender header even within one sender's run.
const GROUP_GAP_MINUTES: i64 = 5;

/// Width of the `HH:MM ` stamp that precedes a sender name. Message bodies are indented by
/// exactly this much so they line up with the name rather than with the timestamp.
const TIME_WIDTH: usize = 6;

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    let [transcript_area, compose_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(area);

    render_transcript(frame, transcript_area, app);
    render_compose(frame, compose_area, app);
}

fn render_transcript(frame: &mut Frame, area: Rect, app: &mut App) {
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

    let lines = if buffer.loaded {
        build_lines(buffer, inner.width as usize)
    } else {
        vec![Line::from(Span::styled(
            "loading messages...",
            Style::default().fg(Color::Yellow),
        ))]
    };

    let total = lines.len();
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
    let visible = lines[start..end].to_vec();

    frame.render_widget(Paragraph::new(visible).block(block), area);
}

fn build_lines(buffer: &ChatBuffer, width: usize) -> Vec<Line<'static>> {
    // Bodies hang under the sender name, past the timestamp column.
    let body_width = width.saturating_sub(TIME_WIDTH);
    let mut lines = Vec::new();

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

        for line in wrap(&message.text, body_width) {
            lines.push(Line::from(format!("{:TIME_WIDTH$}{line}", "")));
        }
        previous = Some(message);
    }

    lines
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
    use crate::test_support::{message, page, peer};

    fn at(id: i32, sender: Option<&str>, outgoing: bool, minute: i64) -> ChatMessage {
        ChatMessage {
            id,
            outgoing,
            sender: sender.map(str::to_string),
            text: format!("message {id}"),
            date: message(id, "").date + TimeDelta::minutes(minute),
        }
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

        let lines = build_lines(&buffer, 40);
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

        let lines = build_lines(&buffer, 40);
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

    #[test]
    fn a_message_sent_much_later_gets_its_own_header() {
        let mut buffer = ChatBuffer::new(peer(1));
        buffer.set_initial(vec![]);
        buffer.messages.push_back(at(1, Some("Alice"), false, 0));
        buffer.messages.push_back(at(2, Some("Alice"), false, 60));

        let headers = build_lines(&buffer, 40)
            .iter()
            .filter(|line| line.spans.iter().any(|s| s.content.contains("Alice")))
            .count();

        assert_eq!(headers, 2);
    }
}
