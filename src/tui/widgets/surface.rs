//! A bordered region of the screen, and how it sits on the field behind it.
//!
//! The field is drawn once per frame under everything. A surface is what a
//! screen draws on top of it: a bordered region that either lets the field show
//! through, or covers it.
//!
//! Every edge names the terminal's own foreground. A border drawn with a default
//! style keeps whatever colour the cells under it already had, and when the
//! field is under the border, that is the field's grey rather than the theme's
//! border colour. Naming the foreground is what makes the field safe to draw
//! beneath a surface at all.

use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Clear},
};

/// The style every edge takes, whatever is behind it.
///
/// A reset rather than a foreground: the field sets a dim modifier on the cells
/// it draws, and a foreground-only style would leave the border dimmed by a
/// surface that is no longer there.
pub fn outline() -> Style {
    Style::reset()
}

/// Cover an area a widget is about to write in.
///
/// The field is a backdrop, not a layer every widget is expected to know about:
/// anything that is meant to be opaque covers its own rectangle first. A row,
/// a control bar, a button footer — all of them are opaque. Only the gaps a
/// screen deliberately leaves open let the contours through.
pub fn cover(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
}

/// A horizontal divider across one panel column, joined to its sides.
///
/// `area` is the inside of one column: the divider starts at the surface's
/// left border, runs across the column, and meets the vertical divider on the
/// right. Both ends are joined to whatever was already there rather than
/// replaced with a letter chosen by this function, so a divider ending at a
/// vertical divider makes a tee and one ending at nothing makes a corner.
pub fn horizontal_divider(frame: &mut Frame, area: Rect, y: u16) {
    if area.width == 0 || y < area.y || y >= area.bottom() {
        return;
    }
    let outline = outline();
    let buffer = frame.buffer_mut();
    if let Some(left) = area.x.checked_sub(1)
        && join(buffer, left, y, RIGHT)
    {
        buffer[(left, y)].set_style(outline);
    }
    for x in area.x..area.right() {
        buffer[(x, y)].set_symbol("─").set_style(outline);
    }
    if join(buffer, area.right(), y, LEFT) {
        buffer[(area.right(), y)].set_style(outline);
    }
}

// Which strokes leave a cell. A junction is the union of what was there and what
// is being drawn through it, which is why a divider joined to a vertical line makes
// a tee rather than a cross.
const UP: u8 = 1;
const DOWN: u8 = 2;
const LEFT: u8 = 4;
const RIGHT: u8 = 8;

fn join(buffer: &mut Buffer, x: u16, y: u16, added: u8) -> bool {
    if x < buffer.area.x
        || x >= buffer.area.right()
        || y < buffer.area.y
        || y >= buffer.area.bottom()
    {
        return false;
    }
    let directions = strokes(buffer[(x, y)].symbol()) | added;
    buffer[(x, y)].set_symbol(stroke(directions));
    true
}

fn strokes(symbol: &str) -> u8 {
    match symbol {
        "│" => UP | DOWN,
        "─" => LEFT | RIGHT,
        "┌" => DOWN | RIGHT,
        "┐" => DOWN | LEFT,
        "└" => UP | RIGHT,
        "┘" => UP | LEFT,
        "├" => UP | DOWN | RIGHT,
        "┤" => UP | DOWN | LEFT,
        "┬" => DOWN | LEFT | RIGHT,
        "┴" => UP | LEFT | RIGHT,
        "┼" => UP | DOWN | LEFT | RIGHT,
        _ => 0,
    }
}

fn stroke(directions: u8) -> &'static str {
    match (
        directions & UP != 0,
        directions & DOWN != 0,
        directions & LEFT != 0,
        directions & RIGHT != 0,
    ) {
        (false, false, false, false) => " ",
        (true, false, false, false) => "╵",
        (false, true, false, false) => "╷",
        (false, false, true, false) => "╴",
        (false, false, false, true) => "╶",
        (true, true, false, false) => "│",
        (false, false, true, true) => "─",
        (false, true, false, true) => "┌",
        (false, true, true, false) => "┐",
        (true, false, false, true) => "└",
        (true, false, true, false) => "┘",
        (true, true, false, true) => "├",
        (true, true, true, false) => "┤",
        (false, true, true, true) => "┬",
        (true, false, true, true) => "┴",
        (true, true, true, true) => "┼",
    }
}

pub fn vertical_divider(frame: &mut Frame, inner: Rect, x: u16) {
    if inner.height == 0 || x < inner.x || x >= inner.right() {
        return;
    }
    let outline = outline();
    let buffer = frame.buffer_mut();
    for y in inner.top()..inner.bottom() {
        buffer[(x, y)].set_symbol("│").set_style(outline);
    }
    let top = inner.y.saturating_sub(1);
    let bottom = inner.bottom();
    buffer[(x, top)].set_symbol("┬").set_style(outline);
    if bottom < buffer.area.bottom() {
        buffer[(x, bottom)].set_symbol("┴").set_style(outline);
    }
}

pub struct Surface {
    block: Block<'static>,
    opaque: bool,
}

impl Surface {
    pub fn new() -> Self {
        Self {
            block: Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(outline()),
            opaque: false,
        }
    }

    pub fn opaque(mut self) -> Self {
        self.opaque = true;
        self
    }

    pub fn draw(self, frame: &mut Frame, area: Rect) -> Rect {
        let inner = self.block.inner(area);
        if self.opaque {
            frame.render_widget(Clear, area);
        }
        frame.render_widget(self.block, area);
        inner
    }
}

impl Default for Surface {
    fn default() -> Self {
        Self::new()
    }
}
