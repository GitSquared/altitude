//! Establish the exact environment, stage a usable capsule, then transfer ownership.

use crate::{
    config::DEFAULT_PORT,
    isolation::{IsolationEngine, capsule::verify_environment},
    remote::{
        Handover, HandoverRecord, LinkClient, RemoteKeyStatus, RemoteKeys, TaskSummary, Transfer,
        TransferPart, meet,
    },
    task::{Task, TaskArrival, TaskEventLoop, TaskHistoryEvent},
};
use microsandbox::Snapshot;
use std::fs;

pub async fn bring_home<E>(
    address: &str,
    summary: &TaskSummary,
    keys: &RemoteKeys,
    client: &LinkClient,
    event_loop: &TaskEventLoop<E>,
    transfer: &Transfer,
) -> Result<String, String>
where
    E: IsolationEngine + Clone + 'static,
{
    let remembered = keys.remembered(address).await.map_err(|e| e.to_string())?;
    let sighting = meet(address, DEFAULT_PORT, remembered.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    if sighting.key_status == RemoteKeyStatus::Changed {
        return Err("the remote's key changed; approve its identity before taking a task".into());
    }
    let peer: iroh::EndpointAddr = sighting.ticket.into();
    let handover = client
        .prepare_take(&peer, &summary.id)
        .await
        .map_err(|e| e.to_string())?;
    if handover.task_id != summary.id {
        return Err("the remote prepared another task".into());
    }
    uuid::Uuid::parse_str(&handover.transfer_id)
        .map_err(|_| "invalid transfer identifier".to_owned())?;
    // The staged record survives a lost acknowledgement or either process restarting.
    for task in event_loop.list().await.map_err(|e| e.to_string())? {
        let history = event_loop
            .events(&task.id)
            .await
            .map_err(|e| e.to_string())?;
        let is_arrival = history.iter().any(|(_, event)| {
            matches!(event,
            TaskHistoryEvent::TransferStaged { transfer_id, peer: source, .. }
            if transfer_id == &handover.transfer_id && source == &peer.id.to_string())
        });
        if !is_arrival {
            continue;
        }
        let is_landed = history.iter().any(|(_, event)| matches!(event,
            TaskHistoryEvent::TransferLanded { transfer_id, .. } if transfer_id == &handover.transfer_id));
        if !is_landed {
            complete_landing(&peer, &handover.transfer_id, client, event_loop, &task).await?;
        }
        return Ok(task.id);
    }
    let outcome = receive(&peer, &handover, client, event_loop, transfer).await;
    if outcome.is_err() {
        give_back(&peer, &handover.transfer_id, client).await;
    }
    outcome
}

/// Take everything a prepared transfer offers, and land it here.
///
/// Apart from `bring_home` so a stop can be answered where it happened: the
/// machine that stopped the transfer still owns the task, and has to run it
/// again rather than hold it stopped for a transfer nobody is making.
async fn receive<E>(
    peer: &iroh::EndpointAddr,
    handover: &Handover,
    client: &LinkClient,
    event_loop: &TaskEventLoop<E>,
    transfer: &Transfer,
) -> Result<String, String>
where
    E: IsolationEngine + Clone + 'static,
{
    let landing = tempfile::tempdir().map_err(|e| e.to_string())?;
    if handover.history.len > super::protocol::MAX_BLOB_BYTES {
        return Err("task history exceeds the transfer limit".into());
    }
    // The history is the smallest part and it decides the rest, so it arrives
    // before the bar appears: what is counted is what is still to come.
    transfer.waiting("taking over…");
    let history_path = landing.path().join("history.json");
    client
        .fetch_part(
            peer,
            &handover.transfer_id,
            TransferPart::History,
            &handover.history,
            &history_path,
            transfer,
        )
        .await
        .map_err(|e| e.to_string())?;
    let record: HandoverRecord =
        serde_json::from_reader(fs::File::open(history_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let project = record.draft.project;
    // Building the base here is a wait with nothing to count, so the bar gives
    // way to the words until there are bytes again.
    transfer.waiting("building the environment here…");
    event_loop
        .prepare_project(&project, record.draft.agent)
        .await
        .map_err(|e| e.to_string())?;
    // What this machine already has is not moved again: the bar counts what
    // travels, so a base that is here already must not be counted or it can never
    // be filled.
    let base = match &handover.environment {
        Some(base) => {
            let cached = Snapshot::open(&base.snapshot_id)
                .await
                .ok()
                .filter(|snapshot| snapshot.digest() == base.descriptor_digest);
            let snapshot = match cached {
                Some(snapshot) => {
                    transfer.counting(handover.capsule.len);
                    snapshot
                }
                None => {
                    transfer.counting(base.archive.len + handover.capsule.len);
                    let path = landing.path().join("environment.tar.zst");
                    client
                        .fetch_part(
                            peer,
                            &handover.transfer_id,
                            TransferPart::Environment,
                            &base.archive,
                            &path,
                            transfer,
                        )
                        .await
                        .map_err(|e| e.to_string())?;
                    let snapshot = Snapshot::load(&path, None)
                        .await
                        .map_err(|e| e.to_string())?
                        .open()
                        .await
                        .map_err(|e| e.to_string())?;
                    if snapshot.digest() != base.descriptor_digest
                        || snapshot.manifest().snapshot_id.to_string() != base.snapshot_id
                    {
                        return Err(
                            "the environment descriptor differs from the prepared base".into()
                        );
                    }
                    snapshot
                }
            };
            verify_environment(&snapshot, &base.layer_roots).await?;
            Some(snapshot)
        }
        None if handover.capsule.len == 0 => None,
        None => return Err("the sender has no environment to rebuild the task on".into()),
    };
    let capsule = if handover.capsule.len == 0 {
        None
    } else {
        let path = landing.path().join("capsule.tar.zst");
        client
            .fetch_part(
                peer,
                &handover.transfer_id,
                TransferPart::Capsule,
                &handover.capsule,
                &path,
                transfer,
            )
            .await
            .map_err(|e| e.to_string())?;
        let base = base
            .as_ref()
            .ok_or_else(|| "the environment is missing".to_owned())?;
        let loaded = Snapshot::load_with_base(
            &path,
            None,
            &base.path().map_err(|e| e.to_string())?.to_string_lossy(),
        )
        .await
        .map_err(|e| e.to_string())?;
        Some(
            loaded
                .path()
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .into_owned(),
        )
    };
    let history = record
        .events
        .into_iter()
        .map(|recorded| (recorded.recorded_at_ms, recorded.event))
        .collect::<Vec<_>>();
    transfer.waiting("starting it…");
    let task = event_loop
        .land(TaskArrival {
            intent: record.draft.intent,
            project,
            agent: record.draft.agent,
            conversation: record.draft.agent_conversation_id,
            history,
            capsule,
            transfer_id: handover.transfer_id.clone(),
            peer: peer.id.to_string(),
        })
        .await
        .map_err(|e| e.to_string())?;
    if let Err(error) =
        complete_landing(peer, &handover.transfer_id, client, event_loop, &task).await
    {
        // Nothing has been confirmed, so the source still owns this work: what
        // arrived here is a copy that would be a second task on the same issue.
        // It goes, and the source is told to carry on rather than wait.
        if let Err(problem) = event_loop.remove_and_wait(task.clone()).await {
            tracing::warn!(task = %task.id, %problem, "could not take back a task that did not land");
        }
        give_back(peer, &handover.transfer_id, client).await;
        return Err(error);
    }
    Ok(task.id)
}

/// Hand a take over back to the machine that still owns the task.
///
/// Best effort, and said out loud when it fails: the source keeps the task until
/// this machine lands it, so a failure to say so costs it a wait rather than the
/// task. Doing nothing is what leaves a task paused in a transfer nobody is
/// making, with the operator looking at two copies of the same work.
async fn give_back(peer: &iroh::EndpointAddr, transfer_id: &str, client: &LinkClient) {
    if let Err(error) = client.abandon_take(peer, transfer_id).await {
        tracing::warn!(%error, transfer_id, "could not give a transfer back to its owner");
    }
}

async fn complete_landing<E>(
    peer: &iroh::EndpointAddr,
    transfer_id: &str,
    client: &LinkClient,
    event_loop: &TaskEventLoop<E>,
    task: &Task,
) -> Result<(), String>
where
    E: IsolationEngine + Clone + 'static,
{
    event_loop
        .verify_landing(task.clone())
        .await
        .map_err(|e| e.to_string())?;
    client
        .confirm_take(peer, transfer_id)
        .await
        .map_err(|e| e.to_string())?;
    event_loop
        .finish_transfer(&task.id, transfer_id)
        .await
        .map_err(|e| e.to_string())
}
