use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use tokio::sync::mpsc;

use crate::{
    isolation::IsolationEngine,
    task::{TaskEventLoop, TaskService},
    tui::keys::{is_dismiss, is_secondary},
    tui::popups,
    tui::screens,
};

use crate::tui::widgets::alert::Alert;
use crate::tui::widgets::list::Event;
use popups::message::{Decision as MessageDecision, MessagePrompt};
use screens::signals::{SignalFocus, SignalHit, primary_action, secondary_action};

use super::*;

impl App {
    pub(in crate::tui) async fn signals_mouse<E>(
        &mut self,
        mouse: MouseEvent,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let target = self.signal_targets.at(mouse.column, mouse.row).copied();
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                match target {
                    Some(SignalHit::List | SignalHit::Thread(_)) => {
                        self.move_signal_cursor(!up);
                    }
                    Some(SignalHit::Plan) => {
                        self.signal_focus = SignalFocus::Plan;
                        self.scroll_plan(if up { -1 } else { 1 });
                    }
                    Some(SignalHit::Activity) => {
                        self.signal_focus = SignalFocus::Activity;
                        self.scroll_activity(if up { -1 } else { 1 });
                    }
                    Some(
                        SignalHit::Action(_) | SignalHit::ComposerSend | SignalHit::ComposerCancel,
                    ) => {}
                    _ => {
                        self.signal_focus = SignalFocus::Detail;
                        self.scroll_signal(if up { -3 } else { 3 });
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(target) = target else {
                    return;
                };
                match target {
                    SignalHit::List => self.signal_focus = SignalFocus::List,
                    SignalHit::Thread(index) => {
                        self.signals_list.select(index);
                        self.signal_focus = SignalFocus::List;
                        self.reset_signal_message();
                    }
                    SignalHit::Detail => self.signal_focus = SignalFocus::Detail,
                    SignalHit::Plan => self.signal_focus = SignalFocus::Plan,
                    SignalHit::Activity => self.signal_focus = SignalFocus::Activity,
                    SignalHit::Message(index) => {
                        self.signal_focus = SignalFocus::Detail;
                        self.signal_message_cursor = index;
                    }
                    SignalHit::Action(index) => {
                        self.signal_focus = SignalFocus::Action(index);
                        if let Some(action) = self.signal_actions().get(index).copied() {
                            self.act_on_signal(action, task_service, remote_signals, event_loop)
                                .await;
                        }
                    }
                    SignalHit::ComposerSend => {
                        self.composer_key(
                            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                            event_loop,
                            remote_signals,
                        );
                    }
                    SignalHit::ComposerCancel => self.signal_composer = None,
                }
            }
            _ => {}
        }
    }

    pub(in crate::tui) async fn message_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let (task_id, decision) = {
            let Foreground::Message(prompt) = &mut self.foreground else {
                return;
            };
            (prompt.task_id().to_owned(), prompt.handle_key(key))
        };
        match decision {
            Some(MessageDecision::Cancel) => self.foreground = Foreground::Signals,
            Some(MessageDecision::Send(body)) => {
                if let Some(remote) = self.remote_signals.clone() {
                    self.act_on_remote_input(remote, body, remote_signals);
                    self.foreground = Foreground::Signals;
                } else {
                    match event_loop.express_input(&task_id, body).await {
                        Ok(()) => {
                            self.foreground = Foreground::Signals;
                            self.alert = Some(Alert::note("Message sent to the agent."));
                        }
                        Err(error) => self.problem(error.to_string()),
                    }
                }
            }
            None => {}
        }
    }

    pub(in crate::tui) async fn signals_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        if self.signal_composer.is_some() {
            self.composer_key(key, event_loop, remote_signals);
            return;
        }
        // Tab switches the screen's tabs where it has them; the horizontal
        // arrows and h/l move between the parts of the screen. j/k and the
        // vertical arrows move inside the part that has the keys.
        // In the history, moving and folding are the list's own.
        if self.signal_focus == SignalFocus::List
            && let Some(event) = self.signals_list.key(key)
        {
            match event {
                Event::AtStart => {}
                Event::Moved => self.reset_signal_message(),
                Event::AtEnd | Event::Folded { .. } => {}
            }
            return;
        }
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            if self.signal_plan_tabs {
                self.signal_plan_open = !self.signal_plan_open;
            }
            return;
        }
        if matches!(
            key.code,
            KeyCode::Right | KeyCode::Left | KeyCode::Char('l') | KeyCode::Char('h')
        ) {
            let forward = matches!(key.code, KeyCode::Right | KeyCode::Char('l'));
            self.move_signal_focus(forward);
            return;
        }
        match key.code {
            KeyCode::PageDown => match self.signal_focus {
                SignalFocus::List => {
                    self.page_signal_cursor(true);
                }
                SignalFocus::Plan => self.page_plan(false),
                SignalFocus::Activity => self.page_activity(false),
                _ => self.page_signal(false),
            },
            KeyCode::PageUp => match self.signal_focus {
                SignalFocus::List => {
                    self.page_signal_cursor(false);
                }
                SignalFocus::Plan => self.page_plan(true),
                SignalFocus::Activity => self.page_activity(true),
                _ => self.page_signal(true),
            },
            KeyCode::Home => match self.signal_focus {
                SignalFocus::Plan => self.signal_plan_scroll = 0,
                SignalFocus::Activity => self.scroll_activity(-i32::from(u16::MAX)),
                _ => {
                    self.signal_scroll = 0;
                    self.select_signal_message_at_scroll();
                }
            },
            KeyCode::End => match self.signal_focus {
                SignalFocus::Plan => self.signal_plan_scroll = u16::MAX,
                SignalFocus::Activity => self.signal_activity_scroll = u16::MAX,
                _ => {
                    self.signal_scroll = u16::MAX;
                    self.select_signal_message_at_scroll();
                }
            },
            _ if is_dismiss(key) || key.code == KeyCode::Char('q') => self.leave_signals(),
            KeyCode::Down | KeyCode::Char('j') => match self.signal_focus {
                SignalFocus::List => {
                    let len = self.signal_threads.len();
                    if self.signals_list.selected() + 1 < len {
                        self.move_signal_cursor(true);
                    }
                }
                SignalFocus::Detail => self.move_signal_message(true),
                SignalFocus::Plan => self.scroll_plan(1),
                SignalFocus::Activity => self.scroll_activity(1),
                SignalFocus::Action(_) => self.move_signal_focus(true),
            },
            KeyCode::Up | KeyCode::Char('k') => match self.signal_focus {
                SignalFocus::List => self.move_signal_cursor(false),
                SignalFocus::Detail => self.move_signal_message(false),
                SignalFocus::Plan => self.scroll_plan(-1),
                SignalFocus::Activity => self.scroll_activity(-1),
                SignalFocus::Action(_) => self.move_signal_focus(false),
            },
            KeyCode::Char('y') if self.signal_focus == SignalFocus::Detail => {
                self.copy_signal_message();
            }
            KeyCode::Char('p') => self.signal_plan_open = !self.signal_plan_open,
            KeyCode::Char('n') if self.remote_signals.is_none() => {
                if let Some(task_id) = self.signal_task_id.clone() {
                    let agent = self.signal_agent_label();
                    self.foreground =
                        Foreground::Message(Box::new(MessagePrompt::new(task_id, agent)));
                }
            }
            KeyCode::Enter if is_secondary(key) => {
                if let Some(action) = self.signal_thread().and_then(secondary_action) {
                    self.act_on_signal(action, task_service, remote_signals, event_loop)
                        .await
                }
            }
            KeyCode::Enter => match self.signal_focus {
                SignalFocus::Action(index) => {
                    if let Some(action) = self.signal_actions().get(index).copied() {
                        self.act_on_signal(action, task_service, remote_signals, event_loop)
                            .await
                    }
                }
                SignalFocus::List
                | SignalFocus::Detail
                | SignalFocus::Plan
                | SignalFocus::Activity => {
                    if let Some(action) = self.signal_thread().and_then(primary_action) {
                        self.act_on_signal(action, task_service, remote_signals, event_loop)
                            .await
                    }
                }
            },
            _ => {}
        }
    }
}
