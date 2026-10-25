//! Columns inside a surface, with the divider between them drawn as part of the
//! layout.
//!
//! The caller draws the surface first and hands this the area inside its border.
//! Each divider occupies one column of the gap, and is joined to the surface's
//! border at both ends; a screen that asks for columns cannot end up without
//! them.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
};

use super::surface;

/// How narrow the last column may get before the others give width back.
const MIN_DETAIL: u16 = 24;
/// The column a divider occupies.
const DIVIDER: u16 = 1;

/// A run of columns inside a surface's inner area.
pub struct Columns<'a> {
    widths: &'a [u16],
}

impl<'a> Columns<'a> {
    /// The widths the columns before the last one ask for. The last column takes
    /// what is left.
    pub fn new(widths: &'a [u16]) -> Self {
        Self { widths }
    }

    /// Lay the columns out, draw the dividers between them, and answer the full
    /// rectangles. `inner` is inside the surface's border.
    pub fn draw(self, frame: &mut Frame, inner: Rect) -> Vec<Rect> {
        // Narrow screens give width back from the requested columns first; the
        // last column is the one that cannot be scrolled sideways.
        let dividers = self.widths.len() as u16 * DIVIDER;
        let room = inner.width.saturating_sub(dividers + MIN_DETAIL);
        let mut left = room;
        let widths: Vec<u16> = self
            .widths
            .iter()
            .map(|width| {
                let width = (*width).min(left);
                left = left.saturating_sub(width);
                width
            })
            .collect();
        let constraints = widths
            .into_iter()
            .map(Constraint::Length)
            .chain(Some(Constraint::Min(0)));
        let rects = Layout::horizontal(constraints)
            .spacing(DIVIDER)
            .split(inner)
            .to_vec();
        for pair in rects.windows(2) {
            surface::vertical_divider(frame, inner, pair[0].right());
        }
        rects
    }
}
