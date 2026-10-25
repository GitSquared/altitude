//! Signals: what is waiting on the operator, one thread at a time.
//!
//! Fullscreen, in two columns. The list on the left is every thread, fat enough
//! to read at a glance, and the one it has selected is the conversation on the
//! right: what each side said, and the actions available on it. The left
//! column draws no background, so the terrain behind it shows through the gaps
//! between rows; the conversation is an opaque pane.
//!
//! A thread has one state: unresolved or resolved. Reading is not a separate
//! fact. An info log stays unresolved until the operator leaves the thread it
//! was reading, so it cannot disappear before it has been seen. Accepting a
//! review is a resolve that also ends the task, behind a confirmation.

use std::{collections::HashMap, time::Duration};

use chrono::Utc;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{
    task::{TaskOperatorSignalAuthor, TaskOperatorSignalKind, TaskOperatorSignalThread},
    tui::{
        App,
        widgets::{
            card, chrome,
            columns::Columns,
            header,
            list::{self, Row},
            palette, signal,
            surface::{self, Surface},
            text::TextBox,
            utils::{ago, shimmer, since, truncate},
        },
    },
};

/// How wide the list asks to be, inside the frame, gutter included. Wide enough
/// for a title and a line of what was said, and no wider: the conversation is
/// what the room is for.
const LIST_WIDTH: u16 = 40;
/// The gutter between the list's rows and the frame, so the rows do not sit on
/// the border.
const LIST_PADDING: u16 = 1;
/// How many lines the reply box gets, border included.
const COMPOSER_HEIGHT: u16 = 5;

/// Which threads the list shows.
///
/// The groups are the filters: a kind has one heading, and choosing that heading
/// is how the operator asks for the whole history of that kind. `Unresolved`
/// keeps a just-resolved thread for a moment so it can be seen leaving rather
/// than vanishing under the cursor.
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
    /// Whether a thread belongs in this view. The kind filters keep that kind's
    /// history; the unresolved view adds a just-resolved thread for its linger.
    pub fn matches(self, thread: &TaskOperatorSignalThread, now_ms: i64) -> bool {
        match self {
            Self::All => true,
            Self::Unresolved => thread.is_unresolved() || is_lingering(thread, now_ms),
            Self::Resolved => thread.is_resolved(),
            Self::NeedsYou => thread.kind == TaskOperatorSignalKind::Blocked,
            Self::Review => thread.kind == TaskOperatorSignalKind::InReview,
            Self::Decisions => thread.kind == TaskOperatorSignalKind::Decision,
            Self::Info => thread.kind == TaskOperatorSignalKind::Update,
        }
    }
}

/// How long a resolved thread stays in the unresolved view.
pub const LINGER_MS: i64 = 5_000;

pub fn is_lingering(thread: &TaskOperatorSignalThread, now_ms: i64) -> bool {
    thread.is_resolved()
        && thread
            .resolved_at_ms
            .is_some_and(|at| now_ms.saturating_sub(at) < LINGER_MS)
}

/// Which way the threads inside a tier read, by when the thread was raised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalSort {
    Oldest,
    Newest,
}

/// What the keyboard is focused on inside the signals screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalFocus {
    List,
    Detail,
    /// One of the available actions, by its place in the footer.
    Action(usize),
    Filter,
    Sort,
}

impl SignalFocus {
    /// The next focus in the ring. The action buttons are only in the ring when
    /// this thread has any.
    pub fn next(self, forward: bool, actions: usize) -> Self {
        let mut ring = vec![Self::List, Self::Detail];
        ring.extend((0..actions).map(Self::Action));
        ring.push(Self::Filter);
        ring.push(Self::Sort);
        let at = ring.iter().position(|focus| *focus == self).unwrap_or(0);
        let at = if forward {
            (at + 1) % ring.len()
        } else {
            (at + ring.len() - 1) % ring.len()
        };
        ring[at]
    }
}

/// What a click on the signals screen can land on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalHit {
    List,
    Thread(usize),
    Detail,
    Message(usize),
    Action(usize),
    ComposerSend,
    ComposerCancel,
    Filter,
    Sort,
    FilterOffer(usize),
    SortOffer(usize),
}

/// What the operator can do with the selected thread.
///
/// The screen shows one primary action and, where there is a second, one
/// secondary action. They are the same gestures everywhere: enter is primary,
/// ctrl-enter is secondary. What they mean follows from the kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalAction {
    Reply,
    Resolve,
    Accept,
}

impl SignalAction {
    /// What the button and the hint call this action in a given thread.
    pub fn label(self, kind: TaskOperatorSignalKind, resolved: bool) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Accept => "accept",
            Self::Resolve => match (kind, resolved) {
                (TaskOperatorSignalKind::Update, _) => "mark read",
                (TaskOperatorSignalKind::Decision, true) => "reopen",
                (TaskOperatorSignalKind::Decision, false) => "acknowledge",
                _ => "resolve",
            },
        }
    }
}

/// The action enter takes on this thread.
pub fn primary_action(thread: &TaskOperatorSignalThread) -> Option<SignalAction> {
    if thread.is_resolved() {
        return (thread.kind == TaskOperatorSignalKind::Decision).then_some(SignalAction::Resolve);
    }
    Some(match thread.kind {
        TaskOperatorSignalKind::Blocked => SignalAction::Reply,
        TaskOperatorSignalKind::Update | TaskOperatorSignalKind::Decision => SignalAction::Resolve,
        TaskOperatorSignalKind::InReview => SignalAction::Accept,
    })
}

/// The action ctrl-enter takes. A blocked thread's primary is already a reply,
/// so there is no second thing to offer there.
pub fn secondary_action(thread: &TaskOperatorSignalThread) -> Option<SignalAction> {
    if thread.is_resolved() {
        return matches!(
            thread.kind,
            TaskOperatorSignalKind::Decision | TaskOperatorSignalKind::Blocked
        )
        .then_some(SignalAction::Reply);
    }
    (thread.kind != TaskOperatorSignalKind::Blocked).then_some(SignalAction::Reply)
}

/// The actions available on a thread, in footer order.
pub fn actions(thread: &TaskOperatorSignalThread) -> Vec<SignalAction> {
    primary_action(thread)
        .into_iter()
        .chain(secondary_action(thread))
        .collect()
}

/// How a group reads: its name, the mark beside it, and its tone.
struct Group {
    title: &'static str,
    symbol: char,
    tone: Style,
}

fn group_of(thread: &TaskOperatorSignalThread, linger: bool) -> Group {
    if thread.is_resolved() && !linger {
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
    // The runner clears what was behind this screen and draws the field; the
    // surface below is what the screen puts on top of it.
    let [header_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let task = app.signal_task_title();
    let showing = app.signal_filter.chosen_value().copied();
    let count = match showing {
        Some(SignalFilter::Unresolved) => {
            let unresolved = app
                .signal_threads
                .iter()
                .filter(|thread| thread.is_unresolved())
                .count();
            match unresolved {
                1 => "1 unresolved".to_owned(),
                count => format!("{count} unresolved"),
            }
        }
        Some(SignalFilter::Resolved) => match app.signal_threads.len() {
            1 => "1 resolved".to_owned(),
            count => format!("{count} resolved"),
        },
        _ => match app.signal_threads.len() {
            1 => "1 thread".to_owned(),
            count => format!("{count} threads"),
        },
    };
    header::draw(frame, header_area, "SIGNALS", Some(&task), Some(&count));
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
    // The conversation is opaque: it is a pane, not a window onto the field.
    frame.render_widget(Clear, detail);
    draw_threads(frame, threads, app);
    draw_conversation(frame, detail, app);
    // The controls sit above the list they change in their own subdivision of the
    // panel; their open lists overlay the rows rather than pushing them.
    surface::cover(frame, toolbar);
    draw_controls(frame, toolbar, threads, app);
    surface::horizontal_divider(frame, list, divider.y);
}

/// The filter and sort pills at the top of the list.
fn draw_controls(frame: &mut Frame, toolbar: Rect, popup_area: Rect, app: &mut App) {
    let filter_at = Rect {
        x: toolbar.x.saturating_add(LIST_PADDING),
        y: toolbar.y,
        width: app
            .signal_filter
            .pill_width(true)
            .min(toolbar.width.saturating_sub(LIST_PADDING)),
        height: 1,
    };
    let sort_at = Rect {
        x: filter_at.right().saturating_add(1),
        y: toolbar.y,
        width: app.signal_sort.pill_width(true).min(
            toolbar
                .right()
                .saturating_sub(filter_at.right().saturating_add(1)),
        ),
        height: 1,
    };
    let filter_focused = app.signal_focus == SignalFocus::Filter || app.signal_filter.is_open();
    let sort_focused = app.signal_focus == SignalFocus::Sort || app.signal_sort.is_open();
    app.signal_filter
        .draw_pill(frame, filter_at, filter_focused, true);
    app.signal_sort
        .draw_pill(frame, sort_at, sort_focused, true);
    app.signal_targets.add(filter_at, SignalHit::Filter);
    app.signal_targets.add(sort_at, SignalHit::Sort);
    if app.signal_filter.is_open() {
        for (rect, index) in app
            .signal_filter
            .draw_list_below(frame, popup_area, filter_at)
        {
            app.signal_targets.add(rect, SignalHit::FilterOffer(index));
        }
    }
    if app.signal_sort.is_open() {
        for (rect, index) in app.signal_sort.draw_list_below(frame, popup_area, sort_at) {
            app.signal_targets.add(rect, SignalHit::SortOffer(index));
        }
    }
}

/// The list of threads, in the left half of the frame, with the field visible
/// through it.
fn draw_threads(frame: &mut Frame, area: Rect, app: &mut App) {
    app.signal_targets.add(area, SignalHit::List);
    let agent = app.signal_agent_label();
    let now = Utc::now().timestamp_millis();
    let keeping = app.signal_filter.chosen_value() == Some(&SignalFilter::Unresolved);
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for thread in &app.signal_threads {
        let linger = keeping && is_lingering(thread, now);
        *counts.entry(group_of(thread, linger).title).or_default() += 1;
    }
    let mut previous: Option<&'static str> = None;
    let rows = app
        .signal_threads
        .iter()
        .map(|thread| {
            let linger = keeping && is_lingering(thread, now);
            let group = group_of(thread, linger);
            let starts = previous != Some(group.title);
            previous = Some(group.title);
            let view = signal::view(thread.kind);
            let unresolved = thread.is_unresolved();
            let last = thread.messages.last();
            let note = last.map(|message| since(message.at_ms, now));
            let detail = match (unresolved, last) {
                (false, Some(message)) => format!(
                    "resolved by {} · {}",
                    who(thread.resolved_by, &agent),
                    one_line(&message.body)
                ),
                (false, None) => format!("resolved by {}", who(thread.resolved_by, &agent)),
                (true, Some(message)) => {
                    let author = match message.author {
                        TaskOperatorSignalAuthor::Operator => "you",
                        TaskOperatorSignalAuthor::Agent => agent.as_str(),
                    };
                    format!("{author}: {}", one_line(&message.body))
                }
                (true, None) => view.label.to_owned(),
            };
            let arriving = app.animations()
                && unresolved
                && app
                    .signal_arrivals
                    .get(&thread.id)
                    .is_some_and(|noticed| noticed.elapsed() < Duration::from_millis(1600));
            // The group heading carries the kind's mark and colour, so the row
            // can spend its width on the title and the age instead.
            let mut row = Row::new(thread.title.clone()).detail(detail).gap(true);
            if starts {
                let mark = vec![Span::styled(
                    format!("{} ", group.symbol),
                    group.tone.bold(),
                )];
                row = row
                    .heading(group.title, counts[group.title].to_string())
                    .heading_kind(mark, group.tone.bold());
            }
            if arriving {
                row = row.tone(palette::accent());
            } else if !unresolved {
                row = row.tone(palette::muted());
            } else {
                row = row.tone(view.tone);
            }
            if let Some(note) = note {
                row = row.note(note);
            }
            row
        })
        .collect::<Vec<_>>();
    let visible = list::visible(area.height, &rows);
    app.signals_cursor
        .follow(rows.len(), visible, app.animations());
    let selected = (!rows.is_empty()).then(|| app.signals_cursor.selected());
    let focused = (app.signal_focus == SignalFocus::List)
        .then_some(selected)
        .flatten();
    let hitboxes = list::draw(
        frame,
        area,
        &rows,
        app.signals_cursor.offset(),
        selected,
        focused,
        LIST_PADDING,
    );
    for (rect, index) in hitboxes {
        app.signal_targets.add(rect, SignalHit::Thread(index));
    }
}

/// The selected thread: its title, conversation and actions, on the surface of
/// the right half.
fn draw_conversation(frame: &mut Frame, area: Rect, app: &mut App) {
    app.signal_targets.add(area, SignalHit::Detail);
    // Opaque by construction: the field is never drawn on this side.
    let padded = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(2),
        ..area
    };
    let heading = match app.signal_thread() {
        Some(thread) => truncate(&thread.title, usize::from(padded.width)),
        None if app.remote_signals_loading => "Asking the machine that owns this task".to_owned(),
        None => "Nothing is asking for you".to_owned(),
    };
    let [title, meta, _, rest] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .areas(padded);
    frame.render_widget(
        Paragraph::new(Line::styled(
            heading,
            if app.signal_thread().is_some() {
                if app.signal_focus == SignalFocus::Detail {
                    palette::accent()
                } else {
                    palette::heading()
                }
            } else {
                palette::muted()
            },
        )),
        title,
    );
    let Some(thread) = app.signal_thread().cloned() else {
        draw_empty(frame, rest, app);
        return;
    };
    let agent = app.signal_agent_label();
    let now = Utc::now().timestamp_millis();
    let view = signal::view(thread.kind);
    let meta_tone = if thread.is_resolved() {
        palette::muted()
    } else {
        view.tone
    };
    let meta_mark = if thread.is_resolved() {
        '✓'
    } else {
        view.symbol
    };
    let mut meta_line = vec![Span::styled(
        format!("{meta_mark} {}", view.label.to_lowercase()),
        meta_tone,
    )];
    if thread.is_resolved() {
        meta_line.push(Span::styled(
            format!(
                " · resolved by {} {} · raised {}",
                who(thread.resolved_by, &agent),
                ago(
                    thread.resolved_at_ms.unwrap_or(thread.last_raised_at_ms),
                    now
                ),
                ago(thread.last_raised_at_ms, now)
            ),
            palette::muted(),
        ));
    } else {
        meta_line.push(Span::styled(
            format!(" · raised {}", ago(thread.last_raised_at_ms, now)),
            palette::muted(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(meta_line)), meta);
    let composing = app.signal_composer.is_some();
    let tail_height = if composing { COMPOSER_HEIGHT + 2 } else { 2 };
    let [cards, tail] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(tail_height)]).areas(rest);
    let mut scroll = app.signal_scroll;
    let selected_message = (app.signal_focus == SignalFocus::Detail).then_some(
        app.signal_message_cursor
            .min(thread.messages.len().saturating_sub(1)),
    );
    // A resolution is part of the conversation, not a message: it follows the
    // last card as a quiet line rather than another bordered block.
    let status = thread.is_resolved().then(|| {
        let at = thread.resolved_at_ms.unwrap_or(thread.last_raised_at_ms);
        Line::styled(
            format!(
                "✓ resolved by {} · {}",
                who(thread.resolved_by, &agent),
                ago(at, now)
            ),
            palette::muted(),
        )
    });
    let messages = draw_messages(
        frame,
        cards,
        &thread,
        &agent,
        &mut scroll,
        selected_message,
        status,
    );
    for (rect, index) in messages {
        app.signal_targets.add(rect, SignalHit::Message(index));
    }
    if composing {
        let [box_area, actions_area] =
            Layout::vertical([Constraint::Length(COMPOSER_HEIGHT), Constraint::Length(2)])
                .areas(tail);
        draw_composer(frame, box_area, app, &agent);
        let buttons = draw_buttons(
            frame,
            area,
            actions_area,
            &[
                chrome::Action::primary("send").focused(true),
                chrome::Action::secondary("cancel"),
            ],
            None,
        );
        if let Some(rect) = buttons.first() {
            app.signal_targets.add(*rect, SignalHit::ComposerSend);
        }
        if let Some(rect) = buttons.get(1) {
            app.signal_targets.add(*rect, SignalHit::ComposerCancel);
        }
    } else {
        let actions = actions(&thread);
        let buttons: Vec<_> = actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let label = action.label(thread.kind, thread.is_resolved());
                let focused = app.signal_focus == SignalFocus::Action(index);
                if index == 0 {
                    chrome::Action::primary(label).focused(focused)
                } else {
                    chrome::Action::secondary(label).focused(focused)
                }
            })
            .collect();
        let status = thread.is_resolved().then(|| {
            format!(
                "resolved {}",
                ago(
                    thread.resolved_at_ms.unwrap_or(thread.last_raised_at_ms),
                    now
                )
            )
        });
        let rects = draw_buttons(frame, area, tail, &buttons, status.as_deref());
        for (index, rect) in rects.into_iter().enumerate() {
            app.signal_targets.add(rect, SignalHit::Action(index));
        }
    }
    app.signal_scroll = scroll;
}

/// The cards of the thread, scrolled by whole messages.
///
/// Scrolling by the card rather than the line keeps the conversation readable:
/// there is no state in which the top of a message is missing with its body
/// underneath. `scroll` is the first message shown, and it is clamped here so
/// asking for the end is asking for the newest messages that fit.
fn draw_messages(
    frame: &mut Frame,
    area: Rect,
    thread: &TaskOperatorSignalThread,
    agent: &str,
    scroll: &mut u16,
    selected: Option<usize>,
    status: Option<Line<'static>>,
) -> Vec<(Rect, usize)> {
    let width = area.width;
    let now = Utc::now().timestamp_millis();
    let mut used = 0;
    let mut latest_first = thread.messages.len();
    for index in (0..thread.messages.len()).rev() {
        let height = card::height(width, &thread.messages[index].body);
        let gap = u16::from(index + 1 < thread.messages.len());
        if latest_first < thread.messages.len() && used + height + gap > area.height {
            break;
        }
        used += height + gap;
        latest_first = index;
    }
    let first = usize::from(*scroll).min(latest_first);
    *scroll = u16::try_from(first).unwrap_or(u16::MAX);
    let mut y = area.y;
    let mut hitboxes = Vec::new();
    for (index, message) in thread.messages.iter().enumerate().skip(first) {
        let height = card::height(width, &message.body);
        if y + height > area.bottom() {
            break;
        }
        let rect = Rect { y, height, ..area };
        let from_operator = message.author == TaskOperatorSignalAuthor::Operator;
        card::draw(
            frame,
            rect,
            &card::Message {
                author: if from_operator { "you" } else { agent },
                when: &ago(message.at_ms, now),
                body: &message.body,
                from_operator,
                focused: selected == Some(index),
            },
        );
        hitboxes.push((rect, index));
        y += height + 1;
    }
    if let Some(status) = status
        && y < area.bottom()
    {
        // The gap after the last card is the blank line; the status takes the
        // next one.
        let y = y.saturating_add(1);
        if y < area.bottom() {
            frame.render_widget(
                Paragraph::new(status),
                Rect {
                    y,
                    height: 1,
                    ..area
                },
            );
        }
    }
    hitboxes
}

/// The reply being written, where the conversation is, rather than in a box over
/// it: the sentence belongs to the thread it will join.
fn draw_composer(frame: &mut Frame, box_area: Rect, app: &App, agent: &str) {
    let Some(composer) = &app.signal_composer else {
        return;
    };
    composer
        .body
        .draw_titled(frame, box_area, true, Some(&format!("reply to {agent}")));
}

/// The buttons at the foot of the conversation. The keys are in the frame's
/// guidance; the buttons say what they do, not which key does it.
fn draw_buttons(
    frame: &mut Frame,
    pane: Rect,
    area: Rect,
    actions: &[chrome::Action<'_>],
    status: Option<&str>,
) -> Vec<Rect> {
    surface::cover(frame, area);
    // The divider starts at the column's divider and ends at the panel's edge,
    // so the footer belongs to the pane rather than floating inside it.
    let frame_rect = Rect {
        x: pane.x.saturating_sub(1),
        width: pane.width.saturating_add(2),
        ..pane
    };
    let rects = chrome::actions(frame, frame_rect, area, actions);
    if let Some(status) = status {
        // With no buttons the whole footer is the status, so it sits at the
        // right edge. With buttons it gets the room to their left.
        let width = rects
            .first()
            .map_or(area.width, |button| button.x.saturating_sub(area.x));
        frame.render_widget(
            Paragraph::new(Line::styled(status.to_owned(), palette::muted())).right_aligned(),
            Rect {
                x: area.x,
                y: area.y.saturating_add(1),
                width,
                height: 1,
            },
        );
    }
    rects
}

fn draw_empty(frame: &mut Frame, area: Rect, app: &App) {
    if app.remote_signals_loading {
        frame.render_widget(
            Paragraph::new(Line::from(shimmer(
                &app.signals_summary().unwrap_or_default(),
                app.animation_tick(),
            ))),
            area,
        );
        return;
    }
    if let Some(summary) = app.signals_summary() {
        frame.render_widget(
            Paragraph::new(summary)
                .style(palette::muted())
                .wrap(Wrap { trim: true }),
            area,
        );
    }
}

/// What the agent's message reads as on one line: its first line, so a list row
/// says what the thread last said without becoming a paragraph.
fn one_line(body: &str) -> String {
    body.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// How the side that resolved a thread reads.
fn who(resolved_by: Option<TaskOperatorSignalAuthor>, agent: &str) -> &str {
    match resolved_by {
        Some(TaskOperatorSignalAuthor::Operator) => "you",
        _ => agent,
    }
}

/// The reply being written, and the thread it will answer.
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

    /// Whether there is nothing to send.
    pub fn is_blank(&self) -> bool {
        self.body.value().trim().is_empty()
    }
}
