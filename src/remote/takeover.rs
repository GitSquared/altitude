//! Bringing a task home from the machine that owns it.
//!
//! A take-over is one thing from the operator's side and four from this one: ask the
//! machine for the task, fetch the capsule it bundles, build the environment the capsule
//! was taken against, and found the task here with the history it already had. None of
//! that is the interface's business, so none of it is in the interface.

use std::{env, fs};

use microsandbox::Snapshot;

use crate::{
    config::DEFAULT_PORT,
    isolation::{Environment, IsolationEngine},
    remote::{LinkClient, RemoteKeys, TaskSummary, meet},
    task::{TaskEventLoop, TaskHistoryEvent},
};

/// Ask a machine for a task, and found it here.
///
/// The whole of a take-over that happens away from the interface: ask, fetch, unpack and
/// found. What the interface does with the result is the interface's business, which is
/// why none of this is in it.
pub async fn bring_home<E>(
    address: &str,
    summary: &TaskSummary,
    keys: &RemoteKeys,
    client: &LinkClient,
    environments: &[Environment],
    event_loop: &TaskEventLoop<E>,
) -> Result<String, String>
where
    E: IsolationEngine + Clone + 'static,
{
    let environment = environments
        .iter()
        .find(|environment| environment.name == summary.environment)
        .cloned()
        .ok_or_else(|| format!("this machine has no {} environment", summary.environment))?;
    let remembered = keys.remembered(address).await.ok().flatten();
    let sighting = meet(address, DEFAULT_PORT, remembered.as_deref())
        .await
        .map_err(|error| error.to_string())?;
    // Arrivals land in their own place, so a machine that both serves and takes over
    // is not reading an archive it is still writing.
    let landing = env::temp_dir()
        .join("altitude-incoming")
        .join(format!("{}.tar.zst", summary.id));
    let handover = client
        .take_task(sighting.ticket, &summary.id, &landing)
        .await
        .map_err(|error| error.to_string())?;
    // What travelled is the layers above the environment, so that environment has to
    // exist here first: the same work a first run would do, done now because the capsule
    // needs something to land on, and its snapshot is what resolves the layers that
    // stayed behind.
    let base = event_loop
        .prepare_environment(&environment, handover.draft.agent)
        .await
        .map_err(|error| error.to_string())?;
    // The capsule is loaded into this machine's own store, and where it landed is
    // what the task will run from.
    let capsule = match handover.capsule.len {
        0 => None,
        _ => {
            let loaded = Snapshot::load_with_base(&landing, None, &base.to_string_lossy())
                .await
                .map_err(|error| error.to_string())?;
            Some(
                loaded
                    .path()
                    .map_err(|error| error.to_string())?
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    };
    let history: Vec<(i64, TaskHistoryEvent)> = handover
        .events
        .iter()
        .map(|recorded| (recorded.recorded_at_ms, recorded.event.clone()))
        .collect();
    let task = event_loop
        .land(
            handover.draft.intent.clone(),
            environment,
            handover.draft.agent,
            handover.draft.agent_conversation_id.clone(),
            &history,
            capsule,
        )
        .await
        .map_err(|error| error.to_string())?;
    let _ = fs::remove_file(&landing);
    Ok(task.id)
}
