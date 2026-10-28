//! The logins tasks run under: the shell a profile is signed in through, the check
//! that says whether it worked, and what a saved login is on disk.
//!
//! Its own concern rather than a task's. Nothing here is recorded, travels, or belongs
//! to a capsule, and none of it needs a task to exist: it uses the same profile
//! recipes and isolation a task does, so what it writes is what a task finds. The
//! interface asks it about logins; the task layer asks it for the profiles a task
//! could mount.

mod files;
mod model;
mod shell;

use std::{error::Error, fmt, path::PathBuf};

use crate::vcs;

pub use model::{Credential, Service, Slot, new_id};
pub use shell::{
    CredentialShell, CredentialShellCancel, CredentialShellEvent, credential_shell_id,
};

use crate::{
    agent::{AgentChannels, AgentEngines},
    config::Config,
    isolation::{
        CREDENTIAL_PREFIX, CredentialCheck, CredentialSandbox, IsolationEngine, IsolationError,
        PortRelays, ProfileRecipe, ProfileSummary, Unwatched, mounts,
    },
    project::Project,
};

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
    relays: PortRelays,
}

impl<E> Credentials<E>
where
    E: IsolationEngine + Clone,
{
    pub fn new(isolation: E, config: Config, agents: AgentEngines, relays: PortRelays) -> Self {
        Self {
            isolation,
            channels: AgentChannels::new(config.agent_channel_dir()),
            relays,
            config,
            agents,
        }
    }
}

impl<E> Credentials<E>
where
    E: IsolationEngine + Clone,
{
    pub fn summary(&self, profile: &str) -> Option<ProfileSummary> {
        let recipe = self.recipe(profile).ok()?;
        Some(ProfileSummary {
            profile: recipe.profile,
            display: recipe.display,
            sign_in: recipe.sign_in,
            sign_in_command: recipe.sign_in_command,
        })
    }

    /// The one recipe a profile's sign-in needs.
    ///
    /// One rather than the set a task gets: a login is written by the CLI that owns it,
    /// so the environment is that CLI's and the wait is its install rather than somebody
    /// else's.
    pub(crate) fn recipe(&self, profile: &str) -> Result<ProfileRecipe, CredentialsError> {
        if let Some(recipe) = mounts::every(&self.agents)
            .into_iter()
            .find(|recipe| recipe.profile == profile)
        {
            return Ok(recipe);
        }
        let service = Service::from_id(profile).ok_or_else(|| {
            CredentialsError::Refused(format!(
                "no profile recipe for {profile}; nothing can sign it in"
            ))
        })?;
        let mut recipe = match service {
            Service::Agent(kind) => self.agents.engine(kind).profile_recipe(),
            Service::VersionControl(kind) => vcs::profile_recipe(kind),
            Service::Tracker(_) => {
                return Err(CredentialsError::Refused(format!(
                    "{profile} is a host-side identity, not a sandbox profile"
                )));
            }
        };
        recipe.profile = profile.to_owned();
        Ok(recipe)
    }
}

impl<E> Credentials<E>
where
    E: IsolationEngine + Clone + 'static,
{
    /// Whether a sign-in for this profile would find its build ready.
    ///
    /// Asked before a shell is opened, so a wait is only shown when there is one. The
    /// answer is the same lookup the shell makes, so a box that flashes for nothing
    /// cannot happen.
    pub fn build_ready(&self, profile: &str) -> bool {
        self.credential_build(profile)
            .is_ok_and(|(_, snapshot)| snapshot.is_some())
    }

    /// What profiles' own checks say about them.
    ///
    /// The checks run in the build a credential shell would use, so an answer is what
    /// pressing enter would find, and they read a login from the profile rather than a
    /// task's state. A built snapshot can answer; only with none is building necessary,
    /// and a page is not the place for minutes of work, so the login files answer
    /// instead. A page that said "not signed in" because nothing had been built yet
    /// sent the operator into a build to fix a sign-in they already had.
    ///
    /// Profiles of different services share one composition. Two identities of
    /// one service cannot, because they would mount the same guest paths.
    pub async fn answers(
        &self,
        mut profiles: Vec<String>,
    ) -> Result<Vec<(String, CredentialAnswer)>, CredentialsError> {
        profiles.sort_unstable();
        // Two identities of one service mount the same guest paths, so they
        // cannot share a check sandbox. Different services can, which keeps the
        // common page to one build.
        let mut by_service: Vec<(Option<Service>, Vec<String>)> = Vec::new();
        for profile in profiles {
            let service = Service::from_id(&profile);
            if let Some((_, group)) = by_service.iter_mut().find(|(known, _)| *known == service) {
                group.push(profile);
            } else {
                by_service.push((service, vec![profile]));
            }
        }
        let mut batches = Vec::new();
        let mut shared = Vec::new();
        for (_, group) in by_service {
            if group.len() == 1 {
                shared.extend(group);
            } else {
                batches.extend(group.into_iter().map(|profile| vec![profile]));
            }
        }
        if !shared.is_empty() {
            batches.push(shared);
        }
        let mut answers = Vec::new();
        for batch in batches {
            answers.extend(self.answer_batch(batch).await?);
        }
        answers.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(answers)
    }

    async fn answer_batch(
        &self,
        profiles: Vec<String>,
    ) -> Result<Vec<(String, CredentialAnswer)>, CredentialsError> {
        let mut recipes = Vec::new();
        let mut checks = Vec::new();
        for profile in &profiles {
            let recipe = self.recipe(profile)?;
            checks.push(CredentialCheck {
                profile: profile.clone(),
                command: recipe.status_command.to_owned(),
            });
            recipes.push(recipe);
        }
        let Some(snapshot) = self
            .isolation
            .project_snapshot(Project::profile_build(), recipes.clone())
        else {
            return Ok(profiles
                .into_iter()
                .map(|profile| (profile.clone(), self.saved_login(&profile)))
                .collect());
        };
        let sandbox = CredentialSandbox {
            id: check_id(&profiles),
            recipes,
            snapshot: Some(snapshot),
            vsock: Vec::new(),
        };
        let reports = self
            .isolation
            .credential_reports(sandbox, checks, &Unwatched)
            .await
            .map_err(CredentialsError::Isolation)?
            .unwrap_or_default();
        Ok(reports
            .into_iter()
            .map(|(profile, report)| {
                (
                    profile,
                    CredentialAnswer {
                        signed_in: report.signed_in,
                        output: report.output,
                        is_verified: true,
                    },
                )
            })
            .collect())
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

    /// The recipes a sign-in mounts, and the snapshot it can run in.
    ///
    /// Only the CLI that owns the login is needed to write it, so this is the
    /// profile's own build and never the project a task happens to run in: a sign-in
    /// waits on its own tool rather than on everything a project installs.
    fn credential_build(
        &self,
        profile: &str,
    ) -> Result<(Vec<ProfileRecipe>, Option<String>), CredentialsError> {
        let recipes = vec![self.recipe(profile)?];
        let snapshot = self
            .isolation
            .project_snapshot(Project::profile_build(), recipes.clone());
        Ok((recipes, snapshot))
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

    fn profile_directory(&self, profile: &str) -> PathBuf {
        self.config.profile_dir(profile)
    }
}

/// The name the sandbox a profile's login is checked in has.
///
/// A check and a shell can be asked for at the same time — the list asks about the
/// profiles it shows, and the operator can open one while an answer is still coming —
/// so they cannot share a name.
fn check_id(profiles: &[String]) -> String {
    let first = profiles.first().map(String::as_str).unwrap_or("profiles");
    format!("{CREDENTIAL_PREFIX}check-{}", first.replace('/', "-"))
}
