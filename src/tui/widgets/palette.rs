//! What colour means, in one place.
//!
//! Every style the interface draws with comes from here, named for its role
//! rather than its colour, so a screen that means "this failed" asks for
//! [`bad`] and cannot quietly decide that red means something else. The colours
//! themselves are the terminal's own, so the theme an operator has already
//! chosen is the one they see.

use ratatui::style::{Color, Modifier, Style};

/// The cyan bar marks where the keys go.
pub fn focus() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// The answer a modal is asking for: a filled block, so it reads as the one to
/// take even while the keys are somewhere else.
pub fn answer() -> Style {
    Style::new().fg(Color::Black).bg(Color::Cyan)
}

/// A filled thing holding the keys: the terminal's own colours turned over, so
/// it cannot be mistaken for the answer's fill. Turning the theme's own colours
/// over, rather than naming two, is what keeps this visible on a light terminal
/// as well as a dark one.
pub fn pressed() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

pub fn inactive_selection() -> Style {
    Style::new().fg(Color::White).bg(Color::DarkGray)
}

/// The edge of whatever holds the caret, and marks that invite a second look.
pub fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

/// What you are looking at, named.
///
/// White rather than the accent, because the accent is for the edge of the thing you
/// are on: a heading is not a caret, and a screen whose headings and focus share a
/// colour makes you look for the caret.
pub fn heading() -> Style {
    Style::new().fg(Color::White).add_modifier(Modifier::BOLD)
}

/// Text that is the point of the screen rather than its furniture.
pub fn primary() -> Style {
    Style::new().fg(Color::White)
}

/// The interface talking about itself: hints, labels, metadata.
///
/// The foreground is named even though it is the terminal's own, so text drawn
/// over a coloured backdrop takes the default colour back rather than inheriting
/// whatever the backdrop left in the cell.
pub fn muted() -> Style {
    Style::new().fg(Color::Reset).add_modifier(Modifier::DIM)
}

/// Work that is going well: running, accepted, in review.
pub fn good() -> Style {
    Style::new().fg(Color::Green)
}

/// Work that is waiting on a person.
pub fn attention() -> Style {
    Style::new().fg(Color::Yellow)
}

/// Work that has failed, or something the operator has to see.
pub fn bad() -> Style {
    Style::new().fg(Color::Red)
}

/// The travelling highlight on text that is waiting.
///
/// Greys rather than a colour, because waiting is not a status: nothing is
/// wrong and nothing is finished, and a coloured word would claim otherwise.
pub fn waiting() -> Style {
    Style::new().fg(Color::Gray)
}

/// The text the waiting highlight travels over.
pub fn waiting_rest() -> Style {
    Style::new().fg(Color::DarkGray)
}
