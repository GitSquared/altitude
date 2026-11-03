use std::mem;

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::Modifier,
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, Padding, Paragraph, Wrap},
};

use crate::{
    tui::{keys::HelpSection, widgets::palette},
    version::VERSION,
};

const TITLE: &str = "⛰  altitude";

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

pub fn draw(frame: &mut Frame, hints: &[Hint], guidance_enabled: bool) -> Rect {
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
    let [header, content, guidance_area] = frame_areas(inner, guidance.len() as u16);

    let [title, version] =
        Layout::horizontal([Constraint::Min(0), Constraint::Min(0)]).areas(header);
    frame.render_widget(Line::styled(TITLE, palette::heading()), title);
    frame.render_widget(
        Line::styled(VERSION, palette::muted()).right_aligned(),
        version,
    );
    if !guidance.is_empty() {
        frame.render_widget(
            Paragraph::new(guidance)
                .style(palette::muted())
                .right_aligned(),
            guidance_area,
        );
    }
    content
}

pub fn content_area(area: Rect, hints: &[Hint], guidance_enabled: bool) -> Rect {
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
    frame_areas(inner, guidance_rows)[1]
}

fn frame_areas(inner: Rect, guidance_rows: u16) -> [Rect; 3] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(guidance_rows),
    ])
    .areas(inner)
}

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

pub fn dim(frame: &mut Frame, area: Rect) {
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buffer[(x, y)].modifier |= Modifier::DIM;
        }
    }
}

pub fn draw_help(
    frame: &mut Frame,
    area: Rect,
    sections: &[HelpSection],
    scroll: &mut u16,
) -> Rect {
    // Narrower than the screen and a little short of it: the guide is a column to
    // read, not a wall.
    let popup = centered_width(area, 84, 88);
    frame.render_widget(Clear, popup);
    let block = modal("Keyboard shortcuts");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    // The keys are one left-aligned column and the words another, with a leader
    // between them, so the eye can run down either one.
    let key_width = sections
        .iter()
        .flat_map(|section| section.bindings.iter())
        .map(|(keys, _)| keys.chars().count())
        .max()
        .unwrap_or(0);
    let description_column = key_width + 4;
    let mut lines = Vec::new();
    for section in sections {
        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::styled(section.title, palette::heading()));
        for (keys, description) in section.bindings {
            let leader = ".".repeat(
                description_column
                    .saturating_sub(keys.chars().count() + 2)
                    .max(2),
            );
            lines.push(Line::from(vec![
                Span::styled((*keys).to_owned(), palette::accent()),
                Span::raw(" "),
                Span::styled(leader, palette::muted()),
                Span::raw(" "),
                Span::raw(*description),
            ]));
        }
    }
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let maximum = paragraph
        .line_count(inner.width)
        .saturating_sub(usize::from(inner.height));
    *scroll = (*scroll).min(maximum.min(usize::from(u16::MAX)) as u16);
    frame.render_widget(paragraph.scroll((*scroll, 0)), inner);
    popup
}

pub fn modal(title: impl Into<String>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(Line::styled(
            format!(" {} ", title.into()),
            palette::heading(),
        ))
}

pub fn centered(area: Rect, percent_x: u16, height: u16) -> Rect {
    centered_at_most(area, percent_x, u16::MAX, height)
}

pub fn centered_at_most(area: Rect, percent_x: u16, max_width: u16, height: u16) -> Rect {
    let width = (area.width * percent_x / 100).min(max_width).max(1);
    let [vertical] = Layout::vertical([Constraint::Length(height.min(area.height))])
        .flex(Flex::Center)
        .areas(area);
    let [horizontal] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(vertical);
    horizontal
}

fn centered_width(area: Rect, width: u16, height_percent: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width.min(area.width))])
        .flex(Flex::Center)
        .areas(area);
    Layout::vertical([Constraint::Percentage(height_percent)])
        .flex(Flex::Center)
        .areas::<1>(area)[0]
}

pub struct Body {
    pub content: Rect,
    pub note: Rect,
    pub actions: Rect,
}

pub fn body(inner: Rect) -> Body {
    let [_, content, note, actions] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(inner);
    Body {
        content,
        note,
        actions,
    }
}

pub fn modal_height(rows: u16) -> u16 {
    rows.saturating_add(6)
}
