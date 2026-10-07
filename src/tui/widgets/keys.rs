//! What a key means, in one place.
//!
//! Three gestures recur on every surface, so they are defined once rather than
//! spelled out per overlay. All three are conventions an operator can carry
//! between screens, which is the point: `q` and escape always mean "go away",
//! `d`, delete and backspace always mean "remove this", and `?` always means
//! "what do the keys do here".
//!
//! Inside a text field the first two are characters, which is why the field is
//! left with escape. `?` is the one that is only a character there.
use crossterm::event::{KeyCode, KeyEvent};

/// Whether a key means "go away": close what is in front of you, or quit the
/// application when there is nothing to close.
pub fn is_dismiss(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
}

/// Whether a key means "remove the thing the focus is on".
pub fn is_remove(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace
    )
}

/// Whether a key means "show me what the keys do here".
pub fn is_help(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('?'))
}
