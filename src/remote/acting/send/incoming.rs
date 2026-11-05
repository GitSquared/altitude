use std::{
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use iroh::EndpointId;
use tokio::sync::Mutex;

use crate::{
    config::Config,
    isolation::{MicrosandboxIsolation, Unwatched, UnwatchedFiles, private_directory},
    remote::{
        HISTORY_FILE, Handover, HandoverRecord, TRANSCRIPT_FILE, TransferPart, WORK_FILE,
        WORK_PLAIN_FILE, archive,
        protocol::{MAX_BLOB_BYTES, TRANSFER_PIECE_BYTES},
    },
    task::{Task, TaskArrival, TaskEventLoop, TaskHistoryEvent},
};

use super::{
    COMMITTED_FILE, INCOMING_DIRECTORY, TRANSFER_FILE, TRANSFERS_DIRECTORY, message, part_details,
    validate_archive, validated_transfer_id, verify_file, write_json,
};

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct IncomingTransfer {
    peer: String,
    handover: Handover,
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
        tracing::info!(
            %transfer_id,
            peer = %peer,
            work_bytes = handover.work.len,
            transcript_bytes = handover.transcript.len,
            "a remote is handing over a task"
        );
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
        let metadata = self.metadata(&transfer_id, peer)?;
        // A sender that gives up has given up on the copy it made here too. It is
        // not work that arrived: nobody finished with it, and leaving it behind
        // would make one task into two, on two machines, with the same issue.
        if let Some(task) = self.landed_task(&transfer_id, &metadata.peer).await?
            && let Err(error) = self.tasks.remove_and_wait(task).await
        {
            tracing::warn!(
                %error,
                transfer_id,
                "could not take back a task whose move was abandoned"
            );
        }
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
            .prepare_project_as_operator(
                &metadata.peer,
                &record.draft.project,
                record.draft.agent,
                &Unwatched,
            )
            .await
            .map_err(message)?;
        let files = unpack_task_files(&directory, handover).await?;
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
        // Provisioning is part of landing, and the task it returns is the one that
        // names this machine's sandbox: delivering into the record that arrived would
        // be reaching for the sandbox of the machine the task came from.
        let task = self.tasks.verify_landing(task).await.map_err(message)?;
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
        // What was filed under the task's own id is the copy that matters now.
        let _ = fs::remove_dir_all(directory.join("files"));
        tracing::info!(%transfer_id, "a task's transfer is committed here");
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

// The work archive arrives compressed and is expanded here, because the sandbox it
// is about to go into reads a tree by offset. Nothing is unpacked into this disk.

async fn unpack_task_files(
    directory: &Path,
    handover: &Handover,
) -> Result<Option<PathBuf>, String> {
    if handover.work.len == 0 && handover.transcript.len == 0 {
        return Ok(None);
    }
    let files = directory.join("files");
    let _ = fs::remove_dir_all(&files);
    fs::create_dir_all(&files).map_err(message)?;
    if handover.work.len > 0 {
        let packed = directory.join(WORK_FILE);
        let plain = files.join(WORK_PLAIN_FILE);
        // The compressed archive stays until the transfer is committed: taking the
        // same task again after an interruption checks it, and does not need it
        // expanded twice.
        if !plain.is_file() {
            let started = Instant::now();
            let bytes = archive::expand(packed, plain)
                .await
                .map_err(|error| format!("the working files could not be expanded: {error}"))?;
            tracing::info!(
                bytes,
                packed_bytes = handover.work.len,
                elapsed_ms = started.elapsed().as_millis(),
                "expanded a task's arriving working files"
            );
        }
    }
    if handover.transcript.len > 0 {
        fs::rename(directory.join(TRANSCRIPT_FILE), files.join(TRANSCRIPT_FILE))
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
