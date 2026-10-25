//! Signals: what is waiting on the operator, one thread at a time.
//!
//! Fullscreen, in two columns. The list on the left is every thread, fat enough
//! to read at a glance, and the one it has selected is the conversation on the
//! right: what each side said, and the keys that reply or resolve it. The left
//! column draws no background, so the terrain behind it shows through the gaps
//! between rows.
//!
//! A thread has one state: unresolved or resolved. Reading is not a separate
//! fact. Info logs resolve themselves when the screen reads them; every other
//! kind is resolved by the operator or the agent. Accepting a review is a
//! resolve that also ends the task, behind a confirmation.

use std::{collections::HashMap, time::Duration};

use chrono::Utc;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{
    task::{TaskOperatorSignalAuthor, TaskOperatorSignalKind, TaskOperatorSignalThread},
    tui::{
        App,
        widgets::{
            card,
            columns::Columns,
            header,
            list::{self, Row},
            palette, signal,
            surface::Surface,
            text::TextBox,
            utils::{Dot, ago, shimmer, since, truncate},
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalFilter {
    Unresolved,
    All,
}

/// Which way the threads inside a tier read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalSort {
    Oldest,
    Newest,
}

/// What the operator can do with the selected thread.
///
/// A reply is always a reply: the words are the meaning, and the agent reads
/// them. Resolve is the one explicit thread state. Accept is only a review's
/// resolve, because it also ends the task; it is never on Enter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignalAction {
    Reply,
    Resolve,
    Accept,
}

impl SignalAction {
    pub fn verb(self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Resolve => "resolve",
            Self::Accept => "accept",
        }
    }
}

/// The keys a thread of this kind offers.
///
/// Info has no manual resolve: reading it is resolving it. Answering is the
/// operator's move on a blocked thread; the agent resolves it once it has
/// resumed. A decision can be resolved directly, and a review is accepted with
/// its own deliberate key.
pub fn actions_for(kind: TaskOperatorSignalKind) -> &'static [SignalAction] {
    const REPLY: &[SignalAction] = &[SignalAction::Reply];
    const DISCUSS: &[SignalAction] = &[SignalAction::Reply, SignalAction::Resolve];
    const REVIEW: &[SignalAction] = &[SignalAction::Reply, SignalAction::Accept];
    match kind {
        TaskOperatorSignalKind::Update | TaskOperatorSignalKind::Blocked => REPLY,
        TaskOperatorSignalKind::Decision => DISCUSS,
        TaskOperatorSignalKind::InReview => REVIEW,
    }
}

pub fn draw(frame: &mut Frame, area: Rect, app: &mut App) {
    // The runner clears what was behind this screen and draws the field; the
    // surface below is what the screen puts on top of it.
    let [header_area, body] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let task = app.signal_task_title();
    let unresolved = app
        .signal_threads
        .iter()
        .filter(|thread| thread.is_unresolved())
        .count();
    let showing = app.signal_filter.chosen_value();
    let count = if showing == Some(&SignalFilter::All) {
        match app.signal_threads.len() {
            1 => "1 thread".to_owned(),
            count => format!("{count} threads"),
        }
    } else if unresolved == 1 {
        "1 unresolved".to_owned()
    } else {
        format!("{unresolved} unresolved")
    };
    header::draw(frame, header_area, "SIGNALS", Some(&task), Some(&count));
    // One frame, split by a single rule: the sections are two halves of the same
    // window rather than two windows that happen to touch.
    let inner = Surface::new().draw(frame, body);
    let columns = Columns::new(&[LIST_WIDTH]).draw(frame, inner);
    let list = columns[0];
    let detail = columns[1];
    let [toolbar, threads] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(list);
    // Opaque: the thread's half is a surface, so the field is cleared from it
    // after the frame and rule have taken their own cells.
    frame.render_widget(Clear, detail);
    draw_threads(frame, threads, app);
    draw_conversation(frame, detail, app);
    // The controls sit above the list they change; their open lists overlay the
    // rows rather than pushing them.
    draw_controls(frame, toolbar, threads, app);
}

/// The filter and sort pills at the top of the list.
fn draw_controls(frame: &mut Frame, toolbar: Rect, popup_area: Rect, app: &App) {
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
    app.signal_filter
        .draw_pill(frame, filter_at, app.signal_filter.is_open(), true);
    app.signal_sort
        .draw_pill(frame, sort_at, app.signal_sort.is_open(), true);
    if app.signal_filter.is_open() {
        app.signal_filter
            .draw_list_below(frame, popup_area, filter_at);
    }
    if app.signal_sort.is_open() {
        app.signal_sort.draw_list_below(frame, popup_area, sort_at);
    }
}

/// The list of threads, in the left half of the frame, with the field visible
/// through it.
fn draw_threads(frame: &mut Frame, area: Rect, app: &mut App) {
    let tick = app.animation_tick();
    let agent = app.signal_agent_label();
    let now = Utc::now().timestamp_millis();
    let group_of = |thread: &TaskOperatorSignalThread| {
        if thread.is_resolved() {
            "RESOLVED"
        } else {
            match thread.kind {
                TaskOperatorSignalKind::Blocked | TaskOperatorSignalKind::InReview => "NEEDS YOU",
                TaskOperatorSignalKind::Decision => "DECISIONS",
                TaskOperatorSignalKind::Update => "INFO LOG",
            }
        }
    };
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for thread in &app.signal_threads {
        *counts.entry(group_of(thread)).or_default() += 1;
    }
    let mut previous = None;
    let rows = app
        .signal_threads
        .iter()
        .map(|thread| {
            let group = group_of(thread);
            let starts = previous != Some(group);
            previous = Some(group);
            let view = signal::view(thread.kind);
            let unresolved = thread.is_unresolved();
            let style = if unresolved {
                view.tone
            } else {
                palette::muted()
            };
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
            // A thread that just changed moves before it settles: the symbol
            // travels and the name carries the accent for a moment.
            let arriving = app.animations()
                && unresolved
                && app
                    .signal_arrivals
                    .get(&thread.id)
                    .is_some_and(|noticed| noticed.elapsed() < Duration::from_millis(1600));
            let mark = if arriving {
                Dot::travelling(view.symbol, palette::accent())
            } else {
                Dot::still(if unresolved { view.symbol } else { '✓' }, style)
            };
            let mut row = Row::new(thread.title.clone())
                .detail(detail)
                .mark(mark.spans(tick))
                .gap(true);
            if starts {
                row = row.heading(group, counts[group].to_string());
            }
            // Inside the two homogeneous groups the heading already says what
            // the kind is; the two-kind group and the mixed resolved history
            // need the row to say which one it is.
            if !matches!(group, "DECISIONS" | "INFO LOG") {
                row = row.label(Span::styled(view.label, style));
            }
            if let Some(note) = note {
                row = row.note(note);
            }
            if arriving {
                row = row.tone(palette::accent());
            } else if !unresolved {
                row = row.tone(palette::muted());
            }
            row
        })
        .collect::<Vec<_>>();
    let visible = list::visible(area.height, &rows);
    app.signals_cursor
        .follow(rows.len(), visible, app.animations());
    let selected = (!rows.is_empty()).then(|| app.signals_cursor.selected());
    list::draw(
        frame,
        area,
        &rows,
        app.signals_cursor.offset(),
        selected,
        selected,
        LIST_PADDING,
    );
}

/// The selected thread: its title, conversation and keys, on the surface of the
/// right half.
fn draw_conversation(frame: &mut Frame, area: Rect, app: &mut App) {
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
                palette::heading()
            } else {
                palette::muted()
            },
        )),
        title,
    );
    let Some(thread) = app.signal_thread() else {
        draw_empty(frame, rest, app);
        return;
    };
    let agent = app.signal_agent_label();
    let now = Utc::now().timestamp_millis();
    let view = signal::view(thread.kind);
    let mut meta_line = vec![Span::styled(view.label.to_lowercase(), view.tone)];
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
    let tail_height = if composing { COMPOSER_HEIGHT + 1 } else { 2 };
    let [cards, tail] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(tail_height)]).areas(rest);
    let mut scroll = app.signal_scroll;
    draw_messages(frame, cards, thread, &agent, &mut scroll);
    if composing {
        let [box_area, hint] =
            Layout::vertical([Constraint::Length(COMPOSER_HEIGHT), Constraint::Length(1)])
                .areas(tail);
        draw_composer(frame, box_area, hint, app, &agent);
    } else {
        let [rule, keys] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(tail);
        // Full width, padding included, so it belongs to the pane rather than to
        // the text inside it.
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(usize::from(area.width)),
                palette::muted(),
            )),
            Rect {
                x: area.x,
                y: rule.y,
                width: area.width,
                height: 1,
            },
        );
        frame.render_widget(Paragraph::new(action_keys(thread, &agent)), keys);
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
) {
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
    for message in &thread.messages[first..] {
        let height = card::height(width, &message.body);
        if y + height > area.bottom() {
            break;
        }
        let from_operator = message.author == TaskOperatorSignalAuthor::Operator;
        card::draw(
            frame,
            Rect { y, height, ..area },
            &card::Message {
                author: if from_operator { "you" } else { agent },
                when: &ago(message.at_ms, now),
                body: &message.body,
                from_operator,
            },
        );
        y += height + 1;
    }
}

/// The reply being written, where the conversation is, rather than in a box over
/// it: the sentence belongs to the thread it will join.
fn draw_composer(frame: &mut Frame, box_area: Rect, hint: Rect, app: &App, agent: &str) {
    let Some(composer) = &app.signal_composer else {
        return;
    };
    composer
        .body
        .draw_titled(frame, box_area, true, Some(&format!("reply to {agent}")));
    frame.render_widget(
        Paragraph::new(Line::styled(
            "enter send · shift-enter new line · esc cancel",
            palette::muted(),
        )),
        hint,
    );
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

/// The keys the thread offers. Reply is always on Enter; accepting is its own
/// deliberate key because it also ends the task.
fn action_keys(thread: &TaskOperatorSignalThread, agent: &str) -> Line<'static> {
    if thread.is_resolved() {
        let status = match thread.kind {
            TaskOperatorSignalKind::Update => "✓ read".to_owned(),
            TaskOperatorSignalKind::InReview => {
                format!("✓ accepted by {}", who(thread.resolved_by, agent))
            }
            _ => format!("✓ resolved by {}", who(thread.resolved_by, agent)),
        };
        let mut spans = vec![Span::styled(status, palette::muted())];
        let replyable = matches!(
            thread.kind,
            TaskOperatorSignalKind::Decision | TaskOperatorSignalKind::Blocked
        );
        if replyable {
            spans.push(Span::raw("   "));
            spans.push(Span::styled(" enter ", palette::focus()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("reply", palette::primary()));
        }
        if thread.kind == TaskOperatorSignalKind::Decision {
            spans.push(Span::raw("   "));
            spans.push(Span::styled(" x ", palette::accent()));
            spans.push(Span::raw(" "));
            spans.push(Span::styled("reopen", palette::muted()));
        }
        return Line::from(spans);
    }
    let mut spans = Vec::new();
    for action in actions_for(thread.kind) {
        if !spans.is_empty() {
            spans.push(Span::raw("   "));
        }
        let key = match action {
            SignalAction::Reply => "enter",
            SignalAction::Resolve => "x",
            SignalAction::Accept => "c",
        };
        let is_reply = *action == SignalAction::Reply;
        spans.push(Span::styled(
            format!(" {key} "),
            if is_reply {
                palette::focus()
            } else {
                palette::accent()
            },
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            action.verb(),
            if is_reply {
                palette::primary()
            } else {
                palette::muted()
            },
        ));
    }
    Line::from(spans)
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
