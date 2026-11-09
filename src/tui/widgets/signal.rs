use ratatui::style::Style;

use super::palette;
use crate::task::TaskAskKind;

pub struct SignalView {
    pub label: &'static str,
    pub task_label: &'static str,
    pub symbol: char,
    pub tone: Style,
}

pub fn view(kind: TaskAskKind) -> SignalView {
    match kind {
        TaskAskKind::Review => SignalView {
            label: "Ready to review",
            task_label: "Ready to review",
            symbol: '◆',
            tone: palette::good(),
        },
        TaskAskKind::Steer => SignalView {
            label: "Blocked",
            task_label: "Needs you",
            symbol: '?',
            tone: palette::attention(),
        },
    }
}
