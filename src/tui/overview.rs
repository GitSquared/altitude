use std::{collections::HashMap, iter};

use ratatui::{
    Frame,
    layout::{Constraint, HorizontalAlignment, Layout, Rect},
    style::{Style, Stylize},
    text::Line,
    widgets::{Block, BorderType, Padding, Paragraph},
};

use crate::tasks::{
    Task, TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus,
    TaskOperatorAttentionLevel,
};

pub fn draw(
    frame: &mut Frame,
    tasks: &[Task],
    selected: usize,
    latest_output: &HashMap<String, String>,
    notice: Option<&str>,
) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
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
    draw_tasks(frame, body, tasks, selected, latest_output);

    if let Some(notice) = notice {
        frame.render_widget(Line::from(notice).red(), footer);
    }
    frame.render_widget(
        Line::from("c create · i info · h/j select · enter shell · delete remove · q quit")
            .dim()
            .alignment(HorizontalAlignment::Right),
        footer,
    );
}

fn draw_tasks(
    frame: &mut Frame,
    area: Rect,
    tasks: &[Task],
    selected: usize,
    latest_output: &HashMap<String, String>,
) {
    let panel = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::uniform(1));
    let content = panel.inner(area);
    frame.render_widget(panel, area);

    if tasks.is_empty() {
        frame.render_widget(Paragraph::new("No tasks yet".dim()), content);
        return;
    }

    let cards = Layout::vertical(
        tasks
            .iter()
            .map(|_| Constraint::Length(4))
            .chain(iter::once(Constraint::Min(0))),
    )
    .spacing(1)
    .split(content);

    for (index, (task, area)) in tasks.iter().zip(cards.iter()).enumerate() {
        let selected = index == selected;
        let mut status = match &task.current_state.execution_status {
            TaskExecutionStatus::Pending => Line::from("pending").yellow(),
            TaskExecutionStatus::Running => Line::from(match task.current_state.execution_mode {
                TaskExecutionMode::Autopilot => "autopilot · working",
                TaskExecutionMode::AutopilotReport => "autopilot · reporting",
                TaskExecutionMode::Operator => "operator override",
            })
            .green(),
            TaskExecutionStatus::Paused => {
                Line::from(match task.current_state.execution_pause_reason {
                    Some(TaskExecutionPauseReason::AwaitingOperator) => {
                        "paused · awaiting operator"
                    }
                    Some(TaskExecutionPauseReason::ApplicationShutdown) => "paused · app stopped",
                    None => "paused",
                })
                .yellow()
            }
            TaskExecutionStatus::Completed => Line::from("completed").dim(),
            TaskExecutionStatus::Failed => {
                Line::from(task.current_state.last_error.as_deref().map_or_else(
                    || "execution failed".to_owned(),
                    |error| format!("execution failed · {error}"),
                ))
                .red()
            }
        };
        match task.current_state.operator_attention_level {
            TaskOperatorAttentionLevel::Alarm => {
                status.spans.push(" · ALARM".red().bold());
            }
            TaskOperatorAttentionLevel::Requested => {
                status.spans.push(" · attention required".yellow().bold());
            }
            TaskOperatorAttentionLevel::Nominal => {}
        }
        if task.current_state.unread_operator_signal_count > 0 {
            status.spans.push(
                format!(" · ● {}", task.current_state.unread_operator_signal_count)
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
        let output = (selected)
            .then(|| latest_output.get(&task.id))
            .flatten()
            .and_then(|output| output.lines().last())
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(vec![status, Line::from(output).dim()])
                .style(style)
                .block(
                    Block::bordered()
                        .title(format!(" {} ", task.current_state.intent))
                        .border_type(BorderType::Rounded)
                        .border_style(style)
                        .padding(Padding::horizontal(1)),
                ),
            *area,
        );
    }
}
