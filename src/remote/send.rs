//! Sending a task that is already here to a machine that can carry it, and
//! receiving one.
//!
//! A transfer is not a copy. The source stops the task and keeps ownership while
//! the destination stages the exact environment, imports the disk delta, and
//! boots it; only then does the source relinquish, and only then does the
//! destination begin. Either side can stop between any two steps, and what was
//! recorded is enough to finish the acknowledgement or leave the work stopped
//! for the operator to retry deliberately.
//!
//! The coordinator wires this to the link and the task loop; nothing here opens
//! a connection of its own or decides when a task runs.

use std::{
    fmt, fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use iroh::{EndpointAddr, EndpointId};
use microsandbox::Snapshot;
use tokio::sync::Mutex;

use crate::isolation::private_directory;

use crate::{
    config::{Config, DEFAULT_PORT},
    isolation::{IsolationEngine, MicrosandboxIsolation, capsule::verify_environment},
    remote::{
        ConfigBundle, ConfigPush, Handover, HandoverRecord, LinkClient, RemoteKeyStatus,
        RemoteKeys, Transfer, TransferArchive, TransferPart, meet,
        protocol::{MAX_BLOB_BYTES, TRANSFER_PIECE_BYTES},
    },
    task::{Task, TaskArrival, TaskEventLoop, TaskHistoryEvent},
};

const TRANSFERS_DIRECTORY: &str = "transfers";
const INCOMING_DIRECTORY: &str = "incoming";
const DESTINATION_FILE: &str = "outgoing.json";
const TRANSFER_FILE: &str = "transfer.json";
const COMMITTED_FILE: &str = "committed";
const HISTORY_FILE: &str = "history.json";
const ENVIRONMENT_FILE: &str = "environment.tar.zst";
const CAPSULE_FILE: &str = "capsule.tar.zst";

fn message(error: impl fmt::Display) -> String {
    error.to_string()
}

/// What the source keeps so a restarted client can finish an outgoing transfer
/// whose final acknowledgement was lost.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OutgoingTransfer {
    task_id: String,
    address: String,
    peer: EndpointAddr,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct IncomingTransfer {
    peer: String,
    needs_environment: bool,
    handover: Handover,
}

/// Send an existing local task to a remote, and return the id the remote gave it.
///
/// The remote must be the one this address was approved as, and it must hold this
/// client's setup before the capsule travels, because the capsule is a delta on
/// top of an environment the remote builds for itself.
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
    if project.host.as_deref() != Some(address) {
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
        super::Logins::Always,
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

    // Packing is the first wait, and it has nothing to count: the archives do not
    // exist until it finishes.
    transfer.waiting("packaging…");
    let handover =
        super::serve::export_task(config, event_loop, task_id, &peer.id.to_string()).await?;
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

    let needs_environment = client
        .prepare_import(&peer, &handover)
        .await
        .map_err(message)?;
    if let Err(error) = upload_handover(
        client,
        &peer,
        &directory,
        &handover,
        needs_environment,
        transfer,
    )
    .await
    {
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
    needs_environment: bool,
    transfer: &Transfer,
) -> Result<(), String> {
    let mut parts = vec![TransferPart::History];
    if needs_environment && handover.environment.is_some() {
        parts.push(TransferPart::Environment);
    }
    if handover.capsule.len > 0 {
        parts.push(TransferPart::Capsule);
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
                return Err(super::transfer::STOPPED.to_owned());
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

/// Finish outgoing transfers the source already committed but never
/// acknowledged, after a client restart.
///
/// Only transfers whose source history records the commitment are touched, and
/// only the acknowledgement is retried. One still stopped at the source is left
/// where the operator can see it and decide.
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

/// The receiving side of one machine's incoming transfers.
///
/// Every file it writes is under one directory named by a validated transfer id,
/// and every request is bound to the peer that authenticated it. It never reads
/// or writes a path a caller supplied.
#[derive(Clone)]
pub struct IncomingTransfers {
    config: Config,
    tasks: Arc<TaskEventLoop<MicrosandboxIsolation>>,
    lock: Arc<Mutex<()>>,
}

impl IncomingTransfers {
    pub fn new(config: Config, tasks: Arc<TaskEventLoop<MicrosandboxIsolation>>) -> Self {
        Self {
            config,
            tasks,
            lock: Arc::new(Mutex::new(())),
        }
    }

    /// Accept the shape of a transfer, write down who sent it, and say whether
    /// the environment archive has to travel.
    ///
    /// The environment is skipped when the exact base is already here: the
    /// capsule is a delta, and a base that matches by descriptor is the base the
    /// delta was taken against.
    pub async fn prepare(&self, peer: EndpointId, handover: Handover) -> Result<bool, String> {
        let _serialized = self.lock.lock().await;
        let transfer_id = validated_transfer_id(&handover.transfer_id)?;
        if handover.history.len == 0 || handover.history.len > MAX_BLOB_BYTES {
            return Err("the task history is outside the transfer limit".into());
        }
        validate_archive(&handover.history, "history")?;
        validate_archive(&handover.capsule, "capsule")?;
        if let Some(environment) = &handover.environment {
            validate_archive(&environment.archive, "environment")?;
        }
        let needs_environment = match &handover.environment {
            Some(base) => !Snapshot::open(&base.snapshot_id)
                .await
                .ok()
                .is_some_and(|snapshot| snapshot.digest() == base.descriptor_digest),
            None => false,
        };
        let metadata = IncomingTransfer {
            peer: peer.to_string(),
            needs_environment,
            handover,
        };
        let directory = self.incoming_directory(&transfer_id);
        private_directory(&directory).map_err(message)?;
        let path = directory.join(TRANSFER_FILE);
        match fs::read(&path) {
            Ok(bytes) => {
                let existing: IncomingTransfer = serde_json::from_slice(&bytes).map_err(message)?;
                if existing.peer != metadata.peer || existing.handover != metadata.handover {
                    return Err("this transfer is already prepared for another recipient".into());
                }
                Ok(existing.needs_environment)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                write_json(&path, &metadata)?;
                Ok(needs_environment)
            }
            Err(error) => Err(message(error)),
        }
    }

    pub async fn put(
        &self,
        peer: EndpointId,
        transfer_id: &str,
        part: TransferPart,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), String> {
        let _serialized = self.lock.lock().await;
        if bytes.is_empty() || bytes.len() as u64 > TRANSFER_PIECE_BYTES {
            return Err("invalid transfer piece length".into());
        }
        let transfer_id = validated_transfer_id(transfer_id)?;
        let metadata = self.metadata(&transfer_id, peer)?;
        let (_, filename, archive) = part_details(&metadata.handover, part)?;
        let end = offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| "transfer piece offset overflow".to_owned())?;
        if end > archive.len {
            return Err("transfer piece is outside the archive".into());
        }
        write_piece(
            &self.incoming_directory(&transfer_id).join(filename),
            offset,
            bytes,
        )
    }

    /// Drop a staged import the source stopped sending.
    ///
    /// Nothing here has been landed, so there is nothing to undo: the staging
    /// directory goes and the source keeps its task.
    pub async fn abandon(&self, peer: EndpointId, transfer_id: &str) -> Result<(), String> {
        let _serialized = self.lock.lock().await;
        let transfer_id = validated_transfer_id(transfer_id)?;
        self.metadata(&transfer_id, peer)?;
        match fs::remove_dir_all(self.incoming_directory(&transfer_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(message(error)),
        }
    }

    /// Verify the arriving archives, land the task on the exact environment, and
    /// boot it, returning the id this machine gave it.
    ///
    /// Landing and booting happen before any acknowledgement, because the source
    /// keeps ownership until the recipient has really run something.
    pub async fn stage(&self, peer: EndpointId, transfer_id: &str) -> Result<String, String> {
        let _serialized = self.lock.lock().await;
        let transfer_id = validated_transfer_id(transfer_id)?;
        let metadata = self.metadata(&transfer_id, peer)?;
        let directory = self.incoming_directory(&transfer_id);
        if let Some(task) = self.landed_task(&transfer_id, &metadata.peer).await? {
            self.tasks
                .verify_landing(task.clone())
                .await
                .map_err(message)?;
            return Ok(task.id);
        }
        let handover = &metadata.handover;
        verify_file(&directory.join(HISTORY_FILE), &handover.history, "history")?;
        if let Some(environment) = &handover.environment {
            let path = directory.join(ENVIRONMENT_FILE);
            if path.exists() {
                verify_file(&path, &environment.archive, "environment")?;
            }
        }
        if handover.capsule.len > 0 {
            verify_file(&directory.join(CAPSULE_FILE), &handover.capsule, "capsule")?;
        }
        let record: HandoverRecord =
            serde_json::from_reader(fs::File::open(directory.join(HISTORY_FILE)).map_err(message)?)
                .map_err(message)?;
        let history = record
            .events
            .into_iter()
            .map(|recorded| (recorded.recorded_at_ms, recorded.event))
            .collect::<Vec<_>>();
        refuse_foreign_founder(&history, &metadata.peer)?;
        self.tasks
            .prepare_project_as_operator(&metadata.peer, &record.draft.project, record.draft.agent)
            .await
            .map_err(message)?;
        let capsule = import_snapshots(&directory, handover).await?;
        let task = self
            .tasks
            .land(TaskArrival {
                intent: record.draft.intent,
                project: record.draft.project,
                agent: record.draft.agent,
                conversation: record.draft.agent_conversation_id,
                history,
                capsule,
                transfer_id: transfer_id.clone(),
                peer: metadata.peer.clone(),
            })
            .await
            .map_err(message)?;
        self.tasks
            .verify_landing(task.clone())
            .await
            .map_err(message)?;
        Ok(task.id)
    }

    /// Begin the landed task, and only now let its archives go.
    ///
    /// The committed marker is written and flushed before the archives are
    /// removed, so a repeated commit after a crash finds either the archives
    /// still there or a record that they are not needed.
    pub async fn commit(&self, peer: EndpointId, transfer_id: &str) -> Result<(), String> {
        let _serialized = self.lock.lock().await;
        let transfer_id = validated_transfer_id(transfer_id)?;
        let metadata = self.metadata(&transfer_id, peer)?;
        let directory = self.incoming_directory(&transfer_id);
        if directory.join(COMMITTED_FILE).exists() {
            return Ok(());
        }
        let task = self
            .landed_task(&transfer_id, &metadata.peer)
            .await?
            .ok_or_else(|| "the transfer was never staged".to_owned())?;
        let history = self.tasks.events(&task.id).await.map_err(message)?;
        let landed = history.iter().any(|(_, event)| {
            matches!(event,
                TaskHistoryEvent::TransferLanded { transfer_id: id, .. } if id == &transfer_id)
        });
        if !landed {
            self.tasks
                .finish_remote_transfer(&task.id, &transfer_id)
                .await
                .map_err(message)?;
        }
        fs::File::create(directory.join(COMMITTED_FILE))
            .map_err(message)?
            .sync_all()
            .map_err(message)?;
        for name in [HISTORY_FILE, ENVIRONMENT_FILE, CAPSULE_FILE] {
            let _ = fs::remove_file(directory.join(name));
        }
        Ok(())
    }

    fn incoming_directory(&self, transfer_id: &str) -> PathBuf {
        self.config
            .data_dir()
            .join(TRANSFERS_DIRECTORY)
            .join(INCOMING_DIRECTORY)
            .join(transfer_id)
    }

    fn metadata(&self, transfer_id: &str, peer: EndpointId) -> Result<IncomingTransfer, String> {
        let bytes =
            fs::read(self.incoming_directory(transfer_id).join(TRANSFER_FILE)).map_err(message)?;
        let metadata: IncomingTransfer = serde_json::from_slice(&bytes).map_err(message)?;
        if metadata.peer != peer.to_string() {
            return Err("this transfer belongs to another recipient".into());
        }
        Ok(metadata)
    }

    /// The task this transfer already landed here, derived from its own history
    /// rather than kept as a second fact that could disagree.
    async fn landed_task(&self, transfer_id: &str, peer: &str) -> Result<Option<Task>, String> {
        for task in self.tasks.list().await.map_err(message)? {
            let history = self.tasks.events(&task.id).await.map_err(message)?;
            let landed = history.iter().any(|(_, event)| {
                matches!(event,
                    TaskHistoryEvent::TransferStaged { transfer_id: id, peer: who, .. }
                    if id == transfer_id && who == peer)
            });
            if landed {
                return Ok(Some(task));
            }
        }
        Ok(None)
    }
}

async fn import_snapshots(directory: &Path, handover: &Handover) -> Result<Option<String>, String> {
    let Some(environment) = &handover.environment else {
        return if handover.capsule.len == 0 {
            Ok(None)
        } else {
            Err("the disk capsule has no exact environment".into())
        };
    };
    let cached = Snapshot::open(&environment.snapshot_id)
        .await
        .ok()
        .filter(|snapshot| snapshot.digest() == environment.descriptor_digest);
    let base = match cached {
        Some(snapshot) => snapshot,
        None => {
            let snapshot = Snapshot::load(&directory.join(ENVIRONMENT_FILE), None)
                .await
                .map_err(message)?
                .open()
                .await
                .map_err(message)?;
            if snapshot.digest() != environment.descriptor_digest
                || snapshot.manifest().snapshot_id.to_string() != environment.snapshot_id
            {
                return Err("the environment descriptor differs from the prepared base".into());
            }
            snapshot
        }
    };
    verify_environment(&base, &environment.layer_roots).await?;
    if handover.capsule.len == 0 {
        return Ok(None);
    }
    let loaded = Snapshot::load_with_base(
        &directory.join(CAPSULE_FILE),
        None,
        &base.path().map_err(message)?.to_string_lossy(),
    )
    .await
    .map_err(message)?;
    Ok(Some(
        loaded
            .path()
            .map_err(message)?
            .to_string_lossy()
            .into_owned(),
    ))
}

/// Refuse history founded under an identity other than the one that authenticated.
///
/// A caller can prove who it is but not who it was, so a task founded by another
/// operator is never adopted under the sender's key.
fn refuse_foreign_founder(history: &[(i64, TaskHistoryEvent)], peer: &str) -> Result<(), String> {
    let founder = history.iter().find_map(|(_, event)| match event {
        TaskHistoryEvent::IntentExpressed { identity_id, .. } => Some(identity_id.as_str()),
        _ => None,
    });
    match founder {
        Some(identity) if identity == peer => Ok(()),
        _ => Err("the arriving task was founded by another operator".into()),
    }
}

fn part_details(
    handover: &Handover,
    part: TransferPart,
) -> Result<(&'static str, &'static str, &TransferArchive), String> {
    Ok(match part {
        TransferPart::History => ("history", HISTORY_FILE, &handover.history),
        TransferPart::Capsule => ("capsule", CAPSULE_FILE, &handover.capsule),
        TransferPart::Environment => (
            "environment",
            ENVIRONMENT_FILE,
            handover
                .environment
                .as_ref()
                .map(|environment| &environment.archive)
                .ok_or_else(|| "this transfer carries no environment".to_owned())?,
        ),
    })
}

fn validated_transfer_id(id: &str) -> Result<String, String> {
    uuid::Uuid::parse_str(id)
        .map(|parsed| parsed.to_string())
        .map_err(|_| "invalid transfer identifier".to_owned())
}

fn validate_archive(archive: &TransferArchive, name: &str) -> Result<(), String> {
    let digest = &archive.digest;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("the {name} archive has an unreadable digest"));
    }
    Ok(())
}

fn transfer_directory(config: &Config, transfer_id: &str) -> Result<PathBuf, String> {
    let transfer_id = validated_transfer_id(transfer_id)?;
    Ok(config
        .data_dir()
        .join(TRANSFERS_DIRECTORY)
        .join(transfer_id))
}

fn write_piece(path: &Path, offset: u64, bytes: &[u8]) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(message)?;
    let length = file.metadata().map_err(message)?.len();
    if offset > length {
        return Err("transfer pieces must arrive without gaps".into());
    }
    if length > offset {
        let overlap = (length - offset).min(bytes.len() as u64) as usize;
        let mut existing = vec![0u8; overlap];
        file.seek(SeekFrom::Start(offset)).map_err(message)?;
        file.read_exact(&mut existing).map_err(message)?;
        if existing != bytes[..overlap] {
            return Err("a transfer piece conflicts with bytes already staged".into());
        }
    }
    file.seek(SeekFrom::Start(offset)).map_err(message)?;
    file.write_all(bytes).map_err(message)?;
    file.sync_all().map_err(message)
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(message)?;
    let incoming = path.with_extension("json.incoming");
    let mut file = fs::File::create(&incoming).map_err(message)?;
    file.write_all(&bytes).map_err(message)?;
    file.sync_all().map_err(message)?;
    fs::rename(&incoming, path).map_err(message)
}

fn verify_file(path: &Path, archive: &TransferArchive, name: &str) -> Result<(), String> {
    let length = fs::metadata(path).map_err(message)?.len();
    if length != archive.len {
        return Err(format!(
            "the {name} archive is {length} bytes, not {}",
            archive.len
        ));
    }
    if file_digest(path)? != archive.digest {
        return Err(format!("the {name} archive does not match its digest"));
    }
    Ok(())
}

fn file_digest(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(message)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let read = file.read(&mut buffer).map_err(message)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}
