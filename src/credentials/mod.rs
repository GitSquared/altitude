//! The logins tasks run under: the shell a profile is signed in through, the check
//! that says whether it worked, and what a saved login is on disk.
//!
//! Its own concern rather than a task's. Nothing here is recorded, travels, or belongs
//! to a capsule, and none of it needs a task to exist: it uses the same profile
//! recipes and isolation a task does, so what it writes is what a task finds. The
//! interface asks it about logins; the task layer asks it for the profiles a task
//! could mount.

mod files;
mod shell;

use std::{error::Error, fmt, path::PathBuf};

pub use shell::{
    CredentialShell, CredentialShellCancel, CredentialShellEvent, credential_shell_id,
};

use crate::{
    agent::{AgentChannels, AgentEngines, AgentKind},
    config::Config,
    isolation::{
        CREDENTIAL_PREFIX, CredentialCheck, CredentialSandbox, Environment, EnvironmentRecipe,
        IsolationEngine, IsolationError, ProfileSummary, Unwatched, mounts,
    },
};

/// What one profile's login came to, and how that was found.
pub struct CredentialAnswer {
    pub signed_in: bool,
    pub output: String,
    /// Whether a profile's own CLI answered, rather than a login file on disk being
    /// found. A file is a login a task would mount; only the CLI can say it still
    /// works.
    pub is_verified: bool,
}

#[derive(Debug)]
pub enum CredentialsError {
    Isolation(IsolationError),
    /// Nothing could be done about this profile, and here is why: no recipe owns it, no
    /// login files are known, or one could not be removed.
    Refused(String),
}

impl fmt::Display for CredentialsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Isolation(error) => error.fmt(formatter),
            Self::Refused(error) => formatter.write_str(error),
        }
    }
}

impl Error for CredentialsError {}

/// The profiles a task mounts, and the sandboxes that sign them in.
///
/// Holds no task state: an isolation engine, the configuration that says where
/// profiles and channels live, and the recipes to mount. That is deliberately less
/// than a task service, so a login can exist without a task and a task need not know
/// how a login is written.
#[derive(Clone)]
pub struct Credentials<E> {
    isolation: E,
    config: Config,
    agents: AgentEngines,
    channels: AgentChannels,
}

impl<E> Credentials<E>
where
    E: IsolationEngine + Clone,
{
    pub fn new(isolation: E, config: Config, agents: AgentEngines) -> Self {
        Self {
            isolation,
            channels: AgentChannels::new(config.agent_channel_dir()),
            config,
            agents,
        }
    }
}

impl<E> Credentials<E>
where
    E: IsolationEngine + Clone + 'static,
{
    /// Every profile Altitude can mount, so an interface can say what logins exist.
    ///
    /// The profiles a task mounts depend on the agent it runs; this is the whole of
    /// them, which is what a login list is about.
    pub fn profiles(&self) -> Vec<ProfileSummary> {
        mounts::every(&self.agents)
            .into_iter()
            .map(|recipe| ProfileSummary {
                profile: recipe.profile,
                display: recipe.display,
                sign_in: recipe.sign_in,
                sign_in_command: recipe.sign_in_command,
            })
            .collect()
    }

    /// Whether a sign-in for this profile would find its environment ready.
    ///
    /// Asked before a shell is opened, so a wait is only shown when there is one. The
    /// answer is the same lookup the shell makes, so a box that flashes for nothing
    /// cannot happen.
    pub fn environment_built(&self, environment: Environment, profile: &str) -> bool {
        self.environment_for(&environment, profile)
            .is_ok_and(|(_, snapshot)| snapshot.is_some())
    }

    /// What profiles' own checks say about them.
    ///
    /// The checks run in the environment their credential shells would use, so an
    /// answer is what pressing enter would find, and they read a login from the profile
    /// rather than a task's state. A built snapshot can answer; only with none is
    /// building necessary, and a page is not the place for minutes of work, so the login
    /// files answer instead. A page that said "not signed in" because nothing had been
    /// built yet sent the operator into a build to fix a sign-in they already had.
    ///
    /// Making a sandbox is the expensive part of asking, so profiles that can be read in
    /// the same environment share one. A profile whose environment is not built does
    /// not pull another sandbox into being for it: it is answered from disk.
    pub async fn answers(
        &self,
        environment: Environment,
        profiles: Vec<&'static str>,
    ) -> Result<Vec<(&'static str, CredentialAnswer)>, CredentialsError> {
        // A batch per environment, keyed by the snapshot that would answer it.
        let mut batches: Vec<(String, Vec<EnvironmentRecipe>, Vec<CredentialCheck>)> = Vec::new();
        let mut answers = Vec::new();
        for profile in profiles {
            let recipe = self.recipe(profile)?;
            let (recipes, snapshot) = self.environment_for(&environment, profile)?;
            let Some(snapshot) = snapshot else {
                answers.push((profile, self.saved_login(profile)));
                continue;
            };
            let check = CredentialCheck {
                profile,
                command: recipe.status_command.to_owned(),
            };
            match batches
                .iter_mut()
                .find(|(answering, _, _)| *answering == snapshot)
            {
                Some((_, _, checks)) => checks.push(check),
                None => batches.push((snapshot, recipes, vec![check])),
            }
        }
        for (snapshot, recipes, checks) in batches {
            let profiles: Vec<&'static str> = checks.iter().map(|check| check.profile).collect();
            let sandbox = CredentialSandbox {
                id: check_id(&profiles),
                environment: environment.clone(),
                recipes,
                snapshot: Some(snapshot),
            };
            let reports = self
                .isolation
                .credential_reports(sandbox, checks, &Unwatched)
                .await
                .map_err(CredentialsError::Isolation)?
                .unwrap_or_default();
            answers.extend(reports.into_iter().map(|(profile, report)| {
                (
                    profile,
                    CredentialAnswer {
                        signed_in: report.signed_in,
                        output: report.output,
                        is_verified: true,
                    },
                )
            }));
        }
        Ok(answers)
    }

    /// Forget a mounted profile's saved login, leaving everything else in it.
    ///
    /// Only the files a profile's CLI writes a sign-in to are removed: its settings,
    /// models, managed MCP files, and the environment it was built in stay. The Linear
    /// tracker is not a mounted profile; its sign-in is forgotten through the tracker
    /// provider's `clear_credentials`.
    pub fn forget(&self, profile: &str) -> Result<(), CredentialsError> {
        files::forget(&self.profile_directory(profile), profile).map_err(CredentialsError::Refused)
    }

    /// The recipes a sign-in or check mounts, and the snapshot it can run in.
    ///
    /// A login is read or written in the profile, and the CLI that owns it is the only
    /// thing needed to do that, so the profile's own environment is preferred. The
    /// environment a task driven by either agent runs in answers too, because it has the
    /// same CLI installed — which is what lets a sign-in skip an install a task has
    /// already paid for, and what keeps the two from building the same thing twice. With
    /// nothing built to reuse, only the profile's own is built: a sign-in waits on its
    /// own tool, not on everything a task runs on.
    fn environment_for(
        &self,
        environment: &Environment,
        profile: &str,
    ) -> Result<(Vec<EnvironmentRecipe>, Option<String>), CredentialsError> {
        for recipes in self.readable_environments(profile)? {
            if let Some(snapshot) = self
                .isolation
                .environment_snapshot(environment.clone(), recipes.clone())
            {
                return Ok((recipes, Some(snapshot)));
            }
        }
        Ok((vec![self.recipe(profile)?], None))
    }

    /// The compositions a profile's login can be read in, best first.
    ///
    /// Its own, which a sign-in builds, then each agent's task environment that has the
    /// profile mounted. A task runs one agent, so there are two such environments rather
    /// than one; either can answer for the profiles they share.
    fn readable_environments(
        &self,
        profile: &str,
    ) -> Result<Vec<Vec<EnvironmentRecipe>>, CredentialsError> {
        let mut environments = vec![vec![self.recipe(profile)?]];
        for agent in [AgentKind::Pi, AgentKind::Claude] {
            let recipes = mounts::task(&self.agents, agent);
            if recipes.iter().any(|recipe| recipe.profile == profile) {
                environments.push(recipes);
            }
        }
        Ok(environments)
    }

    /// The one recipe a profile's sign-in needs.
    ///
    /// One rather than the set a task gets: a login is written by the CLI that owns it,
    /// so the environment is that CLI's and the wait is its install rather than somebody
    /// else's.
    fn recipe(&self, profile: &str) -> Result<EnvironmentRecipe, CredentialsError> {
        mounts::every(&self.agents)
            .into_iter()
            .find(|recipe| recipe.profile == profile)
            .ok_or_else(|| {
                CredentialsError::Refused(format!(
                    "no profile recipe for {profile}; nothing can sign it in"
                ))
            })
    }

    /// What the login files in a profile's directory say, without a sandbox.
    ///
    /// Only presence and size are read, never the contents. A file there is a saved
    /// login a task would mount, even when no environment has been built to ask the CLI
    /// whether it still accepts it.
    fn saved_login(&self, profile: &str) -> CredentialAnswer {
        let signed_in = files::has_saved_login(&self.profile_directory(profile), profile);
        CredentialAnswer {
            signed_in,
            output: if signed_in {
                "a saved login is here".to_owned()
            } else {
                String::new()
            },
            is_verified: false,
        }
    }

    /// Where a mounted profile keeps its files on this host.
    fn profile_directory(&self, profile: &str) -> PathBuf {
        self.config.profile_dir(profile)
    }
}

/// The name the sandbox a profile's login is checked in has.
///
/// A check and a shell can be asked for at the same time — the list asks about the
/// profiles it shows, and the operator can open one while an answer is still coming —
/// so they cannot share a name.
fn check_id(profiles: &[&str]) -> String {
    let first = profiles.first().copied().unwrap_or("profiles");
    format!("{CREDENTIAL_PREFIX}check-{}", first.replace('/', "-"))
}
