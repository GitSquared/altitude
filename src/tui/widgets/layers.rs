//! What a surface was opened over.
//!
//! A terminal or a dialog that covers a screen has two jobs that are really one
//! fact: draw that screen under itself, and close back to it. Both are the screen
//! that was in front when it opened, so it is kept here once rather than each
//! overlay assuming the screen it usually sits on. A credential terminal opened
//! from Settings and one opened from the first screen are the same widget; only
//! the thing underneath is different.

/// The screen a surface covered when it took the foreground.
#[derive(Default)]
pub struct Behind<T>(Option<T>);

impl<T> Behind<T> {
    /// Record what is covered, as the surface takes the foreground.
    pub fn cover(&mut self, under: T) {
        self.0 = Some(under);
    }

    /// Take back what was covered, when the surface closes.
    pub fn uncover(&mut self) -> Option<T> {
        self.0.take()
    }

    /// What is covered, for drawing under the surface.
    pub fn under(&self) -> Option<&T> {
        self.0.as_ref()
    }
}
