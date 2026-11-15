mod files;
pub mod freshness;
pub mod host;
pub mod keeper;
mod model;
mod shell;

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};

use tokio::sync::watch;

#[cfg(any(test, debug_assertions, feature = "testkit"))]
use std::collections::HashMap;

use crate::isolation::{CredentialAsk, GuestPath, IsolationStatus, guest_absolute};
use crate::vcs;

pub use model::{Credential, Service, Slot, agent_of, describe, new_id};

// What came of trying to renew a profile: the harness was used and its session is
// newer, there was nothing for it to renew, or it refused and said why.
#[derive(Clone, Debug)]
pub enum Renewal {
    Renewed,
    NothingToRenew,
    Refused(String),
}
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
    // Recipes a test put in place of the real ones, so a scripted provider does not
    // have to install anything on the machine to answer for a profile.
    #[cfg(any(test, debug_assertions, feature = "testkit"))]
    overrides: HashMap<String, ProfileRecipe>,
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
            #[cfg(any(test, debug_assertions, feature = "testkit"))]
            overrides: HashMap::new(),
        }
    }

    #[cfg(any(test, debug_assertions, feature = "testkit"))]
    pub(crate) fn override_recipe(&mut self, profile: &str, recipe: ProfileRecipe) {
        self.overrides.insert(profile.to_owned(), recipe);
    }
}

impl<E> Credentials<E>
where
    E: IsolationEngine + Clone,
{
    pub fn summary(&self, profile: &str) -> Option<ProfileSummary> {
        let recipe = self.recipe(profile).ok()?;
        Some(ProfileSummary {
            profile: recipe.profile.clone(),
            display: recipe.display,
            sign_in: recipe.sign_in,
            sign_in_command: recipe.sign_in_command,
            expires: expiry_of(self, &recipe.profile),
        })
    }
}

// A profile's recipe is built from the profile id and this build's providers, not
// from an operator's settings, so a machine that holds the profile's files can
// rebuild it from the id alone.
pub(crate) fn profile_recipe(
    profile: &str,
    agents: &AgentEngines,
) -> Result<ProfileRecipe, CredentialsError> {
    if let Some(recipe) = mounts::every(agents)
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
        Service::Agent(kind) => agents.engine(kind).profile_recipe(),
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

    // Using the harness is the only thing that renews its session: the status check
    // reads a file and cannot. So a profile close to running out is renewed by running
    // the harness once, in the same kind of sandbox its checks use.
    pub async fn renew(&self, profile: &str) -> Result<Renewal, CredentialsError> {
        let recipe = self.recipe(profile)?;
        // Whether a credential can renew itself is the harness's business, not this
        // layer's: the one that can is asked for the command, and the ones that cannot
        // say nothing.
        let Some(command) = recipe.session_renew else {
            return Ok(Renewal::NothingToRenew);
        };
        // A sandbox is only handed out for a layer that exists, so a machine that has
        // never built this profile's harness gets one built first.
        let recipes = vec![recipe.clone()];
        let mut snapshot = self
            .isolation
            .project_snapshot(Project::profile_build(), recipes.clone());
        if snapshot.is_none() {
            let (quiet, _) = watch::channel(IsolationStatus::Starting);
            self.isolation.prepare(&quiet).await.ok();
            snapshot = self
                .isolation
                .project_snapshot(Project::profile_build(), recipes.clone());
        }
        let sandbox = CredentialSandbox {
            id: check_id(&[profile.to_owned()]),
            recipes,
            snapshot,
            vsock: Vec::new(),
        };
        let keep = recipe
            .session_keep
            .iter()
            .filter_map(|name| {
                let root = recipe.directories.first()?;
                let root = match root.guest_path {
                    GuestPath::Absolute(path) | GuestPath::HomeRelative(path) => path,
                };
                Some((
                    format!("{}/{name}", guest_absolute(root).ok()?),
                    (*name).to_owned(),
                ))
            })
            .collect::<Vec<_>>();
        let ask = CredentialAsk {
            checks: vec![CredentialCheck {
                profile: profile.to_owned(),
                command: command.to_owned(),
            }],
            keep,
        };
        let outcome = self
            .isolation
            .credential_reports(sandbox, ask, &Unwatched)
            .await
            .map_err(CredentialsError::Isolation)?;
        let Some(outcome) = outcome else {
            return Ok(Renewal::Refused(
                "no sandbox can be made for this credential here".to_owned(),
            ));
        };
        if !outcome.reports.iter().any(|(_, report)| report.signed_in) {
            return Ok(Renewal::Refused("its harness could not be used".to_owned()));
        }
        let files = outcome.files;
        // Whatever the provider says holds the session is what this machine keeps: the
        // renewal happened somewhere else, and this is the only copy of it.
        let profile_dir = self.config.profile_dir(profile);
        if fs::create_dir_all(&profile_dir).is_err() {
            return Err(CredentialsError::Refused(
                "could not keep a renewed session".to_owned(),
            ));
        }
        for (name, bytes) in files {
            debug_assert!(
                !name.contains('/'),
                "a session file is a file in its profile, not a path"
            );
            fs::write(profile_dir.join(&name), bytes)
                .map_err(|error| CredentialsError::Refused(error.to_string()))?;
        }
        Ok(Renewal::Renewed)
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
            .credential_reports(
                sandbox,
                CredentialAsk {
                    checks,
                    keep: Vec::new(),
                },
                &Unwatched,
            )
            .await
            .map_err(CredentialsError::Isolation)?
            .unwrap_or_default()
            .reports;
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

// What a profile's files say about its session, read from the directory the operator's
// copy lives in so the list says the same thing the check would find out later.
// The same answer, for files that have travelled instead of staying on this machine: a
// host is handed a profile's files, and asking whether they still work uses the provider
// that made them.
pub fn freshness_of(
    profile: &str,
    agents: &AgentEngines,
    files: &[(String, String)],
) -> Option<freshness::Standing> {
    let recipe = profile_recipe(profile, agents).ok()?;
    let file = recipe.session_file?;
    let contents = files
        .iter()
        .find(|(path, _)| path.ends_with(file))
        .map(|(_, contents)| contents.as_str())?;
    (recipe.session_deadline?)(contents).map(freshness::Standing::new)
}

fn expiry_of(credentials: &Credentials<impl IsolationEngine>, profile: &str) -> Option<String> {
    credentials
        .session_expiry(profile)
        .map(freshness::Standing::new)
        .map(|standing| standing.describe(can_renew(profile, credentials.agents())))
}

// Whether a profile's provider can renew its session at all: asked of the provider, which
// answers with the command that does it, or with nothing.
pub fn can_renew(profile: &str, agents: &AgentEngines) -> bool {
    profile_recipe(profile, agents).is_ok_and(|recipe| recipe.session_renew.is_some())
}

fn check_id(profiles: &[String]) -> String {
    let first = profiles.first().map(String::as_str).unwrap_or("profiles");
    format!("{CREDENTIAL_PREFIX}check-{}", first.replace('/', "-"))
}

impl<E: IsolationEngine> Credentials<E> {
    pub(crate) fn recipe(&self, profile: &str) -> Result<ProfileRecipe, CredentialsError> {
        #[cfg(any(test, debug_assertions, feature = "testkit"))]
        if let Some(recipe) = self.overrides.get(profile) {
            return Ok(recipe.clone());
        }
        profile_recipe(profile, &self.agents)
    }

    // Whether this profile's provider can renew its session at all: the provider knows,
    // and answers with the command that does it or with nothing.
    pub fn can_renew(&self, profile: &str) -> bool {
        self.recipe(profile)
            .is_ok_and(|recipe| recipe.session_renew.is_some())
    }

    pub(crate) fn agents(&self) -> &AgentEngines {
        &self.agents
    }

    // What this profile's own provider says about the session in it: the file it keeps, and
    // the deadline inside it. The credential layer carries the answer and nothing else.
    pub fn session_expiry(&self, profile: &str) -> Option<i64> {
        let recipe = self.recipe(profile).ok()?;
        let name = recipe.session_file?;
        let contents = fs::read_to_string(self.config.profile_dir(profile).join(name)).ok()?;
        (recipe.session_deadline?)(&contents)
    }
}

// The rule both machines share, in the layer that owns the question: a copy whose session
// ends sooner never replaces one that lasts longer. Answers whether the copy already on
// this machine should be kept instead of the one arriving.
pub fn stored_outlasts(
    profile_dir: &Path,
    profile: &str,
    agents: &AgentEngines,
    arriving: &[(String, String)],
) -> bool {
    let Some(arriving) = freshness_of(profile, agents, arriving) else {
        return false;
    };
    let Ok(recipe) = profile_recipe(profile, agents) else {
        return false;
    };
    let here = recipe
        .session_keep
        .iter()
        .filter_map(|name| {
            let contents = fs::read_to_string(profile_dir.join(name)).ok()?;
            Some((name.to_string(), contents))
        })
        .collect::<Vec<_>>();
    let Some(here) = freshness_of(profile, agents, &here) else {
        return false;
    };
    here.outlasts(arriving)
}
