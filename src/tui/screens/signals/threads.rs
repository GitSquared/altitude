use super::*;
use std::{collections::HashMap, time::Duration};

use chrono::Utc;
use ratatui::{Frame, layout::Rect};

use crate::{
    task::TaskAskAuthor,
    tui::{
        App,
        widgets::{format::since, list::Row, palette, signal},
    },
};

// The task's history: every ask it ever made, newest first, the open one at the top.
// A day heading is enough to place a row in time, and the mark says what it was — a
// question, a delivery, or one that has been settled.
pub(super) fn draw_threads(frame: &mut Frame, area: Rect, app: &mut App) {
    app.signal_targets.add(area, SignalHit::List);
    let agent = app.signal_agent_label();
    let now = Utc::now().timestamp_millis();
    let tone = |thread: &TaskAsk| {
        if thread.is_resolved() {
            palette::muted()
        } else {
            signal::view(thread.kind).tone
        }
    };
    // Rows are grouped by the day they were asked on, so the list reads as a
    // timeline rather than as a pile of one-off decisions.
    let mut counts: HashMap<String, String> = HashMap::new();
    for thread in &app.signal_threads {
        let day = day_of(thread.opened_at_ms, now);
        let count = counts.entry(day).or_insert_with(|| "0".to_owned());
        *count = count
            .parse::<usize>()
            .unwrap_or_default()
            .saturating_add(1)
            .to_string();
    }
    let mut previous: Option<String> = None;
    let rows = app
        .signal_threads
        .iter()
        .map(|thread| {
            let day = day_of(thread.opened_at_ms, now);
            let starts = previous.as_ref() != Some(&day);
            previous = Some(day.clone());
            let mark = if thread.is_resolved() {
                '✓'
            } else {
                signal::view(thread.kind).symbol
            };
            let detail = thread
                .messages
                .last()
                .map(|message| {
                    let who = match message.author {
                        TaskAskAuthor::Operator => "you",
                        TaskAskAuthor::Agent => agent.as_str(),
                    };
                    format!("{who}: {}", one_line(&message.body))
                })
                .unwrap_or_else(|| signal::view(thread.kind).label.to_owned());
            let arriving = app.animations()
                && thread.is_unresolved()
                && app
                    .signal_arrivals
                    .get(&thread.id)
                    .is_some_and(|noticed| noticed.elapsed() < Duration::from_millis(1600));
            let mut row = Row::new(thread.title.clone()).detail(detail);
            if starts {
                row = row.heading(day.clone(), counts[&day].clone(), mark, palette::muted());
            }
            row = row.tone(if arriving {
                palette::accent()
            } else {
                tone(thread)
            });
            row = row.note(since(thread.opened_at_ms, now));
            row
        })
        .collect::<Vec<_>>();
    let hitboxes = app.signals_list.draw(
        frame,
        area,
        &rows,
        app.animation_tick(),
        app.signal_focus == SignalFocus::List,
        LIST_PADDING,
    );
    for (rect, index) in hitboxes {
        app.signal_targets.add(rect, SignalHit::Thread(index.row));
    }
}

// Which day a row belongs under, said the way the operator would say it: today, then
// yesterday, then the date itself.
fn day_of(at_ms: i64, now_ms: i64) -> String {
    let at = chrono::DateTime::from_timestamp_millis(at_ms).unwrap_or_default();
    let now = chrono::DateTime::from_timestamp_millis(now_ms).unwrap_or_default();
    let days = (now.date_naive() - at.date_naive()).num_days();
    match days {
        0 => "TODAY".to_owned(),
        1 => "YESTERDAY".to_owned(),
        _ => at.format("%-d %b").to_string().to_uppercase(),
    }
}
