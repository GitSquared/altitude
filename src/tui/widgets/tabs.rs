use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{palette, surface};

pub struct Tab {
    pub label: String,
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

pub fn draw(frame: &mut Frame, area: Rect, tabs: &[Tab], selected: usize) -> Vec<(Rect, usize)> {
    if area.height == 0 {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut targets = Vec::new();
    let mut separators = Vec::new();
    let mut x = area.x;
    for (index, tab) in tabs.iter().enumerate() {
        if index > 0 {
            // The separator is a line, not a label: it continues the rule under the
            // tabs, so it is drawn in the rule's own colour rather than half-lit
            // against it.
            spans.push(Span::styled("│", surface::outline()));
            separators.push(x);
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
    for x in separators {
        // From the pane's own top border down to the rule under the tabs: a stroke
        // that stops one row short reads as a line hanging in mid-air rather than as
        // the edge of a tab.
        surface::join_down(frame, x, area.y.saturating_sub(1), area.height + 1);
    }
    targets
}
