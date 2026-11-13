use super::frame::{draw_focus_rail, draw_pane_title, pane_body};
use super::*;
use crate::brand;
use std::mem;

use ratatui::{Frame, layout::Rect, style::Style, text::Line, widgets::Paragraph};

use crate::{
    agent::ActivityKind,
    task::TaskPlanStatus,
    tui::{
        App,
        widgets::{empty, palette},
    },
};

pub(super) fn draw_plan(frame: &mut Frame, area: Rect, app: &mut App) {
    let (title, _, body) = pane_body(area);
    let plan = app.signal_plan();
    let done = plan
        .iter()
        .filter(|step| step.status == TaskPlanStatus::Completed)
        .count();
    app.signal_focus_ring.add(SignalFocus::Plan);
    let focused = app.signal_focus == SignalFocus::Plan;
    draw_pane_title(
        frame,
        title,
        "PLAN",
        if plan.is_empty() {
            String::new()
        } else {
            format!("  {done}/{}", plan.len())
        },
    );
    if plan.is_empty() {
        // The pane kept a row of padding under this one, so the line goes on the
        // body's first row rather than being centred across both.
        empty::draw(frame, Rect { height: 1, ..body }, "No plan yet");
        app.signal_plan_scroll_max = 0;
        app.signal_plan_scroll = u16::MAX;
        return;
    }
    // The icon gets its own column so a wrapped step stays aligned under itself
    // rather than under the mark. Completed and canceled steps are the past, so
    // they are the quiet ones; what is still ahead stays at full strength.
    let icon_width = 2u16;
    let text_width = body.width.saturating_sub(icon_width).max(1);
    let mut rows: Vec<(String, String, Style)> = Vec::new();
    for step in &plan {
        let (symbol, tone) = match step.status {
            TaskPlanStatus::Pending => ('○', palette::primary()),
            TaskPlanStatus::InProgress => ('◐', palette::heading()),
            TaskPlanStatus::Completed => ('✓', palette::muted()),
            TaskPlanStatus::Canceled => ('✕', palette::muted()),
        };
        for (index, line) in wrap_lines(&step.content, text_width)
            .into_iter()
            .enumerate()
        {
            rows.push((
                if index == 0 {
                    format!("{symbol} ")
                } else {
                    "  ".to_owned()
                },
                line,
                tone,
            ));
        }
    }
    app.signal_plan_rows = body.height.max(1);
    app.signal_plan_scroll_max =
        u16::try_from(rows.len().saturating_sub(usize::from(body.height))).unwrap_or(0);
    let scroll = if app.signal_plan_scroll == u16::MAX {
        app.signal_plan_scroll_max
    } else {
        app.signal_plan_scroll.min(app.signal_plan_scroll_max)
    };
    app.signal_targets.add(body, SignalHit::Plan);
    draw_focus_rail(frame, area, body, focused);
    for (offset, (mark, line, tone)) in rows
        .iter()
        .skip(usize::from(scroll))
        .take(usize::from(body.height))
        .enumerate()
    {
        let y = body.y.saturating_add(offset as u16);
        frame.render_widget(
            Paragraph::new(Line::styled(mark.clone(), *tone)),
            Rect {
                y,
                height: 1,
                ..body
            },
        );
        frame.render_widget(
            Paragraph::new(Line::styled(line.clone(), *tone)),
            Rect {
                x: body.x.saturating_add(icon_width),
                y,
                width: text_width,
                height: 1,
            },
        );
    }
}

pub(super) fn draw_activity(frame: &mut Frame, area: Rect, app: &mut App) {
    let lines = app.signal_activity().cloned().unwrap_or_default();
    let (title, _, body) = pane_body(area);
    app.signal_focus_ring.add(SignalFocus::Activity);
    let focused = app.signal_focus == SignalFocus::Activity;
    draw_pane_title(frame, title, "ACTIVITY", String::new());
    app.signal_activity_rows = body.height.max(1);
    app.signal_targets.add(body, SignalHit::Activity);
    draw_focus_rail(frame, area, body, focused);
    if lines.is_empty() {
        empty::draw(frame, body, "Nothing yet");
        app.signal_activity_scroll_max = 0;
        app.signal_activity_scroll = u16::MAX;
        return;
    }
    // A line the agent reported is one reading, however many rows it needs: it gets a
    // mark where it starts so a wrapped row is not mistaken for a new one, and the
    // newest one is at full strength across all of its rows rather than its tail.
    const MARK_WIDTH: u16 = 2;
    let text_width = body.width.saturating_sub(MARK_WIDTH).max(1);
    let mut rows: Vec<(String, bool, Style, char)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let newest = index + 1 == lines.len();
        // The gutter says who is speaking at a glance: the machine, the agent, or the
        // agent thinking. A wrapped row leaves it empty so one line reads as one line.
        let mark = match line.kind {
            ActivityKind::Altitude => brand::MOUNTAIN_CHAR,
            ActivityKind::Thinking => '·',
            ActivityKind::Speech | ActivityKind::Remote => '›',
        };
        for (part, wrapped) in wrap_lines(&line.text, text_width).into_iter().enumerate() {
            rows.push((wrapped, part == 0, activity_tone(line.kind, newest), mark));
        }
    }
    app.signal_activity_scroll_max =
        u16::try_from(rows.len().saturating_sub(usize::from(body.height))).unwrap_or(0);
    let scroll = if app.signal_activity_scroll == u16::MAX {
        app.signal_activity_scroll_max
    } else {
        app.signal_activity_scroll
            .min(app.signal_activity_scroll_max)
    };
    let first = usize::from(scroll);
    for (offset, (row, starts, style, mark)) in rows
        .iter()
        .skip(first)
        .take(usize::from(body.height))
        .enumerate()
    {
        let y = body.y.saturating_add(offset as u16);
        frame.render_widget(
            Paragraph::new(Line::styled(
                if *starts {
                    format!("{mark} ")
                } else {
                    "  ".to_owned()
                },
                *style,
            )),
            Rect {
                x: body.x,
                y,
                width: MARK_WIDTH,
                height: 1,
            },
        );
        frame.render_widget(
            Paragraph::new(row.clone()).style(*style),
            Rect {
                x: body.x.saturating_add(MARK_WIDTH),
                y,
                width: text_width,
                height: 1,
            },
        );
    }
}

// The newest line is the one being read, and everything above it is there to be looked
// back through. What the agent says is the thing itself; a thought it is having is the
// draft of it, so a thought sits a step back from where its age would put it.
fn activity_tone(kind: ActivityKind, newest: bool) -> Style {
    match (kind, newest) {
        (ActivityKind::Thinking, true) => palette::muted(),
        (ActivityKind::Thinking, false) => palette::backdrop(),
        (ActivityKind::Altitude, true) => palette::attention(),
        (ActivityKind::Altitude, false) => palette::muted(),
        (ActivityKind::Speech | ActivityKind::Remote, true) => palette::primary(),
        (ActivityKind::Speech | ActivityKind::Remote, false) => palette::muted(),
    }
}

pub(super) fn wrap_lines(text: &str, width: u16) -> Vec<String> {
    let width = usize::from(width.max(1));
    let mut lines = Vec::new();
    for line in text.lines() {
        let mut current = String::new();
        for word in line.split_whitespace() {
            if !current.is_empty() && current.chars().count() + 1 + word.chars().count() > width {
                lines.push(mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}
