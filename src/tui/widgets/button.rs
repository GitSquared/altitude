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
    Primary,
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

const ACTION_GAP: u16 = 3;
