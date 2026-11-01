use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
};

use super::surface;

const MIN_DETAIL: u16 = 24;
const DIVIDER: u16 = 1;

pub struct Columns<'a> {
    widths: &'a [u16],
}

impl<'a> Columns<'a> {
    pub fn new(widths: &'a [u16]) -> Self {
        Self { widths }
    }

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
