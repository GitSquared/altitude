use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub fn is_dismiss(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
}

pub fn is_secondary(key: KeyEvent) -> bool {
    key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL)
}

pub fn is_remove(key: KeyEvent) -> bool {
    matches!(
        key.code,
        KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace
    )
}

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
                ("j / k · h / l", "Select the next or previous card"),
                ("← / → · ↑ / ↓", "Move across the grid"),
                (
                    "Tab / Shift-Tab",
                    "Switch screen tabs; in a form, move between fields",
                ),
                ("Enter", "Activate the selected item or action"),
                (
                    "Esc / q",
                    "Close a panel; quit from the task list or the first screen",
                ),
                ("Ctrl-C", "Quit Altitude"),
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
                ("Ctrl-L", "Link a Linear issue and start work on it"),
                (
                    "Tab / Shift-Tab · [ / ]",
                    "Switch project tabs; the grid and a new task follow",
                ),
                ("s", "Open Settings"),
                ("Enter / i", "Read and answer the selected task's signals"),
                (
                    "Ctrl-Enter",
                    "Take over the task; a remote task moves here first",
                ),
                (
                    "p",
                    "Send the selected task to the machine its project runs on",
                ),
                ("b", "Open its linked issue in your browser"),
                (
                    "d / Delete / Backspace",
                    "Delete the selected task, after confirmation",
                ),
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
                ("n", "Add a project or remote"),
                ("e", "Edit a project or remote"),
                (
                    "d / Delete / Backspace",
                    "Remove a project or remote, after confirmation",
                ),
                (
                    "t (Remotes)",
                    "Open the guide for piloting tasks from a tracker",
                ),
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
                ("Ctrl-Enter", "Start work using the displayed settings"),
                ("Enter (prompt)", "Add a line to the prompt"),
                ("Enter (project)", "Open the projects"),
                (
                    "j/k · ↑/↓ · Enter/Esc",
                    "Move through the open list, take or close it",
                ),
                (
                    "/ (five choices or more)",
                    "Filter the open list; esc clears the filter, then closes",
                ),
                ("Ctrl-E", "Open the project list"),
                (
                    "An issue ID or URL",
                    "Switch the prompt to linking that issue",
                ),
                (
                    "Tab / Shift-Tab",
                    "Move between the prompt, the project and the actions",
                ),
                (
                    "j/k · ←/→ · ↓/↑",
                    "Move between the prompt, the project and the actions",
                ),
                ("↓ / ↑ (issue search)", "Select a search result"),
                ("Esc", "Cancel; q also cancels outside a text field"),
            ],
        },
        HelpSection {
            title: "Signals",
            bindings: &[
                ("j/k · ↓/↑", "Move between signal threads"),
                (
                    "Enter",
                    "Take the primary action: acknowledge a decision, accept a review, resolve a log the agent finished, or reply where that is all there is",
                ),
                (
                    "Ctrl-Enter",
                    "Take the secondary action where the thread has one: reply to the agent. A reply reopens the thread it answers",
                ),
                ("f / s", "Filter or sort the thread list"),
                ("n", "Write a new message to the agent"),
                (
                    "Tab / Shift-Tab · p",
                    "Switch between the conversation and the plan on a narrow screen",
                ),
                (
                    "PgUp / PgDn · Home / End",
                    "Scroll the conversation by a message",
                ),
                (
                    "Enter (writing a reply)",
                    "Send it; shift-enter adds a line and esc puts it away",
                ),
                ("Esc / q", "Close signals; esc leaves the reply box first"),
            ],
        },
        HelpSection {
            title: "Forms, choices and confirmations",
            bindings: &[
                ("Tab / Shift-Tab", "Move between fields or buttons"),
                (
                    "Ctrl-Enter",
                    "Save a project or remote from wherever the caret is",
                ),
                ("← / → (confirmation)", "Switch between cancel and confirm"),
                (
                    "/ (five choices or more)",
                    "Filter the open list instead of moving in it",
                ),
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
                    "Scroll the instructions, or choose a tracker's guide",
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
