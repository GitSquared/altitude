use ratatui::layout::{Constraint, Flex, Layout, Rect};

pub fn centered_width(area: Rect, width: u16, height_percent: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    Layout::vertical([Constraint::Percentage(height_percent)])
        .flex(Flex::Center)
        .areas::<1>(area)[0]
}
