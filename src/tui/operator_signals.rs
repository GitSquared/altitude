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

pub fn draw(
    frame: &mut Frame,
    operator_signals: &[TaskOperatorSignal],
    selected: usize,
    scroll: &mut u16,
) {
    let area = centered_width(frame.area(), 72, 65);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(signal_keys(
            operator_signals.get(selected).map(|signal| signal.kind),
        ))
        .title_bottom(" pgup/pgdn scroll · home/end · h/j select ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let list_height = operator_signals
        .len()
        .min(8)
        .min(usize::from(inner.height / 3)) as u16;
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Length(list_height), Constraint::Min(1)])
            .spacing(1)
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
    let paragraph = Paragraph::new(detail).white().wrap(Wrap { trim: true });
    let max_scroll = paragraph
        .line_count(detail_area.width)
        .saturating_sub(usize::from(detail_area.height))
        .min(usize::from(u16::MAX)) as u16;
    *scroll = (*scroll).min(max_scroll);
    frame.render_widget(paragraph.scroll((*scroll, 0)), detail_area);
}

/// What the selected signal can be answered with, so the keys are visible rather
/// than remembered.
fn signal_keys(kind: Option<TaskOperatorSignalKind>) -> &'static str {
    match kind {
        Some(TaskOperatorSignalKind::Update) => " Signals · enter acknowledge · esc close ",
        Some(TaskOperatorSignalKind::InReview) => " Signals · c confirm · enter reply · esc close ",
        Some(_) => " Signals · enter reply · esc close ",
        None => " Signals · esc close ",
    }
}

fn kind_label(kind: TaskOperatorSignalKind) -> &'static str {
    match kind {
        TaskOperatorSignalKind::Update => "update",
        TaskOperatorSignalKind::Decision => "decision",
        TaskOperatorSignalKind::InReview => "in review",
        TaskOperatorSignalKind::Failure => "failure",
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
