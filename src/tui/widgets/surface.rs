use ratatui::{
    Frame,
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Clear},
};

pub fn outline() -> Style {
    Style::reset()
}

pub fn cover(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
}

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

pub fn join_down(frame: &mut Frame, x: u16, y: u16, rows: u16) {
    let buffer = frame.buffer_mut();
    // One stroke, one colour: the cell it starts in owns the style, so a separator
    // that meets a rule does not go pale for its last row.
    let stroke = buffer[(x, y)].style();
    for row in 0..rows {
        // A stroke that joins something at its ends does not continue past them: the
        // first row is where it hangs from (a rule there becomes a tee), and the last
        // row is the rule the caller drew (a stroke arriving there ends in a foot).
        let added = if row + 1 == rows {
            UP
        } else if row == 0 {
            DOWN
        } else {
            UP | DOWN
        };
        let at = y.saturating_add(row);
        if !join(buffer, x, at, added) {
            return;
        }
        buffer[(x, at)].set_style(stroke);
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
