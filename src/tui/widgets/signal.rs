//! How a signal kind reads on screen.
//!
//! The kind is the agent's word for what it is doing; this table is the
//! operator's. It is shared by the task list and the signals screen so a blocked
//! task and a blocked thread cannot end up with different names or colours.

use ratatui::style::Style;

use super::palette;
use crate::task::TaskOperatorSignalKind;

pub struct SignalView {
    pub label: &'static str,
    /// The same kind as a task card reads it, where there is no group heading to
    /// carry the action.
    pub task_label: &'static str,
    pub symbol: char,
    pub tone: Style,
}

pub fn view(kind: TaskOperatorSignalKind) -> SignalView {
    match kind {
        TaskOperatorSignalKind::Update => SignalView {
            label: "Info",
            task_label: "Info",
            symbol: '○',
            tone: palette::muted(),
        },
        TaskOperatorSignalKind::Decision => SignalView {
            label: "Decision",
            task_label: "Decision",
            symbol: '→',
            tone: palette::decision(),
        },
        TaskOperatorSignalKind::InReview => SignalView {
            label: "Ready to review",
            task_label: "Ready to review",
            symbol: '◆',
            tone: palette::good(),
        },
        TaskOperatorSignalKind::Blocked => SignalView {
            label: "Blocked",
            task_label: "Needs you",
            symbol: '?',
            tone: palette::attention(),
        },
    }
}
