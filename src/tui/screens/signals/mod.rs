use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
};

use crate::{
    task::{TaskAsk, TaskAskAuthor, TaskAskKind},
    tui::{
        App,
        widgets::{
            columns::Columns,
            header, markdown,
            surface::{self, Surface},
            text::TextBox,
        },
    },
};

pub(super) fn one_line(body: &str) -> String {
    markdown::preview(body)
}

fn who(resolved_by: Option<TaskAskAuthor>, agent: &str) -> &str {
    match resolved_by {
        Some(TaskAskAuthor::Operator) => "you",
        _ => agent,
    }
}

mod conversation;
mod frame;
mod plan;
mod threads;

use conversation::draw_conversation;
use frame::{draw_pane_title, draw_plan_tabs, split_plan_and_activity};
use plan::draw_activity;
use threads::draw_threads;

const LIST_WIDTH: u16 = 40;
const PLAN_PADDING: u16 = 1;
// Wide enough for the list, the conversation and a side column of the same width as
// the list: the two halves of the window are meant to look like halves.
const PLAN_COLUMN_MIN: u16 = 122;
const PANE_TITLE_ROWS: u16 = 2;
const LIST_PADDING: u16 = 1;
const COMPOSER_HEIGHT: u16 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalFocus {
    List,
    Detail,
    Plan,
    Activity,
    Action(usize),
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalAction {
    Reply,
    Resolve,
    Reopen,
    Accept,
    Apply,
}

impl SignalAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Accept => "accept",
            Self::Apply => "apply change",
            Self::Resolve => "resolve",
            Self::Reopen => "reopen",
        }
    }
}

pub fn primary_action(thread: &TaskAsk) -> Option<SignalAction> {
    if thread.is_resolved() {
        return Some(SignalAction::Reopen);
    }
    // A thread that carries a change is answered by making it: the operator's other
    // option, replying, is still there beside it.
    if thread.proposal.is_some() {
        return Some(SignalAction::Apply);
    }
    Some(match thread.kind {
        TaskAskKind::BlockingQuestion => SignalAction::Reply,
        TaskAskKind::ReadyForReview => SignalAction::Accept,
    })
}

// Closing is theirs on any thread, an unanswered review included: they can see that
// the work has moved on, and the agent reading the thread is told what they decided.
pub fn secondary_action(thread: &TaskAsk) -> Option<SignalAction> {
    if thread.is_resolved() {
        return None;
    }
    Some(
        if thread.kind == TaskAskKind::BlockingQuestion || thread.proposal.is_some() {
            SignalAction::Resolve
        } else {
            SignalAction::Reply
        },
    )
}

pub fn actions(thread: &TaskAsk) -> Vec<SignalAction> {
    secondary_action(thread)
        .into_iter()
        .chain(primary_action(thread))
        .collect()
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
    let [title, divider, threads] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(list);
    surface::cover(frame, title);
    draw_pane_title(
        frame,
        Rect {
            x: title.x.saturating_add(LIST_PADDING),
            width: title.width.saturating_sub(LIST_PADDING),
            ..title
        },
        "HISTORY",
        format!("  {}", app.signal_threads.len()),
    );
    surface::horizontal_divider(frame, list, divider.y);
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
