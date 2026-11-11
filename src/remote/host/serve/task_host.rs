use crate::isolation::Unwatched;
use crate::remote::protocol::{self, SignalCommand};
use crate::remote::server::Watching;
use crate::remote::{
    HISTORY_FILE, HeldConfig, TRANSCRIPT_FILE, TransferPart, WORK_FILE, WatchEvent,
};

use std::{
    collections::HashSet,
    fs, io,
    io::{Read, Seek, SeekFrom},
    sync::Arc,
};

use crate::task::TaskExecutionPauseReason;
use iroh::EndpointId;
use tokio::{sync::watch, task::spawn_blocking};

use crate::remote::Handover;

use crate::{
    config::InstallationIdentity,
    remote::protocol::{TaskRequest, TaskSummary},
    remote::{
        announce::Phase,
        server::{BoxFuture, TaskHost},
    },
    task::{TaskAsk, TaskHistoryEvent},
};

use super::OwnTasks;
use super::run::{prepare_projects, wait_until_ready};
use super::transfer::{PreparedTransfer, WatchedCollect, export_task, transfer_committed};
use crate::remote::transfer::TransferUpdate;

// Says the machine can be given work again, however the build it was announcing
// ended. A build is announced through a value that outlives it, so the reset has
// to be something the announcement cannot outlive.
struct PhaseWhileBuilding<'a>(&'a watch::Sender<Phase>);

impl Drop for PhaseWhileBuilding<'_> {
    fn drop(&mut self) {
        self.0.send_replace(Phase::Ready);
    }
}

impl TaskHost for OwnTasks {
    fn signal_threads(
        &self,
        peer: EndpointId,
        task_id: String,
    ) -> BoxFuture<Result<Vec<TaskAsk>, String>> {
        let tasks = self.tasks.clone();
        Box::pin(async move {
            let task = tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| {
                    task.id == task_id && task.operator_id() == Some(peer.to_string().as_str())
                })
                .ok_or("this task is not active under your identity")?;
            tasks
                .service()
                .signal_threads(&task.id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn signal(
        &self,
        peer: EndpointId,
        command: protocol::SignalCommand,
    ) -> BoxFuture<Result<(), String>> {
        let tasks = self.tasks.clone();
        Box::pin(async move {
            let task = match &command {
                SignalCommand::Reply {
                    task_id, signal_id, ..
                }
                | SignalCommand::Resolve {
                    task_id, signal_id, ..
                }
                | SignalCommand::Accept { task_id, signal_id } => tasks
                    .service()
                    .task_for_signal(task_id, signal_id)
                    .await
                    .map_err(|error| error.to_string())?,
            }
            .ok_or("this task is no longer active")?;
            if task.operator_id() != Some(peer.to_string().as_str()) {
                return Err("this task belongs to another operator".into());
            }
            if !tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .iter()
                .any(|active| active.id == task.id)
            {
                return Err("this task is no longer active".into());
            }
            match command {
                SignalCommand::Reply {
                    task_id,
                    signal_id,
                    body,
                } => tasks
                    .reply(&task_id, &signal_id, body)
                    .await
                    .map_err(|error| error.to_string())?,
                SignalCommand::Resolve {
                    task_id,
                    signal_id,
                    resolved,
                } => tasks
                    .close_signal(&task_id, &signal_id, resolved)
                    .await
                    .map_err(|error| error.to_string())?,
                SignalCommand::Accept { task_id, signal_id } => {
                    tasks
                        .accept(&task_id, &signal_id)
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        })
    }

    fn input(
        &self,
        peer: EndpointId,
        task_id: String,
        body: String,
    ) -> BoxFuture<Result<(), String>> {
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move {
            let task = tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| {
                    task.id == task_id && task.operator_id() == Some(peer.to_string().as_str())
                })
                .ok_or("this task is not active under your identity")?;
            tasks
                .express_input(&task.id, body)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn take_progress(&self, task_id: &str) -> Option<TransferUpdate> {
        self.collecting
            .lock()
            .ok()
            .and_then(|collecting| collecting.get(task_id).cloned())
    }

    fn hand_over(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<Handover, String>> {
        let tasks = self.tasks.clone();
        let config = self.config.clone();
        let lock = self.preparation_lock.clone();
        let transfers = Arc::clone(&self.transfers);
        let collecting = Arc::clone(&self.collecting);
        Box::pin(async move {
            let _preparing = lock.lock().await;
            if let Some(task) = tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| task.id == task_id)
                && task.operator_id() != Some(peer.to_string().as_str())
            {
                return Err("that task belongs to another operator".into());
            }
            let watched = WatchedCollect {
                task_id: task_id.clone(),
                collecting: Arc::clone(&collecting),
            };
            let handover =
                export_task(&config, &tasks, &task_id, &peer.to_string(), &watched).await;
            if let Ok(mut held) = collecting.lock() {
                held.remove(&task_id);
            }
            let handover = handover?;
            // Packaging is done, so the wait for the first piece starts now rather
            // than from when the task was staged.
            transfers.moved(&handover.transfer_id);
            Ok(handover)
        })
    }

    fn transfer_piece(
        &self,
        peer: EndpointId,
        transfer_id: String,
        part: TransferPart,
        offset: u64,
        len: u64,
    ) -> BoxFuture<Result<Vec<u8>, String>> {
        let directory = self.transfer_directory(&transfer_id);
        let read_slots = Arc::clone(&self.read_slots);
        let transfers = Arc::clone(&self.transfers);
        Box::pin(async move {
            if len == 0 || len > protocol::TRANSFER_PIECE_BYTES {
                return Err("invalid transfer piece length".into());
            }
            let _permit = read_slots
                .acquire_owned()
                .await
                .map_err(|e| e.to_string())?;
            let directory = directory?;
            let prepared: PreparedTransfer = serde_json::from_slice(
                &fs::read(directory.join("transfer.json")).map_err(|e| e.to_string())?,
            )
            .map_err(|_| "this transfer was prepared by another version of altitude".to_owned())?;
            if prepared.recipient != peer.to_string() {
                return Err("this transfer belongs to another recipient".into());
            }
            transfers.moved(&transfer_id);
            let filename = match part {
                TransferPart::History => HISTORY_FILE,
                TransferPart::Work => WORK_FILE,
                TransferPart::Transcript => TRANSCRIPT_FILE,
            };
            let path = directory.join(filename);
            spawn_blocking(move || {
                let mut file = fs::File::open(path).map_err(|e| e.to_string())?;
                let size = file.metadata().map_err(|e| e.to_string())?.len();
                if offset.checked_add(len).is_none_or(|end| end > size) {
                    return Err("transfer piece is outside the archive".into());
                }
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| e.to_string())?;
                let mut bytes = vec![0; len as usize];
                file.read_exact(&mut bytes).map_err(|e| e.to_string())?;
                Ok(bytes)
            })
            .await
            .map_err(|e| e.to_string())?
        })
    }

    fn prepare_import(
        &self,
        peer: EndpointId,
        handover: Handover,
    ) -> BoxFuture<Result<(), String>> {
        let incoming = self.incoming.clone();
        Box::pin(async move { incoming.prepare(peer, handover).await })
    }

    fn put_import_part(
        &self,
        peer: EndpointId,
        transfer_id: String,
        part: TransferPart,
        offset: u64,
        bytes: Vec<u8>,
    ) -> BoxFuture<Result<(), String>> {
        let incoming = self.incoming.clone();
        Box::pin(async move { incoming.put(peer, &transfer_id, part, offset, &bytes).await })
    }

    fn abandon_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<(), String>> {
        let incoming = self.incoming.clone();
        Box::pin(async move { incoming.abandon(peer, &transfer_id).await })
    }

    fn abandon_take(&self, peer: EndpointId, transfer_id: String) -> BoxFuture<Result<(), String>> {
        let tasks = Arc::clone(&self.tasks);
        let root = self.config.data_dir().join("transfers");
        Box::pin(async move {
            let mut staged = None;
            for task in tasks.list().await.map_err(|error| error.to_string())? {
                let history = tasks
                    .events(&task.id)
                    .await
                    .map_err(|error| error.to_string())?;
                let matches = history.iter().rev().find_map(|(_, event)| match event {
                    TaskHistoryEvent::TransferStaged {
                        transfer_id: current,
                        peer: recipient,
                        ..
                    } => Some(current == &transfer_id && recipient == &peer.to_string()),
                    _ => None,
                });
                if matches == Some(true) {
                    staged = Some(task);
                    break;
                }
            }
            let task = staged.ok_or("that transfer is no longer staged")?;
            tasks
                .abandon_transfer(&task.id, &transfer_id)
                .await
                .map_err(|error| error.to_string())?;
            for name in [WORK_FILE, TRANSCRIPT_FILE, HISTORY_FILE] {
                let _ = fs::remove_file(root.join(&transfer_id).join(name));
            }
            Ok(())
        })
    }

    fn stage_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<String, String>> {
        let incoming = self.incoming.clone();
        let tasks = Arc::clone(&self.tasks);
        let watchers = Arc::clone(&self.watchers);
        Box::pin(async move {
            let task_id = incoming.stage(peer, &transfer_id).await?;
            // A landing is a change like any other, and the machine the task left is
            // watching: without this the task seems to vanish until something asks
            // again, which is the one thing a person checking on it will not do.
            if let Ok(all) = tasks.list().await
                && let Some(task) = all.iter().find(|task| task.id == task_id)
            {
                super::publish_change(&watchers, &tasks, task).await;
            }
            Ok(task_id)
        })
    }

    fn commit_import(
        &self,
        peer: EndpointId,
        transfer_id: String,
    ) -> BoxFuture<Result<(), String>> {
        let incoming = self.incoming.clone();
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move {
            incoming.commit(peer, &transfer_id).await?;
            // The task this transfer landed, named by the transfer rather than guessed
            // from which one looks like an arrival: it was finished with by a person
            // rather than staged for one, so the machine that owns it runs it.
            for task in tasks.list().await.map_err(|e| e.to_string())? {
                let landed_here = tasks
                    .events(&task.id)
                    .await
                    .map_err(|e| e.to_string())?
                    .iter()
                    .any(|(_, event)| {
                        matches!(event,
                            TaskHistoryEvent::TransferStaged { transfer_id: landed, .. }
                            if landed == &transfer_id)
                    });
                if landed_here {
                    tasks.admit();
                    break;
                }
            }
            Ok(())
        })
    }

    fn handed_over(&self, peer: EndpointId, transfer_id: String) -> BoxFuture<Result<(), String>> {
        let directory = self.transfer_directory(&transfer_id);
        let tasks = Arc::clone(&self.tasks);
        let preparation_lock = Arc::clone(&self.preparation_lock);
        let transfers = Arc::clone(&self.transfers);
        Box::pin(async move {
            let _preparing = preparation_lock.lock().await;
            let directory = directory?;
            let prepared: PreparedTransfer = serde_json::from_slice(
                &fs::read(directory.join("transfer.json")).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            if prepared.recipient != peer.to_string() {
                return Err("this transfer belongs to another recipient".into());
            }
            if transfer_committed(&tasks, &prepared).await? {
                return Ok(());
            }
            let task = tasks
                .list()
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .find(|task| task.id == prepared.handover.task_id)
                .ok_or_else(|| "the source task is missing".to_owned())?;
            if task.facts.execution_pause_reason != Some(TaskExecutionPauseReason::Transfer) {
                return Err("the source task is no longer staged for transfer".into());
            }
            let matches_preparation = tasks
                .events(&task.id)
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .rev()
                .find_map(|(_, event)| match event {
                    TaskHistoryEvent::TransferStaged {
                        transfer_id: current,
                        peer: recipient,
                        ..
                    } => Some(current == transfer_id && recipient == peer.to_string()),
                    _ => None,
                })
                .unwrap_or(false);
            if !matches_preparation {
                return Err("the task belongs to another transfer preparation".into());
            }
            tasks
                .relinquish(task, &transfer_id, &peer.to_string())
                .await
                .map_err(|e| e.to_string())?;
            for name in [WORK_FILE, TRANSCRIPT_FILE, HISTORY_FILE] {
                let _ = fs::remove_file(directory.join(name));
            }
            // It arrived, so there is no moment left at which it would be stale.
            transfers.finished(&transfer_id);
            Ok(())
        })
    }

    fn configure(&self, peer: EndpointId, forget_logins: bool) -> BoxFuture<Result<(), String>> {
        let config = Arc::clone(&self.config);
        let tasks = Arc::clone(&self.tasks);
        let phase = self.readiness.clone();
        let mut runtime = self.runtime.clone();
        Box::pin(async move {
            wait_until_ready(&mut runtime).await?;
            let owner = peer.to_string();
            let settings = config
                .for_operator(&owner)
                .map_err(|e| e.to_string())?
                .load_or_create_settings()
                .map_err(|e| e.to_string())?;
            let agent = settings.settings().agent().unwrap_or_default();
            // Scoped to the build: finished, failed, or dropped with the link that
            // asked for it — a phase left naming a project that stopped building is
            // a word that never moves again, and "installing tools" with no end
            // reads as a hang as long as the machine stays up.
            let built = {
                let _building = PhaseWhileBuilding(&phase);
                prepare_projects(
                    &tasks,
                    &owner,
                    settings.settings().projects(),
                    agent,
                    &phase,
                )
                .await
            };
            built?;
            // What was pushed is the complete set of projects this machine runs
            // for the operator, so anything else under its store is work that
            // moved away. A project with a task in play keeps its builds until the
            // task is no longer here.
            let configured: HashSet<String> = settings
                .settings()
                .projects()
                .iter()
                .map(|project| project.id.clone())
                .collect();
            let mut keep = configured.clone();
            for task in tasks.list().await.map_err(|e| e.to_string())? {
                if task.operator_id() == Some(owner.as_str())
                    && let Some(project) = task.project()
                {
                    keep.insert(project.id.clone());
                }
            }
            let nothing_theirs = configured.is_empty() && keep.is_empty();
            let scoped = config.for_operator(&owner).map_err(|e| e.to_string())?;
            let pruned = scoped.clone();
            let removed = spawn_blocking(move || pruned.prune_projects(&keep))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            if !removed.is_empty() {
                tracing::info!(
                    count = removed.len(),
                    "forgot projects this machine no longer runs"
                );
            }
            // A machine that runs none of an operator's projects keeps none of
            // their logins. A task still in play needs them, so its presence
            // postpones this until the task is no longer here.
            if forget_logins && nothing_theirs {
                let directory = scoped.scoped_data_dir().to_path_buf();
                spawn_blocking(move || {
                    // What this machine said it held is gone with the files, or a
                    // later push would think they were still here and skip them.
                    let held_path = directory.join("profiles.json");
                    let mut held = HeldConfig::load(&held_path);
                    let profiles = held
                        .profiles
                        .iter()
                        .map(|profile| profile.profile.clone())
                        .collect::<Vec<_>>();
                    scoped.forget_profiles(&profiles)?;
                    held.profiles.clear();
                    held.save(&held_path)?;
                    Ok::<(), io::Error>(())
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
                tracing::info!(operator = %owner, "forgot logins for a machine with no projects");
            }
            Ok(())
        })
    }

    fn run(&self, peer: EndpointId, request: TaskRequest) -> BoxFuture<Result<(), String>> {
        let config = Arc::clone(&self.config);
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move {
            let owner = peer.to_string();
            let settings = config
                .for_operator(&owner)
                .map_err(|e| e.to_string())?
                .load_or_create_settings()
                .map_err(|e| e.to_string())?;
            let project = settings
                .settings()
                .project(&request.project_id)
                .cloned()
                .ok_or_else(|| format!("this machine has no project {}", request.project_id))?;
            tasks
                .prepare_project_as_operator(&owner, &project, request.agent, &Unwatched)
                .await
                .map_err(|e| e.to_string())?;
            let operator = InstallationIdentity {
                id: owner,
                label: request.identity_label,
            };
            match request.origin {
                protocol::TaskOrigin::Intent(intent) => {
                    tasks
                        .service()
                        .create(intent, project, request.agent, Some(operator))
                        .await
                        .map_err(|error| error.to_string())?;
                    tasks.admit();
                }
                // An issue a client linked is a reference it keeps to itself: the
                // work was asked for on the client, so that is where its conversation
                // and its action stay. Only a delegation makes the tracker the place
                // the work is talked about.
                protocol::TaskOrigin::Issue(issue) => {
                    tasks
                        .create_issue_for_operator(*issue, project, request.agent, operator, None)
                        .await
                        .map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        })
    }

    fn list(&self, peer: EndpointId) -> BoxFuture<Result<Vec<TaskSummary>, String>> {
        let tasks = Arc::clone(&self.tasks);
        Box::pin(async move {
            let owner = peer.to_string();
            let all = tasks.list().await.map_err(|e| e.to_string())?;
            let standings = tasks.standings(&all);
            Ok(all
                .iter()
                .filter(|task| task.operator_id() == Some(owner.as_str()))
                .map(|task| super::summary_of(task, standings.get(&task.id)))
                .collect())
        })
    }

    fn watch(&self, peer: EndpointId) -> BoxFuture<Result<Watching, String>> {
        let tasks = Arc::clone(&self.tasks);
        let watchers = Arc::clone(&self.watchers);
        let readiness = self.readiness.clone();
        Box::pin(async move {
            // Registered before the snapshot is read, so a change made while it is
            // being read is queued behind it rather than lost.
            let (say, changes) = watchers.subscribe(peer);
            // Read after subscribing, not before: a build that finishes in between
            // is the one case this read could miss, and there the broadcast that
            // follows it reaches a peer already in the list.
            if readiness.borrow().is_ready() {
                let _ = say.send(WatchEvent::Ready);
            }
            let owner = peer.to_string();
            let all = tasks.list().await.map_err(|error| error.to_string())?;
            let standings = tasks.standings(&all);
            let snapshot = all
                .iter()
                .filter(|task| task.operator_id() == Some(owner.as_str()))
                .map(|task| super::summary_of(task, standings.get(&task.id)))
                .collect();
            Ok(Watching { snapshot, changes })
        })
    }

    fn remove(&self, peer: EndpointId, task_id: String) -> BoxFuture<Result<(), String>> {
        let tasks = Arc::clone(&self.tasks);
        let tracker = self.tracker.clone();
        Box::pin(async move {
            let owner = peer.to_string();
            let task = tasks
                .list()
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|task| task.id == task_id)
                .ok_or("that task is no longer here")?;
            if task.operator_id() != Some(owner.as_str()) {
                return Err("that task belongs to another operator".into());
            }
            tasks
                .remove_and_wait(task.clone())
                .await
                .map_err(|error| error.to_string())?;
            // Deleting a task is the operator saying the work is gone. The issue
            // should stop pointing at this machine, and its thread should not look
            // like it is still thinking.
            if let Some(tracker) = &tracker {
                tracker.forget_task(&task.id).await;
            }
            Ok(())
        })
    }
}
