//! What colour means, in one place.
//!
//! The palette is the terminal's own. It has sixteen colours, and a theme picks
//! them to be legible on the background it also picked, so they are the only
//! hues named here; each hue has a base half and a bright half, which is the
//! shade scale this interface uses rather than hand-picked greys. Anything that
//! is simply text asks for the terminal's foreground instead of naming a
//! colour, because the terminal is the only thing that knows what colour its
//! text is.
//!
//! Nothing here is RGB. A sixteen-colour terminal is the floor, and it is a
//! good floor: it already knows what "quiet" looks like on its own background.
//!
//! Roles are named for what they mean rather than how they look, so a screen
//! that means "this failed" asks for [`bad`] and cannot quietly decide that red
//! means something else.

use ratatui::style::{Color, Modifier, Style};

/// The cyan bar marks where the keys go.
pub fn focus() -> Style {
    Style::new()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}

/// A thing holding the keys: the terminal's own colours turned over. Turning
/// them over rather than naming two is what keeps this visible on a light
/// terminal as well as a dark one, and it is the one focus cue this interface
/// uses on anything filled.
pub fn pressed() -> Style {
    Style::new().add_modifier(Modifier::REVERSED)
}

pub fn inactive_selection() -> Style {
    Style::new().fg(Color::Reset).bg(Color::DarkGray)
}

/// The edge of whatever holds the caret, and marks that invite a second look.
pub fn accent() -> Style {
    Style::new().fg(Color::Cyan)
}

/// What you are looking at, named.
///
/// The theme's own foreground rather than the accent, because the accent is for
/// the edge of the thing you are on: a heading is not a caret, and a screen whose
/// headings and focus share a colour makes you look for the caret.
pub fn heading() -> Style {
    Style::new().fg(Color::Reset).add_modifier(Modifier::BOLD)
}

/// Text that is the point of the screen rather than its furniture.
///
/// The theme's foreground at full strength. Its difference from [`muted`] is the
/// difference between a value and the word naming it, so it has to be the same
/// colour the rest of the terminal writes in.
pub fn primary() -> Style {
    Style::new().fg(Color::Reset)
}

/// The interface talking about itself: hints, labels, and the name of a value.
///
/// The dim half of the text colour — ANSI 8 — rather than the terminal's own
/// foreground dimmed, because a terminal is allowed to ignore the dim attribute,
/// and a name that looks exactly like the value it names is worse than a shade
/// that is a little too loud.
pub fn muted() -> Style {
    Style::new().fg(Color::DarkGray)
}

/// The field behind everything: one shade quieter than furniture, because it is
/// the background rather than something on it.
///
/// The dim attribute on top of [`muted`] is what makes it a shade below the
/// sixteen; a terminal that ignores it still draws the quiet shade, which is
/// closer to the background than the text is on either kind of terminal.
pub fn backdrop() -> Style {
    muted().add_modifier(Modifier::DIM)
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
/// The theme's own foreground rather than a colour, because waiting is not a
/// status: nothing is wrong and nothing is finished, and a coloured word would
/// claim otherwise.
pub fn waiting() -> Style {
    Style::new().fg(Color::Reset)
}

/// The text the waiting highlight travels over: the same quiet shade as
/// furniture, so the highlight is a brightness rather than a second colour.
pub fn waiting_rest() -> Style {
    muted()
}
