use crate::tui::{App, widgets::palette};

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Padding, Paragraph, Wrap},
};

use crate::{
    isolation::EnvironmentProgress,
    task::{
        Task, TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus,
        TaskOperatorAttentionLevel, TaskOperatorSignalKind,
    },
    tui::widgets::utils::{shimmer, truncate},
};

/// The narrowest a card may be and still show an intent.
const MIN_CARD_WIDTH: u16 = 26;

/// How the cards are laid out in an area.
///
/// A card is 4:3, which on a terminal of ordinary height means one row of them:
/// a card tall enough for a sentence is also tall enough to fill the body. So
/// the grid pages rather than scrolling by a card, and the page is what fits.
fn grid(area: Rect) -> (usize, u16, u16) {
    let columns = (area.width / MIN_CARD_WIDTH).max(1) as usize;
    let card_width = area.width / columns as u16;
    let card_height = (card_width * 3 / 4).clamp(5, area.height);
    (columns, card_width, card_height)
}

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    let (columns, card_width, height) = grid(area);
    app.grid_columns = columns;
    draw_tasks(frame, area, app, columns, card_width, height);
}

fn draw_tasks(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    columns: usize,
    card_width: u16,
    height: u16,
) {
    let tasks = &app.tasks;
    if tasks.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("No tasks yet", palette::muted())),
            area,
        );
        return;
    }
    let rows = (area.height / (height + 1)).max(1) as usize;
    let page = (columns * rows).max(1);
    // Page by page rather than card by card: a grid that scrolls by one card
    // rearranges itself under the focus.
    let first = &mut app.first_visible_task;
    let selected = app.selected;
    let selected_page = selected / page;
    let page_count = tasks.len().div_ceil(page);
    *first = selected_page.min(page_count.saturating_sub(1)) * page;

    let tick = app.shimmer;
    let capacity = app.capacity;
    let occupied = tasks
        .iter()
        .filter(|task| task.facts.sandbox_available)
        .count();
    let at_capacity = capacity.is_some_and(|limit| occupied >= limit);

    for (offset, task) in tasks.iter().skip(*first).take(page).enumerate() {
        let row = offset / columns;
        let column = offset % columns;
        let card = Rect {
            x: area.x + column as u16 * card_width,
            y: area.y + row as u16 * (height + 1),
            width: card_width.saturating_sub(1),
            height,
        };
        let index = *first + offset;
        let focused = index == selected;
        draw_card(
            frame,
            card,
            task,
            focused,
            at_capacity,
            tick,
            app.environment_builds.get(&task.id),
        );
    }

    // Which page of how many, when there is more than one.
    if page_count > 1 {
        let area = Rect {
            x: area.x,
            y: area.bottom().saturating_sub(1),
            width: area.width,
            height: 1,
        };
        frame.render_widget(
            Line::styled(
                format!("page {} of {page_count}", selected_page + 1),
                palette::muted(),
            )
            .right_aligned(),
            area,
        );
    }
}

/// One task, as a card: what it is, and how it is going.
fn draw_card(
    frame: &mut Frame,
    area: Rect,
    task: &Task,
    focused: bool,
    at_capacity: bool,
    tick: usize,
    build: Option<&EnvironmentProgress>,
) {
    let border = if focused {
        palette::accent()
    } else {
        palette::muted()
    };
    let block = Block::bordered()
        .title(Span::styled(
            format!(
                " {} ",
                truncate(
                    &task.facts.intent,
                    usize::from(area.width.saturating_sub(4)),
                )
            ),
            if focused {
                palette::heading()
            } else {
                palette::muted()
            },
        ))
        .border_type(BorderType::Rounded)
        .border_style(border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let status = card_status(task, at_capacity, tick, build);
    let mut lines = vec![status];
    // Everything else the task has to say, in the space the card has for it.
    for extra in card_extras(task) {
        lines.push(extra);
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

/// How a task is going, in the words the card has room for.
fn card_status(
    task: &Task,
    at_capacity: bool,
    tick: usize,
    build: Option<&EnvironmentProgress>,
) -> Line<'static> {
    if let Some(progress) = build {
        return Line::from_iter(shimmer(&build_label(progress, 40), tick));
    }
    match task.facts.execution_status {
        TaskExecutionStatus::Pending => {
            // Both are recorded as `Pending`; the only difference is whether the
            // host has room yet, and neither lasts long enough to be worth
            // distinguishing further.
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
        TaskExecutionStatus::Paused => Line::from(match task.facts.execution_pause_reason {
            Some(TaskExecutionPauseReason::AwaitingOperator) => "paused · awaiting operator",
            Some(TaskExecutionPauseReason::ApplicationShutdown) => "paused · app stopped",
            None => "paused",
        })
        .style(palette::attention()),
        TaskExecutionStatus::Completed => Line::styled("completed", palette::muted()),
        TaskExecutionStatus::Failed => Line::styled("blocked · failed", palette::bad()),
    }
}

/// What else a card has to say: the things that mean someone has to look.
fn card_extras(task: &Task) -> Vec<Line<'static>> {
    let mut extras = Vec::new();
    // A previous report's request is not a current interruption after execution
    // resumes.
    if task.facts.execution_status == TaskExecutionStatus::Paused
        && task.facts.execution_pause_reason == Some(TaskExecutionPauseReason::AwaitingOperator)
    {
        match task.facts.awaiting_signal_kind {
            // A wrap-up request is not a problem to solve. It is work waiting to
            // be accepted, so it reads as ready rather than as an alarm.
            Some(TaskOperatorSignalKind::InReview) => {
                extras.push(Line::styled("in review", palette::good().bold()))
            }
            _ => match task.facts.operator_attention_level {
                TaskOperatorAttentionLevel::Alarm => {
                    extras.push(Line::styled("ALARM", palette::bad().bold()))
                }
                TaskOperatorAttentionLevel::Requested => extras.push(Line::styled(
                    "attention required",
                    palette::attention().bold(),
                )),
                TaskOperatorAttentionLevel::Nominal => {}
            },
        }
    }
    if task.facts.unread_operator_signal_count > 0 {
        extras.push(Line::styled(
            format!("● {} unread", task.facts.unread_operator_signal_count),
            palette::accent().bold(),
        ));
    }
    match task.code_contributions.len() {
        0 => {}
        1 => extras.push(Line::styled("1 contribution", palette::accent())),
        count => extras.push(Line::styled(
            format!("{count} contributions"),
            palette::accent(),
        )),
    }
    if task.facts.execution_status == TaskExecutionStatus::Failed
        && let Some(error) = &task.facts.last_error
    {
        extras.push(Line::styled(error.clone(), palette::bad()));
    }
    extras
}

/// What a waiting task is doing, with its place in the build kept visible.
fn build_label(progress: &EnvironmentProgress, width: usize) -> String {
    let count = format!(" ({}/{})", progress.completed + 1, progress.total);
    format!(
        "{}{count}",
        truncate(&progress.label, width.saturating_sub(count.chars().count()))
    )
}
