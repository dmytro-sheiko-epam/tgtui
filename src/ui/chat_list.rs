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
            // Digits between two spaces, so `len` is the display width here.
            let badge = (dialog.unread > 0).then(|| format!(" {} ", dialog.unread));
            // ASCII, so the width arithmetic below stays honest in terminals that render an
            // emoji speaker or pushpin as two columns.
            let marks = match (dialog.pinned, dialog.muted) {
                (true, true) => "^~",
                (true, false) => "^ ",
                (false, true) => "~ ",
                (false, false) => "",
            };
            let used = badge.as_deref().map_or(0, str::len) + marks.len();
            let name_width = inner_width.saturating_sub(used);
            let name = truncate(&dialog.name, name_width);
            let preview = truncate(&dialog.preview, inner_width);

            // The badge is pushed to the right edge by padding the name out to fill the row.
            let mut name_line = vec![Span::styled(
                format!("{name:<name_width$}"),
                Style::default().bold(),
            )];
            if !marks.is_empty() {
                name_line.push(Span::styled(marks, Style::default().fg(Color::DarkGray)));
            }
            if let Some(badge) = badge {
                name_line.push(Span::styled(
                    badge,
                    // A muted chat still counts, but it has already said it will not interrupt —
                    // so the count stays and the colour stops shouting.
                    if dialog.muted {
                        Style::default().fg(Color::Black).bg(Color::DarkGray)
                    } else {
                        Style::default().fg(Color::Black).bg(Color::Blue).bold()
                    },
                ));
            }

            ListItem::new(vec![
                Line::from(name_line),
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
