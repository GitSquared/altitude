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
    layout::Rect,
    widgets::{Clear, Paragraph, Wrap},
};

use crate::{
    isolation::IsolationEngine,
    project::ProjectCommits,
    task::TaskEventLoop,
    tui::{
        App,
        app::{Foreground, RemoteSignalAction, RemoteSignalsReply, RemoveTaskReply},
        keys,
        screens::settings::Target,
        widgets::{button, chrome},
    },
};

pub struct Confirm {
    pub question: String,
    pub destructive: &'static str,
    pub button: usize,
}

pub enum Decision {
    Cancel,
    Confirm,
}

impl Confirm {
    pub fn new(question: impl Into<String>, destructive: &'static str) -> Self {
        Self {
            question: question.into(),
            destructive,
            // The keys start on the answer the question is asking for, which is
            // the one on the right. Escape is still the way out, and cancel is
            // one tab away.
            button: 1,
        }
    }

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
    let width_percent = 60;
    let width = (area.width * width_percent / 100).max(20);
    let text_width = width.saturating_sub(4).max(1);
    let rows = Paragraph::new(confirm.question.clone())
        .wrap(Wrap { trim: true })
        .line_count(text_width);
    let height = chrome::modal_height(u16::try_from(rows).unwrap_or(u16::MAX));
    let popup = chrome::centered(area, width_percent, height.min(area.height));
    frame.render_widget(Clear, popup);
    let block = chrome::modal("Are you sure?");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let body = chrome::body(inner);
    frame.render_widget(
        Paragraph::new(confirm.question.clone()).wrap(Wrap { trim: true }),
        body.content,
    );
    button::actions(
        frame,
        popup,
        body.actions,
        &[
            button::Action::secondary("cancel").focused(confirm.button == 0),
            button::Action::primary(confirm.destructive).focused(confirm.button == 1),
        ],
    );
    popup
}

pub(in crate::tui) struct PendingConfirm {
    dialog: Confirm,
    action: ConfirmAction,
}

impl PendingConfirm {
    pub(in crate::tui) fn forgetting_credentials(id: String, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "delete"),
            action: ConfirmAction::ForgetCredentials(id),
        }
    }

    pub(in crate::tui) fn deleting(target: Target, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "delete"),
            action: ConfirmAction::DeleteSetting(target),
        }
    }

    pub(in crate::tui) fn deleting_commit_identity(
        commits: ProjectCommits,
        question: String,
    ) -> Self {
        Self {
            dialog: Confirm::new(question, "delete"),
            action: ConfirmAction::DeleteCommitIdentity(commits),
        }
    }

    pub(in crate::tui) fn taking_over(task_id: String, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "take over"),
            action: ConfirmAction::TakeOver { id: task_id },
        }
    }

    /// Confirming deletes a task another machine holds, which is that machine's
    /// to dispose of.
    pub(in crate::tui) fn removing_remote(
        address: String,
        task_id: String,
        question: String,
    ) -> Self {
        Self {
            dialog: Confirm::new(question, "delete"),
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

    pub(in crate::tui) fn removing(task_id: String, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "delete"),
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
            dialog: Confirm::new(question, "accept"),
            action: ConfirmAction::ConfirmSignal { task_id, signal_id },
        }
    }

    pub(in crate::tui) fn is_settings(&self) -> bool {
        matches!(
            self.action,
            ConfirmAction::DeleteSetting(_)
                | ConfirmAction::DeleteCommitIdentity(_)
                | ConfirmAction::ForgetCredentials(_)
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
    DeleteCommitIdentity(ProjectCommits),
    ForgetCredentials(String),
    ConfirmSignal { task_id: String, signal_id: String },
}

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
    // An answer leaves the operator on the screen the question was asked over. The
    // foreground was taken to get the dialog out of the way, and an action that
    // fails partway — a refusal, a forgotten identity that a project still binds —
    // must say so over that screen rather than over whatever the enum defaults to.
    // An action that means to go somewhere sets the foreground itself.
    app.foreground = if pending.is_settings() {
        Foreground::Panel
    } else if pending.is_signal() {
        Foreground::Signals
    } else {
        Foreground::Tasks
    };
    match pending.dialog.handle_key(key) {
        None => app.foreground = Foreground::Confirm(pending),
        Some(Decision::Cancel) => {}
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
            ConfirmAction::DeleteSetting(target) => app.delete(target).await,
            ConfirmAction::DeleteCommitIdentity(commits) => {
                app.delete_commit_identity(&commits);
            }
            ConfirmAction::ForgetCredentials(target) => {
                app.forget_credentials(target, event_loop.service()).await
            }
            ConfirmAction::ConfirmSignal { task_id, signal_id } => {
                if app.remote_signals.is_some() {
                    app.foreground = Foreground::Signals;
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
                        // Accepting is the end of the task, so there is nothing
                        // left to read here: the list is where the next one is.
                        app.foreground = Foreground::Tasks;
                    }
                    Err(error) => {
                        app.foreground = Foreground::Signals;
                        app.problem(error.to_string());
                    }
                }
            }
        },
    }
}
