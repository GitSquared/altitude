use super::frame::draw_pane_title;
use super::*;

use chrono::Utc;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect, Size},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    project::Project,
    task::{AskTransition, ProjectProposal, TaskAsk, TaskAskAuthor},
    tui::{
        App, Pictures,
        widgets::{
            button, card, empty,
            format::{ago, clock},
            markdown, palette,
            shimmer::shimmer,
            signal,
            surface::{self},
        },
    },
};

pub(super) fn draw_conversation(frame: &mut Frame, area: Rect, app: &mut App) {
    app.signal_targets.add(area, SignalHit::Detail);
    // Opaque by construction: the field is never drawn on this side.
    let padded = Rect {
        x: area.x.saturating_add(1),
        width: area.width.saturating_sub(2),
        ..area
    };
    // The pane is named like the plan and the activity beside it, and the line under
    // that says which thread is being read: one blank row between the two and the first
    // message, so the conversation starts after the frame rather than against it.
    let [title, meta, _, rest] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
    ])
    .areas(padded);
    draw_pane_title(
        frame,
        title,
        "SIGNAL THREAD",
        match app.signal_thread() {
            Some(thread) => format!("  {}", thread.title),
            None => String::new(),
        },
        app.signal_focus == SignalFocus::Detail,
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
    // What the account running this task has spent, and when one of its windows is
    // holding the work, when that window lets go: one row, and it says nothing at all
    // about a plain task.
    if let Some(account) = app.signal_task_standing() {
        let windows: Vec<String> = account
            .windows
            .iter()
            .map(|reading| match reading.used_percent {
                Some(used) => format!("{} {used:.0}%", reading.window.word()),
                None => reading.window.word().to_owned(),
            })
            .collect();
        meta_line.push(Span::styled(
            format!(
                " · {} · {}",
                windows.join(" · "),
                ago(account.observed_at_ms, Utc::now().timestamp_millis())
            ),
            palette::muted(),
        ));
    }
    if let Some((window, clears_at_ms)) = app.signal_task_limit() {
        meta_line.push(Span::styled(
            format!(
                " · {} limit · resumes {}",
                window.word(),
                clock(clears_at_ms)
            ),
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
    let working = app.signal_agent_working();
    let project = app.signal_task_project();
    let conversation = draw_messages(
        frame,
        cards,
        Conversation {
            thread: &thread,
            project: project.as_ref(),
            agent: &agent,
            selected: selected_message,
            flashing: app
                .signal_message_flash
                .filter(|_| app.message_flash_is_lit()),
            working,
            tick: app.animation_tick(),
        },
        &mut scroll,
        &mut app.pictures,
    );
    app.signal_message_rows = conversation.rows;
    app.signal_scroll_max = conversation.scroll_max;
    app.signal_viewport_rows = cards.height;
    for (rect, index) in conversation.hitboxes {
        app.signal_targets.add(rect, SignalHit::Message(index));
    }
    if composing {
        let [box_area, actions_area] =
            Layout::vertical([Constraint::Length(COMPOSER_HEIGHT), Constraint::Length(2)])
                .areas(tail);
        draw_composer(frame, box_area, app, &agent);
        // Secondary first, primary at the right edge: the answer the surface
        // exists for is the one under the operator's hand.
        let buttons = draw_buttons(
            frame,
            area,
            actions_area,
            &[
                button::Action::secondary("cancel"),
                button::Action::primary("send").focused(true),
            ],
            None,
        );
        if let Some(rect) = buttons.first() {
            app.signal_targets.add(*rect, SignalHit::ComposerCancel);
        }
        if let Some(rect) = buttons.get(1) {
            app.signal_targets.add(*rect, SignalHit::ComposerSend);
        }
    } else {
        let actions = actions(&thread);
        let primary = primary_action(&thread);
        let buttons: Vec<_> = actions
            .iter()
            .enumerate()
            .map(|(index, action)| {
                let label = action.label(thread.is_resolved());
                let focused = app.signal_focus == SignalFocus::Action(index);
                if Some(*action) == primary {
                    button::Action::primary(label).focused(focused)
                } else {
                    button::Action::secondary(label).focused(focused)
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
            app.signal_focus_ring.add(SignalFocus::Action(index));
        }
    }
    app.signal_scroll = scroll;
}

pub(super) struct Conversation<'a> {
    thread: &'a TaskAsk,
    project: Option<&'a Project>,
    agent: &'a str,
    selected: Option<usize>,
    flashing: Option<usize>,
    working: bool,
    tick: Option<usize>,
}

pub(super) struct ConversationLayout {
    hitboxes: Vec<(Rect, usize)>,
    rows: Vec<(u16, u16)>,
    scroll_max: u16,
}

fn proposal_lines(
    proposal: &ProjectProposal,
    project: Option<&Project>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(command) = &proposal.setup_command {
        lines.push(Line::styled("setup command", palette::heading()));
        field_diff(
            &mut lines,
            project.map(|project| project.setup_command.as_str()),
            command,
            width,
        );
    }
    if let Some(caches) = &proposal.caches {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::styled("shared caches", palette::heading()));
        let before = project.map(|project| project.caches.join("\n"));
        field_diff(&mut lines, before.as_deref(), &caches.join("\n"), width);
    }
    lines
}

fn field_diff(out: &mut Vec<Line<'static>>, before: Option<&str>, after: &str, width: usize) {
    let old: Vec<&str> = before
        .unwrap_or_default()
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    let new: Vec<&str> = after
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect();
    let mut changed = false;
    for line in &old {
        if !new.contains(line) {
            changed = true;
            out.extend(diff_line('-', line, width, palette::muted()));
        }
    }
    for line in &new {
        if !old.contains(line) {
            changed = true;
            out.extend(diff_line('+', line, width, palette::accent()));
        }
    }
    if before.is_some() && !changed {
        out.push(Line::styled("unchanged", palette::muted()));
    }
}

fn diff_line(marker: char, text: &str, width: usize, tone: Style) -> Vec<Line<'static>> {
    let indent = "  ";
    let lead = format!("{indent}{marker} ");
    let room = width.saturating_sub(lead.chars().count()).max(1);
    let mut lines = Vec::new();
    let mut rest = text.trim();
    loop {
        let take = rest
            .char_indices()
            .nth(room)
            .map_or(rest.len(), |(index, _)| index);
        let (chunk, tail) = rest.split_at(take);
        let prefix = if lines.is_empty() {
            lead.clone()
        } else {
            " ".repeat(lead.chars().count())
        };
        lines.push(Line::styled(format!("{prefix}{chunk}"), tone));
        if tail.is_empty() {
            break;
        }
        rest = tail;
    }
    lines
}

fn draw_messages(
    frame: &mut Frame,
    area: Rect,
    conversation: Conversation<'_>,
    scroll: &mut u16,
    pictures: &mut Pictures,
) -> ConversationLayout {
    let Conversation {
        thread,
        project,
        agent,
        selected,
        flashing,
        working,
        tick,
    } = conversation;
    let width = area.width;
    let now = Utc::now().timestamp_millis();
    let last_operator = thread
        .messages
        .iter()
        .rposition(|message| message.author == TaskAskAuthor::Operator);
    let last_agent = thread
        .messages
        .iter()
        .rposition(|message| message.author == TaskAskAuthor::Agent);
    let proposed = thread.proposal.as_ref().map(|proposal| card::Nested {
        title: "proposed change",
        lines: proposal_lines(proposal, project, usize::from(width.saturating_sub(6))),
    });
    // A diagram this terminal can draw leaves the text and becomes a picture
    // inside the card that carried it. A message whose diagrams will not all draw
    // keeps its fences, so the source is never lost to a render that failed.
    let text_width = width.saturating_sub(4).max(1);
    let stripped: Vec<(String, Vec<String>)> = thread
        .messages
        .iter()
        .map(|message| {
            if pictures.has_pictures() {
                markdown::take_pictures(&message.body)
            } else {
                (message.body.clone(), Vec::new())
            }
        })
        .collect();
    // One scale for the whole conversation. A diagram at its own scale reads at the
    // size of the words around it, so the only diagram that sets the scale is the
    // widest one: two pictures in a thread never disagree about the size of text.
    let scale = stripped
        .iter()
        .flat_map(|(_, sources)| sources.iter())
        .map(|source| pictures.allowance(source, text_width))
        .fold(1.0_f32, f32::min);
    let bodies: Vec<Body> = stripped
        .into_iter()
        .zip(&thread.messages)
        .map(|((text, sources), message)| {
            Body::of(text, sources, &message.body, pictures, text_width, scale)
        })
        .collect();
    // A close or a reopen is part of the conversation, not a message: it sits
    // between the cards at the point it happened, as a quiet line.
    let entries = conversation_entries(thread);
    let mut rows = vec![(0u16, 0u16); thread.messages.len()];
    let mut layout: Vec<(Entry, u16, u16)> = Vec::with_capacity(entries.len());
    let mut cursor = 0u32;
    for entry in entries {
        let height = match entry {
            Entry::Message(index) => {
                let nested = proposed.as_ref().filter(|_| Some(index) == last_agent);
                let height = card::height(width, &bodies[index].text, nested, &bodies[index].sizes);
                rows[index] = (u16::try_from(cursor).unwrap_or(u16::MAX), height);
                height
            }
            Entry::Transition(_) => 1,
        };
        layout.push((entry, u16::try_from(cursor).unwrap_or(u16::MAX), height));
        // A blank row after every entry, which is what makes the line following a
        // card sit a line below it.
        cursor = cursor.saturating_add(u32::from(height)).saturating_add(1);
    }
    let content = if working {
        cursor.saturating_add(1)
    } else {
        cursor.saturating_sub(1)
    };
    let scroll_max =
        u16::try_from(content.saturating_sub(u32::from(area.height))).unwrap_or(u16::MAX);
    let offset = if *scroll == u16::MAX {
        scroll_max
    } else {
        (*scroll).min(scroll_max)
    };
    *scroll = offset;

    let viewport_top = i32::from(area.y);
    let viewport_bottom = i32::from(area.bottom());
    let mut hitboxes = Vec::new();
    for (entry, top, height) in layout {
        let entry_top = i32::from(area.y) + i32::from(top) - i32::from(offset);
        let entry_bottom = entry_top + i32::from(height);
        let visible_top = entry_top.max(viewport_top);
        let visible_bottom = entry_bottom.min(viewport_bottom);
        if visible_bottom <= visible_top {
            continue;
        }
        let visible = Rect {
            y: u16::try_from(visible_top).unwrap_or(0),
            height: u16::try_from(visible_bottom - visible_top).unwrap_or(0),
            ..area
        };
        let index = match entry {
            Entry::Transition(transition) => {
                frame.render_widget(
                    Paragraph::new(transition_line(transition, agent, now)),
                    visible,
                );
                continue;
            }
            Entry::Message(index) => index,
        };
        let message = &thread.messages[index];
        let from_operator = message.author == TaskAskAuthor::Operator;
        let tick = from_operator.then(|| {
            if last_agent.is_some_and(|agent| agent > index) {
                card::Tick::Read
            } else if last_operator == Some(index) && working {
                card::Tick::Delivered
            } else {
                card::Tick::Sent
            }
        });
        let body = &bodies[index];
        let slots = card::draw_clipped(
            frame,
            visible,
            height,
            u16::try_from(visible_top - entry_top).unwrap_or(0),
            &card::Message {
                author: if from_operator { "you" } else { agent },
                when: &ago(message.at_ms, now),
                body: &body.text,
                from_operator,
                focused: selected == Some(index),
                flashing: flashing == Some(index),
                tick,
                nested: proposed
                    .as_ref()
                    .filter(|_| Some(index) == last_agent)
                    .map(|nested| card::Nested {
                        title: nested.title,
                        lines: nested.lines.clone(),
                    }),
                pictures: &body.sizes,
            },
        );
        for slot in slots {
            pictures.draw(
                frame,
                area,
                &body.sources[slot.index],
                slot.width,
                body.scale,
                (
                    i16::try_from(i32::from(slot.x) - i32::from(area.x)).unwrap_or(0),
                    i16::try_from(slot.top - viewport_top).unwrap_or(0),
                ),
            );
        }
        hitboxes.push((visible, index));
    }

    if working {
        let line_top =
            i32::from(area.y) + i32::try_from(cursor).unwrap_or(i32::MAX) - i32::from(offset);
        if line_top >= viewport_top && line_top < viewport_bottom {
            let line = Rect {
                x: area.x.saturating_add(1),
                y: u16::try_from(line_top).unwrap_or(0),
                width: area.width.saturating_sub(2),
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(Line::from(shimmer(&format!("{agent} is working…"), tick))),
                line,
            );
        }
    }
    ConversationLayout {
        hitboxes,
        rows,
        scroll_max,
    }
}

#[derive(Clone, Copy)]
pub(super) enum Entry {
    Message(usize),
    Transition(AskTransition),
}

pub(super) struct Body {
    text: String,
    sources: Vec<String>,
    sizes: Vec<Size>,
    scale: f32,
}

impl Body {
    fn of(
        text: String,
        sources: Vec<String>,
        body: &str,
        pictures: &mut Pictures,
        width: u16,
        scale: f32,
    ) -> Self {
        if sources.is_empty() || !pictures.has_pictures() {
            return Self::fenced(body);
        }
        let sizes: Option<Vec<Size>> = sources
            .iter()
            .map(|source| pictures.size(source, width, scale))
            .collect();
        match sizes {
            Some(sizes) => Self {
                text,
                sources,
                sizes,
                scale,
            },
            None => Self::fenced(body),
        }
    }

    fn fenced(body: &str) -> Self {
        Self {
            text: body.to_owned(),
            sources: Vec::new(),
            sizes: Vec::new(),
            scale: 1.0,
        }
    }
}

fn conversation_entries(thread: &TaskAsk) -> Vec<Entry> {
    let mut entries: Vec<(i64, Entry)> = thread
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| (message.at_ms, Entry::Message(index)))
        .chain(
            thread
                .transitions
                .iter()
                .map(|transition| (transition.at_ms, Entry::Transition(*transition))),
        )
        .collect();
    entries.sort_by_key(|(at_ms, _)| *at_ms);
    entries.into_iter().map(|(_, entry)| entry).collect()
}

fn transition_line(transition: AskTransition, agent: &str, now: i64) -> Line<'static> {
    let who = who(Some(transition.by), agent);
    let what = if transition.resolved {
        format!("✓ resolved by {who}")
    } else {
        format!("↺ reopened by {who}")
    };
    Line::styled(
        format!(" {what} · {}", ago(transition.at_ms, now)),
        palette::muted(),
    )
}

fn draw_composer(frame: &mut Frame, box_area: Rect, app: &App, agent: &str) {
    let Some(composer) = &app.signal_composer else {
        return;
    };
    composer
        .body
        .draw_titled(frame, box_area, true, Some(&format!("reply to {agent}")));
}

fn draw_buttons(
    frame: &mut Frame,
    pane: Rect,
    area: Rect,
    actions: &[button::Action<'_>],
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
    let rects = button::actions(frame, frame_rect, area, actions);
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
        let asking = app
            .remote_signals
            .as_ref()
            .map(|remote| format!("asking {}…", remote.address))
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(Line::from(shimmer(&asking, app.animation_tick()))).centered(),
            empty::row(area),
        );
        return;
    }
    empty::draw(frame, area, "Agent hasn't raised any signals yet");
}
