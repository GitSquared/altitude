use super::*;
use std::time::Duration;

use chrono::Utc;
use ratatui::{Frame, layout::Rect, text::Span};

use crate::{
    task::TaskAskAuthor,
    tui::{
        App,
        widgets::{format::since, list::Row, palette, signal},
    },
};

// The task's history: every ask it ever made, the open one at the top and the rest
// newest first. A row says what it was with one mark and what it said with its first
// line; a task rarely asks enough questions to be worth a heading, and the day an ask
// was made is what the age beside it already says.
pub(super) fn draw_threads(frame: &mut Frame, area: Rect, app: &mut App) {
    app.signal_targets.add(area, SignalHit::List);
    let agent = app.signal_agent_label();
    let now = Utc::now().timestamp_millis();
    let rows = app
        .signal_threads
        .iter()
        .map(|thread| {
            let resolved = thread.is_resolved();
            let view = signal::view(thread.kind);
            let tone = if resolved {
                palette::muted()
            } else {
                view.tone
            };
            let mark = if resolved { '\u{2713}' } else { view.symbol };
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
                .unwrap_or_else(|| view.label.to_owned());
            let arriving = app.animations()
                && !resolved
                && app
                    .signal_arrivals
                    .get(&thread.id)
                    .is_some_and(|noticed| noticed.elapsed() < Duration::from_millis(1600));
            Row::new(thread.title.clone())
                .detail(detail)
                .mark(vec![Span::styled(format!("{mark} "), tone)])
                .tone(if arriving { palette::accent() } else { tone })
                .age(since(thread.opened_at_ms, now))
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
