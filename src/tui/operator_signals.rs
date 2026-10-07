use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use chrono::{Local, TimeZone};

use super::palette;
use crate::{
    tasks::{TaskOperatorSignal, TaskOperatorSignalKind, TaskOperatorSignalPriority},
    tui::utils::centered_width,
};

/// What the operator can do with the signal they have selected.
///
/// Replying is always offered, because every signal can be answered with
/// instructions. The affirmative is offered only where it means something: an
/// update can be dismissed, and a wrap-up request can be accepted. A decision or a
/// failure has no text-free answer — the reply *is* the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalAction {
    Reply,
    Acknowledge,
    Confirm,
}

impl SignalAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Acknowledge => "acknowledge",
            Self::Confirm => "confirm",
        }
    }
}

/// The actions a signal offers, in the order they are shown.
pub fn actions_for(kind: TaskOperatorSignalKind) -> &'static [SignalAction] {
    const REPLY: &[SignalAction] = &[SignalAction::Reply];
    const REPLY_OR_ACKNOWLEDGE: &[SignalAction] = &[SignalAction::Reply, SignalAction::Acknowledge];
    const REPLY_OR_CONFIRM: &[SignalAction] = &[SignalAction::Reply, SignalAction::Confirm];
    match kind {
        TaskOperatorSignalKind::Update => REPLY_OR_ACKNOWLEDGE,
        TaskOperatorSignalKind::InReview => REPLY_OR_CONFIRM,
        TaskOperatorSignalKind::Decision | TaskOperatorSignalKind::Failure => REPLY,
    }
}

pub fn draw(
    frame: &mut Frame,
    operator_signals: &[TaskOperatorSignal],
    selected: usize,
    focused_action: usize,
    scroll: &mut u16,
) {
    let area = centered_width(frame.area(), 72, 65);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Signals · tab focus · enter validate · esc close ")
        .title_bottom(" pgup/pgdn scroll · home/end · h/j select ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let list_height = operator_signals
        .len()
        .min(8)
        .min(usize::from(inner.height / 3)) as u16;
    let [list_area, detail_area, actions_area] = Layout::vertical([
        Constraint::Length(list_height),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
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
            // A wrap-up request is the one signal that asks for acceptance rather
            // than attention, so it reads green instead of like a problem.
            let style = match signal.kind {
                TaskOperatorSignalKind::InReview => palette::good(),
                _ => Style::new(),
            };
            ListItem::new(
                Line::from(format!(
                    "{timestamp}  {:<6}  {:<9}  {}",
                    priority_label(signal.priority),
                    kind_label(signal.kind),
                    signal.title
                ))
                .style(style),
            )
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_symbol("❯ ")
        .highlight_style(palette::accent());
    let mut state = ListState::default();
    state.select((!operator_signals.is_empty()).then_some(selected));
    frame.render_stateful_widget(list, list_area, &mut state);
    let detail = operator_signals
        .get(selected)
        .map(|signal| signal.body.as_str())
        .unwrap_or("No unresolved signals for this task.");
    let paragraph = Paragraph::new(detail)
        .style(palette::primary())
        .wrap(Wrap { trim: true });
    let max_scroll = paragraph
        .line_count(detail_area.width)
        .saturating_sub(usize::from(detail_area.height))
        .min(usize::from(u16::MAX)) as u16;
    *scroll = (*scroll).min(max_scroll);
    frame.render_widget(paragraph.scroll((*scroll, 0)), detail_area);
    frame.render_widget(
        actions(operator_signals.get(selected), focused_action),
        actions_area,
    );
}

/// The buttons for the selected signal, with the focused one picked out.
fn actions(signal: Option<&TaskOperatorSignal>, focused_action: usize) -> Paragraph<'static> {
    let Some(signal) = signal else {
        return Paragraph::new(Line::default());
    };
    let actions = actions_for(signal.kind);
    let focused = focused_action.min(actions.len().saturating_sub(1));
    let spans = actions
        .iter()
        .enumerate()
        .flat_map(|(index, action)| {
            let style = if index == focused {
                palette::focus()
            } else {
                palette::muted()
            };
            [
                Span::styled(format!("[ {} ]", action.label()), style),
                Span::raw(" "),
            ]
        })
        .collect::<Vec<_>>();
    Paragraph::new(Line::from(spans))
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
