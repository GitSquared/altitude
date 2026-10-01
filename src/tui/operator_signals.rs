use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Style, Stylize},
    text::Line,
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use chrono::{Local, TimeZone};

use crate::{
    tasks::{TaskOperatorSignal, TaskOperatorSignalKind, TaskOperatorSignalPriority},
    tui::utils::centered_width,
};

pub fn draw(frame: &mut Frame, operator_signals: &[TaskOperatorSignal], selected: usize) {
    let area = centered_width(frame.area(), 72, 65);
    frame.render_widget(Clear, area);
    let block = Block::bordered().title(" Signals · enter acknowledge/answer · esc close ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [list_area, detail_area] = Layout::vertical([Constraint::Length(8), Constraint::Min(1)])
        .margin(1)
        .areas(inner);
    let items = operator_signals
        .iter()
        .map(|signal| {
            let timestamp = Local
                .timestamp_millis_opt(signal.raised_at_ms)
                .single()
                .map(|time| time.format("%H:%M").to_string())
                .unwrap_or_else(|| "--:--".into());
            ListItem::new(Line::from(format!(
                "{timestamp}  {:<6}  {:<8}  {}",
                priority_label(signal.priority),
                kind_label(signal.kind),
                signal.title
            )))
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_symbol("❯ ")
        .highlight_style(Style::new().cyan());
    let mut state = ListState::default();
    state.select((!operator_signals.is_empty()).then_some(selected));
    frame.render_stateful_widget(list, list_area, &mut state);
    let detail = operator_signals
        .get(selected)
        .map(|signal| signal.body.as_str())
        .unwrap_or("No unresolved signals for this task.");
    frame.render_widget(
        Paragraph::new(detail).white().wrap(Wrap { trim: true }),
        detail_area,
    );
}

fn kind_label(kind: TaskOperatorSignalKind) -> &'static str {
    match kind {
        TaskOperatorSignalKind::Update => "update",
        TaskOperatorSignalKind::Decision => "decision",
        TaskOperatorSignalKind::Review => "review",
    }
}

fn priority_label(priority: TaskOperatorSignalPriority) -> &'static str {
    match priority {
        TaskOperatorSignalPriority::Low => "low",
        TaskOperatorSignalPriority::Normal => "normal",
        TaskOperatorSignalPriority::High => "high",
        TaskOperatorSignalPriority::Urgent => "urgent",
    }
}
