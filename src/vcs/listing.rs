//! Listing the repositories a provider's login can reach.
//!
//! A picker needs the list before a project can name one, and the login that can
//! see it lives in a profile rather than on this machine. So the command runs
//! where the login does: in a throwaway sandbox built from the same profile a
//! sign-in uses, which is also why opening the list costs an install at most once.

use std::{error::Error, fmt};

use crate::{
    isolation::{CommandEvent, IsolationEngine, OutputStream, Unwatched, WATCHER_PREFIX},
    project::Project,
    vcs::{Repository, VcsProviderKind},
};

use super::answering;

/// The repositories a provider's login can reach, newest first.
///
/// The profile-only project is the one a sign-in builds, so the list reuses what
/// signing in already installed instead of paying for a second sandbox image.
pub async fn repositories<E>(
    engine: &E,
    kind: VcsProviderKind,
) -> Result<Vec<Repository>, ListingError>
where
    E: IsolationEngine + Clone,
{
    let provider = answering(kind);
    let recipes = vec![provider.profile_recipe()];
    let project = Project::profile_only("credentials");
    if engine
        .project_snapshot(project.clone(), recipes.clone())
        .is_none()
    {
        engine
            .ensure_project(project.clone(), recipes.clone(), &Unwatched)
            .await
            .map_err(|error| ListingError(error.to_string()))?;
    }
    // Prefixed like every throwaway profile sandbox, so one left by a crash is
    // swept at the next start rather than kept forever.
    let id = format!(
        "{WATCHER_PREFIX}repositories-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    engine
        .watcher(id.clone(), project, recipes, &Unwatched)
        .await
        .map_err(|error| ListingError(error.to_string()))?;
    let listed = run(engine, &id, provider.repository_list_command()).await;
    if let Err(error) = engine.remove(id).await {
        tracing::warn!(%error, "could not remove a repository listing sandbox");
    }
    provider
        .repositories(&listed?)
        .ok_or_else(|| ListingError("the provider's answer could not be read".to_owned()))
}

/// Run the listing command to completion and answer with what it printed.
async fn run<E>(engine: &E, id: &str, command: String) -> Result<String, ListingError>
where
    E: IsolationEngine + Clone,
{
    let mut session = engine
        .run_command(id.to_owned(), command)
        .await
        .map_err(|error| ListingError(error.to_string()))?;
    // Only stdout is the answer; a CLI that warns on stderr must not turn its
    // JSON into something unreadable, and its warning is the useful part when it
    // fails.
    let mut output = String::new();
    let mut errors = String::new();
    while let Some(event) = session.events.recv().await {
        match event {
            CommandEvent::Output { stream, bytes } => {
                let text = String::from_utf8_lossy(&bytes);
                match stream {
                    OutputStream::Stdout => output.push_str(&text),
                    OutputStream::Stderr => errors.push_str(&text),
                }
            }
            CommandEvent::Exited(0) => return Ok(output),
            CommandEvent::Exited(code) => {
                let said = if errors.trim().is_empty() {
                    output.trim()
                } else {
                    errors.trim()
                };
                return Err(ListingError(format!(
                    "the repository list exited with code {code}: {said}"
                )));
            }
            CommandEvent::Failed(error) => return Err(ListingError(error)),
        }
    }
    Err(ListingError(
        "the repository list closed without an exit status".to_owned(),
    ))
}

/// Why a listing could not be read, in the words the picker shows.
#[derive(Debug)]
pub struct ListingError(String);

impl fmt::Display for ListingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ListingError {}
