//! What a view says when it has nothing to show.
//!
//! One shape everywhere: a muted line, in the middle of the space it is given, so
//! an empty pane reads as empty rather than as a paragraph that failed to wrap.
//! No full stop — the line is the whole of what is there, and a sentence that has
//! nowhere to go next does not need to be closed.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::Paragraph,
};

use super::palette;

/// The line an empty state is written on, centred vertically in `area`.
///
/// Shared so a pane with a wait to show can put its own line on the same row
/// rather than inventing a second kind of nothing.
pub fn row(area: Rect) -> Rect {
    let [_, row, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);
    row
}

pub fn draw(frame: &mut Frame, area: Rect, message: &str) {
    frame.render_widget(
        Paragraph::new(Line::styled(message, palette::muted())).centered(),
        row(area),
    );
}
