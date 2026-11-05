use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;

use crate::isolation::{Command, CommandEvent, IsolationEngine};

use super::terminal::CachedTerminalOutput;
use super::{Task, TaskEventLoop, TaskExecutionUpdate};

// A shell the operator opened inside a sandbox they are operating. It lives beside
// the agent's terminal for as long as the take-over does, because the point of it is
// to step out of the conversation for a moment and step back in.
pub(crate) struct TaskShell {
    commands: mpsc::UnboundedSender<Command>,
}

#[derive(Clone, Default)]
pub(crate) struct TaskShells {
    open: Arc<Mutex<HashMap<String, TaskShell>>>,
    output: Arc<Mutex<HashMap<String, CachedTerminalOutput>>>,
}

impl TaskShells {
    pub(super) fn is_open(&self, task_id: &str) -> bool {
        self.open
            .lock()
            .expect("task shell lock poisoned")
            .contains_key(task_id)
    }

    pub(super) fn input(&self, task_id: &str, bytes: Vec<u8>) {
        if let Some(shell) = self
            .open
            .lock()
            .expect("task shell lock poisoned")
            .get(task_id)
        {
            let _ = shell.commands.send(Command::Input(bytes));
        }
    }

    pub(super) fn resize(&self, task_id: &str, rows: u16, cols: u16) {
        if let Some(shell) = self
            .open
            .lock()
            .expect("task shell lock poisoned")
            .get(task_id)
        {
            let _ = shell.commands.send(Command::Resize { rows, cols });
        }
    }

    // Closing the session is what ends the shell: its input closes, bash reads end of
    // file, and the guest process goes with it.
    pub(super) fn close(&self, task_id: &str) {
        self.open
            .lock()
            .expect("task shell lock poisoned")
            .remove(task_id);
        self.output
            .lock()
            .expect("task shell output lock poisoned")
            .remove(task_id);
    }

    pub(super) fn cached(&self, task_id: &str) -> Vec<Vec<u8>> {
        self.output
            .lock()
            .expect("task shell output lock poisoned")
            .get(task_id)
            .map(|output| output.chunks.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn record(&self, task_id: &str, bytes: &[u8]) {
        self.output
            .lock()
            .expect("task shell output lock poisoned")
            .entry(task_id.to_owned())
            .or_default()
            .push(bytes);
    }
}

// Whether the operator's switch landed on a shell that was already there, a new one,
// or nothing at all: the terminal in front has to know, because a second shell keeps
// the first one's screen and a fresh one does not.
pub enum ShellOpening {
    Attached,
    Opened,
    NoSandbox,
}

impl<E: IsolationEngine + Clone + 'static> TaskEventLoop<E> {
    // Opens a shell in the task's sandbox, or attaches to the one already open. What
    // is returned is a session the sandbox keeps alive on its own: nothing here is
    // recorded as a run, because a shell is the operator looking, not the task doing.
    //
    // Only reachable from a take-over, which is what keeps the sandbox awake: the
    // agent's run is still the thing holding it, and the shell ends when that does.
    pub fn open_task_shell(&self, task: Task, rows: u16, cols: u16) -> ShellOpening {
        if self.shells.is_open(&task.id) {
            self.shells.resize(&task.id, rows, cols);
            return ShellOpening::Attached;
        }
        if !task.facts.sandbox_available || task.facts.sandbox_id.is_none() {
            return ShellOpening::NoSandbox;
        }
        let event_loop = self.clone();
        let task_id = task.id.clone();
        tokio::spawn(async move {
            let opening = event_loop.service.start_task_shell(&task).await;
            let mut session = match opening {
                Ok(session) => session,
                Err(error) => {
                    tracing::warn!(task = %task_id, %error, "could not open a shell in the task's sandbox");
                    return;
                }
            };
            // Registered only once it is open: a shell that failed to start must not
            // leave the interface believing one is there to switch to.
            if !event_loop
                .shells
                .register(&task_id, session.commands.clone())
            {
                return;
            }
            let _ = session.commands.send(Command::Resize { rows, cols });
            while let Some(event) = session.events.recv().await {
                match event {
                    CommandEvent::Output { bytes, .. } => {
                        event_loop.shells.record(&task_id, &bytes);
                        let _ = event_loop.events.send(TaskExecutionUpdate::ShellOutput {
                            task_id: task_id.clone(),
                            bytes,
                        });
                    }
                    CommandEvent::Exited(_) => break,
                    CommandEvent::Failed(error) => {
                        tracing::warn!(task = %task_id, %error, "a shell in the sandbox failed");
                        break;
                    }
                }
            }
            event_loop.shells.close(&task_id);
        });
        ShellOpening::Opened
    }

    pub fn send_task_shell_input(&self, task_id: &str, bytes: Vec<u8>) {
        self.shells.input(task_id, bytes);
    }

    pub fn resize_task_shell(&self, task_id: &str, rows: u16, cols: u16) {
        self.shells.resize(task_id, rows, cols);
    }

    pub fn close_task_shell(&self, task_id: &str) {
        self.shells.close(task_id);
    }

    pub fn cached_task_shell_output(&self, task_id: &str) -> Vec<Vec<u8>> {
        self.shells.cached(task_id)
    }
}

impl TaskShells {
    fn register(&self, task_id: &str, commands: mpsc::UnboundedSender<Command>) -> bool {
        self.open
            .lock()
            .expect("task shell lock poisoned")
            .insert(task_id.to_owned(), TaskShell { commands })
            .is_none()
    }
}
