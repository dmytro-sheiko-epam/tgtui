//! Left pane: the list of conversations.

use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState};

use crate::app::{App, Focus};
use crate::ui::widgets::pane;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Chats;
    let block = pane("Chats", focused);
    let inner_width = block.inner(area).width as usize;

    let mut items: Vec<ListItem> = app
        .dialogs
        .items
        .iter()
        .map(|dialog| {
            let name = truncate(&dialog.name, inner_width);
            let preview = truncate(&dialog.preview, inner_width);
            ListItem::new(vec![
                Line::from(Span::styled(name, Style::default().bold())),
                Line::from(Span::styled(preview, Style::default().fg(Color::DarkGray))),
            ])
        })
        .collect();

    if app.dialogs.loading {
        items.push(ListItem::new(Line::from(Span::styled(
            "loading...",
            Style::default().fg(Color::Yellow),
        ))));
    } else if items.is_empty() {
        items.push(ListItem::new(Line::from(Span::styled(
            "no chats",
            Style::default().fg(Color::DarkGray),
        ))));
    }

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(if focused {
                Color::Blue
            } else {
                Color::DarkGray
            })
            .fg(Color::White),
    );

    // Rebuilt each frame: ratatui scrolls as needed to keep the selection on screen.
    let mut state = ListState::default();
    if !app.dialogs.items.is_empty() {
        state.select(Some(app.dialogs.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn truncate(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut chars: Vec<char> = text.chars().collect();
    if chars.len() <= width {
        return text.to_string();
    }
    chars.truncate(width.saturating_sub(1));
    chars.iter().collect::<String>() + "…"
}
