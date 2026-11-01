mod files;
mod model;
mod shell;

use std::{error::Error, fmt, path::PathBuf};

use crate::vcs;

pub use model::{Credential, Service, Slot, agent_of, describe, new_id};
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
    pub is_verified: bool,
}

#[derive(Debug)]
pub enum CredentialsError {
    Isolation(IsolationError),
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

#[derive(Clone)]
pub struct Credentials<E> {
    isolation: E,
    config: Config,
    agents: AgentEngines,
    channels: AgentChannels,
    relays: PortRelays,
}

impl<E> Credentials<E> {
    pub fn config(&self) -> &Config {
        &self.config
    }
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
    pub fn build_ready(&self, profile: &str) -> bool {
        self.credential_build(profile)
            .is_ok_and(|(_, snapshot)| snapshot.is_some())
    }

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

    pub fn forget(&self, profile: &str) -> Result<(), CredentialsError> {
        files::forget(&self.profile_directory(profile), profile).map_err(CredentialsError::Refused)
    }

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

fn check_id(profiles: &[String]) -> String {
    let first = profiles.first().map(String::as_str).unwrap_or("profiles");
    format!("{CREDENTIAL_PREFIX}check-{}", first.replace('/', "-"))
}
