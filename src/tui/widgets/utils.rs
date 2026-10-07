use ratatui::{
    layout::{Constraint, Flex, Layout, Rect},
    text::Span,
};

use crate::tui::widgets::palette;

/// How many characters wide the travelling highlight is.
const SHIMMER_WIDTH: usize = 3;

pub fn centered_width(area: Rect, width: u16, height_percent: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    Layout::vertical([Constraint::Percentage(height_percent)])
        .flex(Flex::Center)
        .areas::<1>(area)[0]
}

/// A highlight that travels across `text`, so a wait reads as work in progress.
///
/// `tick` is any increasing counter; each call wraps it, so callers only need
/// to keep counting.
pub fn shimmer(text: &str, tick: usize) -> Vec<Span<'static>> {
    let position = tick % (text.chars().count() + SHIMMER_WIDTH);
    text.chars()
        .enumerate()
        .map(|(index, character)| {
            let style = if index.abs_diff(position) < SHIMMER_WIDTH {
                palette::waiting()
            } else {
                palette::waiting_rest()
            };
            Span::styled(character.to_string(), style)
        })
        .collect()
}

/// Shortens `text` to `width` characters, marking the cut with an ellipsis.
pub fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut truncated = text.chars().take(width - 1).collect::<String>();
    truncated.push('…');
    truncated
}
