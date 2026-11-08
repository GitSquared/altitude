use super::plan::draw_plan;
use super::*;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::tui::{
    App,
    widgets::{
        palette,
        surface::{self},
        tabs::{self, Tab},
    },
};

pub(super) fn draw_plan_tabs(frame: &mut Frame, area: Rect, app: &App) {
    if area.height == 0 {
        return;
    }
    // Only the labels are the tabs' to clear: the rule under them is the pane's,
    // and the separators between tabs are drawn onto it.
    surface::cover(frame, Rect { height: 1, ..area });
    let tabs = [Tab::new("Threads"), Tab::new("Plan")];
    let selected = usize::from(app.signal_plan_open);
    tabs::draw(frame, area, &tabs, selected);
}

pub(super) fn split_plan_and_activity(frame: &mut Frame, area: Rect, app: &mut App) {
    // The column covers what is behind it before anything is written into it: the
    // panes inside it are text, and text does not clear the terrain it lands on.
    surface::cover(frame, area);
    // Two panes of the same window, so they share it: the plan is a list that
    // scrolls and the stream is a list that scrolls, and neither owes the other its
    // room. Below this there is not enough height for two titles and a line each,
    // which is the one case where the plan has the column to itself.
    if area.height < PANE_TITLE_ROWS * 2 + 4 {
        app.signal_activity_scroll_max = 0;
        app.signal_activity_scroll = u16::MAX;
        draw_plan(frame, area, app);
        return;
    }
    let usable = area.height.saturating_sub(1);
    let plan_height = usable / 2;
    let [plan, divider, activity] = Layout::vertical([
        Constraint::Length(plan_height),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    surface::horizontal_divider(frame, area, divider.y);
    draw_plan(frame, plan, app);
    surface::cover(frame, activity);
    draw_activity(frame, activity, app);
}

pub(super) fn draw_pane_title(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    detail: String,
    focused: bool,
) {
    frame.render_widget(
        Paragraph::new(Line::from_iter([
            Span::styled(
                title.to_owned(),
                if focused {
                    palette::accent()
                } else {
                    palette::heading()
                },
            ),
            Span::styled(detail, palette::muted()),
        ])),
        Rect { height: 1, ..area },
    );
}

// A border would have to fight the dividers the panes already share, so the focus
// is a rail in the padding column each pane has to itself: it runs the height of
// the pane, it cannot collide with a corner, and it is where the eye already is
// when it is scrolling.
pub(super) fn draw_focus_rail(frame: &mut Frame, pane: Rect, body: Rect, focused: bool) {
    if !focused {
        return;
    }
    for y in body.y..body.bottom() {
        frame.render_widget(
            Paragraph::new(Line::styled("▌", palette::accent())),
            Rect {
                x: pane.x,
                y,
                width: 1,
                height: 1,
            },
        );
    }
}

pub(super) fn pane_body(area: Rect) -> (Rect, Rect, Rect) {
    let text = Rect {
        x: area.x.saturating_add(PLAN_PADDING),
        width: area.width.saturating_sub(PLAN_PADDING * 2),
        ..area
    };
    let [title, spacer, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(text);
    (title, spacer, body)
}
