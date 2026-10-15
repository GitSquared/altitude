//! The frame every screen shares: who this is, and what the keys do.
//!
//! A screen draws inside the area this hands back and says which keys would do
//! something there. The header and the guidance belong to the frame rather than
//! to any screen, so they are the same everywhere: a popup cannot invent its own
//! keys, and the keys it does have appear in the one place an operator already
//! looks.

use std::mem;

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use crate::tui::widgets::palette;

/// The name every screen wears.
const TITLE: &str = "⛰  altitude";

/// A key, and what it does here.
#[derive(Clone)]
pub struct Hint {
    pub keys: &'static str,
    pub what: &'static str,
}

impl Hint {
    pub fn new(keys: &'static str, what: &'static str) -> Self {
        Self { keys, what }
    }
}

/// Draws the header and the guidance, and answers with the area a screen may
/// draw in.
///
/// The header is the first row and the guidance the last, with nothing above
/// either: the screen is the whole surface between them.
pub fn draw(
    frame: &mut Frame,
    hints: &[Hint],
    guidance_enabled: bool,
    notice: Option<&str>,
) -> Rect {
    let area = frame.area();
    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    let guidance = if guidance_enabled {
        guidance_lines(hints, usize::from(inner.width))
    } else {
        Vec::new()
    };
    let notice_rows = notice
        .map(|text| wrapped_rows(text, usize::from(inner.width)))
        .unwrap_or(0);
    let [header, content, notice_area, guidance_area] =
        frame_areas(inner, notice_rows, guidance.len() as u16);

    let [title, version] =
        Layout::horizontal([Constraint::Min(0), Constraint::Min(0)]).areas(header);
    frame.render_widget(Line::styled(TITLE, palette::heading()), title);
    frame.render_widget(
        Line::styled(concat!("v", env!("CARGO_PKG_VERSION")), palette::muted()).right_aligned(),
        version,
    );
    if let Some(notice) = notice {
        frame.render_widget(
            Paragraph::new(notice)
                .style(palette::bad())
                .wrap(Wrap { trim: false }),
            notice_area,
        );
    }
    if !guidance.is_empty() {
        frame.render_widget(
            Paragraph::new(guidance).style(palette::muted()),
            guidance_area,
        );
    }
    content
}

pub fn content_area(
    area: Rect,
    hints: &[Hint],
    guidance_enabled: bool,
    notice: Option<&str>,
) -> Rect {
    let inner = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    };
    let guidance_rows = if guidance_enabled {
        guidance_lines(hints, usize::from(inner.width)).len() as u16
    } else {
        0
    };
    let notice_rows = notice.map_or(0, |text| wrapped_rows(text, usize::from(inner.width)));
    frame_areas(inner, notice_rows, guidance_rows)[1]
}

fn frame_areas(inner: Rect, notice_rows: u16, guidance_rows: u16) -> [Rect; 4] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(notice_rows),
        Constraint::Length(guidance_rows),
    ])
    .areas(inner)
}

/// The guidance, wrapped to the width.
fn guidance_lines(hints: &[Hint], width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for hint in hints {
        let text = format!("{} {}", hint.keys, hint.what);
        if !line.is_empty() {
            if Line::from(format!("{line} · {text}")).width() > width {
                lines.push(Line::from(mem::take(&mut line)));
            } else {
                line.push_str(" · ");
            }
        }
        line.push_str(&text);
    }
    if !line.is_empty() {
        lines.push(Line::from(line));
    }
    lines
}

/// How many rows `text` needs at `width`.
fn wrapped_rows(text: &str, width: usize) -> u16 {
    let width = width.max(1);
    text.lines()
        .map(|line| line.chars().count().div_ceil(width).max(1))
        .sum::<usize>()
        .min(6) as u16
}

/// Dims what has already been drawn, so a popup reads as being on top of it
/// rather than beside it.
pub fn dim(frame: &mut Frame, area: Rect) {
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buffer[(x, y)].modifier |= Modifier::DIM;
        }
    }
}

/// The help popup: the keys for wherever you are.
///
/// Shown whether or not the guidance is, because a key you cannot find is a key
/// you do not have.
pub fn draw_help(frame: &mut Frame, area: Rect, hints: &[Hint]) {
    let height = (hints.len() as u16 + 4).min(area.height);
    let popup = centered(area, 56, height);
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .title(" Keys ")
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let lines: Vec<Line> = hints
        .iter()
        .map(|hint| {
            Line::from(vec![
                Span::styled(format!("{:>10}   ", hint.keys), palette::accent()),
                Span::raw(hint.what),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// A button: the one holding the focus is inverted, and the primary one is
/// accented when it is not.
pub fn button(frame: &mut Frame, area: Rect, label: &str, focused: bool, primary: bool) {
    let style = if focused {
        palette::focus()
    } else if primary {
        palette::accent()
    } else {
        palette::muted()
    };
    let [_, line, _] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {label} "), style)),
        line,
    );
}

/// A box a percentage of the width and a fixed height, centred.
pub fn centered(area: Rect, percent_x: u16, height: u16) -> Rect {
    let [vertical] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Percentage(percent_x)])
        .flex(Flex::Center)
        .areas(vertical);
    horizontal
}
