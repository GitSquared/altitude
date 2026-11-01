use crossterm::event::KeyEvent;
use ratatui::{DefaultTerminal, layout::Rect};

use crate::isolation::IsolationEngine;
use crate::task::{TaskEventLoop, TaskService};
use crate::tui::screens;
use crate::tui::widgets::chrome;

use super::{
    App, CredentialShellState, Foreground, GuestRequest, ShellId, copy_to_clipboard,
    open_in_browser,
};
use crate::tui::links::Link;

impl App {
    pub(in crate::tui) fn send_shell_input<E>(
        &self,
        id: &ShellId,
        bytes: Vec<u8>,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        match id {
            ShellId::Task(task_id) => event_loop.send_terminal_input(task_id, bytes),
            ShellId::Credentials(_) => {
                if let CredentialShellState::Open(shell) = &self.credential_shell {
                    shell.input(bytes);
                }
            }
        }
    }

    pub(in crate::tui) fn resize_shell<E>(
        &mut self,
        rows: u16,
        cols: u16,
        event_loop: &TaskEventLoop<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(id) = self.open_shell().cloned() else {
            return;
        };
        if let Some(shell) = self.shells.get_mut(&id) {
            shell.resize(rows, cols);
        }
        match &id {
            ShellId::Task(task_id) => event_loop.resize_terminal(task_id, rows, cols),
            ShellId::Credentials(_) => {
                if let CredentialShellState::Open(shell) = &self.credential_shell {
                    shell.resize(rows, cols);
                }
            }
        }
    }

    pub(in crate::tui) fn hide_shell(&mut self) {
        tracing::info!(shell = ?self.open_shell(), "detaching terminal");
        self.foreground = Foreground::Tasks;
    }

    pub(in crate::tui) fn remove_shell(&mut self, task_id: &str) {
        let id = ShellId::Task(task_id.to_owned());
        self.shells.remove(&id);
        if self.open_shell() == Some(&id) {
            self.foreground = Foreground::Tasks;
        }
    }

    pub(in crate::tui) async fn shell_key<E>(
        &mut self,
        key: KeyEvent,
        event_loop: &TaskEventLoop<E>,
        task_service: &TaskService<E>,
    ) where
        E: IsolationEngine + Clone + 'static,
    {
        let Some(id) = self.open_shell().cloned() else {
            return;
        };
        if screens::shell::is_exit_key(key) {
            match &id {
                ShellId::Task(task_id) => {
                    // A task that was carried here belongs to the machine it came
                    // from: leaving it hands it back rather than piloting it here.
                    let borrowed = self
                        .tasks
                        .iter()
                        .find(|task| task.id == *task_id)
                        .filter(|task| task.facts.arrived_from.is_some())
                        .cloned();
                    match borrowed {
                        Some(task) => event_loop.hand_back(task),
                        None => {
                            if !event_loop.return_to_autopilot(task_id) {
                                self.say(
                                    "This task is no longer running; control stayed with autopilot",
                                );
                            }
                        }
                    }
                    self.hide_shell();
                }
                ShellId::Credentials(_) => self.close_credential_shell(task_service).await,
            }
            return;
        }
        let bytes = self
            .shells
            .get_mut(&id)
            .and_then(|shell| shell.key_bytes(key));
        if let Some(bytes) = bytes {
            self.send_shell_input(&id, bytes, event_loop);
        }
    }

    pub(in crate::tui) fn shell_size(&mut self, terminal: &DefaultTerminal) -> Option<(u16, u16)> {
        match terminal.size() {
            Ok(size) => Some(screens::shell::size(self.shell_area(size.into()))),
            Err(error) => {
                self.problem(error.to_string());
                None
            }
        }
    }

    pub(in crate::tui) fn shell_area(&self, area: Rect) -> Rect {
        chrome::content_area(area, &self.hints(), self.keyboard_hints())
    }

    pub(in crate::tui) fn shell_has_active_notice(&self) -> bool {
        self.open_shell()
            .and_then(|id| self.shells.get(id))
            .is_some_and(|shell| shell.has_active_notice() || shell.has_reachable_ports())
    }

    pub(in crate::tui) fn forward_shell_output(
        &mut self,
        id: &ShellId,
        bytes: Vec<u8>,
    ) -> Vec<Vec<u8>> {
        let mut asked_to_open = None;
        let mut replies = Vec::new();
        {
            let Some(shell) = self.shells.get_mut(id) else {
                return replies;
            };
            for request in shell.process(bytes) {
                match request {
                    GuestRequest::Copy(text) => match copy_to_clipboard(text) {
                        Ok(()) => {
                            shell.show_clipboard_confirmation();
                            tracing::info!("copied terminal text to clipboard");
                        }
                        Err(error) => tracing::error!(%error, "could not copy terminal text"),
                    },
                    GuestRequest::Open(url) => asked_to_open = Some(url),
                    GuestRequest::Reply(reply) => replies.push(reply),
                }
            }
        }
        if let Some(url) = asked_to_open {
            match Link::parse(&url) {
                Some(link) => {
                    // The address is checked into a page; opening it is not a
                    // question the operator asked to be interrupted for.
                    match open_in_browser(link.url()) {
                        Ok(()) => tracing::info!(%url, "opened a sandbox's link"),
                        Err(error) => {
                            tracing::warn!(%error, "could not open a sandbox's link");
                            self.say(format!("Could not open {url}: {error}"));
                        }
                    }
                }
                None => {
                    tracing::debug!(%url, "a sandbox asked to open something that is not a page")
                }
            }
        }
        replies
    }

    pub(in crate::tui) fn port_is_reachable(&mut self, sandbox: &str, port: u16) {
        let id = self
            .tasks
            .iter()
            .find(|task| task.facts.sandbox_id.as_deref() == Some(sandbox))
            .map(|task| ShellId::Task(task.id.clone()))
            .unwrap_or_else(|| ShellId::Credentials(sandbox.to_owned()));
        if let Some(shell) = self.shells.get_mut(&id) {
            shell.show_reachable_port(port);
        }
    }
}
