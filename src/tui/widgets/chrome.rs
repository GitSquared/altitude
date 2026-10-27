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

use crate::tui::{
    keys::HelpSection,
    widgets::{palette, utils::centered_width},
};

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
/// either: the screen is the whole surface between them. What the interface has
/// to say is a modal and does not take a row of this frame.
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
        Line::styled(concat!("v", env!("CARGO_PKG_VERSION")), palette::muted()).right_aligned(),
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

/// One answer at the foot of a modal.
pub struct Action<'a> {
    pub label: &'a str,
    pub focused: bool,
    pub emphasis: Emphasis,
}

pub use super::button::{Button, Emphasis};

impl<'a> Action<'a> {
    pub fn primary(label: &'a str) -> Self {
        Self {
            label,
            focused: false,
            emphasis: Emphasis::Primary,
        }
    }

    pub fn secondary(label: &'a str) -> Self {
        Self {
            label,
            focused: false,
            emphasis: Emphasis::Secondary,
        }
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }
}

/// Draws the divider that closes a modal's body and the answers under it, and
/// answers with where each one was drawn so a click can find it.
///
/// `area` is two rows: the divider, then the buttons. The divider runs the full
/// width of the modal and breaks its borders, because the answers belong to the
/// window rather than to the form inside it.
///
/// A button is a word with room around it. The answer the page asks for is in
/// the accent, the one holding the keys is filled with it, and both are bold so
/// a footer made of two words still reads as somewhere to press.
pub fn actions(frame: &mut Frame, modal: Rect, area: Rect, actions: &[Action]) -> Vec<Rect> {
    if area.height < 2 || area.width == 0 {
        return Vec::new();
    }
    let buffer = frame.buffer_mut();
    buffer[(modal.x, area.y)].set_symbol("├");
    for column in (modal.x + 1)..modal.right().saturating_sub(1) {
        buffer[(column, area.y)].set_symbol("─");
    }
    buffer[(modal.right().saturating_sub(1), area.y)].set_symbol("┤");

    let row = Rect {
        y: area.y + 1,
        height: 1,
        ..area
    };
    let widths: Vec<Constraint> = actions
        .iter()
        .map(|action| Constraint::Length(Button::new(action.label, action.emphasis).width()))
        .collect();
    let drawn = Layout::horizontal(widths)
        .flex(Flex::End)
        .spacing(ACTION_GAP)
        .split(row);
    actions
        .iter()
        .zip(drawn.iter())
        .map(|(action, area)| {
            // The spaces belong to the label so the fill covers the whole button
            // and not just the letters, which is what makes it read as pressed
            // rather than as a highlighted word.
            Button::new(action.label, action.emphasis)
                .focused(action.focused)
                .draw(frame, *area);
            *area
        })
        .collect()
}

/// Between two answers, so they are two things rather than one row of text.
const ACTION_GAP: u16 = 3;

/// A box a percentage of the width and a fixed height, centred.
///
/// The frame a modal wears: a rounded border with what it is at the top.
///
/// The keys that work are not printed here. They belong to the frame at the
/// bottom of the screen, which every screen shares, so a modal cannot invent its
/// own keys or fall out of step with them.
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

/// The same box, never wider than `max_width` columns.
///
/// A share of the width is a share of whatever the terminal is, and a paragraph
/// in a box that wide is a line the eye loses its place in.
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

/// Where a modal's words and its answers go, inside its frame.
pub struct Body {
    /// What the modal says, one empty row from the top of the frame and one
    /// from the rule under it.
    pub content: Rect,
    /// The rule, and the answers under it.
    pub actions: Rect,
}

/// Splits the inside of a modal into what it says and how it is answered.
///
/// One empty row above the words and one below them, everywhere, so no two
/// modals are spaced differently. A modal that laid this out itself would be the
/// one that drifts.
pub fn body(inner: Rect) -> Body {
    let [_, content, _, actions] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(2),
    ])
    .areas(inner);
    Body { content, actions }
}

/// The height a modal needs to say `rows` of words: a border on each side, an
/// empty row above and below the words, and the rule and answers under them.
pub fn modal_height(rows: u16) -> u16 {
    rows.saturating_add(6)
}
