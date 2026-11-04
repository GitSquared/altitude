use super::*;
use std::{collections::HashMap, time::Duration};

use chrono::Utc;
use ratatui::{Frame, layout::Rect};

use crate::{
    task::TaskOperatorSignalAuthor,
    tui::{
        App,
        widgets::{format::since, list::Row, palette, signal},
    },
};

pub(super) fn draw_controls(frame: &mut Frame, toolbar: Rect, popup_area: Rect, app: &mut App) {
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

pub(super) fn draw_threads(frame: &mut Frame, area: Rect, app: &mut App) {
    app.signal_targets.add(area, SignalHit::List);
    let agent = app.signal_agent_label();
    let now = Utc::now().timestamp_millis();
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for thread in &app.signal_threads {
        *counts.entry(group_of(thread).title).or_default() += 1;
    }
    let mut previous: Option<&'static str> = None;
    let rows = app
        .signal_threads
        .iter()
        .map(|thread| {
            let group = group_of(thread);
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
            let mut row = Row::new(thread.title.clone()).detail(detail);
            if starts {
                row = row.heading(
                    group.title,
                    counts[group.title].to_string(),
                    group.symbol,
                    group.tone.bold(),
                );
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
