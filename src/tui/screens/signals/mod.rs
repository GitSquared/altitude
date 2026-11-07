use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
};

use crate::{
    task::{TaskOperatorSignalAuthor, TaskOperatorSignalKind, TaskOperatorSignalThread},
    tui::{
        App,
        widgets::{
            columns::Columns,
            header, markdown, palette, signal,
            surface::{self, Surface},
            text::TextBox,
        },
    },
};

pub(super) fn one_line(body: &str) -> String {
    markdown::preview(body)
}

fn who(resolved_by: Option<TaskOperatorSignalAuthor>, agent: &str) -> &str {
    match resolved_by {
        Some(TaskOperatorSignalAuthor::Operator) => "you",
        _ => agent,
    }
}

mod conversation;
mod frame;
mod plan;
mod threads;

use conversation::draw_conversation;
use frame::{draw_plan_tabs, split_plan_and_activity};
use plan::draw_activity;
use threads::{draw_controls, draw_threads};

const LIST_WIDTH: u16 = 40;
const PLAN_PADDING: u16 = 1;
// Wide enough for the list, the conversation and a side column of the same width as
// the list: the two halves of the window are meant to look like halves.
const PLAN_COLUMN_MIN: u16 = 122;
const PANE_TITLE_ROWS: u16 = 2;
const LIST_PADDING: u16 = 1;
const COMPOSER_HEIGHT: u16 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalFilter {
    All,
    Unresolved,
    NeedsYou,
    Review,
    Decisions,
    Info,
    Resolved,
}

impl SignalFilter {
    pub fn matches(self, thread: &TaskOperatorSignalThread) -> bool {
        match self {
            Self::All => true,
            Self::Unresolved => thread.is_unresolved(),
            Self::Resolved => thread.is_resolved(),
            Self::NeedsYou => thread.kind == TaskOperatorSignalKind::Blocked,
            Self::Review => thread.kind == TaskOperatorSignalKind::InReview,
            Self::Decisions => thread.kind == TaskOperatorSignalKind::Decision,
            Self::Info => thread.kind == TaskOperatorSignalKind::Update,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalSort {
    Oldest,
    Newest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalFocus {
    List,
    Detail,
    Plan,
    Activity,
    Action(usize),
    Filter,
    Sort,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalHit {
    List,
    Thread(usize),
    Detail,
    Plan,
    Activity,
    Message(usize),
    Action(usize),
    ComposerSend,
    ComposerCancel,
    Filter,
    Sort,
    FilterOffer(usize),
    SortOffer(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalAction {
    Reply,
    Resolve,
    Accept,
    Apply,
}

impl SignalAction {
    pub fn label(self, kind: TaskOperatorSignalKind, resolved: bool) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Accept => "accept",
            Self::Apply => "apply change",
            Self::Resolve => match (kind, resolved) {
                (TaskOperatorSignalKind::Update, _) => "resolve",
                (_, true) => "reopen",
                (TaskOperatorSignalKind::Decision, false) => "acknowledge",
                _ => "resolve",
            },
        }
    }
}

pub fn primary_action(thread: &TaskOperatorSignalThread) -> Option<SignalAction> {
    if thread.is_resolved() {
        // A closed log stays closed; a question or a decision can be opened again
        // by the person who closed it, and reopening is what puts it back in front
        // of the agent.
        return matches!(
            thread.kind,
            TaskOperatorSignalKind::Decision | TaskOperatorSignalKind::Blocked
        )
        .then_some(SignalAction::Resolve);
    }
    // A blocked thread that carries a change is answered by making it: the
    // operator's other option, replying, is still there beside it.
    if thread.proposal.is_some() {
        return Some(SignalAction::Apply);
    }
    Some(match thread.kind {
        TaskOperatorSignalKind::Blocked => SignalAction::Reply,
        TaskOperatorSignalKind::Update | TaskOperatorSignalKind::Decision => SignalAction::Resolve,
        TaskOperatorSignalKind::InReview => SignalAction::Accept,
    })
}

pub fn secondary_action(thread: &TaskOperatorSignalThread) -> Option<SignalAction> {
    if thread.is_resolved() {
        return matches!(
            thread.kind,
            TaskOperatorSignalKind::Decision | TaskOperatorSignalKind::Blocked
        )
        .then_some(SignalAction::Reply);
    }
    (thread.kind != TaskOperatorSignalKind::Blocked || thread.proposal.is_some())
        .then_some(SignalAction::Reply)
}

pub fn actions(thread: &TaskOperatorSignalThread) -> Vec<SignalAction> {
    secondary_action(thread)
        .into_iter()
        .chain(primary_action(thread))
        .collect()
}

struct Group {
    title: &'static str,
    symbol: char,
    tone: Style,
}

fn group_of(thread: &TaskOperatorSignalThread) -> Group {
    if thread.is_resolved() {
        return Group {
            title: "RESOLVED",
            symbol: '✓',
            tone: palette::muted(),
        };
    }
    let view = signal::view(thread.kind);
    let title = match thread.kind {
        TaskOperatorSignalKind::Blocked => "NEEDS YOU",
        TaskOperatorSignalKind::InReview => "READY TO REVIEW",
        TaskOperatorSignalKind::Decision => "DECISIONS",
        TaskOperatorSignalKind::Update => "INFO LOG",
    };
    Group {
        title,
        symbol: view.symbol,
        tone: view.tone,
    }
}

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    app.signal_targets.clear();
    app.signal_focus_ring.clear();
    app.signal_focus_ring.add(SignalFocus::List);
    // The runner clears what was behind this screen and draws the field; the
    // surface below is what the screen puts on top of it.
    let [header_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    // The screen is the task, not only the thread: it says which task it is the way the
    // list row does, and what the account running it costs, which the pane under this
    // has no row to spare for.
    let mut identification = vec![app.signal_task_title()];
    identification.extend(app.signal_task_identification());
    let detail = identification
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" \u{b7} ");
    let mut standing = vec![app.signal_agent_label()];
    standing.extend(app.signal_task_spend());
    let note = standing.join(" \u{b7} ");
    header::draw(
        frame,
        header_area,
        "TASK DETAILS",
        Some(&detail),
        Some(&note),
    );
    // One frame, split by a single divider: the sections are two halves of the
    // same window rather than two windows that happen to touch.
    let inner = Surface::new().draw(frame, body);
    let columns = Columns::new(&[LIST_WIDTH]).draw(frame, inner);
    let list = columns[0];
    let detail = columns[1];
    let [toolbar, divider, threads] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(list);
    draw_threads(frame, threads, app);
    app.signal_plan_tabs = inner.width < PLAN_COLUMN_MIN;
    if inner.width >= PLAN_COLUMN_MIN {
        // The divider takes a column of the gap rather than a column of the
        // plan, so the plan is cleared over everything it was given — padding
        // included — the way the conversation is.
        let [conversation, _, plan] = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(LIST_WIDTH),
        ])
        .areas(detail);
        surface::vertical_divider(frame, inner, conversation.right());
        app.signal_focus_ring.add(SignalFocus::Detail);
        surface::cover(frame, conversation);
        draw_conversation(frame, conversation, app);
        split_plan_and_activity(frame, plan, app);
    } else {
        // The tabs and the rule under them are one band, so the separators
        // between the tabs run down onto that rule.
        let [band, pane] =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(detail);
        surface::horizontal_divider(frame, detail, band.y.saturating_add(1));
        draw_plan_tabs(frame, band, app);
        surface::cover(frame, pane);
        if app.signal_plan_open {
            split_plan_and_activity(frame, pane, app);
        } else {
            app.signal_focus_ring.add(SignalFocus::Detail);
            draw_conversation(frame, pane, app);
        }
    }
    // The controls sit above the list they change in their own subdivision of the
    // panel; their open lists overlay the rows rather than pushing them.
    surface::cover(frame, toolbar);
    app.signal_focus_ring.add(SignalFocus::Filter);
    app.signal_focus_ring.add(SignalFocus::Sort);
    draw_controls(frame, toolbar, threads, app);
    surface::horizontal_divider(frame, list, divider.y);
    // Whatever the focus was on may have gone with the last draw — a tab closed, a
    // thread with no actions left. Saying so once here is cheaper than every key
    // handler asking whether the thing it is about to act on is still on screen.
    if !app.signal_focus_ring.contains(app.signal_focus) {
        app.signal_focus = SignalFocus::List;
    }
}

pub struct Composer {
    pub task_id: String,
    pub signal_id: String,
    pub body: TextBox,
}

impl Composer {
    pub fn new(task_id: String, signal_id: String) -> Self {
        Self {
            task_id,
            signal_id,
            body: TextBox::new(""),
        }
    }

    pub fn is_blank(&self) -> bool {
        self.body.value().trim().is_empty()
    }
}
