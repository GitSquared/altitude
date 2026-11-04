use std::mem;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use tokio::sync::mpsc;

use crate::{
    isolation::IsolationEngine,
    task::{TaskEventLoop, TaskService},
    tui::keys::is_dismiss,
};

use super::*;

impl App {
    pub(in crate::tui) async fn new_task_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
        delegations: &mpsc::Sender<TaskDelegationReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::NewTask(mut question) = mem::take(&mut self.foreground) else {
            return;
        };
        let answer = question.handle_key(key);
        self.resolve_new_task(
            question,
            answer,
            event_loop,
            task_service,
            issue_reads,
            delegations,
        )
        .await;
    }

    pub(in crate::tui) async fn new_task_mouse<E>(
        &mut self,
        mouse: MouseEvent,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
        issue_reads: &mpsc::UnboundedSender<IssueReadReply>,
        delegations: &mpsc::Sender<TaskDelegationReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Foreground::NewTask(mut question) = mem::take(&mut self.foreground) else {
            return;
        };
        let answer = question.mouse(mouse, &self.new_task_targets);
        self.resolve_new_task(
            question,
            answer,
            event_loop,
            task_service,
            issue_reads,
            delegations,
        )
        .await;
    }

    pub(in crate::tui) fn composer_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if key.code == KeyCode::Esc {
            self.signal_composer = None;
            return;
        }
        let newline = key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);
        if key.code == KeyCode::Enter && !newline {
            let Some(composer) = self.signal_composer.take() else {
                return;
            };
            if composer.is_blank() {
                self.signal_composer = Some(composer);
                return;
            }
            let answer = composer.body.value().trim().to_owned();
            let task_id = composer.task_id;
            let signal_id = composer.signal_id;
            if self.remote_signals.is_some() {
                self.act_on_remote_signal(
                    signal_id.clone(),
                    RemoteSignalAction::Reply,
                    Some(answer.clone()),
                    remote_signals,
                );
            } else {
                event_loop.reply_signal(task_id, signal_id.clone(), answer.clone());
            }
            self.remember_signal_reply(&signal_id, answer);
            return;
        }
        if let Some(composer) = self.signal_composer.as_mut() {
            composer.body.handle_key(key);
        }
    }

    pub(in crate::tui) fn setup_key(&mut self, key: KeyEvent) {
        if is_dismiss(key) {
            // A provider's guide steps back to the list it came from; everything
            // else closes to the panel.
            let stays = match &mut self.foreground {
                Foreground::Setup(setup) => setup.back(),
                _ => false,
            };
            if !stays {
                let to_card = matches!(
                    &self.foreground,
                    Foreground::Setup(setup) if setup.returns_to_card()
                );
                if to_card {
                    self.foreground = Foreground::GetStarted(self.started_panel());
                } else {
                    self.foreground = Foreground::Panel;
                }
                self.alert = None;
            }
        } else if let Some(text) = match &self.foreground {
            Foreground::Setup(setup) if key.code == KeyCode::Char('c') => {
                setup.copy_text().map(str::to_owned)
            }
            _ => None,
        } {
            match copy_to_clipboard(text) {
                Ok(()) => self.say("Public key copied"),
                Err(error) => self.problem(format!("Could not copy the key: {error}")),
            }
        } else if let Foreground::Setup(setup) = &mut self.foreground {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') if setup.is_picker() => {
                    setup.move_selection(true)
                }
                KeyCode::Up | KeyCode::Char('k') if setup.is_picker() => {
                    setup.move_selection(false)
                }
                KeyCode::Enter if setup.is_picker() => setup.open_selected(),
                _ => setup.scroll(key),
            }
        }
    }

    pub(in crate::tui) fn help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | '?') => self.dismiss_keys(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = self.help_scroll.saturating_add(1)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = self.help_scroll.saturating_sub(1)
            }
            KeyCode::PageDown => self.help_scroll = self.help_scroll.saturating_add(12),
            KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(12),
            KeyCode::Home => self.help_scroll = 0,
            KeyCode::End => self.help_scroll = u16::MAX,
            _ => {}
        }
    }
}
