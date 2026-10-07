//! What colour means, in one place.
//!
//! Every style the interface draws with comes from here, named for its role
//! rather than its colour, so a screen that means "this failed" asks for
//! [`bad`] and cannot quietly decide that red means something else. The colours
//! themselves are the terminal's own, so the theme an operator has already
//! chosen is the one they see.
//!
//! Six ideas, and nothing outside them:
//!
//! - **focus** — the one thing you are on. The only style with a background.
//! - **accent** — the caret's edge, and marks that invite a second look.
//! - **heading** — what you are looking at, named.
//! - **primary** — text that is the point of the screen.
//! - **muted** — the interface talking about itself.
//! - **good / attention / bad** — how the work is going.

use ratatui::style::{Color, Modifier, Style};

/// The one thing the focus is on.
///
/// The only style in the interface with a background, which is what makes it
/// unambiguous: wherever a filled bar appears, that is where the keys go.
pub fn focus() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// The edge of whatever holds the caret, and marks that invite a second look.
pub fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

/// What you are looking at, named.
pub fn heading() -> Style {
    accent().add_modifier(Modifier::BOLD)
}

/// Text that is the point of the screen rather than its furniture.
pub fn primary() -> Style {
    Style::new().fg(Color::White)
}

/// The interface talking about itself: hints, labels, metadata, anything read
/// once and then stopped seeing.
pub fn muted() -> Style {
    Style::new().add_modifier(Modifier::DIM)
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
