//! What a key means, in one place.
//!
//! Two gestures recur on every surface, so they are defined once rather than
//! spelled out per overlay. Both are conventions an operator can carry between
//! screens, which is the point: `q` and escape always mean "go away", and `d`,
//! delete and backspace always mean "remove this" — except inside a text field,
//! where those keys type, and the field is left with escape.
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
