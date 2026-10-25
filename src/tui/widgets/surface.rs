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
    layout::Rect,
    style::Style,
    widgets::{Block, BorderType, Clear, Padding},
};

/// The style every edge takes, whatever is behind it.
///
/// A reset rather than a foreground: the field sets a dim modifier on the cells
/// it draws, and a foreground-only style would leave the border dimmed by a
/// surface that is no longer there.
pub fn outline() -> Style {
    Style::reset()
}

/// A single rule between columns, joined to the frame at both ends.
pub fn divider(frame: &mut Frame, inner: Rect, x: u16) {
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
    buffer[(x, bottom)].set_symbol("┴").set_style(outline);
}

/// A bordered region over the field.
pub struct Surface {
    block: Block<'static>,
    opaque: bool,
}

impl Surface {
    /// A surface the field shows through.
    pub fn new() -> Self {
        Self {
            block: Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(outline()),
            opaque: false,
        }
    }

    pub fn padding(mut self, padding: Padding) -> Self {
        self.block = self.block.padding(padding);
        self
    }

    /// Cover the field inside the border, so the surface reads as a solid pane.
    pub fn opaque(mut self) -> Self {
        self.opaque = true;
        self
    }

    /// Draw the surface and answer the area it leaves inside the border.
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
