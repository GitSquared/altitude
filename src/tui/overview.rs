use super::palette;

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Padding, Paragraph},
};

use crate::{
    isolation::EnvironmentProgress,
    tasks::{
        TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus,
        TaskOperatorAttentionLevel, TaskOperatorSignalKind,
    },
    tui::utils::{shimmer, truncate},
};

pub fn draw(frame: &mut Frame, area: Rect, app: &mut super::App) {
    draw_tasks(frame, area, app);
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
        frame.render_widget(
            Paragraph::new(Span::styled("No tasks yet", palette::muted())),
            content,
        );
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
            Line::from_iter(shimmer(&build_label(progress, text_width), tick))
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
                    Line::from_iter(shimmer(label, tick))
                }
                TaskExecutionStatus::Running => Line::from(match task.facts.execution_mode {
                    TaskExecutionMode::Autopilot => "autopilot · working",
                    TaskExecutionMode::AutopilotReport => "autopilot · reporting",
                    TaskExecutionMode::Operator => "operator override",
                })
                .style(palette::good()),
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
                    .style(palette::attention())
                }
                TaskExecutionStatus::Completed => Line::styled("completed", palette::muted()),
                TaskExecutionStatus::Failed => {
                    Line::styled("blocked · execution failed · i details", palette::bad())
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
                    status
                        .spans
                        .push(Span::styled(" · in review", palette::good().bold()));
                }
                _ => match task.facts.operator_attention_level {
                    TaskOperatorAttentionLevel::Alarm => {
                        status
                            .spans
                            .push(Span::styled(" · ALARM", palette::bad().bold()));
                    }
                    TaskOperatorAttentionLevel::Requested => {
                        status.spans.push(Span::styled(
                            " · attention required",
                            palette::attention().bold(),
                        ));
                    }
                    TaskOperatorAttentionLevel::Nominal => {}
                },
            }
        }
        if task.facts.unread_operator_signal_count > 0 {
            status.spans.push(Span::styled(
                format!(" · ● {}", task.facts.unread_operator_signal_count),
                palette::accent().bold(),
            ));
        }
        match task.code_contributions.len() {
            0 => {}
            1 => status
                .spans
                .push(Span::styled(" · 1 contribution", palette::accent())),
            count => status.spans.push(Span::styled(
                format!(" · {count} contributions"),
                palette::accent(),
            )),
        }
        let style = if selected {
            palette::primary().bold()
        } else {
            palette::muted()
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
            Paragraph::new(vec![status, Line::styled(output, palette::muted())])
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
