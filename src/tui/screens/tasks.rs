use crate::{
    agent::AgentKind,
    remote::TaskSummary,
    task::tasks_ahead_in_queue,
    tui::{
        App,
        app::PendingTask,
        widgets::{
            empty, palette, signal,
            surface::{self, Surface},
            tabs::{self, Tab},
        },
    },
};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use crate::{
    isolation::EnvironmentProgress,
    task::{
        TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus,
        TaskOperatorAttentionLevel,
    },
    tui::widgets::{format::truncate, shimmer::shimmer},
    vcs::{ContributionReadiness, now_ms},
};

const MIN_CARD_WIDTH: u16 = 26;

fn grid(area: Rect) -> (usize, u16, u16) {
    let columns = (area.width / MIN_CARD_WIDTH).max(1) as usize;
    let card_width = area.width / columns as u16;
    let card_height = (card_width * 3 / 8).clamp(5, area.height);
    (columns, card_width, card_height)
}

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    app.task_targets.clear();
    app.project_targets.clear();
    // The projects are the pane's header: a row of tabs inside the border, with
    // the pane's divider under it. The field under the surface is drawn by the
    // runner, so every screen shows the same one.
    let inner = Surface::new().draw(frame, area);
    let [header, body] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    draw_tabs(frame, header, inner, app);
    let body = Rect {
        x: inner.x.saturating_add(1),
        width: inner.width.saturating_sub(2),
        ..body
    };
    let (columns, card_width, height) = grid(body);
    app.grid_columns = columns;
    draw_tasks(frame, body, app, columns, card_width, height);
}

fn draw_tabs(frame: &mut Frame, area: Rect, pane: Rect, app: &mut App) {
    // The tabs own their row: the gutter to their right is solid rather than a
    // window onto the field, so a contour never moves behind a project name.
    surface::cover(frame, area);
    let projects = app.projects();
    if projects.is_empty() {
        return;
    }
    let active = app.active_project_id();
    let tabs: Vec<Tab> = projects
        .iter()
        .map(|project| {
            Tab::new(project.name.clone()).attention(app.attention_in_project(&project.id))
        })
        .collect();
    let selected = projects
        .iter()
        .position(|project| project.id == active)
        .unwrap_or(0);
    // The rule under the tabs is drawn first, so the separators between tabs end
    // on it rather than stopping a row short of it.
    surface::horizontal_divider(frame, pane, area.y.saturating_add(1));
    for (rect, index) in tabs::draw(frame, area, &tabs, selected) {
        app.project_targets.add(rect, index);
    }
}

fn draw_tasks(
    frame: &mut Frame,
    area: Rect,
    app: &mut App,
    columns: usize,
    card_width: u16,
    height: u16,
) {
    let cards = cards(app);
    let total = cards.len();
    if total == 0 {
        empty::draw(frame, area, "No tasks yet");
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

    // A task that has been asked for takes the next place in the grid, not a row
    // above it: the list is a grid of cards, and one more is what it looks like.
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
        app.task_targets.add(area, first + offset);
        draw_card(frame, area, card, focused);
    }
    let drawn = cards.len().saturating_sub(first).min(page);
    for (offset, waiting) in app
        .pending_tasks
        .iter()
        .take(page.saturating_sub(drawn))
        .enumerate()
    {
        let place = drawn + offset;
        let row = place / columns;
        let column = place % columns;
        let area = Rect {
            x: area.x + column as u16 * card_width,
            y: area.y + row as u16 * (height + 1),
            width: card_width.saturating_sub(1),
            height,
        };
        draw_pending(frame, area, waiting, app.animation_tick());
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

struct Card {
    title: String,
    project: String,
    agent: Option<AgentKind>,
    model: Option<String>,
    host: String,
    issue: Option<String>,
    is_alarm: bool,
    moving: Option<String>,
    status: Line<'static>,
    extras: Vec<Line<'static>>,
}

#[derive(Clone, Copy)]
enum Standing {
    NotQueued,
    Queued { ahead: usize, has_room: bool },
}

impl Card {
    fn of(
        summary: &TaskSummary,
        standing: Standing,
        tick: Option<usize>,
        build: Option<&EnvironmentProgress>,
        now_ms: i64,
    ) -> Self {
        Self {
            title: summary.title.clone(),
            project: summary.project.clone(),
            agent: summary.agent,
            model: summary.model.clone(),
            host: summary.host.clone(),
            issue: summary
                .tracker_issue
                .as_ref()
                .map(|issue| match summary.tracker_state {
                    Some(state) => format!("{} · {}", issue.identifier, state.word()),
                    None => format!("{} · Linear", issue.identifier),
                }),
            is_alarm: is_alarming(summary),
            moving: None,
            status: card_summary(summary, standing, tick, build),
            extras: card_extras(summary, now_ms),
        }
    }
}

fn moving_stage(app: &App, task_id: &str) -> Option<String> {
    let moving = app.moving.as_ref()?;
    if moving.task_id != task_id {
        return None;
    }
    let update = moving.watching.updates.borrow().clone();
    Some(if update.total == 0 {
        update.stage
    } else {
        format!(
            "{} {}/{}",
            update.stage.trim_end_matches('…'),
            update.done,
            update.total
        )
    })
}

fn cards(app: &App) -> Vec<Card> {
    let occupied = app
        .tasks
        .iter()
        .filter(|task| task.facts.sandbox_available)
        .count();
    let has_room = app
        .capacity
        .is_none_or(|capacity| capacity.free_slots_after(occupied) > 0);
    let tick = app.animation_tick();
    let now = now_ms();
    let mut cards: Vec<Card> = app
        .visible_tasks()
        .map(|task| {
            let summary = TaskSummary::of(task);
            let standing = match tasks_ahead_in_queue(&app.tasks, &task.id) {
                Some(ahead) => Standing::Queued { ahead, has_room },
                None => Standing::NotQueued,
            };
            let mut card = Card::of(
                &summary,
                standing,
                tick,
                app.environment_builds.get(&task.id),
                now,
            );
            if let Some(stage) = moving_stage(app, &task.id) {
                card.status = Line::styled(stage, palette::muted());
                card.moving = Some(task.id.clone());
            }
            card
        })
        .collect();
    cards.extend(
        app.visible_remote_tasks()
            .map(|held| Card::of(&held.summary, Standing::NotQueued, tick, None, now)),
    );
    cards
}

fn draw_pending(frame: &mut Frame, area: Rect, pending: &PendingTask, tick: Option<usize>) {
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(palette::muted())
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }
    let label = Rect { height: 1, ..inner };
    frame.render_widget(
        Paragraph::new(Line::from(shimmer(&pending.label, tick))).wrap(Wrap { trim: true }),
        label,
    );
    let where_to = match &pending.address {
        Some(address) => format!("sending to {address}…"),
        None => "starting…".to_owned(),
    };
    let below = Rect {
        y: inner.y.saturating_add(1),
        height: 1,
        ..inner
    };
    frame.render_widget(
        Paragraph::new(Line::styled(where_to, palette::muted())),
        below,
    );
}

fn draw_card(frame: &mut Frame, area: Rect, card: &Card, focused: bool) {
    frame.render_widget(Clear, area);
    // What has to be looked at is marked out by its frame, so it can be found
    // without reading every card. Focus still owns the accent, and the alarm
    // keeps the title, so the two do not cancel each other on the card in front.
    let border = match (focused, card.is_alarm, card.moving.is_some()) {
        (_, _, true) => palette::muted(),
        (true, _, false) => palette::accent(),
        (false, true, false) => palette::bad(),
        (false, false, false) => palette::muted(),
    };
    let title_style = match (focused, card.is_alarm) {
        (_, true) => palette::bad().bold(),
        (true, false) => palette::heading(),
        (false, false) => palette::primary(),
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(border)
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height == 0 {
        return;
    }

    let model = card.model.as_deref().map(|model| {
        model
            .rsplit('/')
            .next()
            .unwrap_or(model)
            .to_ascii_lowercase()
    });
    let agent = match (card.agent, model) {
        (Some(kind), Some(model)) => {
            format!("{}-{model}", kind.to_string().to_ascii_lowercase())
        }
        (Some(kind), None) => kind.to_string(),
        (None, Some(model)) => model,
        (None, None) => "no agent".to_owned(),
    };
    // The title is part of the card, not its frame: a long title wraps inside
    // the card rather than being cut short on the border.
    let mut lines = vec![
        Line::styled(card.title.clone(), title_style),
        card.status.clone(),
    ];
    if let Some(issue) = &card.issue {
        lines.push(Line::styled(issue.clone(), palette::accent()));
    }
    lines.push(Line::styled(card.project.clone(), palette::muted()));
    lines.push(Line::styled(
        format!("{agent} on {}", card.host),
        palette::muted(),
    ));
    lines.extend(card.extras.iter().cloned());
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

fn card_summary(
    summary: &TaskSummary,
    standing: Standing,
    tick: Option<usize>,
    build: Option<&EnvironmentProgress>,
) -> Line<'static> {
    let mut line = if let Some(kind) = summary.blocking_signal_kind {
        let view = signal::view(kind);
        Line::from(Span::styled(
            format!("{} {}", view.symbol, view.task_label),
            view.tone.bold(),
        ))
    } else {
        card_status(summary, standing, tick, build)
    };
    if summary.unresolved_signal_count > 0 {
        let count = summary.unresolved_signal_count;
        let style = line.style;
        line = Line::from_iter(line.spans.into_iter().chain([
            Span::styled(" · ", palette::muted()),
            Span::styled(
                if count == 1 {
                    "1 unresolved".to_owned()
                } else {
                    format!("{count} unresolved")
                },
                palette::muted(),
            ),
        ]))
        .style(style);
    }
    line
}

fn card_status(
    summary: &TaskSummary,
    standing: Standing,
    tick: Option<usize>,
    build: Option<&EnvironmentProgress>,
) -> Line<'static> {
    if let Some(progress) = build {
        return Line::from_iter(shimmer(&build_label(progress, 40), tick));
    }
    match summary.status {
        TaskExecutionStatus::Pending => {
            // Waiting for room and starting are both recorded as `Pending`; how
            // many tasks are served first is the whole difference to the person
            // watching, so it is what the card says.
            let label = match standing {
                Standing::Queued {
                    ahead: 0,
                    has_room: false,
                } => "next up · waiting for capacity…".to_owned(),
                Standing::Queued { ahead, .. } if ahead > 0 => {
                    format!("waiting for capacity · {ahead} ahead")
                }
                _ => "starting up…".to_owned(),
            };
            Line::from_iter(shimmer(&label, tick))
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
            Some(TaskExecutionPauseReason::ApplicationShutdown) => "paused · machine restarted",
            Some(TaskExecutionPauseReason::AgentWentQuiet) => "paused · the agent went quiet",
            Some(TaskExecutionPauseReason::AwaitingLimit) => "paused · waiting on a usage limit",
            Some(TaskExecutionPauseReason::Transfer) => "paused · transferring",
            Some(TaskExecutionPauseReason::TrackerEnded) => "paused · the issue ended",
            Some(TaskExecutionPauseReason::StoppedInTracker) => "paused · stopped in the tracker",
            Some(TaskExecutionPauseReason::DelegationWithdrawn) => "paused · no longer delegated",
            None => "paused",
        })
        .style(palette::attention()),
        TaskExecutionStatus::Completed => Line::styled("completed", palette::muted()),
        TaskExecutionStatus::Failed => Line::styled("failed", palette::bad()),
    }
}

fn is_alarming(summary: &TaskSummary) -> bool {
    summary.status == TaskExecutionStatus::Paused
        && summary.pause_reason.as_ref() == Some(&TaskExecutionPauseReason::AwaitingOperator)
        && summary.attention == TaskOperatorAttentionLevel::Alarm
}

fn card_extras(summary: &TaskSummary, now_ms: i64) -> Vec<Line<'static>> {
    let mut extras = Vec::new();
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

fn build_label(progress: &EnvironmentProgress, width: usize) -> String {
    let count = format!(" ({}/{})", progress.completed + 1, progress.total);
    format!(
        "{}{count}",
        truncate(&progress.label, width.saturating_sub(count.chars().count()))
    )
}
