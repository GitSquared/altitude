use crate::{
    agent::AgentKind,
    remote::TaskSummary,
    tui::{App, widgets::palette},
};

use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use crate::{
    isolation::EnvironmentProgress,
    task::{
        TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus,
        TaskOperatorAttentionLevel, TaskOperatorSignalKind,
    },
    tui::widgets::utils::{shimmer, truncate},
    vcs::{ContributionReadiness, now_ms},
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
    // The backdrop goes inside the frame, not under its border, so the contour
    // lines never crowd the edge they are read against.
    if app.animations() {
        super::super::background::terrain::draw(frame, content, app.shimmer);
    }
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

/// What a card draws, whichever machine owns the task.
///
/// Built from a [`TaskSummary`], which is what a remote answers with and what a
/// local task projects to, so a card cannot say one thing about a task here and
/// another about the same kind of task on another machine.
struct Card {
    title: String,
    environment: String,
    agent: Option<AgentKind>,
    host: String,
    /// The tracker issue this task is bound to, when it is bound to one.
    issue: Option<String>,
    /// How it is going, already in the words the card has room for.
    status: Line<'static>,
    /// What else it has to say, in the order it should be read.
    extras: Vec<Line<'static>>,
}

impl Card {
    /// `at_capacity` is whether the machine this card is drawn on has room for
    /// another sandbox. It is only ever true of this machine's tasks: a remote
    /// reports its own reasons for a task that is not starting.
    fn of(
        summary: &TaskSummary,
        at_capacity: bool,
        tick: Option<usize>,
        build: Option<&EnvironmentProgress>,
        now_ms: i64,
    ) -> Self {
        Self {
            title: summary.display_title().to_owned(),
            environment: summary.environment.clone(),
            agent: summary.agent,
            host: summary.host.clone(),
            issue: summary
                .tracker_issue
                .as_ref()
                .map(|issue| format!("{} · Linear", issue.identifier)),
            status: card_status(summary, at_capacity, tick, build),
            extras: card_extras(summary, now_ms),
        }
    }
}

/// The list: this machine's tasks, then what the remotes are holding.
///
/// A remote's tasks are read from it rather than copied, so they are here because it
/// answered and they are gone when it does not. Its card is built from the same
/// summary this machine builds for its own.
fn cards(app: &App) -> Vec<Card> {
    let occupied = app
        .tasks
        .iter()
        .filter(|task| task.facts.sandbox_available)
        .count();
    let at_capacity = app.capacity.is_some_and(|limit| occupied >= limit);
    let tick = app.animation_tick();
    let now = now_ms();
    let mut cards: Vec<Card> = app
        .tasks
        .iter()
        .map(|task| {
            let summary = TaskSummary::of(task);
            Card::of(
                &summary,
                at_capacity,
                tick,
                app.environment_builds.get(&task.id),
                now,
            )
        })
        .collect();
    cards.extend(
        app.remote_tasks
            .iter()
            .map(|held| Card::of(&held.summary, false, tick, None, now)),
    );
    cards
}

fn draw_card(frame: &mut Frame, area: Rect, card: &Card, focused: bool) {
    frame.render_widget(Clear, area);
    let border = if focused {
        palette::accent()
    } else {
        palette::muted()
    };
    let block = Block::bordered()
        .title(Span::styled(
            format!(
                " {} ",
                truncate(&card.title, usize::from(area.width.saturating_sub(4)))
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
    if let Some(issue) = &card.issue {
        lines.push(Line::styled(issue.clone(), palette::accent()));
    }
    lines.push(Line::styled(card.environment.clone(), palette::muted()));
    lines.push(Line::styled(
        format!("{agent} on {}", card.host),
        palette::muted(),
    ));
    lines.extend(card.extras.iter().cloned());
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

/// How a task is going, in the words a card has room for.
///
/// `build` is this machine's own progress building the environment, which only
/// exists here: a task on another machine is built there.
fn card_status(
    summary: &TaskSummary,
    at_capacity: bool,
    tick: Option<usize>,
    build: Option<&EnvironmentProgress>,
) -> Line<'static> {
    if let Some(progress) = build {
        return Line::from_iter(shimmer(&build_label(progress, 40), tick));
    }
    match summary.status {
        TaskExecutionStatus::Pending => {
            // Both are recorded as `Pending`; the only difference is whether the
            // host has room yet, and neither lasts long enough to be worth
            // distinguishing further.
            let label = if at_capacity {
                "waiting for capacity…"
            } else {
                "starting up…"
            };
            Line::from_iter(shimmer(label, tick))
        }
        TaskExecutionStatus::Running => Line::from(match summary.mode {
            TaskExecutionMode::Autopilot => "autopilot · working",
            TaskExecutionMode::AutopilotReport => "autopilot · reporting",
            TaskExecutionMode::Operator => "at the terminal",
        })
        .style(palette::good()),
        TaskExecutionStatus::Paused => Line::from(match summary.pause_reason {
            Some(TaskExecutionPauseReason::AwaitingOperator) => "paused · awaiting operator",
            Some(TaskExecutionPauseReason::AwaitingReviewChecks) => "paused · watching PRs",
            Some(TaskExecutionPauseReason::ApplicationShutdown) => "paused · altitude closed",
            Some(TaskExecutionPauseReason::Transfer) => "paused · transferring",
            None => "paused",
        })
        .style(palette::attention()),
        TaskExecutionStatus::Completed => Line::styled("completed", palette::muted()),
        TaskExecutionStatus::Failed => Line::styled("failed", palette::bad()),
    }
}

/// What else a card has to say: the things that mean someone has to look.
fn card_extras(summary: &TaskSummary, now_ms: i64) -> Vec<Line<'static>> {
    let mut extras = Vec::new();
    // A previous report's request is not a current interruption after execution
    // resumes.
    if summary.status == TaskExecutionStatus::Paused
        && summary.pause_reason.as_ref() == Some(&TaskExecutionPauseReason::AwaitingOperator)
    {
        match summary.awaiting_signal_kind {
            // A wrap-up request is not a problem to solve. It is work waiting to
            // be accepted, so it reads as ready rather than as an alarm.
            Some(TaskOperatorSignalKind::InReview) => {
                extras.push(Line::styled("in review", palette::good().bold()))
            }
            _ => match summary.attention {
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
    if summary.unread_operator_signal_count > 0 {
        extras.push(Line::styled(
            format!("● {} unread", summary.unread_operator_signal_count),
            palette::accent().bold(),
        ));
    }
    match summary.contributions.len() {
        0 => {}
        1 => extras.push(Line::styled("1 contribution", palette::accent())),
        count => extras.push(Line::styled(
            format!("{count} contributions"),
            palette::accent(),
        )),
    }
    let shown = summary.contributions.len().min(2);
    for contribution in summary.contributions.iter().take(shown) {
        let label = format!("PR #{}", contribution.reference);
        let status = contribution
            .summary
            .as_deref()
            .unwrap_or("status not observed");
        let style = match contribution
            .state
            .as_ref()
            .map(|state| state.readiness(now_ms))
        {
            Some(ContributionReadiness::NeedsAction) => palette::bad(),
            Some(ContributionReadiness::Pending) => palette::attention(),
            _ => palette::muted(),
        };
        extras.push(Line::styled(format!("{label} · {status}"), style));
    }
    if summary.contributions.len() > shown {
        // The card lists a couple of pull requests; say so, so the two shown are
        // not read as the whole set.
        extras.push(Line::styled(
            format!("+{} more", summary.contributions.len() - shown),
            palette::muted(),
        ));
    }
    if summary.status == TaskExecutionStatus::Failed
        && let Some(error) = &summary.last_error
    {
        extras.push(Line::styled(error.clone(), palette::bad()));
    }
    // Whose work this is, when it is not the machine holding the capsule. A task
    // delegated here from another operator names them.
    if let Some(operator) = summary.operator_label.as_deref()
        && !operator.is_empty()
        && operator != summary.host.as_str()
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
