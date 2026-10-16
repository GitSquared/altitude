//! A shell for setting up one profile's credentials.
//!
//! Not a task: nothing records it, nothing travels from it, and it is thrown away
//! when the shell ends. It exists because a login is a command a CLI runs and a
//! terminal to run it in, and Altitude never handles the credential itself — it
//! hands over a shell in the sandbox a task would get, and the CLI writes where
//! the profile is mounted.

use std::{
    fs, io,
    path::PathBuf,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
};

use tokio::{net::UnixListener, sync::mpsc};

use crate::isolation::{
    CREDENTIAL_PREFIX, Command, CommandEvent, CredentialReport, Environment, EnvironmentProgress,
    EnvironmentProgressSink, IsolationEngine, IsolationError, Unwatched, private_directory,
};

use crate::{
    agent::{AgentKind, mcp, tools::AgentToolRegistry},
    vcs,
};

use super::{AgentChannel, TaskService, TaskServiceError};

pub fn credential_shell_id() -> String {
    // Keep the local socket path below macOS's Unix socket limit.
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    format!("{CREDENTIAL_PREFIX}shell-{}", &nonce[..16])
}

/// The name the sandbox a profile's login is checked in has.
///
/// A check and a shell can be asked for at the same time — the list asks about every
/// profile as it is shown, and the operator can open one while an answer is still
/// coming — so they cannot share a name.
fn credential_check_id(profile: &str) -> String {
    format!("{CREDENTIAL_PREFIX}check-{}", profile.replace('/', "-"))
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
    /// What the shell printed, and which sandbox printed it, because that is where
    /// the terminal drawing it is kept.
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
    /// Whether this is the last thing the shell will say. The first two answer the
    /// question of opening one, and a shell that never opened says nothing else.
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

/// Says that a credential shell being prepared is no longer wanted.
///
/// Opening one can wait on an environment build, and the operator can leave the page
/// long before it opens. Without this, the build would finish into a sandbox nobody
/// can reach and leave it behind.
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

impl<E> TaskService<E>
where
    E: IsolationEngine + Clone + 'static,
{
    /// Open a shell in a throwaway sandbox, and answer with it.
    ///
    /// The environment is the one a task driven by `agent` would get, and the sandbox
    /// mounts exactly what such a task mounts, so what is signed in to here is what a
    /// task finds. It has to be the same environment for the same reason a task does:
    /// a different set of setup steps is a different snapshot, and building one would
    /// throw away the one tasks are using.
    ///
    /// What the shell prints comes back on `events`: a shell outlives no frame, and
    /// the interface draws it while it runs. `cancel` says the shell is no longer
    /// wanted while it is being prepared, in which case nothing is left behind.
    pub async fn open_credential_shell(
        &self,
        id: String,
        environment: Environment,
        profile: &'static str,
        chosen: Option<AgentKind>,
        cancel: CredentialShellCancel,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
    ) -> Result<(), TaskServiceError> {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let agent = self.credential_agent(profile, chosen);
        self.bind_credential_channel(&id)?;
        let command = self
            .isolation
            .credential_shell(
                id.clone(),
                environment,
                [
                    vec![self.agents.engine(agent).environment_recipe()],
                    vcs::environment_recipes(),
                ]
                .concat(),
                profile,
                &CredentialProgress { id: &id, events },
            )
            .await;
        let command = match command {
            Ok(command) => command,
            Err(error) => {
                self.discard_credential_shell(&id).await;
                return Err(TaskServiceError::Isolation(error));
            }
        };
        if cancel.is_cancelled() {
            self.discard_credential_shell(&id).await;
            return Ok(());
        }
        let mut session = match self.isolation.run_command(id.clone(), command).await {
            Ok(session) => session,
            Err(error) => {
                self.discard_credential_shell(&id).await;
                return Err(TaskServiceError::Isolation(error));
            }
        };
        if cancel.is_cancelled() {
            self.discard_credential_shell(&id).await;
            return Ok(());
        }
        let opened = CredentialShell {
            id: id.clone(),
            commands: session.commands.clone(),
        };
        if events.send(CredentialShellEvent::Opened(opened)).is_err() {
            self.discard_credential_shell(&id).await;
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

    /// Bind the MCP channel a credential shell's harness dials.
    ///
    /// The profile a shell mounts names Altitude's bridge, and without a channel the
    /// bridge fails and the harness reports a server that would not start. The endpoint
    /// serves no tools, because a shell sets up a login and has no task for tools to act
    /// on.
    ///
    fn bind_credential_channel(&self, id: &str) -> Result<(), TaskServiceError> {
        self.release_agent_channel(id);
        private_directory(&self.agent_channel_dir)
            .map_err(|error| TaskServiceError::Isolation(IsolationError::new(error.to_string())))?;
        let path = self.agent_channel_dir.join(format!("{id}.sock"));
        let listener = UnixListener::bind(&path)
            .map_err(|error| TaskServiceError::Isolation(IsolationError::new(error.to_string())))?;
        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(async move {
                            let (reader, writer) = stream.into_split();
                            if let Err(error) =
                                mcp::serve_agent(AgentToolRegistry::new(), reader, writer).await
                            {
                                tracing::debug!(%error, "a credential shell's MCP session ended");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "a credential shell's MCP channel stopped listening");
                        break;
                    }
                }
            }
        });
        self.agent_channels
            .lock()
            .expect("agent channel lock poisoned")
            .insert(id.to_owned(), AgentChannel { handle, path });
        Ok(())
    }

    /// Throw away a shell nobody is waiting for, and the channel that would serve it.
    async fn discard_credential_shell(&self, id: &str) {
        self.release_agent_channel(id);
        if let Err(error) = self.isolation.remove(id.to_owned()).await {
            tracing::debug!(%error, "no credential shell sandbox to remove");
        }
    }

    /// The agent whose environment a credential shell for this profile runs in.
    ///
    /// A profile that belongs to an agent gets that agent's environment, because
    /// signing in needs the command its recipe installs. GitHub is signed in to with a
    /// login of its own, so it gets whichever agent tasks are driven by — which is the
    /// environment most likely already built, and the one the next task will use.
    fn credential_agent(&self, profile: &str, chosen: Option<AgentKind>) -> AgentKind {
        [AgentKind::Claude, AgentKind::Pi]
            .into_iter()
            .find(|kind| self.agents.engine(*kind).environment_recipe().profile == profile)
            .or(chosen)
            .unwrap_or_default()
    }

    /// What a profile's own check says about it, or nothing when it cannot be asked
    /// here.
    ///
    /// The check runs in the environment a credential shell for that profile would
    /// use, so a report is what pressing enter would find. An environment that is not
    /// built here is answered with nothing rather than built: a page is not the place
    /// for minutes of work, and opening the shell is what builds it.
    pub async fn credential_report(
        &self,
        environment: Environment,
        profile: &'static str,
        chosen: Option<AgentKind>,
    ) -> Result<Option<CredentialReport>, TaskServiceError> {
        let mut recipes = vec![
            self.agents
                .engine(self.credential_agent(profile, chosen))
                .environment_recipe(),
        ];
        recipes.extend(vcs::environment_recipes());
        let Some(command) = recipes
            .iter()
            .find(|recipe| recipe.profile == profile)
            .map(|recipe| recipe.status_command.to_owned())
        else {
            return Ok(None);
        };
        if !self
            .isolation
            .environment_built(environment.clone(), recipes.clone())
        {
            return Ok(None);
        }
        self.isolation
            .credential_report(
                credential_check_id(profile),
                environment,
                recipes,
                command,
                &Unwatched,
            )
            .await
            .map(Some)
            .map_err(TaskServiceError::Isolation)
    }

    /// Take a credential shell's sandbox away.
    ///
    /// The shell is not a task, so there is nothing to keep: what it wrote is in the
    /// profile, which is a host directory, and the sandbox itself is only ever meant
    /// to last as long as the terminal in front of the operator.
    pub async fn close_credential_shell(&self, id: &str) -> Result<(), IsolationError> {
        self.release_agent_channel(id);
        self.isolation.remove(id.to_owned()).await
    }

    /// Forget a mounted profile's saved login, leaving everything else in it.
    ///
    /// Only the files a profile's CLI writes a sign-in to are removed: its
    /// settings, models, managed MCP files, and the environment it was built in
    /// stay. The Linear tracker is not a mounted profile; its sign-in is
    /// forgotten through the tracker provider's `clear_credentials`.
    pub async fn forget_credentials(&self, profile: &str) -> Result<(), TaskServiceError> {
        let files = credential_files(profile).ok_or_else(|| {
            TaskServiceError::Credentials(format!(
                "no saved login is known for the {profile} profile"
            ))
        })?;
        let directory = self.profile_directory(profile);
        for file in files {
            let path = directory.join(*file);
            match fs::remove_file(&path) {
                Ok(()) => {}
                // Already signed out is the state the operator asked for.
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(TaskServiceError::Credentials(format!(
                        "could not remove the saved login at {}: {error}",
                        path.display()
                    )));
                }
            }
        }
        Ok(())
    }

    /// Where a mounted profile keeps its files on this host.
    ///
    /// The same `data_dir/<profile>/profile` a recipe mounts, derived from the
    /// snapshots directory rather than kept as a second copy of the data
    /// directory that could disagree with it.
    fn profile_directory(&self, profile: &str) -> PathBuf {
        self.snapshots_dir
            .parent()
            .expect("the snapshots directory is inside the data directory")
            .join(profile)
            .join("profile")
    }
}

/// The login files a profile's CLI writes, relative to its host directory.
///
/// Only a sign-in: settings, models, managed MCP files, and environment setup
/// are not credentials and stay. A profile absent here has no login Altitude
/// knows how to remove.
fn credential_files(profile: &str) -> Option<&'static [&'static str]> {
    match profile {
        // Pi keeps one credential per provider here, and nothing else.
        "agents/pi" => Some(&["auth.json"]),
        // Claude Code's own login file; its settings and MCP files stay.
        "agents/claude" => Some(&[".credentials.json"]),
        // The GitHub CLI's host sign-in. The git identity, SSH keys, and signing
        // keys are separate material and are deliberately not touched.
        "vcs/github" => Some(&["gh/hosts.yml"]),
        _ => None,
    }
}
