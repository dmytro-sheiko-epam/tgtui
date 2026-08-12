//! Left pane: the list of conversations.

use ratatui::prelude::*;
use ratatui::widgets::{List, ListItem, ListState, Paragraph};

use crate::app::{App, Focus};
use crate::state::dialog_list::FolderTab;
use crate::ui::widgets::pane;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    // The strip sits inside the pane border so the whole left column stays one frame.
    let [strip_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(area);

    render_tabs(frame, strip_area, app);
    render_list(frame, list_area, app);
}

fn render_list(frame: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == Focus::Chats;
    let block = pane("Chats", focused);
    let inner_width = block.inner(area).width as usize;
    let visible = app.dialogs.visible();

    let mut items: Vec<ListItem> = visible
        .iter()
        .map(|&index| &app.dialogs.items[index])
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

    if app.dialogs.cursor().loading {
        items.push(ListItem::new(Line::from(Span::styled(
            "loading...",
            Style::default().fg(Color::Yellow),
        ))));
    } else if items.is_empty() {
        // Which of the three it is matters: an empty custom folder is usually a folder still
        // filling as the main list pages in, and "no chats" would read as an account with none.
        let empty = match app.dialogs.tab {
            FolderTab::Main => "no chats",
            FolderTab::Archive => "no archived chats",
            FolderTab::Custom(_) => "nothing in this folder",
        };
        items.push(ListItem::new(Line::from(Span::styled(
            empty,
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
    if !visible.is_empty() {
        state.select(Some(app.dialogs.selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

/// The folder strip: `All`, the account's own folders, then `Archive`.
///
/// Windowed rather than truncated. The pane is 32 columns and an account can easily have more
/// folders than fit, so the active one is drawn first and its neighbours are added outwards until
/// the width runs out — the tab you are on is the one that must always be legible. `‹` and `›`
/// mark that there is more in that direction.
fn render_tabs(frame: &mut Frame, area: Rect, app: &App) {
    let tabs = app.dialogs.tabs();
    let active = app.dialogs.tab_index();
    let width = area.width as usize;

    // One column each is reserved for the two markers, so adding a tab can never push the marker
    // that announces it off the end.
    let budget = width.saturating_sub(2);
    let (mut first, mut last) = (active, active);
    let mut used = tab_width(&tabs[active].1);
    let mut forwards = true;
    loop {
        let can_grow_right = last + 1 < tabs.len();
        let can_grow_left = first > 0;
        if !can_grow_right && !can_grow_left {
            break;
        }
        // Alternating leaves the active tab near the middle of the strip, so both of its
        // neighbours are usually visible and `Ctrl+O`/`Ctrl+E` show where they lead.
        let right = if forwards {
            can_grow_right
        } else {
            !can_grow_left
        };
        let index = if right { last + 1 } else { first - 1 };

        let cost = tab_width(&tabs[index].1);
        if used + cost > budget {
            break;
        }
        used += cost;
        if right {
            last = index
        } else {
            first = index
        }
        forwards = !forwards;
    }

    let mut spans = vec![Span::styled(
        if first > 0 { "‹" } else { " " },
        Style::default().fg(Color::DarkGray),
    )];
    for (index, (_, title)) in tabs.iter().enumerate().take(last + 1).skip(first) {
        spans.push(Span::styled(
            format!(" {title} "),
            if index == active {
                Style::default().fg(Color::Black).bg(Color::Blue).bold()
            } else {
                Style::default().fg(Color::DarkGray)
            },
        ));
    }
    if last + 1 < tabs.len() {
        spans.push(Span::styled("›", Style::default().fg(Color::DarkGray)));
    }

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// A tab costs its title plus the space either side that keeps the highlight off the letters.
fn tab_width(title: &str) -> usize {
    title.chars().count() + 2
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
