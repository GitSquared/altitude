use crate::{
    agent::AgentKind,
    remote::TaskSummary,
    tui::{App, widgets::palette},
};

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
    tracker::IssueReference,
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
    // The list sits in a frame like every other screen, and the grid is laid out in
    // what the frame leaves rather than in the whole area.
    let panel = Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::uniform(1));
    let content = panel.inner(area);
    frame.render_widget(panel, area);
    let (columns, card_width, height) = grid(content);
    app.grid_columns = columns;
    draw_tasks(frame, content, app, columns, card_width, height);
}

fn draw_tasks(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    columns: usize,
    card_width: u16,
    height: u16,
) {
    let total = app.tasks.len() + app.remote_tasks.len();
    if total == 0 {
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
    let selected = app.selected;
    let selected_page = selected / page;
    let page_count = total.div_ceil(page);
    let first = selected_page.min(page_count.saturating_sub(1)) * page;
    app.first_visible_task = first;

    let cards = cards(app);
    for (offset, card) in cards.iter().skip(first).take(page).enumerate() {
        let row = offset / columns;
        let column = offset % columns;
        let area = Rect {
            x: area.x + column as u16 * card_width,
            y: area.y + row as u16 * (height + 1),
            width: card_width.saturating_sub(1),
            height,
        };
        let focused = first + offset == selected;
        draw_card(frame, area, card, focused);
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

/// What a card draws, whether the task is this machine's or one it is reading.
struct Card<'a> {
    title: &'a str,
    environment: &'a str,
    agent: Option<AgentKind>,
    host: &'a str,
    /// The tracker issue this task is bound to, when it is bound to one.
    issue: Option<&'a IssueReference>,
    /// How it is going, already in the words the card has room for.
    status: Line<'static>,
    /// What else it has to say, in the order it should be read.
    extras: Vec<Line<'static>>,
}

/// The list: this machine's tasks, then what the remotes are holding.
///
/// A remote's tasks are read from it rather than copied, so they are here because it
/// answered and they are gone when it does not.
fn cards(app: &App) -> Vec<Card<'_>> {
    let occupied = app
        .tasks
        .iter()
        .filter(|task| task.facts.sandbox_available)
        .count();
    let at_capacity = app.capacity.is_some_and(|limit| occupied >= limit);
    let mut cards: Vec<Card<'_>> = app
        .tasks
        .iter()
        .map(|task| Card {
            title: task.title(),
            environment: &task.facts.environment_name,
            agent: task.agent_kind,
            host: &task.host,
            issue: task.facts.tracker_issue.as_ref(),
            status: card_status(
                task,
                at_capacity,
                app.shimmer,
                app.environment_builds.get(&task.id),
            ),
            extras: card_extras(task),
        })
        .collect();
    cards.extend(app.remote_tasks.iter().map(|held| {
        let summary = &held.summary;
        let mut extras: Vec<Line<'static>> = summary
            .extras
            .iter()
            .map(|extra| Line::styled(extra.clone(), palette::attention()))
            .collect();
        // Whose work this is, when it is not the machine holding the capsule, on
        // the same terms as a local card.
        if let Some(operator) = summary.operator_label.as_deref()
            && !operator.is_empty()
            && operator != summary.host.as_str()
        {
            extras.push(Line::styled(format!("for {operator}"), palette::muted()));
        }
        Card {
            title: summary.title.as_deref().unwrap_or(&summary.intent),
            environment: &summary.environment,
            agent: summary.agent,
            host: &summary.host,
            issue: summary.tracker_issue.as_ref(),
            status: held_status(summary),
            extras,
        }
    }));
    cards
}

/// How a task on another machine is going, in the words the card has room for.
///
/// Nothing here is live: it is what that machine said when it last answered.
fn held_status(held: &TaskSummary) -> Line<'static> {
    match held.status {
        TaskExecutionStatus::Pending => Line::styled("starting up...", palette::muted()),
        TaskExecutionStatus::Running => Line::from(match held.mode {
            TaskExecutionMode::Autopilot => "autopilot · working",
            TaskExecutionMode::AutopilotReport => "autopilot · reporting",
            TaskExecutionMode::Operator => "operator override",
        })
        .style(palette::good()),
        TaskExecutionStatus::Paused => Line::styled("paused", palette::attention()),
        TaskExecutionStatus::Completed => Line::styled("completed", palette::muted()),
        TaskExecutionStatus::Failed => Line::styled("blocked · failed", palette::bad()),
    }
}

fn draw_card(frame: &mut Frame, area: Rect, card: &Card<'_>, focused: bool) {
    let border = if focused {
        palette::accent()
    } else {
        palette::muted()
    };
    let block = Block::bordered()
        .title(Span::styled(
            format!(
                " {} ",
                truncate(card.title, usize::from(area.width.saturating_sub(4)))
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

    let agent = match card.agent {
        Some(kind) => kind.to_string(),
        None => "no agent".to_owned(),
    };
    let mut lines = vec![card.status.clone()];
    if let Some(issue) = card.issue {
        lines.push(Line::styled(
            format!("{} · Linear", issue.identifier),
            palette::accent(),
        ));
    }
    lines.push(Line::styled(card.environment.to_owned(), palette::muted()));
    lines.push(Line::styled(
        format!("{agent} on {}", card.host),
        palette::muted(),
    ));
    lines.extend(card.extras.iter().cloned());
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
            Some(TaskExecutionPauseReason::Transfer) => "paused · transferring",
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
    // Whose work this is, when it is not the machine holding the capsule. A task
    // delegated here from another operator names them.
    if let Some(operator) = task.facts.operator_label.as_deref()
        && !operator.is_empty()
        && operator != task.host.as_str()
    {
        extras.push(Line::styled(format!("for {operator}"), palette::muted()));
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
