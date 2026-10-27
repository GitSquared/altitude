//! A line of tabs over the work they change: the projects above the task list,
//! and the conversation or the plan on a narrow signals screen.
//!
//! The selected tab is bold and the others are quiet, so it reads as the place
//! you are rather than as a word to press. A label keeps the case it was named
//! in: a project called "Altitude" is not a heading.
//!
//! A bar that belongs to a whole pane sits in its header: the tabs on the first
//! row inside the border, and the pane's divider under them, so the content
//! starts at a line rather than under a row that belongs to the tabs.

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::palette;

pub struct Tab {
    pub label: String,
    /// Work waiting on the operator, drawn beside the tab rather than inside it.
    pub attention: usize,
}

impl Tab {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            attention: 0,
        }
    }

    pub fn attention(mut self, count: usize) -> Self {
        self.attention = count;
        self
    }
}

/// Draws the tabs, and answers with where each was drawn so a click can choose
/// one. A selection past the end draws every tab quiet.
pub fn draw(frame: &mut Frame, area: Rect, tabs: &[Tab], selected: usize) -> Vec<(Rect, usize)> {
    if area.height == 0 {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut targets = Vec::new();
    let mut x = area.x;
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("│", palette::muted()));
            x = x.saturating_add(1);
        }
        let label = format!(" {} ", tab.label);
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        let style = if index == selected {
            palette::primary().bold()
        } else {
            palette::muted()
        };
        spans.push(Span::styled(label, style));
        let mut total = width;
        if tab.attention > 0 {
            let count = format!(" {}", tab.attention);
            total = total.saturating_add(u16::try_from(count.chars().count()).unwrap_or(u16::MAX));
            spans.push(Span::styled(count, palette::accent()));
        }
        targets.push((
            Rect {
                x,
                y: area.y,
                width: total,
                height: 1,
            },
            index,
        ));
        x = x.saturating_add(total);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    targets
}
