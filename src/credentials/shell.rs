//! The terminal a profile is signed in through.
//!
//! Not a task: nothing records it, nothing travels from it, and it is thrown away when
//! the shell ends. It exists because a login is a command a CLI runs and a terminal to
//! run it in, and Altitude never handles the credential itself — it runs the profile's
//! own sign-in command in a sandbox that mounts what a task mounts, so the CLI writes
//! where a task will read.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::mpsc;

use crate::{
    agent::tools::AgentToolRegistry,
    credentials::{Credentials, CredentialsError},
    isolation::{
        CREDENTIAL_PREFIX, Command, CommandEvent, CredentialSandbox, Environment,
        EnvironmentProgress, EnvironmentProgressSink, IsolationEngine, IsolationError,
    },
};

/// The name a shell opened to set up a profile's login has.
pub fn credential_shell_id() -> String {
    // Keep the local socket path below macOS's Unix socket limit.
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    format!("{CREDENTIAL_PREFIX}shell-{}", &nonce[..16])
}

/// What came back from a credential shell.
pub enum CredentialShellEvent {
    Preparing {
        id: String,
        label: String,
    },
    /// A terminal is running in a sandbox, and here it is.
    Opened(CredentialShell),
    /// No shell: the sandbox could not be made.
    Unavailable {
        id: String,
        reason: String,
    },
    /// What the shell printed, and which sandbox printed it, because that is where the
    /// terminal drawing it is kept.
    Output {
        id: String,
        bytes: Vec<u8>,
    },
    /// The shell ended, with the status it ended with. Its sandbox is still there
    /// until it is closed.
    Exited {
        id: String,
        code: i32,
    },
    /// The shell broke.
    Failed {
        id: String,
        error: String,
    },
}

impl CredentialShellEvent {
    /// The sandbox this came from, because that is where the terminal drawing it is
    /// kept.
    pub fn id(&self) -> &str {
        match self {
            Self::Opened(shell) => shell.id(),
            Self::Preparing { id, .. }
            | Self::Unavailable { id, .. }
            | Self::Output { id, .. }
            | Self::Exited { id, .. }
            | Self::Failed { id, .. } => id,
        }
    }

    /// Whether this is the last thing the shell will say. The first two answer the
    /// question of opening one, and a shell that never opened says nothing else.
    pub fn is_last(&self) -> bool {
        matches!(self, Self::Exited { .. } | Self::Failed { .. })
    }
}

/// Says that a credential shell being prepared is no longer wanted.
///
/// Opening one can wait on an environment build, and the operator can leave the page
/// long before it opens. Without this, the build would finish into a sandbox nobody can
/// reach and leave it behind.
#[derive(Clone, Default)]
pub struct CredentialShellCancel(Arc<AtomicBool>);

impl CredentialShellCancel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A credential shell that is running, and the channel its keys go down.
pub struct CredentialShell {
    id: String,
    commands: mpsc::UnboundedSender<Command>,
}

impl CredentialShell {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn input(&self, bytes: Vec<u8>) {
        let _ = self.commands.send(Command::Input(bytes));
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.commands.send(Command::Resize { rows, cols });
    }
}

struct CredentialProgress<'a> {
    id: &'a str,
    events: &'a mpsc::UnboundedSender<CredentialShellEvent>,
}

impl EnvironmentProgressSink for CredentialProgress<'_> {
    fn report(&self, progress: EnvironmentProgress) {
        let _ = self.events.send(CredentialShellEvent::Preparing {
            id: self.id.to_owned(),
            label: format!(
                "{} ({}/{})",
                progress.label,
                progress.completed + 1,
                progress.total
            ),
        });
    }
}

impl<E> Credentials<E>
where
    E: IsolationEngine + Clone + 'static,
{
    /// Open a shell in a throwaway sandbox, and answer with it.
    ///
    /// The sandbox mounts the profile a task mounts, so what is signed in to here is
    /// what a task finds. Its environment is only what the login needs, so the wait is
    /// that CLI's install rather than everything a task runs on, and its snapshot is
    /// named by what it installs so building it leaves the task environment's alone.
    ///
    /// What the shell prints comes back on `events`: a shell outlives no frame, and the
    /// interface draws it while it runs. `cancel` says the shell is no longer wanted
    /// while it is being prepared, in which case nothing is left behind.
    pub async fn open_shell(
        &self,
        id: String,
        environment: Environment,
        profile: &'static str,
        cancel: CredentialShellCancel,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
    ) -> Result<(), CredentialsError> {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let (recipes, snapshot) = self.environment_for(&environment, profile)?;
        self.bind_channel(&id)?;
        let sandbox = CredentialSandbox {
            id: id.clone(),
            environment,
            recipes,
            snapshot,
        };
        let command = self
            .isolation
            .credential_shell(sandbox, profile, &CredentialProgress { id: &id, events })
            .await;
        let command = match command {
            Ok(command) => command,
            Err(error) => {
                self.discard_shell(&id).await;
                return Err(CredentialsError::Isolation(error));
            }
        };
        if cancel.is_cancelled() {
            self.discard_shell(&id).await;
            return Ok(());
        }
        let mut session = match self.isolation.run_command(id.clone(), command).await {
            Ok(session) => session,
            Err(error) => {
                self.discard_shell(&id).await;
                return Err(CredentialsError::Isolation(error));
            }
        };
        if cancel.is_cancelled() {
            self.discard_shell(&id).await;
            return Ok(());
        }
        let opened = CredentialShell {
            id: id.clone(),
            commands: session.commands.clone(),
        };
        if events.send(CredentialShellEvent::Opened(opened)).is_err() {
            self.discard_shell(&id).await;
            return Ok(());
        }
        let events = events.clone();
        let said_by = id.clone();
        tokio::spawn(async move {
            while let Some(event) = session.events.recv().await {
                let event = match event {
                    CommandEvent::Output { bytes, .. } => CredentialShellEvent::Output {
                        id: said_by.clone(),
                        bytes,
                    },
                    CommandEvent::Exited(code) => CredentialShellEvent::Exited {
                        id: said_by.clone(),
                        code,
                    },
                    CommandEvent::Failed(error) => CredentialShellEvent::Failed {
                        id: said_by.clone(),
                        error,
                    },
                };
                let last = event.is_last();
                if events.send(event).is_err() || last {
                    break;
                }
            }
        });
        Ok(())
    }

    /// Take a credential shell's sandbox away.
    ///
    /// The shell is not a task, so there is nothing to keep: what it wrote is in the
    /// profile, which is a host directory, and the sandbox itself is only ever meant to
    /// last as long as the terminal in front of the operator.
    pub async fn close_shell(&self, id: &str) -> Result<(), IsolationError> {
        self.channels.release(id);
        self.isolation.remove(id.to_owned()).await
    }

    /// Bind the MCP channel a credential shell's harness dials.
    ///
    /// The profile a shell mounts names Altitude's bridge, and without a channel the
    /// bridge fails and the harness reports a server that would not start. The endpoint
    /// serves no tools, because a shell sets up a login and has no task for tools to
    /// act on.
    fn bind_channel(&self, id: &str) -> Result<(), CredentialsError> {
        self.channels
            .bind(id, AgentToolRegistry::new())
            .map(|_| ())
            .map_err(|error| CredentialsError::Isolation(IsolationError::new(error.to_string())))
    }

    /// Throw away a shell nobody is waiting for, and the channel that would serve it.
    async fn discard_shell(&self, id: &str) {
        self.channels.release(id);
        if let Err(error) = self.isolation.remove(id.to_owned()).await {
            tracing::debug!(%error, "no credential shell sandbox to remove");
        }
    }
}
