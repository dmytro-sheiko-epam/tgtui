//! Small pieces of chrome shared between screens.

use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders};

/// A titled border that highlights when the pane holds keyboard focus.
pub fn pane(title: &str, focused: bool) -> Block<'_> {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            format!(" {title} "),
            if focused {
                Style::default().fg(Color::Cyan).bold()
            } else {
                Style::default().fg(Color::Gray)
            },
        ))
}

/// Carve a centred rectangle of the given size out of `area`.
pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}
