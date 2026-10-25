//! The wait that has nothing to show but itself.
//!
//! A build, a sign-in being prepared: both are one line of what is happening, and
//! neither is a page to read. This is that line in the smallest box that holds
//! it, so a wait does not take the screen it is not using. A transfer is the one
//! wait with something to count, and it draws a bar instead.
//!
//! What cancels it is the caller's to know; the keys are the frame's.

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    text::Line,
    widgets::{Block, BorderType, Clear, Paragraph},
};

use crate::tui::widgets::{palette, utils::shimmer};

/// Draw the wait, and answer with the box it occupies.
pub fn draw(frame: &mut Frame, area: Rect, message: &str, tick: Option<usize>) -> Rect {
    // Wide enough for the line it is waiting on and a space either side, and no
    // wider.
    let width = (message.chars().count() as u16 + 4).min(area.width);
    let [row] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(area);
    let [popup] = Layout::vertical([Constraint::Length(3)])
        .flex(Flex::Center)
        .areas(row);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(palette::accent());
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    frame.render_widget(
        Paragraph::new(Line::from_iter(shimmer(message, tick))).centered(),
        inner,
    );
    popup
}
