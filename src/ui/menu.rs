//! The chat action menu: a popup over the two panes.
//!
//! Unlike the picture viewer this floats rather than replacing the screen — the list underneath is
//! the context for what the menu is about to do, and hiding it would leave the user confirming an
//! action against a chat they can no longer see. That means the area has to be [`Clear`]ed first,
//! or the pane behind would show through the gaps.

use ratatui::prelude::*;
use ratatui::widgets::{Clear, List, ListItem, ListState, Paragraph};

use crate::app::ChatMenu;
use crate::ui::text::wrap;
use crate::ui::widgets::{centered, pane};

/// Columns the popup wants around its widest label: two borders and a space either side.
const CHROME: u16 = 4;

/// A floor, so a menu of short labels still reads as a menu rather than a sliver.
const MIN_WIDTH: u16 = 24;

/// How wide the confirmation asks to be. Questions name a chat and run longer than labels do.
const CONFIRM_WIDTH: u16 = 54;

pub fn render(frame: &mut Frame, area: Rect, menu: &ChatMenu) {
    match menu.prompt() {
        Some(prompt) => render_confirm(frame, area, &prompt),
        None => render_actions(frame, area, menu),
    }
}

fn render_actions(frame: &mut Frame, area: Rect, menu: &ChatMenu) {
    let labels: Vec<&str> = menu
        .actions
        .iter()
        .map(|action| action.label(menu.kind))
        .collect();

    let widest = labels.iter().map(|label| label.len()).max().unwrap_or(0) as u16;
    let popup = centered(
        area,
        widest.saturating_add(CHROME).max(MIN_WIDTH),
        labels.len() as u16 + 2,
    );

    let block = pane(&menu.name, true);
    let items: Vec<ListItem> = labels
        .into_iter()
        .zip(&menu.actions)
        .map(|(label, action)| {
            // The ones that ask before they run are also the ones worth hesitating over, so they
            // are the ones that look different.
            let style = if action.is_destructive() {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(format!(" {label}"), style)))
        })
        .collect();

    let list = List::new(items).block(block).highlight_style(
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );

    let mut state = ListState::default();
    state.select(Some(menu.selected));

    frame.render_widget(Clear, popup);
    frame.render_stateful_widget(list, popup, &mut state);
}

fn render_confirm(frame: &mut Frame, area: Rect, prompt: &str) {
    // Sized to the wrapped question rather than a guess, so a long chat name cannot push the
    // `y/n` line out of the box and leave the user with no visible way to answer.
    let width = CONFIRM_WIDTH.min(area.width);
    let inner_width = width.saturating_sub(CHROME) as usize;
    let question = wrap(prompt, inner_width.max(1));

    let popup = centered(area, width, question.len() as u16 + 4);

    let mut lines: Vec<Line> = question
        .into_iter()
        .map(|line| Line::from(Span::styled(format!(" {line}"), Style::default().bold())))
        .collect();
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " y  yes    n  no",
        Style::default().fg(Color::DarkGray),
    )));

    let block = pane("Confirm", true).border_style(Style::default().fg(Color::Red));

    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(lines).block(block), popup);
}
