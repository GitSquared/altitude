//! Where a click can land.
//!
//! A widget draws itself and says what each rectangle it drew means; the mouse
//! is read later against those rectangles. Keeping them from the last frame
//! rather than recomputing them is what keeps a click and the thing under it in
//! step: a layout that moved because a list opened is a layout that was drawn
//! that way, and a second guess at it would be a second source of truth.
//!
//! The last thing drawn wins, because that is the thing on top.

use ratatui::layout::Rect;

/// What a page's drawn rectangles mean, kept until the next frame replaces them.
pub struct Targets<T> {
    entries: Vec<(Rect, T)>,
}

impl<T> Default for Targets<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<T> Targets<T> {
    /// Forget the last frame's targets, which a page does before drawing again:
    /// what is no longer on screen is no longer clickable.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn add(&mut self, area: Rect, target: T) {
        self.entries.push((area, target));
    }

    /// What a click at a terminal column and row landed on, if anything.
    pub fn at(&self, column: u16, row: u16) -> Option<&T> {
        self.entries
            .iter()
            .rev()
            .find(|(area, _)| {
                column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
            })
            .map(|(_, target)| target)
    }
}
