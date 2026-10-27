//! A word with room around it: the thing a footer or a card asks you to press.
//!
//! One style everywhere, so a button is one thing to recognise. A secondary
//! answer is as legible as a primary one — it is quieter by weight, not by being
//! dim — and a disabled one stays bold and dim rather than disappearing, because
//! the keys can still be on it and what it would do is still worth reading.

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
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

/// One answer at the foot of a modal.
pub struct Action<'a> {
    pub label: &'a str,
    pub focused: bool,
    pub emphasis: Emphasis,
}

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
