//! A word with room around it: the thing a footer or a card asks you to press.
//!
//! One style everywhere, so a button is one thing to recognise. A secondary
//! answer is as legible as a primary one — it is quieter by weight, not by being
//! dim — and a disabled one stays bold and dim rather than disappearing, because
//! the keys can still be on it and what it would do is still worth reading.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::palette;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    /// The answer the surface exists for.
    Primary,
    /// The other way out, still a button.
    Secondary,
}

impl Emphasis {
    pub fn style(self, focused: bool, enabled: bool) -> Style {
        match (focused, enabled) {
            // A grey fill, not the accent: where the keys are is still visible,
            // and there is nothing to do yet.
            (true, false) => palette::inactive_selection().add_modifier(Modifier::BOLD),
            (true, true) => palette::focus(),
            (false, false) => palette::muted().add_modifier(Modifier::BOLD),
            (false, true) => match self {
                Self::Primary => palette::primary().add_modifier(Modifier::BOLD),
                Self::Secondary => palette::primary(),
            },
        }
    }
}

pub struct Button<'a> {
    pub label: &'a str,
    pub emphasis: Emphasis,
    pub focused: bool,
    pub enabled: bool,
}

impl<'a> Button<'a> {
    pub fn new(label: &'a str, emphasis: Emphasis) -> Self {
        Self {
            label,
            emphasis,
            focused: false,
            enabled: true,
        }
    }

    pub fn primary(label: &'a str) -> Self {
        Self::new(label, Emphasis::Primary)
    }

    pub fn secondary(label: &'a str) -> Self {
        Self::new(label, Emphasis::Secondary)
    }

    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// The spaces are part of the label, so a fill covers the whole button and
    /// not just the letters.
    pub fn width(&self) -> u16 {
        self.label.chars().count() as u16 + 2
    }

    pub fn span(&self) -> Span<'static> {
        Span::styled(
            format!(" {} ", self.label),
            self.emphasis.style(self.focused, self.enabled),
        )
    }

    pub fn draw(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Paragraph::new(Line::from(self.span())), area);
    }
}
