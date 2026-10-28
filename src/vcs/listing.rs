//! Listing the repositories a provider's login can reach.
//!
//! A picker needs the list before a project can name one, and the login that can
//! see it lives in a profile rather than on this machine. So the command runs
//! where the login does: in a throwaway sandbox built from the same profile a
//! sign-in uses, which is also why opening the list costs an install at most once.

use std::{error::Error, fmt};

use crate::{
    isolation::{IsolationEngine, Unwatched, WATCHER_PREFIX, run_collecting},
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
    let project = Project::profile_build();
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
    let listed = list(engine, &id, provider.repository_list_command()).await;
    if let Err(error) = engine.remove(id).await {
        tracing::warn!(%error, "could not remove a repository listing sandbox");
    }
    provider
        .repositories(&listed?)
        .ok_or_else(|| ListingError("the provider's answer could not be read".to_owned()))
}

async fn list<E>(engine: &E, id: &str, command: String) -> Result<String, ListingError>
where
    E: IsolationEngine + Clone,
{
    let (status, output, errors) = run_collecting(engine, id.to_owned(), command)
        .await
        .map_err(|error| ListingError(error.to_string()))?;
    if status != 0 {
        let said = if errors.trim().is_empty() {
            output.trim()
        } else {
            errors.trim()
        };
        return Err(ListingError(format!(
            "the repository list exited with code {status}: {said}"
        )));
    }
    Ok(output)
}

#[derive(Debug)]
pub struct ListingError(String);

impl fmt::Display for ListingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for ListingError {}
