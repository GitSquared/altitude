use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use chrono::{Local, TimeZone};

use crate::tui::widgets::{chrome, palette};
use crate::{
    task::{TaskOperatorSignal, TaskOperatorSignalKind, TaskOperatorSignalPriority},
    tui::widgets::utils::centered_width,
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
            Self::Acknowledge => "dismiss",
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
    area: Rect,
    operator_signals: &[TaskOperatorSignal],
    // What the agent last said about the task: the whole of what this popup has
    // to say about a task that has raised nothing.
    latest_turn_summary: Option<&str>,
    selected: usize,
    focused_action: usize,
    scroll: &mut u16,
) -> Rect {
    let area = centered_width(area, 72, 65);
    frame.render_widget(Clear, area);
    let block = chrome::modal("Signals");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let list_height = operator_signals
        .len()
        .min(8)
        .min(usize::from(inner.height / 3)) as u16;
    // The task's own words first: this is the pop-up an operator opens to find
    // out what is happening, and a signal is not always the answer.
    let summary_height = latest_turn_summary.map_or(0, |summary| {
        u16::try_from(
            summary
                .chars()
                .count()
                .div_ceil(usize::from(inner.width.max(1)))
                + 2,
        )
        .unwrap_or(0)
        .min(6)
    });
    let [summary_area, list_area, detail_area, actions_area] = Layout::vertical([
        Constraint::Length(summary_height),
        Constraint::Length(list_height),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .spacing(1)
    .margin(1)
    .areas(inner);
    if let Some(summary) = latest_turn_summary {
        frame.render_widget(
            Paragraph::new(summary)
                .style(palette::muted())
                .wrap(Wrap { trim: true }),
            summary_area,
        );
    }
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
    area
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
