use crate::{
    config::DEFAULT_PORT,
    isolation::IsolationEngine,
    remote::{
        Compatibility, Handover, HandoverRecord, LinkClient, RemoteKeyStatus, RemoteKeys,
        TaskSummary, Transfer, TransferPart, meet,
    },
    task::{Task, TaskArrival, TaskEventLoop, TaskHistoryEvent},
};
use std::{fs, process};

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
    if sighting.compatibility == Compatibility::RequiresUpdate {
        return Err(format!(
            "{} needs an update before it can hand a task over",
            sighting.announcement.label_or_address(address)
        ));
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
            complete_landing(
                &peer,
                &handover.transfer_id,
                client,
                event_loop,
                &task,
                transfer,
            )
            .await?;
        }
        return Ok(task.id);
    }
    let outcome = receive(&peer, &handover, client, event_loop, transfer).await;
    if outcome.is_err() {
        give_back(&peer, &handover.transfer_id, client).await;
    }
    outcome
}

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
    // What travels is the task: its record, the files the work is made of, and the
    // harness's own conversation. Nothing image-shaped does, so the environment is
    // built here and the files go into the sandbox once it exists.
    let files = landing.path().join("files");
    fs::create_dir_all(&files).map_err(|e| e.to_string())?;
    let whole = handover.work.len + handover.transcript.len;
    if whole > 0 {
        transfer.counting(whole);
    }
    if handover.work.len > 0 {
        let path = landing.path().join("work.tar.zst");
        client
            .fetch_part(
                peer,
                &handover.transfer_id,
                TransferPart::Work,
                &handover.work,
                &path,
                transfer,
            )
            .await
            .map_err(|e| e.to_string())?;
        let status = process::Command::new("tar")
            .arg("-xzf")
            .arg(&path)
            .arg("-C")
            .arg(&files)
            .status()
            .map_err(|e| format!("could not run tar: {e}"))?;
        if !status.success() {
            return Err("the working files could not be unpacked".to_owned());
        }
    }
    if handover.transcript.len > 0 {
        client
            .fetch_part(
                peer,
                &handover.transfer_id,
                TransferPart::Transcript,
                &handover.transcript,
                &files.join("transcript.jsonl"),
                transfer,
            )
            .await
            .map_err(|e| e.to_string())?;
    }
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
            files: (whole > 0).then_some(files),
            transfer_id: handover.transfer_id.clone(),
            peer: peer.id.to_string(),
        })
        .await
        .map_err(|e| e.to_string())?;
    if let Err(error) = complete_landing(
        peer,
        &handover.transfer_id,
        client,
        event_loop,
        &task,
        transfer,
    )
    .await
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
    transfer: &Transfer,
) -> Result<(), String>
where
    E: IsolationEngine + Clone + 'static,
{
    // Provisioning is part of landing: the sandbox a task arrives with was the
    // other machine's, so the task that comes back says which one is here.
    let task = event_loop
        .verify_landing(task.clone())
        .await
        .map_err(|e| e.to_string())?;
    transfer.waiting("putting the files in…");
    event_loop
        .deliver_arrivals(&task, transfer)
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
