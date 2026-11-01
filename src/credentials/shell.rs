use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::mpsc;

use crate::{
    agent::tools::AgentToolRegistry,
    credentials::{Credentials, CredentialsError},
    isolation::{
        CREDENTIAL_PREFIX, Command, CommandEvent, CredentialSandbox, EnvironmentProgress,
        EnvironmentProgressSink, IsolationEngine, IsolationError,
    },
};

pub fn credential_shell_id() -> String {
    // Keep the local socket path below macOS's Unix socket limit.
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    format!("{CREDENTIAL_PREFIX}shell-{}", &nonce[..16])
}

pub enum CredentialShellEvent {
    Preparing { id: String, label: String },
    Opened(CredentialShell),
    Unavailable { id: String, reason: String },
    Output { id: String, bytes: Vec<u8> },
    Exited { id: String, code: i32 },
    Failed { id: String, error: String },
}

impl CredentialShellEvent {
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

    pub fn is_last(&self) -> bool {
        matches!(self, Self::Exited { .. } | Self::Failed { .. })
    }
}

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
    pub async fn open_shell(
        &self,
        id: String,
        profile: &str,
        cancel: CredentialShellCancel,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
    ) -> Result<(), CredentialsError> {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let (recipes, snapshot) = self.credential_build(profile)?;
        self.bind_channel(&id)?;
        // The relay is this sandbox's loopback on this machine's: a sign-in that
        // answers in a browser needs its callback to arrive here.
        let vsock = match self.relays.bind(&id) {
            Ok(route) => vec![route],
            Err(error) => {
                self.channels.release(&id);
                return Err(CredentialsError::Isolation(IsolationError::new(
                    error.to_string(),
                )));
            }
        };
        let sandbox = CredentialSandbox {
            id: id.clone(),
            recipes,
            snapshot,
            vsock,
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

    pub async fn close_shell(&self, id: &str) -> Result<(), IsolationError> {
        self.channels.release(id);
        self.relays.release(id);
        self.isolation.remove(id.to_owned()).await
    }

    fn bind_channel(&self, id: &str) -> Result<(), CredentialsError> {
        self.channels
            .bind(id, AgentToolRegistry::new())
            .map(|_| ())
            .map_err(|error| CredentialsError::Isolation(IsolationError::new(error.to_string())))
    }

    async fn discard_shell(&self, id: &str) {
        self.channels.release(id);
        self.relays.release(id);
        if let Err(error) = self.isolation.remove(id.to_owned()).await {
            tracing::debug!(%error, "no credential shell sandbox to remove");
        }
    }
}
