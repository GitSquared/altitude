use std::{io, mem};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use tokio::sync::mpsc;

use crate::{
    isolation::IsolationEngine,
    task::TaskService,
    tui::keys::{is_remove, is_secondary},
    tui::popups,
    tui::screens,
};

use popups::confirm::PendingConfirm;
use popups::new_task::NewTask;
use popups::send_task::SendTask;
use screens::get_started::Focus as SetupFocus;

use super::*;

impl App {
    pub(in crate::tui) async fn list_key<E>(
        &mut self,
        key: KeyEvent,
        task_service: &TaskService<E>,
        remote_signals: &mpsc::Sender<RemoteSignalsReply>,
    ) -> io::Result<bool>
    where
        E: IsolationEngine + Clone + 'static,
    {
        // A task belongs to the machine running it, so a key does the same thing
        // wherever the card is drawn and only the trip differs: a task on another
        // machine is reached over the link rather than called directly. Taking one
        // over has to move it here first, and its question says so.
        if is_secondary(key) {
            let asked = self.selected_held().map(|held| {
                let agent = held
                    .summary
                    .agent
                    .map(|kind| kind.to_string().to_ascii_lowercase())
                    .unwrap_or_else(|| "the agent".to_owned());
                (
                    held.summary.id.clone(),
                    held.summary.title.clone(),
                    agent,
                    Some(self.remote_name(&held.address)),
                )
            });
            let asked = asked.or_else(|| {
                self.selected_task().map(|task| {
                    (
                        task.id.clone(),
                        task.title().to_owned(),
                        task.agent_kind
                            .map(|kind| kind.to_string().to_ascii_lowercase())
                            .unwrap_or_else(|| "the agent".to_owned()),
                        None,
                    )
                })
            });
            if let Some((id, title, agent, from)) = asked {
                self.ask_to_take_over(&id, &title, &agent, from);
            }
            return Ok(false);
        }

        if self.selected_is_remote() {
            // The same screen this machine's tasks use, for a task whose actions
            // travel to the machine that owns it.
            if key.code == KeyCode::Enter || key.code == KeyCode::Char('i') {
                self.open_signals(task_service, remote_signals).await;
                return Ok(false);
            }
            // The issue a task is bound to is a page, and it opens the same way
            // whether the task is here or on another machine: through this
            // machine's browser, by the operator's own act.
            if key.code == KeyCode::Char('b') {
                let issue = self
                    .selected_held()
                    .and_then(|held| held.summary.tracker_issue.clone());
                if let Some(issue) = issue {
                    match open_in_browser(&issue.url) {
                        Ok(()) => tracing::info!(%issue.url, "opened a remote task's issue"),
                        Err(error) => {
                            self.problem(format!("Could not open {}: {error}", issue.url))
                        }
                    }
                }
                return Ok(false);
            }
        }

        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            // The tabs are one keystroke away in either direction, and they are
            // the context every other key works in.
            KeyCode::Tab => {
                self.switch_project(true);
                return Ok(false);
            }
            KeyCode::BackTab => {
                self.switch_project(false);
                return Ok(false);
            }
            KeyCode::Char('[') => {
                self.switch_project(false);
                return Ok(false);
            }
            KeyCode::Char(']') => {
                self.switch_project(true);
                return Ok(false);
            }
            KeyCode::Char('c') => self.open_new_task(false),
            // Linking an issue is the other way to start work: the same question,
            // asked of the tracker instead of the operator's prose. A chord,
            // because `l` is navigation wherever there is a column to its right.
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                if self.reload_settings() {
                    let question = NewTask::new(
                        self.projects(),
                        &self.active_project_id(),
                        self.task_hosts(),
                    );
                    self.foreground = Foreground::NewTask(Box::new(question.linking()));
                }
            }
            KeyCode::Char('s') => {
                open_panel(self, task_service).await;
            }
            KeyCode::Char('i') => self.open_signals(task_service, remote_signals).await,
            // j and k walk the cards, the way a list does; the arrows keep the
            // grid's shape, where up and down are a row rather than a card.
            KeyCode::Char('j') => {
                let columns = self.grid_columns();
                self.select_adjacent(1, columns);
            }
            KeyCode::Char('k') => {
                let columns = self.grid_columns();
                self.select_adjacent(-1, columns);
            }
            KeyCode::Down => {
                let columns = self.grid_columns();
                self.select_adjacent(columns as isize, columns);
            }
            KeyCode::Up => {
                let columns = self.grid_columns();
                self.select_adjacent(-(columns as isize), columns);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                let columns = self.grid_columns();
                self.select_adjacent(1, columns);
            }
            KeyCode::Left | KeyCode::Char('h') => {
                let columns = self.grid_columns();
                self.select_adjacent(-1, columns);
            }
            _ if is_remove(key) => {
                // A remote's task is removed the same way. Which machine holds it
                // is where the act happens, not what the act is.
                if let Some(held) = self.selected_held() {
                    let question = format!(
                        "Delete task \"{}\" from {}?",
                        held.summary.title,
                        self.remote_name(&held.address)
                    );
                    self.foreground = Foreground::Confirm(PendingConfirm::removing_remote(
                        held.address.clone(),
                        held.summary.id.clone(),
                        question,
                    ));
                } else if let Some(task) = self.selected_task() {
                    let question = format!("Stop any agents and delete task \"{}\"?", task.title());
                    self.foreground =
                        Foreground::Confirm(PendingConfirm::removing(task.id, question));
                }
            }
            // Enter reads what the task is asking. Taking over has a key of its
            // own, and asks first, because it ends the agent's autonomy.
            KeyCode::Enter => self.open_signals(task_service, remote_signals).await,
            // Moving a task across is one key: which way it goes is decided by
            // where the task already is, so there is nothing to remember.
            KeyCode::Char('p') => {
                let Some(task) = self.selected_task() else {
                    return Ok(false);
                };
                if task.facts.is_borrowed() {
                    // The way back is leaving the take over, not a key: this task is
                    // held here, and holding it is the only reason it is here.
                    let from = task
                        .facts
                        .arrived_from
                        .as_deref()
                        .map(|address| self.remote_name(address))
                        .unwrap_or_default();
                    self.say(format!(
                        "This task is held here from {from} until the take over ends; it goes back then"
                    ));
                    return Ok(false);
                }
                if self.transfer.is_some() {
                    self.say("A task is already on its way; let that finish first");
                    return Ok(false);
                }
                // A task goes where its project runs, and nowhere else. So this
                // either offers that one machine or says why there is no move.
                let Some(project) = task.project().cloned() else {
                    self.say("This task has no recorded project");
                    return Ok(false);
                };
                let hosts = match self.project_machine(&project) {
                    Ok(Some(address)) => self
                        .task_hosts()
                        .into_iter()
                        .filter(|host| host.address.as_deref() == Some(address.as_str()))
                        .collect::<Vec<_>>(),
                    Ok(None) => {
                        self.say(
                            "New tasks for this project start on this machine. Set their machine \
                             in Settings → Projects to delegate them.",
                        );
                        return Ok(false);
                    }
                    Err(reason) => {
                        self.say(reason);
                        return Ok(false);
                    }
                };
                let title = task.title().to_owned();
                let project_name = project.name.clone();
                let agent = task.agent_kind;
                self.foreground = Foreground::SendTask(Box::new(SendTask::new(
                    task.id,
                    title,
                    project_name,
                    agent,
                    hosts,
                )));
            }
            // The issue a task is bound to is a page, so it opens the same way a
            // sandbox's link does: through this machine's browser, by the
            // operator's own act.
            KeyCode::Char('b') => {
                if let Some(issue) = self
                    .selected_task()
                    .and_then(|task| task.facts.tracker_issue)
                {
                    match open_in_browser(&issue.url) {
                        Ok(()) => tracing::info!(%issue.url, "opened a task's issue"),
                        Err(error) => {
                            self.problem(format!("Could not open {}: {error}", issue.url))
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(false)
    }

    pub(in crate::tui) fn started_mouse(&mut self, mouse: MouseEvent) {
        let Foreground::GetStarted(mut panel) = mem::take(&mut self.foreground) else {
            return;
        };
        let Some(browser) = panel.repositories.as_mut() else {
            self.foreground = Foreground::GetStarted(panel);
            return;
        };
        if let Some(repository) = browser.mouse(mouse) {
            panel.repositories = None;
            self.bind_project_repository(&repository.url);
            panel.choose_repository(&repository);
            panel.focus = SetupFocus::Ready;
        }
        self.foreground = Foreground::GetStarted(panel);
    }
}
