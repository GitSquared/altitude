use std::{
    fs,
    io::{self, Read},
    path::Path,
    sync::Arc,
};

use iroh::EndpointAddr;

use crate::{
    config::{Config, DEFAULT_PORT},
    isolation::IsolationEngine,
    remote::{
        Compatibility, ConfigBundle, ConfigPush, Handover, LinkClient, Logins, RemoteKeyStatus,
        RemoteKeys, Transfer, TransferPart, meet, protocol::TRANSFER_PIECE_BYTES, serve, transfer,
    },
    task::{HomeFuture, SendsTasksHome, Task, TaskEventLoop, TaskHistoryEvent},
};

use super::{
    DESTINATION_FILE, TRANSFERS_DIRECTORY, message, part_details, transfer_directory, verify_file,
    write_json,
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OutgoingTransfer {
    task_id: String,
    address: String,
    peer: EndpointAddr,
}

pub struct HomeSender<E: IsolationEngine + Clone + 'static> {
    config: Arc<Config>,
    keys: RemoteKeys,
    client: LinkClient,
    tasks: TaskEventLoop<E>,
}

impl<E: IsolationEngine + Clone + 'static> HomeSender<E> {
    pub fn new(
        config: Arc<Config>,
        keys: RemoteKeys,
        client: LinkClient,
        tasks: TaskEventLoop<E>,
    ) -> Self {
        Self {
            config,
            keys,
            client,
            tasks,
        }
    }
}

impl<E: IsolationEngine + Clone + 'static> SendsTasksHome for HomeSender<E> {
    fn send_home(&self, task: Task) -> HomeFuture {
        let Some(address) = task.facts.arrived_from.clone() else {
            return Box::pin(async { Ok(()) });
        };
        let (transfer, _) = Transfer::start("handing the task back…");
        self.tasks
            .announce_transfer(task.id.clone(), address.clone(), transfer.watching());
        let config = Arc::clone(&self.config);
        let keys = self.keys.clone();
        let client = self.client.clone();
        let tasks = self.tasks.clone();
        Box::pin(async move {
            send_to_remote(
                &address, &task.id, &config, &keys, &client, &tasks, &transfer,
            )
            .await
            .map(|_| ())
        })
    }
}

pub async fn send_to_remote<E>(
    address: &str,
    task_id: &str,
    config: &Config,
    keys: &RemoteKeys,
    client: &LinkClient,
    event_loop: &TaskEventLoop<E>,
    transfer: &Transfer,
) -> Result<String, String>
where
    E: IsolationEngine + Clone + 'static,
{
    let remembered = keys.remembered(address).await.map_err(message)?;
    let sighting = meet(address, DEFAULT_PORT, remembered.as_deref())
        .await
        .map_err(message)?;
    if sighting.key_status == RemoteKeyStatus::Changed {
        return Err("the remote's key changed; approve its identity before sending a task".into());
    }
    if sighting.compatibility == Compatibility::RequiresUpdate {
        return Err(format!(
            "{} needs an update before it can take tasks",
            sighting.announcement.label_or_address(address)
        ));
    }
    let peer: EndpointAddr = sighting.ticket.into();

    // A project names the machine its work belongs to, and a task does not choose
    // another. The interface only offers that machine, so this is the check that
    // keeps a stale question or a caller from moving work somewhere it does not run.
    let task = event_loop
        .service()
        .task(task_id)
        .await
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "that task is no longer here".to_owned())?;
    let project = task
        .project()
        .ok_or_else(|| "that task has no recorded project".to_owned())?;
    // The machine a project runs on is the operator's setting, not a fact a task
    // carries: a task taken over from a remote was built with that machine's copy
    // of the project, which names no machine at all because a remote does not name
    // itself. So the setting decides, and the record is only the fallback.
    let setting = config
        .load_or_create_settings()
        .ok()
        .and_then(|settings| settings.settings().project(&project.id).cloned());
    let machine = setting
        .and_then(|project| project.host)
        .or_else(|| project.host.clone());
    if machine.as_deref() != Some(address) {
        return Err(format!(
            "{address} is not this project's machine; set it in Settings → Projects to send work there"
        ));
    }

    let settings = fs::read_to_string(config.settings_path()).map_err(message)?;
    let held = client.held_profiles(peer.clone()).await.map_err(message)?;
    let bundle = ConfigBundle::of(
        &settings,
        config.data_dir(),
        &held,
        Some(address),
        Logins::Always,
    )
    .map_err(message)?;
    let config_bytes = serde_json::to_vec(&bundle).map_err(message)?;
    // Sending a task is not where an account is taken over: the background sync
    // asks, and until it has an answer this machine cannot build the task there.
    if let ConfigPush::TrackerLinkedElsewhere {
        provider,
        display_name,
    } = client
        .put_config(peer.clone(), &config_bytes, false)
        .await
        .map_err(message)?
    {
        return Err(format!(
            "the {} account {display_name} is already linked for another client there; \
             take it over from Settings → Remotes first",
            provider.label()
        ));
    }

    transfer.waiting("collecting the working files…");
    let handover =
        serve::export_task(config, event_loop, task_id, &peer.id.to_string(), transfer).await?;
    let directory = transfer_directory(config, &handover.transfer_id)?;
    // Written before a single byte travels: a client that restarts after the
    // source committed but before the recipient acknowledged needs to know where
    // to send the acknowledgement, and nothing else records the address.
    write_json(
        &directory.join(DESTINATION_FILE),
        &OutgoingTransfer {
            task_id: handover.task_id.clone(),
            address: address.to_owned(),
            peer: peer.clone(),
        },
    )?;

    client
        .prepare_import(&peer, &handover)
        .await
        .map_err(message)?;
    if let Err(error) = upload_handover(client, &peer, &directory, &handover, transfer).await {
        if transfer.is_stopped() {
            // Nothing has landed there, so dropping the staging is all it takes
            // for that machine to be as it was.
            if let Err(error) = client.abandon_import(&peer, &handover.transfer_id).await {
                tracing::warn!(%error, "could not drop a stopped import at the recipient");
            }
        }
        return Err(error);
    }
    transfer.waiting("starting it…");
    let destination_task = client
        .stage_import(&peer, &handover.transfer_id)
        .await
        .map_err(message)?;

    // The recipient has staged and booted the task, so ownership can move. A
    // failure after this point leaves the destination ticket in place and the
    // transfer resumable rather than stranding the task between machines.
    let source_task = event_loop
        .list()
        .await
        .map_err(message)?
        .into_iter()
        .find(|task| task.id == handover.task_id)
        .ok_or_else(|| "the source task is no longer here".to_owned())?;
    event_loop
        .relinquish(source_task, &handover.transfer_id, &peer.id.to_string())
        .await
        .map_err(message)?;
    client
        .commit_import(&peer, &handover.transfer_id)
        .await
        .map_err(message)?;
    let _ = fs::remove_dir_all(&directory);
    Ok(destination_task)
}

async fn upload_handover(
    client: &LinkClient,
    peer: &EndpointAddr,
    directory: &Path,
    handover: &Handover,
    transfer: &Transfer,
) -> Result<(), String> {
    let mut parts = vec![TransferPart::History];
    if handover.work.len > 0 {
        parts.push(TransferPart::Work);
    }
    if handover.transcript.len > 0 {
        parts.push(TransferPart::Transcript);
    }
    let mut whole = 0;
    for part in &parts {
        whole += part_details(handover, *part)?.2.len;
    }
    for part in parts {
        let (name, filename, archive) = part_details(handover, part)?;
        let path = directory.join(filename);
        verify_file(&path, archive, name)?;
        transfer.counting(whole);
        let mut file = fs::File::open(&path).map_err(message)?;
        let mut piece = vec![0u8; TRANSFER_PIECE_BYTES as usize];
        let mut offset = 0u64;
        while offset < archive.len {
            if transfer.is_stopped() {
                return Err(transfer::STOPPED.to_owned());
            }
            let length = TRANSFER_PIECE_BYTES.min(archive.len - offset) as usize;
            file.read_exact(&mut piece[..length]).map_err(message)?;
            client
                .put_import_part(peer, &handover.transfer_id, part, offset, &piece[..length])
                .await
                .map_err(message)?;
            offset += length as u64;
            transfer.advanced(length as u64);
        }
    }
    Ok(())
}

pub async fn resume_outgoing<E>(
    config: &Config,
    client: &LinkClient,
    event_loop: &TaskEventLoop<E>,
) -> Result<(), String>
where
    E: IsolationEngine + Clone + 'static,
{
    let root = config.data_dir().join(TRANSFERS_DIRECTORY);
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(message(error)),
    };
    for entry in entries {
        let directory = entry.map_err(message)?.path();
        let Ok(bytes) = fs::read(directory.join(DESTINATION_FILE)) else {
            continue;
        };
        let outgoing: OutgoingTransfer = serde_json::from_slice(&bytes).map_err(message)?;
        let Some(transfer_id) = directory.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let recipient = outgoing.peer.id.to_string();
        let history = event_loop
            .events(&outgoing.task_id)
            .await
            .map_err(message)?;
        let committed = history.iter().any(|(_, event)| {
            matches!(event,
                TaskHistoryEvent::TransferCommitted { transfer_id: id, recipient: who }
                if id == transfer_id && who == &recipient)
        });
        if !committed {
            continue;
        }
        match client.commit_import(&outgoing.peer, transfer_id).await {
            Ok(()) => {
                if let Err(error) = fs::remove_dir_all(&directory) {
                    tracing::warn!(%error, transfer_id, "could not remove a finished outgoing transfer");
                }
            }
            Err(error) => tracing::warn!(
                %error,
                transfer_id,
                address = %outgoing.address,
                "an outgoing transfer is still waiting to be acknowledged"
            ),
        }
    }
    Ok(())
}
