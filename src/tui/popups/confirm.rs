//! Asking before something is destroyed.
//!
//! One question, two answers, and the same keys as everywhere else: escape
//! cancels, and `d`, delete or backspace go through with it. What it is about to
//! do is the caller's business, so the caller keeps the action and hands this
//! only the words.

use std::mem;

use tokio::sync::mpsc;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Constraint, Layout, Rect},
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{
    isolation::IsolationEngine,
    task::TaskEventLoop,
    tui::{
        App,
        app::{Foreground, RemoteSignalAction, RemoteSignalsReply, RemoveTaskReply},
        keys,
        screens::settings::Target,
        widgets::chrome,
    },
};

/// A question with two answers, one of which cannot be undone.
pub struct Confirm {
    pub question: String,
    /// The word on the destructive button: delete, remove, discard.
    pub destructive: &'static str,
    /// 0 is cancel, 1 is the destructive button.
    pub button: usize,
    /// A short question about a deliberate ending; narrower and spaced as such.
    compact: bool,
}

/// What the operator answered.
pub enum Decision {
    Cancel,
    Confirm,
}

impl Confirm {
    pub fn new(question: impl Into<String>, destructive: &'static str) -> Self {
        Self {
            question: question.into(),
            destructive,
            // Starting on cancel means the key that opens this is never the key
            // that destroys something.
            button: 0,
            compact: false,
        }
    }

    /// Put the keys on the confirming answer. Used only where confirming is the
    /// expected next step, never for deleting something irreversible.
    pub fn starting_on_confirm(mut self) -> Self {
        self.button = 1;
        self
    }

    /// Use the narrow, spaced form for a deliberate ending.
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    /// The keys that work here, for the guidance line.
    pub fn hints(&self) -> Vec<chrome::Hint> {
        vec![
            chrome::Hint::new("tab", "switch"),
            chrome::Hint::new("enter", "the one chosen"),
            chrome::Hint::new("d", "go through"),
            chrome::Hint::new("esc", "cancel"),
        ]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Decision> {
        if keys::is_dismiss(key) {
            return Some(Decision::Cancel);
        }
        // The remove keys mean the same here as everywhere: take it away.
        if keys::is_remove(key) {
            return Some(Decision::Confirm);
        }
        match key.code {
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                self.button = 1 - self.button;
                None
            }
            KeyCode::Enter => Some(if self.button == 1 {
                Decision::Confirm
            } else {
                Decision::Cancel
            }),
            _ => None,
        }
    }
}

pub fn draw(frame: &mut Frame, area: Rect, confirm: &Confirm) -> Rect {
    let width_percent = if confirm.compact { 30 } else { 60 };
    let width = (area.width * width_percent / 100).max(20);
    let text_width = width.saturating_sub(4).max(1);
    let lines = Paragraph::new(confirm.question.clone())
        .wrap(Wrap { trim: true })
        .line_count(text_width);
    let height = u16::try_from(lines).unwrap_or(u16::MAX).saturating_add(6);
    let popup = chrome::centered(area, width_percent, height.min(area.height));
    frame.render_widget(Clear, popup);
    let block = chrome::modal("Are you sure?");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    // The compact ending keeps one empty line above the words and one below
    // them; the actions keep their own two rows under the rule.
    let [_, question, _, actions] = Layout::vertical(if confirm.compact {
        [
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(2),
        ]
    } else {
        [
            Constraint::Length(0),
            Constraint::Min(3),
            Constraint::Length(0),
            Constraint::Length(2),
        ]
    })
    .areas(inner);
    frame.render_widget(
        Paragraph::new(confirm.question.clone()).wrap(Wrap { trim: true }),
        question,
    );
    chrome::actions(
        frame,
        popup,
        actions,
        &[
            chrome::Action::secondary("cancel").focused(confirm.button == 0),
            chrome::Action::primary(confirm.destructive).focused(confirm.button == 1),
        ],
    );
    popup
}

/// A question waiting on an answer, and what the answer would do.
pub(in crate::tui) struct PendingConfirm {
    dialog: Confirm,
    action: ConfirmAction,
}

pub(in crate::tui) enum CredentialTarget {
    Profile(&'static str),
    Linear,
}

impl PendingConfirm {
    pub(in crate::tui) fn forgetting_credentials(
        target: CredentialTarget,
        question: String,
    ) -> Self {
        Self {
            dialog: Confirm::new(question, "delete"),
            action: ConfirmAction::ForgetCredentials(target),
        }
    }

    /// Confirming deletes a row the settings screen lists.
    pub(in crate::tui) fn deleting(target: Target, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "delete"),
            action: ConfirmAction::DeleteSetting(target),
        }
    }

    /// Confirming stops the agent and gives the operator the terminal.
    pub(in crate::tui) fn taking_over(task_id: String, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "take over"),
            action: ConfirmAction::TakeOver { id: task_id },
        }
    }

    /// Confirming drops a task another machine holds, which is that machine's to
    /// dispose of.
    pub(in crate::tui) fn removing_remote(
        address: String,
        task_id: String,
        question: String,
    ) -> Self {
        Self {
            dialog: Confirm::new(question, "drop"),
            action: ConfirmAction::RemoveRemoteTask { address, task_id },
        }
    }

    /// Confirming replaces the key remembered for an address with the one that
    /// answered.
    pub(in crate::tui) fn replacing_key(address: String, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "accept"),
            action: ConfirmAction::ReplaceKey { address },
        }
    }

    /// Confirming unhooks a task from the machine running it: the same transfer as
    /// taking it over, with one answer different.
    pub(in crate::tui) fn pulling(task_id: String, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "unhook"),
            action: ConfirmAction::PullTask { id: task_id },
        }
    }

    /// Confirming removes a task from the overview and disposes of its capsule.
    pub(in crate::tui) fn removing(task_id: String, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "drop"),
            action: ConfirmAction::RemoveTask { id: task_id },
        }
    }

    /// Confirming accepts a wrap-up request: the task closes and its capsule
    /// goes. The only way a task ends, which is why it is asked.
    pub(in crate::tui) fn confirming_signal(
        task_id: String,
        signal_id: String,
        question: String,
    ) -> Self {
        Self {
            dialog: Confirm::new(question, "accept")
                .starting_on_confirm()
                .compact(),
            action: ConfirmAction::ConfirmSignal { task_id, signal_id },
        }
    }

    pub(in crate::tui) fn is_settings(&self) -> bool {
        matches!(
            self.action,
            ConfirmAction::DeleteSetting(_) | ConfirmAction::ForgetCredentials(_)
        )
    }

    pub(in crate::tui) fn is_signal(&self) -> bool {
        matches!(self.action, ConfirmAction::ConfirmSignal { .. })
    }

    pub(in crate::tui) fn draw(&self, frame: &mut Frame, area: Rect) -> Rect {
        draw(frame, area, &self.dialog)
    }

    pub(in crate::tui) fn hints(&self) -> Vec<chrome::Hint> {
        self.dialog.hints()
    }
}

/// What confirming would do. The dialog knows the words; the application knows
/// the consequences.
enum ConfirmAction {
    RemoveTask { id: String },
    RemoveRemoteTask { address: String, task_id: String },
    PullTask { id: String },
    ReplaceKey { address: String },
    TakeOver { id: String },
    DeleteSetting(Target),
    ForgetCredentials(CredentialTarget),
    ConfirmSignal { task_id: String, signal_id: String },
}

/// Take an answer to the confirmation, and act on it.
pub(in crate::tui) async fn handle_confirm_key<E>(
    app: &mut App,
    terminal: &DefaultTerminal,
    event_loop: &TaskEventLoop<E>,
    key: KeyEvent,
    landings: &mpsc::UnboundedSender<Result<String, String>>,
    removals: &mpsc::Sender<RemoveTaskReply>,
    remote_signals: &mpsc::Sender<RemoteSignalsReply>,
) where
    E: IsolationEngine + Clone + 'static,
{
    let Foreground::Confirm(mut pending) = mem::take(&mut app.foreground) else {
        return;
    };
    match pending.dialog.handle_key(key) {
        None => app.foreground = Foreground::Confirm(pending),
        Some(Decision::Cancel) => {
            if pending.is_settings() {
                app.foreground = Foreground::Panel;
            } else if pending.is_signal() {
                app.foreground = Foreground::Signals;
            }
        }
        Some(Decision::Confirm) => match pending.action {
            ConfirmAction::RemoveTask { id } => {
                if let Some(task) = app.tasks.iter().find(|task| task.id == id).cloned() {
                    tracing::info!(task_id = %id, "removing task from overview");
                    app.begin_removing(&id);
                    app.remove_shell(&id);
                    event_loop.remove(task);
                }
            }
            ConfirmAction::RemoveRemoteTask { address, task_id } => {
                app.begin_remove_remote(&address, &task_id, removals);
            }
            ConfirmAction::PullTask { id } => app.pull_task(&id, event_loop, landings),
            ConfirmAction::ReplaceKey { address } => app.accept_remote_key(&address).await,
            ConfirmAction::TakeOver { id } => app.take_over(&id, terminal, event_loop, landings),
            ConfirmAction::DeleteSetting(target) => {
                app.foreground = Foreground::Panel;
                app.delete(target).await;
            }
            ConfirmAction::ForgetCredentials(target) => {
                app.forget_credentials(target, event_loop.service()).await
            }
            ConfirmAction::ConfirmSignal { task_id, signal_id } => {
                app.foreground = Foreground::Signals;
                if app.remote_signals.is_some() {
                    app.act_on_remote_signal(
                        signal_id,
                        RemoteSignalAction::Accept,
                        None,
                        remote_signals,
                    );
                    return;
                }
                match event_loop
                    .service()
                    .confirm_operator_signal(&task_id, &signal_id)
                    .await
                {
                    Ok(task) => {
                        app.replace_task(task);
                        app.reload_signals(event_loop.service()).await;
                        app.select_next_open_thread();
                        app.say("Task completed");
                    }
                    Err(error) => app.problem(error.to_string()),
                }
            }
        },
    }
}
