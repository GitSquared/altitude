use std::{collections::HashMap, mem};

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::Line,
    widgets::{Clear, Paragraph},
};

use crate::{
    isolation::EnvironmentProgress,
    remote::TaskSummary,
    task::{
        SIGNAL_ORDER, TaskExecutionMode, TaskExecutionPauseReason, TaskExecutionStatus,
        TaskOperatorSignalKind, place_in_queue,
    },
    tui::{
        App,
        app::PendingTask,
        widgets::{
            empty,
            format::{ago, clock, truncate},
            list::Row,
            palette,
            pointer::Targets,
            select::Select,
            shimmer::shimmer,
            signal,
            surface::{self, Surface},
            tabs::{self, Tab},
        },
    },
    vcs::{ContributionReadiness, now_ms},
};

// One column of air between the list and the pane's frame, on both sides.
const LIST_PADDING: u16 = 1;

// The order the list reads in: what has broken, then what wants a person, then
// what wants reading, then the work in flight, then what is only waiting. The
// order of the variants is the order of the sections.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Standing {
    Failed,
    NeedsYou,
    ReadyToReview,
    Decisions,
    Info,
    Working,
    Queued,
    Limited,
    Paused,
    Done,
}

impl Standing {
    fn of(summary: &TaskSummary) -> Self {
        match summary.status {
            TaskExecutionStatus::Failed => return Self::Failed,
            TaskExecutionStatus::Completed => return Self::Done,
            _ => {}
        }
        match summary.leading_signal_kind {
            Some(TaskOperatorSignalKind::Blocked) => return Self::NeedsYou,
            Some(TaskOperatorSignalKind::InReview) => return Self::ReadyToReview,
            Some(TaskOperatorSignalKind::Decision) => return Self::Decisions,
            Some(TaskOperatorSignalKind::Update) => return Self::Info,
            None => {}
        }
        // A question that came through the pause rather than a signal is the same
        // thing to the person reading the list; a limit is the one pause that lets
        // go on its own, so it is the one that is not a wait on anybody.
        match summary.pause_reason {
            Some(TaskExecutionPauseReason::AwaitingOperator) => return Self::NeedsYou,
            Some(TaskExecutionPauseReason::AwaitingLimit) => return Self::Limited,
            _ => {}
        }
        match summary.status {
            TaskExecutionStatus::Running => Self::Working,
            TaskExecutionStatus::Pending => Self::Queued,
            _ => Self::Paused,
        }
    }

    // The section a row is filed under on the list that shows everything: an update
    // is a report from the same work, not a place of its own.
    fn filed(self) -> Self {
        match self {
            Self::Info => Self::Working,
            other => other,
        }
    }

    fn title(self) -> String {
        match self {
            Self::Failed => "FAILED".to_owned(),
            Self::Working => "WORKING".to_owned(),
            Self::Queued => "QUEUED".to_owned(),
            Self::Limited => "LIMITED".to_owned(),
            Self::Paused => "PAUSED".to_owned(),
            Self::Done => "DONE".to_owned(),
            // The signals' own words, in the case a heading is written in.
            Self::NeedsYou => signal::view(TaskOperatorSignalKind::Blocked)
                .task_label
                .to_uppercase(),
            Self::ReadyToReview => signal::view(TaskOperatorSignalKind::InReview)
                .task_label
                .to_uppercase(),
            Self::Decisions => signal::view(TaskOperatorSignalKind::Decision)
                .task_label
                .to_uppercase(),
            Self::Info => signal::view(TaskOperatorSignalKind::Update)
                .task_label
                .to_uppercase(),
        }
    }

    fn mark(self) -> char {
        match self {
            Self::Failed => '✕',
            Self::NeedsYou => signal::view(TaskOperatorSignalKind::Blocked).symbol,
            Self::ReadyToReview => signal::view(TaskOperatorSignalKind::InReview).symbol,
            Self::Decisions => signal::view(TaskOperatorSignalKind::Decision).symbol,
            Self::Info => signal::view(TaskOperatorSignalKind::Update).symbol,
            Self::Working => '●',
            Self::Queued => '◔',
            Self::Limited => '◷',
            Self::Paused => '▮',
            Self::Done => '✓',
        }
    }

    fn tone(self) -> Style {
        match self {
            Self::Failed => palette::bad(),
            Self::NeedsYou => palette::attention(),
            Self::ReadyToReview => palette::good(),
            Self::Decisions => palette::decision(),
            Self::Info | Self::Queued | Self::Limited | Self::Paused | Self::Done => {
                palette::muted()
            }
            Self::Working => palette::primary(),
        }
    }
}

// What the list is allowed to show. `Active` is everything that is still
// work, which is why done tasks are a filter rather than an absence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskFilter {
    Active,
    NeedsYou,
    ReadyToReview,
    Decisions,
    Info,
    Working,
    Queued,
    Limited,
    Paused,
    Failed,
    Done,
}

impl TaskFilter {
    fn matches(self, standing: Standing) -> bool {
        match self {
            Self::Active => standing != Standing::Done,
            Self::NeedsYou => standing == Standing::NeedsYou,
            Self::ReadyToReview => standing == Standing::ReadyToReview,
            Self::Decisions => standing == Standing::Decisions,
            Self::Info => standing == Standing::Info,
            Self::Working => standing == Standing::Working,
            Self::Queued => standing == Standing::Queued,
            Self::Limited => standing == Standing::Limited,
            Self::Paused => standing == Standing::Paused,
            Self::Failed => standing == Standing::Failed,
            Self::Done => standing == Standing::Done,
        }
    }
}

// How rows are ordered inside their section. The sections themselves are the
// ranking, so there is no "by state" to choose: it is the order they are in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskSort {
    Recent,
    Newest,
    Oldest,
}

// Which task a row is. The order on screen is the ranking's, so a key pressed
// on the list has to find the task it was pressed on by name rather than by
// counting the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaskRef {
    Local(String),
    Remote { address: String, id: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskControl {
    Filter,
    Sort,
    Offer(Menu, usize),
}

// Which menu an offer belongs to, so a click can go back to the pill it came
// from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Menu {
    Filter,
    Sort,
}

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    app.task_targets.clear();
    app.project_targets.clear();
    let inner = Surface::new().draw(frame, area);
    let [header, controls, rule, body] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    surface::horizontal_divider(frame, inner, rule.y);
    draw_tabs(frame, header, inner, app);
    // The list is given the pane's own width so the bar under a row runs edge to
    // edge; the gutter is what keeps the text in.
    let body = Rect {
        x: inner.x,
        width: inner.width,
        ..body
    };
    let pending = u16::try_from(app.pending_tasks.len().min(2)).unwrap_or(0);
    let [list_area, waiting] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(pending)]).areas(body);

    let mut built = rows(app);
    app.task_order = mem::take(&mut built.order);
    if let Some(reveal) = app.pending_reveal.take()
        && let Some(index) = app.task_order.iter().position(|held| *held == reveal)
    {
        app.tasks_list.select(index);
    }
    if built.rows.is_empty() {
        empty::draw(
            frame,
            list_area,
            if built.narrowed {
                "Nothing matches this filter"
            } else {
                "No tasks yet"
            },
        );
    } else {
        let hitboxes = app.tasks_list.draw(
            frame,
            list_area,
            &built.rows,
            app.animation_tick(),
            true,
            LIST_PADDING,
        );
        for (rect, hit) in hitboxes {
            app.task_targets.add(rect, hit);
        }
    }
    draw_controls(frame, controls, inner, app);
    for (offset, pending) in app
        .pending_tasks
        .iter()
        .take(usize::from(pending))
        .enumerate()
    {
        draw_pending(
            frame,
            Rect {
                y: waiting.y.saturating_add(offset as u16),
                height: 1,
                ..waiting
            },
            pending,
            app.animation_tick(),
        );
    }
}

// The pills are the first column's header — it is the column they act on — and
// the columns after it are named where the rows put them.
fn draw_controls(frame: &mut Frame, area: Rect, popup_area: Rect, app: &mut App) {
    app.task_control_targets.clear();
    surface::cover(frame, area);
    // The pills start where the first column's text starts, which is inside the
    // gutter rather than at the frame's edge.
    let left = area.x.saturating_add(LIST_PADDING);
    let mut at = Rect {
        x: left,
        y: area.y,
        height: 1,
        ..area
    };
    at = draw_pill(
        frame,
        at,
        popup_area,
        &app.task_filter,
        TaskControl::Filter,
        Menu::Filter,
        &mut app.task_control_targets,
    );
    at.x = at.right().saturating_add(1);
    draw_pill(
        frame,
        at,
        popup_area,
        &app.task_sort,
        TaskControl::Sort,
        Menu::Sort,
        &mut app.task_control_targets,
    );
    // The pills are the first column's header, so they stop a column short of the
    // next column's name instead of running into it in a narrow pane.
    let column =
        left.saturating_add(u16::try_from(app.tasks_list.title_room()).unwrap_or(u16::MAX));
    let gap = column.saturating_sub(1).max(left);
    if gap < area.right() {
        surface::cover(
            frame,
            Rect {
                x: gap,
                width: 1,
                height: 1,
                ..area
            },
        );
    }
    app.tasks_list.headers(
        frame,
        area,
        LIST_PADDING,
        &["Signals", "Issue", "PRs", "Age"],
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_pill<T>(
    frame: &mut Frame,
    at: Rect,
    popup_area: Rect,
    select: &Select<T>,
    control: TaskControl,
    menu: Menu,
    targets: &mut Targets<TaskControl>,
) -> Rect {
    let at = Rect {
        width: select
            .pill_width(true)
            .min(at.width.max(select.pill_width(true))),
        ..at
    };
    select.draw_pill(frame, at, select.is_open(), true);
    targets.add(at, control);
    if select.is_open() {
        for (rect, index) in select.draw_list_below(frame, popup_area, at) {
            targets.add(rect, TaskControl::Offer(menu, index));
        }
    }
    at
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

struct Built {
    rows: Vec<Row>,
    order: Vec<TaskRef>,
    // What the filter left out, so the empty list can say which emptiness it is.
    narrowed: bool,
}

struct Held {
    standing: Standing,
    changed_at_ms: i64,
    created_at_ms: i64,
    row: Row,
    held: TaskRef,
}

fn rows(app: &App) -> Built {
    let taking_work = app.pressure.is_none();
    let filter = app
        .task_filter
        .chosen_value()
        .copied()
        .unwrap_or(TaskFilter::Active);
    let mut done_hidden = 0;
    let mut entries: Vec<Held> = Vec::new();
    for task in app.visible_tasks() {
        let summary = TaskSummary::of(task);
        let standing = Standing::of(&summary);
        if !filter.matches(standing) {
            done_hidden += usize::from(standing == Standing::Done);
            continue;
        }
        let live = app
            .moving
            .as_ref()
            .is_some_and(|moving| moving.task_id == task.id);
        let row = row_of(
            &summary,
            standing,
            live,
            detail_of(
                app,
                &summary,
                place_in_queue(&app.tasks, &task.id),
                taking_work,
            ),
        );
        entries.push(Held {
            // Where the row is filed: work that is only reporting does not need a
            // section of its own, so on the unfiltered list it sits with the work.
            standing: if filter == TaskFilter::Active {
                standing.filed()
            } else {
                standing
            },
            changed_at_ms: summary.shown_updated_at_ms,
            created_at_ms: summary.created_at_ms,
            row,
            held: TaskRef::Local(task.id.clone()),
        });
    }
    for held in app.visible_remote_tasks() {
        let summary = &held.summary;
        let standing = Standing::of(summary);
        if !filter.matches(standing) {
            done_hidden += usize::from(standing == Standing::Done);
            continue;
        }
        let mut row = row_of(
            summary,
            standing,
            false,
            detail_of(app, summary, None, taking_work),
        );
        // A machine is a name only when the task is not on this one.
        if summary.tracker_issue.is_none() {
            row = row.reference(format!("on {}", summary.host));
        }
        entries.push(Held {
            // Where the row is filed: work that is only reporting does not need a
            // section of its own, so on the unfiltered list it sits with the work.
            standing: if filter == TaskFilter::Active {
                standing.filed()
            } else {
                standing
            },
            changed_at_ms: summary.shown_updated_at_ms,
            created_at_ms: summary.created_at_ms,
            row,
            held: TaskRef::Remote {
                address: held.address.clone(),
                id: summary.id.clone(),
            },
        });
    }
    let sort = app
        .task_sort
        .chosen_value()
        .copied()
        .unwrap_or(TaskSort::Recent);
    entries.sort_by(|left, right| {
        left.standing.cmp(&right.standing).then_with(|| match sort {
            TaskSort::Recent => right.changed_at_ms.cmp(&left.changed_at_ms),
            TaskSort::Newest => right.created_at_ms.cmp(&left.created_at_ms),
            TaskSort::Oldest => left.created_at_ms.cmp(&right.created_at_ms),
        })
    });

    let mut counts: HashMap<Standing, usize> = HashMap::new();
    for held in &entries {
        *counts.entry(held.standing).or_default() += 1;
    }
    // A heading says which group a row is in. When the filter has left one group
    // there is nothing to say it apart from, so the rows stand on their own.
    let grouped = counts.len() > 1;
    let mut rows = Vec::with_capacity(entries.len());
    let mut order = Vec::with_capacity(entries.len());
    let mut previous: Option<Standing> = None;
    for held in entries {
        let mut row = held.row;
        if grouped && previous != Some(held.standing) {
            previous = Some(held.standing);
            let count = counts.get(&held.standing).copied().unwrap_or(0);
            row = row.heading(
                held.standing.title(),
                count.to_string(),
                held.standing.mark(),
                held.standing.tone(),
            );
        }
        rows.push(row);
        order.push(held.held);
    }
    Built {
        narrowed: done_hidden > 0,
        rows,
        order,
    }
}

fn row_of(summary: &TaskSummary, standing: Standing, live: bool, detail: String) -> Row {
    let mut row = Row::new(summary.title.clone()).detail(detail);
    if summary.shown_updated_at_ms > 0 {
        row = row.age(ago(summary.shown_updated_at_ms, now_ms()));
    }
    if let Some(issue) = &summary.tracker_issue {
        row = row.link(issue.identifier.clone(), issue.url.clone());
    }
    // Every pull request the task opened, in one column: they are the work, and
    // there is usually more than one.
    if let Some(first) = summary.contributions.first() {
        let pulls = summary
            .contributions
            .iter()
            .map(|pull| format!("#{}", pull.reference))
            .collect::<Vec<_>>()
            .join(", ");
        row = row.link(pulls, first.url.clone());
    }
    // What the agent has said that nobody has read, by kind: the section says
    // which one leads, the column says how much of each is waiting.
    if let Some(counts) = signal_counts(summary) {
        row = row.note(counts);
    }
    match standing {
        // Nothing is asked of the operator by work that is only waiting, so it
        // leans back; so does a title the agent has not written yet.
        Standing::Queued | Standing::Paused if !live => row = row.tone(palette::muted()),
        // A title the agent has not written yet is the task's id, and it is set
        // back so it does not read as a name.
        _ if summary.title_generated => row = row.tone(palette::muted()),
        _ => {}
    }
    if live || standing == Standing::Working {
        row = row.shimmer();
    }
    row
}

fn detail_of(app: &App, summary: &TaskSummary, ahead: Option<usize>, taking_work: bool) -> String {
    // What the agent is doing right now is the fact that changes what this row
    // means, so it is the row's own line before anything else it might say.
    if summary.status == TaskExecutionStatus::Running
        && let Some(activity) = current_activity(app, summary)
    {
        return activity;
    }
    // A question is worth reading as itself rather than as a count of questions.
    if let Some(question) = &summary.leading_signal_title
        && summary.status != TaskExecutionStatus::Failed
    {
        return question.clone();
    }
    if let Some(moving) = app
        .moving
        .as_ref()
        .filter(|moving| moving.task_id == summary.id)
    {
        let update = moving.watching.updates.borrow().clone();
        return if update.total == 0 {
            update.stage
        } else {
            format!(
                "{} {}/{}",
                update.stage.trim_end_matches('…'),
                update.done,
                update.total
            )
        };
    }
    let building = app
        .environment_builds
        .get(&summary.id)
        .filter(|_| summary.status == TaskExecutionStatus::Pending)
        .map(build_label);
    if let Some(label) = building {
        return label;
    }
    if summary.status == TaskExecutionStatus::Failed {
        return summary
            .last_error
            .clone()
            .unwrap_or_else(|| "the run ended in an error".to_owned());
    }
    let needing_action = summary.contributions.iter().find(|contribution| {
        contribution
            .state
            .as_ref()
            .is_some_and(|state| state.readiness(now_ms()) == ContributionReadiness::NeedsAction)
    });
    if let Some(contribution) = needing_action {
        let status = contribution.summary.as_deref().unwrap_or("needs action");
        return format!("PR #{} · {status}", contribution.reference);
    }
    if summary.unresolved_signal_count > 0 {
        return notes(summary.unresolved_signal_count);
    }
    match summary.status {
        TaskExecutionStatus::Failed => summary
            .last_error
            .clone()
            .unwrap_or_else(|| "the run ended in an error".to_owned()),
        TaskExecutionStatus::Pending => match ahead {
            Some(0) if !taking_work => "next up".to_owned(),
            Some(ahead) => format!("{ahead} ahead"),
            None => "waiting".to_owned(),
        },
        TaskExecutionStatus::Running => match summary.mode {
            TaskExecutionMode::Autopilot => "working".to_owned(),
            TaskExecutionMode::AutopilotReport => "reporting".to_owned(),
            TaskExecutionMode::Operator => "at the terminal".to_owned(),
        },
        TaskExecutionStatus::Paused => match summary.pause_reason {
            Some(TaskExecutionPauseReason::AwaitingLimit) => limit_of(summary),
            Some(TaskExecutionPauseReason::AwaitingOperator) => "awaiting you".to_owned(),
            Some(TaskExecutionPauseReason::AwaitingReviewChecks) => {
                "watching pull requests".to_owned()
            }
            Some(TaskExecutionPauseReason::ApplicationShutdown) => {
                "the machine restarted".to_owned()
            }
            Some(TaskExecutionPauseReason::AgentWentQuiet) => "the agent went quiet".to_owned(),
            Some(TaskExecutionPauseReason::Transfer) => "transferring".to_owned(),
            _ => "paused".to_owned(),
        },
        TaskExecutionStatus::Completed => "done".to_owned(),
    }
}

// A limit is the one wait with a time on it, so it is the one that says when.
fn limit_of(summary: &TaskSummary) -> String {
    let window = summary
        .limit_window
        .as_ref()
        .map(|window| window.word())
        .unwrap_or("usage");
    match summary.resumes_at_ms {
        Some(at) if at > now_ms() => format!("{window} limit · back at {}", clock(at)),
        _ => format!("{window} limit"),
    }
}

fn current_activity(app: &App, summary: &TaskSummary) -> Option<String> {
    if let Some(lines) = app.activities.get(&summary.id)
        && let Some(last) = lines.last()
    {
        return Some(last.text.clone());
    }
    summary.activity.clone()
}

// Every kind with something waiting, in the order the row reads them: what
// stopped it, what is waiting to be read, what it is asking, what it is telling.
fn signal_counts(summary: &TaskSummary) -> Option<String> {
    let counts = SIGNAL_ORDER
        .iter()
        .filter_map(|kind| {
            let count = summary.signal_counts.count(*kind);
            (count > 0).then(|| format!("{}{count}", signal::view(*kind).symbol))
        })
        .collect::<Vec<_>>();
    (!counts.is_empty()).then(|| counts.join(" "))
}

fn notes(count: u32) -> String {
    if count == 1 {
        "1 note".to_owned()
    } else {
        format!("{count} notes")
    }
}

fn build_label(progress: &EnvironmentProgress) -> String {
    format!("building {}/{}", progress.completed + 1, progress.total)
}

fn draw_pending(frame: &mut Frame, area: Rect, pending: &PendingTask, tick: Option<usize>) {
    let label = match &pending.address {
        Some(address) => format!("{} · sending to {address}…", pending.label),
        None => format!("{} · starting…", pending.label),
    };
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(Line::from(shimmer(
            &truncate(&label, usize::from(area.width)),
            tick,
        ))),
        Rect { height: 1, ..area },
    );
}
