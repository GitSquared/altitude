use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::Line,
    widgets::Paragraph,
};

use super::palette;

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
