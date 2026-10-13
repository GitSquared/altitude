//! A shell for setting up one profile's credentials.
//!
//! Not a task: nothing records it, nothing travels from it, and it is thrown away
//! when the shell ends. It exists because a login is a command a CLI runs and a
//! terminal to run it in, and Altitude never handles the credential itself — it
//! hands over a shell in the sandbox a task would get, and the CLI writes where
//! the profile is mounted.

use tokio::sync::mpsc;

use crate::isolation::{
    CREDENTIAL_PREFIX, Command, CommandEvent, CredentialReport, Environment, IsolationEngine,
    IsolationError, Unwatched,
};

use crate::{agent::AgentKind, vcs::GitHub};

use super::{TaskService, TaskServiceError};

/// The name a profile's credential shell has.
///
/// Named rather than anonymous because it can be asked for before there is a session
/// to hold: a shell whose opening nobody is waiting for any more still has to be
/// reachable to be thrown away.
pub fn credential_shell_id(profile: &str) -> String {
    format!("{CREDENTIAL_PREFIX}shell-{}", profile.replace('/', "-"))
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
    /// A terminal is running in a sandbox, and here it is.
    Opened(CredentialShell),
    /// No shell: the sandbox could not be made.
    Unavailable(String),
    /// What the shell printed, and which sandbox printed it, because that is where
    /// the terminal drawing it is kept.
    Output { id: String, bytes: Vec<u8> },
    /// The shell ended, with the status it ended with. Its sandbox is still there
    /// until it is closed.
    Exited(i32),
    /// The shell broke.
    Failed(String),
}

impl CredentialShellEvent {
    /// Whether this is the last thing the shell will say. The first two answer the
    /// question of opening one, and a shell that never opened says nothing else.
    pub fn is_last(&self) -> bool {
        matches!(self, Self::Exited(_) | Self::Failed(_))
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
    /// the interface draws it while it runs.
    pub async fn open_credential_shell(
        &self,
        environment: Environment,
        profile: &'static str,
        chosen: Option<AgentKind>,
        events: &mpsc::UnboundedSender<CredentialShellEvent>,
    ) -> Result<CredentialShell, TaskServiceError> {
        let id = credential_shell_id(profile);
        let agent = self.credential_agent(profile, chosen);
        let command = self
            .isolation
            .credential_shell(
                id.clone(),
                environment,
                vec![
                    self.agents.engine(agent).environment_recipe(),
                    GitHub::environment_recipe(),
                ],
                profile,
                &Unwatched,
            )
            .await
            .map_err(TaskServiceError::Isolation)?;
        let mut session = self
            .isolation
            .run_command(id.clone(), command)
            .await
            .map_err(TaskServiceError::Isolation)?;
        let events = events.clone();
        let said_by = id.clone();
        tokio::spawn(async move {
            while let Some(event) = session.events.recv().await {
                let event = match event {
                    CommandEvent::Output { bytes, .. } => CredentialShellEvent::Output {
                        id: said_by.clone(),
                        bytes,
                    },
                    CommandEvent::Exited(code) => CredentialShellEvent::Exited(code),
                    CommandEvent::Failed(error) => CredentialShellEvent::Failed(error),
                };
                let last = event.is_last();
                if events.send(event).is_err() || last {
                    break;
                }
            }
        });
        Ok(CredentialShell {
            id,
            commands: session.commands,
        })
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
        let recipes = vec![
            self.agents
                .engine(self.credential_agent(profile, chosen))
                .environment_recipe(),
            GitHub::environment_recipe(),
        ];
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
        self.isolation.remove(id.to_owned()).await
    }
}
