//! Right pane: the transcript of the open chat plus the compose box.
//!
//! The transcript is anchored to the bottom, and `ChatBuffer::scroll` counts lines *up* from
//! there. That is what lets older messages be prepended without the viewport jumping.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, ChatViewMetrics, Focus};
use crate::state::chat_buffer::ChatBuffer;
use crate::ui::text::wrap;
use crate::ui::widgets::pane;

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
    // Two columns of indent for message bodies, so senders stand out.
    let body_width = width.saturating_sub(2);
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

    for message in &buffer.messages {
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
                message.local_time().format("%H:%M ").to_string(),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(who, who_style),
        ]));

        for line in wrap(&message.text, body_width) {
            lines.push(Line::from(format!("  {line}")));
        }
    }

    lines
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
