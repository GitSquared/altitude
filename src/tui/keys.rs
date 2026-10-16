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

pub struct HelpSection {
    pub title: &'static str,
    pub bindings: &'static [(&'static str, &'static str)],
}

pub fn help_sections() -> Vec<HelpSection> {
    vec![
        HelpSection {
            title: "Navigation",
            bindings: &[
                ("h / l · ← / →", "Move across columns or cards"),
                ("j / k · ↓ / ↑", "Select the next or previous item"),
                (
                    "Tab / Shift-Tab",
                    "Move forward or backward between controls",
                ),
                ("Enter", "Activate the selected item or action"),
                ("Esc / q", "Close a panel; q quits from the task list"),
                ("Ctrl-C (task list)", "Quit Altitude"),
                ("?", "Open or close this keyboard guide"),
                (
                    "PgUp / PgDn · Home / End",
                    "Scroll this guide; j/k and ↑/↓ also scroll",
                ),
                (
                    "While typing",
                    "Letters, q and ? are text; Esc leaves the field",
                ),
            ],
        },
        HelpSection {
            title: "Tasks",
            bindings: &[
                ("c", "Create a task"),
                ("s", "Open Settings"),
                ("Enter / i", "Read and answer the selected task's signals"),
                ("o", "Take over its terminal, or bring a remote task home"),
                ("r", "Send a local task to a remote"),
                ("b", "Open its linked issue in your browser"),
                (
                    "d / Delete / Backspace",
                    "Remove a local task, after confirmation",
                ),
                ("Tab / Shift-Tab", "Select the next or previous card"),
            ],
        },
        HelpSection {
            title: "Settings",
            bindings: &[
                ("Tab / l / →", "Move to the next column"),
                ("Shift-Tab / h / ←", "Move to the previous column"),
                ("j / k · ↓ / ↑", "Choose a section or an item"),
                (
                    "Enter",
                    "Open a section, sign in, or use the selected action",
                ),
                ("Space (Agents)", "Use the selected agent by default"),
                (
                    "Delete / Backspace",
                    "Remove saved credentials, after confirmation",
                ),
                ("n", "Add an environment or remote"),
                ("e", "Edit an environment or remote"),
                (
                    "d / Delete / Backspace",
                    "Remove an environment or remote, after confirmation",
                ),
                ("t (Remotes)", "Choose the remote's task tracker"),
                (
                    "PgUp / PgDn · Home",
                    "Scroll details, or return to their beginning",
                ),
                ("Esc / q / s", "Close Settings"),
            ],
        },
        HelpSection {
            title: "Task creation",
            bindings: &[
                ("Enter", "Start work using the displayed settings"),
                (
                    "Ctrl/Cmd-Shift-Enter",
                    "Start work and keep the creation window open",
                ),
                ("Ctrl/Cmd-K", "Switch between an issue search and a prompt"),
                ("Shift/Alt-Enter", "Add a line to the prompt"),
                ("Tab / Shift-Tab", "Move between the prompt and settings"),
                ("j/k · ↓/↑ (settings)", "Move between fields"),
                ("h/l · ←/→ (settings)", "Change the selected setting"),
                ("↓ / ↑ (issue search)", "Select a search result"),
                ("Esc", "Cancel; q also cancels outside a text field"),
            ],
        },
        HelpSection {
            title: "Signals and replies",
            bindings: &[
                (
                    "j/k · ↓/↑",
                    "Select a signal (h also selects the previous one)",
                ),
                ("Tab / Shift-Tab", "Select an available signal action"),
                ("Enter", "Use the selected action; send a typed reply"),
                ("PgUp / PgDn · Home / End", "Scroll the signal body"),
                ("Esc / q / i", "Close signals; use Esc when typing a reply"),
            ],
        },
        HelpSection {
            title: "Forms, choices and confirmations",
            bindings: &[
                ("Tab / Shift-Tab", "Move between fields or buttons"),
                ("← / → (confirmation)", "Switch between cancel and confirm"),
                ("j/k · ↓/↑ (send task)", "Choose a remote"),
                ("s (send task)", "Open Settings if no remote is ready"),
                ("Enter", "Use the selected button or submit the form"),
                (
                    "d / Delete / Backspace",
                    "Confirm removal in a removal dialog",
                ),
                ("Esc / q", "Cancel (q remains text while editing)"),
            ],
        },
        HelpSection {
            title: "Setup helpers",
            bindings: &[
                (
                    "j/k · ↓/↑",
                    "Scroll the instructions, or choose a task tracker",
                ),
                ("Enter (task trackers)", "Open the selected tracker's guide"),
                (
                    "PgUp / PgDn · Home / End",
                    "Scroll by a page, or to either end",
                ),
                (
                    "Esc / q",
                    "Close the helper, or step back from a guide; cancel a pending \
                     browser sign-in",
                ),
            ],
        },
        HelpSection {
            title: "Text editing",
            bindings: &[
                ("← / → · Home / End", "Move the cursor"),
                ("Backspace / Delete", "Remove text beside the cursor"),
                ("↑ / ↓ (multiline)", "Move between lines"),
                ("Paste", "Insert text from the clipboard"),
            ],
        },
        HelpSection {
            title: "Guest terminals",
            bindings: &[
                (
                    "Ctrl-A",
                    "Close a login shell, or hand a task back to autopilot",
                ),
                (
                    "PgUp / PgDn",
                    "Scroll terminal history outside fullscreen programs",
                ),
                (
                    "Mouse wheel",
                    "Scroll history, or forward to a mouse-aware program",
                ),
                (
                    "All other keys",
                    "Go to the guest program, including q, Esc and ?",
                ),
                (
                    "Esc / q (preparing)",
                    "Cancel preparation before the shell opens",
                ),
            ],
        },
    ]
}
