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
    tui::{App, app::Foreground, keys, screens::settings::Target, widgets::chrome},
};

/// A question with two answers, one of which cannot be undone.
pub struct Confirm {
    pub question: String,
    /// The word on the destructive button: delete, remove, discard.
    pub destructive: &'static str,
    /// 0 is cancel, 1 is the destructive button.
    pub button: usize,
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
        }
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
    let popup = chrome::centered(area, 60, 7);
    frame.render_widget(Clear, popup);
    let block = chrome::modal("Are you sure?");
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [question, buttons] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(2)]).areas(inner);
    frame.render_widget(
        Paragraph::new(confirm.question.clone()).wrap(Wrap { trim: true }),
        question,
    );
    let [_, cancel, _, destructive] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(10),
        Constraint::Length(3),
        Constraint::Length(10),
    ])
    .areas(buttons);
    chrome::button(frame, cancel, "cancel", confirm.button == 0, false);
    chrome::button(
        frame,
        destructive,
        confirm.destructive,
        confirm.button == 1,
        true,
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

    /// Confirming stops the agent and gives the operator its terminal.
    pub(in crate::tui) fn taking_over(task_id: String, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "take over"),
            action: ConfirmAction::TakeOver { id: task_id },
        }
    }

    /// Confirming removes a task from the overview and disposes of its capsule.
    pub(in crate::tui) fn removing(task_id: String, question: String) -> Self {
        Self {
            dialog: Confirm::new(question, "remove"),
            action: ConfirmAction::RemoveTask { id: task_id },
        }
    }

    pub(in crate::tui) fn is_settings(&self) -> bool {
        matches!(
            self.action,
            ConfirmAction::DeleteSetting(_) | ConfirmAction::ForgetCredentials(_)
        )
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
    TakeOver { id: String },
    DeleteSetting(Target),
    ForgetCredentials(CredentialTarget),
}

/// Take an answer to the confirmation, and act on it.
pub(in crate::tui) async fn handle_confirm_key<E>(
    app: &mut App,
    terminal: &DefaultTerminal,
    event_loop: &TaskEventLoop<E>,
    key: KeyEvent,
    landings: &mpsc::UnboundedSender<Result<String, String>>,
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
            ConfirmAction::TakeOver { id } => app.take_over(&id, terminal, event_loop, landings),
            ConfirmAction::DeleteSetting(target) => {
                app.foreground = Foreground::Panel;
                app.delete(target).await;
            }
            ConfirmAction::ForgetCredentials(target) => {
                app.forget_credentials(target, event_loop.service()).await
            }
        },
    }
}
