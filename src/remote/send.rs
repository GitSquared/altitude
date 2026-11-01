use std::{
    fmt, fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process,
    sync::Arc,
};

use iroh::{EndpointAddr, EndpointId};
use tokio::sync::Mutex;

use crate::isolation::private_directory;

use crate::{
    config::{Config, DEFAULT_PORT},
    isolation::{IsolationEngine, MicrosandboxIsolation, UnwatchedFiles},
    remote::{
        Compatibility, ConfigBundle, ConfigPush, Handover, HandoverRecord, LinkClient,
        RemoteKeyStatus, RemoteKeys, Transfer, TransferArchive, TransferPart, meet,
        protocol::{MAX_BLOB_BYTES, TRANSFER_PIECE_BYTES},
    },
    task::{HomeFuture, SendsTasksHome, Task, TaskArrival, TaskEventLoop, TaskHistoryEvent},
};

const TRANSFERS_DIRECTORY: &str = "transfers";
const INCOMING_DIRECTORY: &str = "incoming";
const DESTINATION_FILE: &str = "outgoing.json";
const TRANSFER_FILE: &str = "transfer.json";
const COMMITTED_FILE: &str = "committed";
const HISTORY_FILE: &str = "history.json";
const WORK_FILE: &str = "work.tar.zst";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";

fn message(error: impl fmt::Display) -> String {
    error.to_string()
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OutgoingTransfer {
    task_id: String,
    address: String,
    peer: EndpointAddr,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct IncomingTransfer {
    peer: String,
    handover: Handover,
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
            .announce_transfer(address.clone(), transfer.watching());
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

    transfer.waiting("collecting the working files…");
    let handover =
        super::serve::export_task(config, event_loop, task_id, &peer.id.to_string(), transfer)
            .await?;
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

    pub async fn prepare(&self, peer: EndpointId, handover: Handover) -> Result<(), String> {
        let _serialized = self.lock.lock().await;
        let transfer_id = validated_transfer_id(&handover.transfer_id)?;
        if handover.history.len == 0 || handover.history.len > MAX_BLOB_BYTES {
            return Err("the task history is outside the transfer limit".into());
        }
        validate_archive(&handover.history, "history")?;
        validate_archive(&handover.work, "work")?;
        validate_archive(&handover.transcript, "transcript")?;
        let metadata = IncomingTransfer {
            peer: peer.to_string(),
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
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                write_json(&path, &metadata)?;
                Ok(())
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
        if handover.work.len > 0 {
            verify_file(&directory.join(WORK_FILE), &handover.work, "work")?;
        }
        if handover.transcript.len > 0 {
            verify_file(
                &directory.join(TRANSCRIPT_FILE),
                &handover.transcript,
                "transcript",
            )?;
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
        let files = unpack_task_files(&directory, handover)?;
        let task = self
            .tasks
            .land(TaskArrival {
                intent: record.draft.intent,
                project: record.draft.project,
                agent: record.draft.agent,
                conversation: record.draft.agent_conversation_id,
                history,
                files,
                transfer_id: transfer_id.clone(),
                peer: metadata.peer.clone(),
            })
            .await
            .map_err(message)?;
        self.tasks
            .verify_landing(task.clone())
            .await
            .map_err(message)?;
        self.tasks
            .deliver_arrivals(&task, &UnwatchedFiles)
            .await
            .map_err(message)?;
        Ok(task.id)
    }

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
        for name in [HISTORY_FILE, WORK_FILE, TRANSCRIPT_FILE] {
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

fn unpack_task_files(directory: &Path, handover: &Handover) -> Result<Option<PathBuf>, String> {
    if handover.work.len == 0 && handover.transcript.len == 0 {
        return Ok(None);
    }
    let files = directory.join("files");
    let _ = fs::remove_dir_all(&files);
    fs::create_dir_all(&files).map_err(message)?;
    if handover.work.len > 0 {
        let status = process::Command::new("tar")
            .arg("-xzf")
            .arg(directory.join(WORK_FILE))
            .arg("-C")
            .arg(&files)
            .status()
            .map_err(|error| format!("could not run tar: {error}"))?;
        if !status.success() {
            return Err("the working files could not be unpacked".to_owned());
        }
    }
    if handover.transcript.len > 0 {
        fs::rename(
            directory.join(TRANSCRIPT_FILE),
            files.join("transcript.jsonl"),
        )
        .map_err(message)?;
    }
    Ok(Some(files))
}

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
        TransferPart::Work => ("work", WORK_FILE, &handover.work),
        TransferPart::Transcript => ("transcript", TRANSCRIPT_FILE, &handover.transcript),
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
