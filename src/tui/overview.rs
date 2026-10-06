use std::mem;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Style, Stylize},
    text::Line,
    widgets::{Block, BorderType, Padding, Paragraph, Wrap},
};

use crate::{
    isolation::EnvironmentProgress,
    tasks::{
        TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus,
        TaskOperatorAttentionLevel, TaskOperatorSignalKind,
    },
    tui::utils::{shimmer, truncate},
};

pub fn draw(frame: &mut Frame, app: &mut super::App) {
    let shortcuts = shortcut_hints(frame.area().width.saturating_sub(2));
    let footer_height = shortcuts.line_count(frame.area().width.saturating_sub(2)) as u16;
    let message_width = usize::from(frame.area().width.saturating_sub(2).max(1));
    let message_height = app.notice.as_deref().map_or(0, |text| {
        text.lines()
            .map(|line| line.chars().count().div_ceil(message_width).max(1))
            .sum::<usize>()
            .min(6) as u16
    });
    let [header, body, message, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(message_height),
        Constraint::Length(footer_height),
    ])
    .margin(1)
    .spacing(1)
    .areas(frame.area());

    let [title, version] =
        Layout::horizontal([Constraint::Min(0), Constraint::Min(0)]).areas(header);
    frame.render_widget(Line::from("⛰  altitude".cyan().bold()), title);
    frame.render_widget(
        Line::from(concat!("v", env!("CARGO_PKG_VERSION")).dim()).right_aligned(),
        version,
    );
    draw_tasks(frame, body, app);

    if let Some(notice) = app.notice.as_deref() {
        frame.render_widget(
            Paragraph::new(notice).red().wrap(Wrap { trim: false }),
            message,
        );
    }
    frame.render_widget(shortcuts, footer);
}

fn shortcut_hints(width: u16) -> Paragraph<'static> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for hint in [
        "c create",
        "s settings",
        "i info",
        "h/j select",
        "enter shell",
        "delete remove",
        "q quit",
    ] {
        if !line.is_empty() {
            if Line::from(format!("{line} · {hint}")).width() > usize::from(width) {
                lines.push(Line::from(mem::take(&mut line)));
            } else {
                line.push_str(" · ");
            }
        }
        line.push_str(hint);
    }
    lines.push(Line::from(line));
    Paragraph::new(lines).dim().wrap(Wrap { trim: true })
}

fn draw_tasks(frame: &mut Frame, area: Rect, app: &mut super::App) {
    let tasks = &app.tasks;
    let selected = app.selected;
    let first_visible = &mut app.first_visible_task;
    let latest_output = &app.latest_output;
    let environment_builds = &app.environment_builds;
    let capacity = app.capacity;
    let tick = app.shimmer;
    // A capsule that exists is a slot in use.
    let occupied = tasks
        .iter()
        .filter(|task| task.facts.sandbox_available)
        .count();
    let panel = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::uniform(1));
    let content = panel.inner(area);
    frame.render_widget(panel, area);

    if tasks.is_empty() {
        frame.render_widget(Paragraph::new("No tasks yet".dim()), content);
        return;
    }

    let visible_count = usize::from((content.height + 1) / 5).max(1);
    *first_visible = (*first_visible)
        .min(selected)
        .max((selected + 1).saturating_sub(visible_count))
        .min(tasks.len().saturating_sub(visible_count));
    for (row, (index, task)) in tasks
        .iter()
        .enumerate()
        .skip(*first_visible)
        .take(visible_count)
        .enumerate()
    {
        let area = Rect::new(
            content.x,
            content.y + row as u16 * 5,
            content.width,
            content.height.saturating_sub(row as u16 * 5).min(4),
        );
        let selected = index == selected;
        // A pending task is waiting on its environment build; nothing else
        // makes it pending after creation.
        let building = (task.facts.execution_status == TaskExecutionStatus::Pending)
            .then(|| environment_builds.get(&task.id))
            .flatten();
        let text_width = usize::from(area.width.saturating_sub(4));
        // Pending without a build to show means the host has not admitted it yet.
        let at_capacity = capacity.is_some_and(|limit| occupied >= limit);
        let mut status = if let Some(progress) = building {
            Line::from_iter(shimmer(&build_label(progress, text_width), tick + index))
        } else {
            match &task.facts.execution_status {
                TaskExecutionStatus::Pending => {
                    // Both are recorded as `Pending`; the only difference is
                    // whether the host has room yet, and neither lasts long
                    // enough to be worth distinguishing further.
                    let label = if at_capacity {
                        "waiting for capacity..."
                    } else {
                        "starting up..."
                    };
                    Line::from_iter(shimmer(label, tick + index)).dim()
                }
                TaskExecutionStatus::Running => Line::from(match task.facts.execution_mode {
                    TaskExecutionMode::Autopilot => "autopilot · working",
                    TaskExecutionMode::AutopilotReport => "autopilot · reporting",
                    TaskExecutionMode::Operator => "operator override",
                })
                .green(),
                TaskExecutionStatus::Paused => {
                    Line::from(match task.facts.execution_pause_reason {
                        Some(TaskExecutionPauseReason::AwaitingOperator) => {
                            "paused · awaiting operator"
                        }
                        Some(TaskExecutionPauseReason::ApplicationShutdown) => {
                            "paused · app stopped"
                        }
                        None => "paused",
                    })
                    .yellow()
                }
                TaskExecutionStatus::Completed => Line::from("completed").dim(),
                TaskExecutionStatus::Failed => {
                    Line::from("blocked · execution failed · i details").red()
                }
            }
        };
        // A previous report's request is not a current interruption after execution resumes.
        if task.facts.execution_status == TaskExecutionStatus::Paused
            && task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::AwaitingOperator)
        {
            match task.facts.awaiting_signal_kind {
                // A wrap-up request is not a problem to solve. It is work waiting to
                // be accepted, so it reads as ready rather than as an alarm.
                Some(TaskOperatorSignalKind::InReview) => {
                    status.spans.push(" · in review".green().bold());
                }
                _ => match task.facts.operator_attention_level {
                    TaskOperatorAttentionLevel::Alarm => {
                        status.spans.push(" · ALARM".red().bold());
                    }
                    TaskOperatorAttentionLevel::Requested => {
                        status.spans.push(" · attention required".yellow().bold());
                    }
                    TaskOperatorAttentionLevel::Nominal => {}
                },
            }
        }
        if task.facts.unread_operator_signal_count > 0 {
            status.spans.push(
                format!(" · ● {}", task.facts.unread_operator_signal_count)
                    .cyan()
                    .bold(),
            );
        }
        match task.code_contributions.len() {
            0 => {}
            1 => status.spans.push(" · 1 contribution".cyan()),
            count => status
                .spans
                .push(format!(" · {count} contributions").cyan()),
        }
        let style = if selected {
            Style::new().white().bold()
        } else {
            Style::new().dim()
        };
        let output = match building {
            Some(progress) => truncate(&progress.command, text_width),
            None if task.facts.execution_status == TaskExecutionStatus::Failed => {
                task.facts.last_error.clone().unwrap_or_default()
            }
            None => (selected)
                .then(|| latest_output.get(&task.id))
                .flatten()
                .and_then(|output| output.lines().last())
                .unwrap_or_default()
                .to_owned(),
        };
        frame.render_widget(
            Paragraph::new(vec![status, Line::from(output).dim()])
                .style(style)
                .block(
                    Block::bordered()
                        .title(format!(" {} ", task.facts.intent))
                        .border_type(BorderType::Rounded)
                        .border_style(style)
                        .padding(Padding::horizontal(1)),
                ),
            area,
        );
    }
}

/// What a waiting task is doing, with its place in the build kept visible.
fn build_label(progress: &EnvironmentProgress, width: usize) -> String {
    let count = format!(" ({}/{})", progress.completed + 1, progress.total);
    format!(
        "{}{count}",
        truncate(&progress.label, width.saturating_sub(count.chars().count()))
    )
}
