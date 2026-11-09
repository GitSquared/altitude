use std::time::Duration;

use crossterm::event::{MouseEvent, MouseEventKind};

use crate::{task::TaskExecutionStatus, tui::popups, tui::screens, tui::widgets::chrome::Hint};

use popups::scopes::Scopes;
use popups::setup::SetupKind;
use screens::settings::Depth;
use screens::signals::{SignalFocus, primary_action, secondary_action};

use super::*;

impl App {
    pub(in crate::tui) fn keyboard_hints(&self) -> bool {
        self.settings_file
            .as_ref()
            .is_none_or(|file| file.settings().interface().keyboard_hints)
    }

    pub(in crate::tui) fn animations(&self) -> bool {
        self.settings_file
            .as_ref()
            .is_none_or(|file| file.settings().interface().animations)
    }

    pub(in crate::tui) fn backdrop_animates(&self) -> bool {
        self.animations() && self.open_shell().is_none()
    }

    pub(in crate::tui) fn animation_tick(&self) -> Option<usize> {
        self.animations().then_some(self.shimmer)
    }

    pub(in crate::tui) fn needs_shimmer(&self) -> bool {
        self.has_modal()
            || self.repositories_listing
            || self
                .panel
                .form
                .as_ref()
                .and_then(|form| form.scopes.as_ref())
                .is_some_and(Scopes::is_listing)
            || self.is_awaiting_progress()
            || self.moving.is_some()
            || !self.environment_builds.is_empty()
            || self
                .remote_tasks
                .iter()
                .any(|held| held.summary.status == TaskExecutionStatus::Pending)
            || self.signal_agent_working()
            || self.signal_message_flash.is_some()
    }

    pub(in crate::tui) fn hints(&self) -> Vec<Hint> {
        // A message in front of the work is what the keys are about until it is
        // read, and a transfer under it keeps the one key it has.
        if self.alert.is_some() || self.transfer.is_some() {
            let mut hints = Vec::new();
            if self.alert.is_some() {
                hints.push(Hint::new("enter", "dismiss"));
            }
            if self.transfer.is_some() {
                hints.push(Hint::new("esc", "stop the transfer"));
            }
            return hints;
        }
        let mut hints = self.hints_for(&self.foreground);
        hints.retain(|hint| {
            !matches!(hint.keys, "tab" | "j/k" | "h/j/k/l" | "pgup/pgdn")
                && !matches!((hint.keys, hint.what), ("esc", "close") | ("q", "quit"))
        });
        for hint in &mut hints {
            if hint.keys == "enter / tab" {
                hint.keys = "enter";
            }
        }
        if !self.is_typing()
            && self.open_shell().is_none()
            && !hints.iter().any(|hint| hint.keys == "?")
        {
            hints.push(Hint::new("?", "all shortcuts"));
        }
        hints
    }

    pub(in crate::tui) fn hints_for(&self, front: &Foreground) -> Vec<Hint> {
        let dismiss = || Hint::new("esc", "close");
        match front {
            // Help owns the screen while it is open, so the line under it shows
            // how to move through and leave the help itself.
            Foreground::Keys(_) => vec![
                Hint::new("↑/↓ · j/k · pgup/pgdn", "scroll"),
                Hint::new("esc / q", "close"),
            ],
            Foreground::IdentitySetup(_) => vec![
                Hint::new("j/k", "move"),
                Hint::new("enter", "choose"),
                Hint::new("esc", "close"),
            ],
            Foreground::RenameIdentity(_) => vec![
                Hint::new("type", "the name"),
                Hint::new("enter", "save"),
                Hint::new("esc", "cancel"),
            ],
            Foreground::GitIdentity(_) => vec![
                Hint::new("tab", "field"),
                Hint::new("space", "toggle signing"),
                Hint::new("enter", "save"),
                Hint::new("esc", "cancel"),
            ],
            Foreground::Confirm(pending) => pending.hints(),
            Foreground::SendTask(sending) => sending.hints(),
            Foreground::NewTask(new_task) => new_task.hints(),
            Foreground::GetStarted(panel) => {
                if matches!(self.credential_shell, CredentialShellState::Opening { .. }) {
                    vec![Hint::new("esc / q", "cancel")]
                } else {
                    panel.hints(&self.setup())
                }
            }
            Foreground::Message(message) => message.hints(),
            Foreground::Panel => {
                // Only a wait with something to show offers leaving it; a shell that
                // opens at once would otherwise flash a key that means nothing yet.
                if let CredentialShellState::Opening {
                    progress: Some(_), ..
                } = &self.credential_shell
                {
                    return vec![Hint::new("esc / q", "cancel")];
                }
                if let Some(form) = &self.panel.form {
                    if let Some(browser) = &form.repositories {
                        return browser.select.hints();
                    }
                    if let Some(hints) = form.list_hints() {
                        return hints;
                    }
                    let hints = vec![
                        Hint::new("tab", "next"),
                        Hint::new("ctrl-enter", "save"),
                        Hint::new("enter", "next, or add a line"),
                        Hint::new("esc", "leave the box, then the page"),
                    ];
                    return hints;
                }
                if self.panel.depth == Depth::Rail {
                    return vec![
                        Hint::new("j/k", "section"),
                        Hint::new("enter / tab", "open"),
                        dismiss(),
                    ];
                }
                let mut hints = vec![Hint::new("tab", "change column"), Hint::new("j/k", "move")];
                // One row, asked once: what it offers, and whether it can be
                // signed out of or removed.
                let row = self.selected_row();
                if let Some(row) = &row {
                    hints.extend(row.hints(self.is_connecting_tracker()));
                }
                if row.as_ref().is_some_and(|row| row.forgets_credentials()) {
                    hints.push(Hint::new("delete", "sign out"));
                }
                if self.panel.section.grows() {
                    hints.push(Hint::new("n", "new"));
                }
                if row.as_ref().is_some_and(|row| row.is_removable()) {
                    hints.push(Hint::new("d", "delete"));
                }
                hints.push(Hint::new("pgup/pgdn", "details"));
                hints.push(dismiss());
                hints
            }
            Foreground::SignIn(sign_in) => sign_in.hints(),
            Foreground::Setup(setup) => match &setup.kind {
                SetupKind::RemoteTrackers => vec![
                    Hint::new("j/k", "choose"),
                    Hint::new("enter", "setup guide"),
                    Hint::new("esc / q", "close"),
                ],
                SetupKind::RemoteTrackerGuide(_) => vec![
                    Hint::new("j/k · pgup/pgdn", "scroll"),
                    Hint::new("esc / q", "back"),
                ],
                SetupKind::SigningKey => {
                    vec![Hint::new("c", "copy"), Hint::new("esc / q", "close")]
                }
                _ => vec![
                    Hint::new("j/k · pgup/pgdn", "scroll"),
                    Hint::new("esc / q", "close"),
                ],
            },
            Foreground::Signals => {
                if self.signal_composer.is_some() {
                    return vec![
                        Hint::new("enter", "send"),
                        Hint::new("shift-enter", "new line"),
                        Hint::new("esc", "cancel"),
                    ];
                }
                let mut hints = vec![
                    Hint::new("tab", "focus"),
                    Hint::new(
                        "j/k",
                        match self.signal_focus {
                            SignalFocus::List => "thread",
                            SignalFocus::Detail => "scroll",
                            SignalFocus::Plan => "plan",
                            SignalFocus::Activity => "activity",
                            SignalFocus::Action(_) => "button",
                        },
                    ),
                ];
                hints.push(Hint::new("n", "message agent"));
                if self.signal_focus == SignalFocus::Detail {
                    hints.push(Hint::new("y", "copy message"));
                }
                if let Some(thread) = self.signal_thread() {
                    if let SignalFocus::Action(index) = self.signal_focus
                        && let Some(action) = self.signal_actions().get(index).copied()
                    {
                        hints.push(Hint::new("enter", action.label()));
                    } else if let Some(action) = primary_action(thread) {
                        hints.push(Hint::new("enter", action.label()));
                    }
                    if let Some(action) = secondary_action(thread) {
                        hints.push(Hint::new("ctrl-enter", action.label()));
                    }
                }
                hints.push(Hint::new("pgup/pgdn", "scroll"));
                hints.push(dismiss());
                hints
            }
            // A shell takes every key there is, so it has no `?` to offer. The agent
            // and the sandbox shell are two halves of one take-over, so each hint
            // names the other.
            Foreground::Shell(ShellId::Task(_)) => vec![
                Hint::new("alt-s", "shell"),
                Hint::new("ctrl-q", "back on autopilot"),
            ],
            Foreground::Shell(ShellId::TaskShell(_)) => vec![
                Hint::new("alt-s", "agent"),
                Hint::new("ctrl-q", "back on autopilot"),
            ],
            // Nothing to hand back to: the sandbox goes with the shell.
            Foreground::Shell(ShellId::Credentials(_)) => vec![Hint::new("ctrl-q", "close")],
            Foreground::Tasks => {
                // Only what there is something to do it to. A remote's tasks sit
                // beside this machine's, so a list holding only theirs still has
                // something to select and act on.
                let mut hints = vec![
                    Hint::new("c", "create"),
                    Hint::new("ctrl-l", "link issue"),
                    Hint::new(",", "settings"),
                ];
                // The tabs are always there; the key is worth showing only when
                // there is more than one to move between.
                if self.projects().len() > 1 {
                    hints.push(Hint::new("[ ]", "projects"));
                }
                let (local, remote) = self.visible_counts();
                if local + remote > 0 {
                    hints.extend([
                        Hint::new("enter", "threads"),
                        Hint::new("ctrl-enter", "take over"),
                    ]);
                }
                // Drawn from different places, but acted on the same way: a task is
                // moved across with one key, and deleted by the machine holding it.
                if self.selected_is_remote() {
                    hints.push(Hint::new("d", "delete"));
                    if self
                        .selected_held()
                        .is_some_and(|held| held.summary.tracker_issue.is_some())
                    {
                        hints.push(Hint::new("b", "open issue"));
                    }
                } else {
                    if !self.tasks.is_empty() {
                        hints.push(Hint::new("d", "delete"));
                        // A task goes where its project runs, so the key is worth
                        // showing only when that is a machine which can take it. A
                        // project that runs here has nothing to offer, and the key
                        // would ask a question with one possible answer.
                        if self.can_send_selected() {
                            hints.push(Hint::new("p", "send to remote"));
                        }
                    }
                    if self
                        .selected_task()
                        .is_some_and(|task| task.facts.tracker_issue.is_some())
                    {
                        hints.push(Hint::new("b", "open issue"));
                    }
                }
                hints.push(Hint::new("?", "keys"));
                hints.push(Hint::new("q", "quit"));
                hints
            }
        }
    }

    pub(in crate::tui) fn is_typing(&self) -> bool {
        match &self.foreground {
            Foreground::NewTask(new_task) => new_task.is_typing(),
            Foreground::GetStarted(panel) => panel.is_typing(),
            Foreground::GitIdentity(identity) => identity.is_typing(),
            Foreground::Message(_) => true,
            _ => {
                self.signal_composer.is_some()
                    || self
                        .panel
                        .form
                        .as_ref()
                        .is_some_and(|form| form.focus.takes_text())
            }
        }
    }

    pub(in crate::tui) fn is_gliding(&self) -> bool {
        self.tasks_list.is_gliding()
            || self.signals_list.is_gliding()
            || self.panel.list.is_gliding()
    }

    pub(in crate::tui) fn glide_lists(&mut self, elapsed: Duration) -> bool {
        self.tasks_list.glide(elapsed)
            | self.signals_list.glide(elapsed)
            | self.panel.list.glide(elapsed)
    }

    pub(in crate::tui) fn has_modal(&self) -> bool {
        // A transfer on its way, or a message, is a modal over the list and
        // leaves like one.
        self.transfer.is_some()
            || self.alert.is_some()
            || matches!(
                self.foreground,
                Foreground::Keys(_)
                    | Foreground::Confirm(_)
                    | Foreground::SendTask(_)
                    | Foreground::NewTask(_)
                    | Foreground::Panel
                    | Foreground::Setup(_)
                    | Foreground::SignIn(_)
                    | Foreground::Signals
                    | Foreground::Message(_)
            )
    }

    pub(in crate::tui) fn wants_mouse(&self) -> bool {
        self.open_shell().is_some()
            || matches!(
                self.foreground,
                Foreground::Keys(_)
                    | Foreground::Setup(_)
                    | Foreground::Signals
                    | Foreground::Panel
                    | Foreground::NewTask(_)
                    | Foreground::Tasks
            )
    }

    pub(in crate::tui) fn scroll_mouse(&mut self, mouse: MouseEvent) {
        let up = match mouse.kind {
            MouseEventKind::ScrollUp => true,
            MouseEventKind::ScrollDown => false,
            _ => return,
        };
        // A wheel notch is three rows.
        const ROWS: u16 = 3;
        let step = |scroll: u16| {
            if up {
                scroll.saturating_sub(ROWS)
            } else {
                scroll.saturating_add(ROWS)
            }
        };
        match &mut self.foreground {
            Foreground::Keys(_) => self.help_scroll = step(self.help_scroll),
            Foreground::Setup(setup) => setup.wheel(up, ROWS),
            Foreground::Panel => self.panel.detail_scroll = step(self.panel.detail_scroll),
            _ => {}
        }
    }
}
