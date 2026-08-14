//! Choosing where a forwarded message is going: a popup over the two panes.
//!
//! Floats like the action menus rather than replacing the screen, and for the same reason — the
//! transcript underneath is the context for what is about to be sent — so the area has to be
//! [`Clear`]ed first or the pane behind would show through.
//!
//! Unlike those menus this one is typed into. Plain characters go into the filter, which is why
//! `App::handle_main_key` claims it before either menu and why it navigates with arrows only.

use ratatui::prelude::*;
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

use crate::app::App;
use crate::ui::widgets::{centered, pane};

/// How much of the screen the popup asks for, and the floors below which it stops shrinking.
const WIDTH: u16 = 46;
const MAX_ROWS: u16 = 12;

/// Borders, the filter line, and the rule under it.
const CHROME: u16 = 4;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = &app.forward else {
        return;
    };
    let matches = app.forward_matches();

    let rows = (matches.len() as u16).clamp(1, MAX_ROWS);
    let popup = centered(area, WIDTH, rows + CHROME);
    let block = pane("Forward to", true);
    let inner = block.inner(popup);

    frame.render_widget(Clear, popup);
    frame.render_widget(block, popup);

    if inner.height < 2 {
        // A terminal too short for both parts shows neither rather than a filter with nothing
        // under it, which would look like a chat list of one empty row.
        return;
    }

    let [filter_area, list_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);

    let filter = if picker.filter.is_empty() {
        Span::styled("type to search", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(picker.filter.as_str())
    };
    frame.render_widget(Paragraph::new(Line::from(filter)), filter_area);

    if matches.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "no chat by that name",
                Style::default().fg(Color::DarkGray),
            ))),
            list_area,
        );
        return;
    }

    let items: Vec<ListItem> = matches
        .iter()
        .map(|&index| {
            let item = &app.dialogs.items[index];
            // The folder is worth saying: the picker searches the whole pool, so a hit can be a
            // chat the list on screen is not currently showing.
            let suffix = if item.archived { "  (archive)" } else { "" };
            ListItem::new(Line::from(vec![
                Span::raw(format!(" {}", item.name)),
                Span::styled(suffix, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let list = List::new(items).highlight_style(
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );

    let mut state = ListState::default();
    state.select(Some(picker.selected.min(matches.len() - 1)));

    frame.render_stateful_widget(list, list_area, &mut state);
}
