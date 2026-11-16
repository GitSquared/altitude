mod files;
pub mod freshness;
pub mod host;
pub mod keeper;
mod model;
mod shell;

use std::time::Duration;

use tokio::sync::watch;

use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
};
use tokio::process::Command;
use tokio::time::timeout;

#[cfg(any(test, debug_assertions, feature = "testkit"))]
use std::collections::HashMap;

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
        CredentialSandbox, GuestPath, IsolationEngine, IsolationError, IsolationStatus, PortRelays,
        ProfileRecipe, ProfileSummary, Unwatched, mounts, renew_session,
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

    // A session is renewed by the harness that owns it. Where this machine has that
    // harness it could run it here, but altitude does not ask an operator's machine to
    // provide one: the renewal runs in a sandbox, which is where a harness lives when the
    // machine has none — and on a remote whose isolation is workers, that sandbox is a
    // worker, spawned for this and gone after.
    pub async fn renew(&self, profile: &str) -> Result<Renewal, CredentialsError> {
        let recipe = self.recipe(profile)?;
        // Whether a credential can renew itself is the harness's business, not this
        // layer's: the one that can is asked for the command, and the ones that cannot
        // say nothing.
        let Some(command) = recipe.session_renew else {
            return Ok(Renewal::NothingToRenew);
        };
        let before = self.session_expiry(profile);
        let recipes = vec![recipe.clone()];
        // A sandbox is only handed out for a layer that exists, so a machine that has
        // never built this profile's harness gets one built first.
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
        // The session files the provider names are what a renewal has to keep: they come
        // back out of the sandbox into the directory this machine owns the profile in.
        let take_back = recipes
            .first()
            .and_then(|recipe| recipe.directories.first())
            .map(|directory| match directory.guest_path {
                GuestPath::Absolute(root) | GuestPath::HomeRelative(root) => root.to_owned(),
            })
            .map(|root| {
                recipe
                    .session_keep
                    .iter()
                    .map(|name| {
                        (
                            format!("{root}/{name}"),
                            self.config.profile_dir(profile).join(name),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let sandbox = CredentialSandbox {
            id: renewal_id(profile),
            recipes,
            snapshot,
            vsock: Vec::new(),
        };
        renew_session(
            &self.isolation,
            &self.config,
            sandbox,
            command.to_owned(),
            take_back,
            &Unwatched,
        )
        .await
        .map_err(CredentialsError::Isolation)?;
        // What the command said about itself is not the question. The profile's own file
        // is: a renewal that leaves the deadline where it was has not renewed anything.
        if self.session_expiry(profile) == before {
            return Ok(Renewal::Refused(
                "its harness ran and the session did not change".to_owned(),
            ));
        }
        Ok(Renewal::Renewed)
    }

    async fn answer_batch(
        &self,
        profiles: Vec<String>,
    ) -> Result<Vec<(String, CredentialAnswer)>, CredentialsError> {
        let mut answers = Vec::new();
        for profile in profiles {
            let recipe = self.recipe(&profile)?;
            let directory = self.profile_directory(&profile);
            let check = Command::new("/bin/sh")
                .arg("-c")
                .arg(recipe.status_command)
                .current_dir(&directory)
                .env("HOME", &directory)
                .env("CLAUDE_CONFIG_DIR", &directory)
                .output();
            let answer = match timeout(CHECK_PATIENCE, check).await {
                Ok(Ok(output)) => CredentialAnswer {
                    signed_in: output.status.success(),
                    output: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
                    is_verified: true,
                },
                // A check that will not run says nothing about the files it was meant to
                // read, so the answer falls back to what they are.
                _ => self.saved_login(&profile),
            };
            answers.push((profile, answer));
        }
        Ok(answers)
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

// Prefixed like every throwaway profile sandbox, so one left by a crash is recognisable and
// swept.
fn renewal_id(profile: &str) -> String {
    format!("credentials-renew-{}", profile.replace('/', "-"))
}

// A status command is a file test or a version probe: one that has not answered by now has
// told us what it is going to.
const CHECK_PATIENCE: Duration = Duration::from_secs(30);
