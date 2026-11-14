use std::{
    collections::HashMap,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

use iroh::EndpointAddr;
use tempfile::NamedTempFile;
use tokio::fs::File as AsyncFile;
use tokio::sync::watch;

use crate::{
    config::Config,
    isolation::{
        BaseSandboxRecipe, BaseSandboxSnapshot, CommandSession, CredentialCheck, CredentialReport,
        CredentialSandbox, EnvironmentDescription, EnvironmentProgress, EnvironmentProgressSink,
        FileTally, FileTransferSink, IsolationEngine, IsolationError, IsolationStatus, LayerView,
        Pressure, ProfileRecipe, SandboxCeiling, SandboxOptions, SandboxPurpose, SandboxRoom,
        VsockRoute, guest_absolute,
        worker_protocol::{WorkerCall, WorkerProfile, WorkerReply, WorkerRequest},
    },
    project::Project,
    remote::LinkClient,
};

use crate::remote::bundle::ProfileBundle;
use crate::remote::cache::share;

use super::workers::WorkerFleet;

// The isolation provider for a remote that has no sandbox runtime of its own: it
// spawns a disposable host per sandbox and speaks to it over the link. The master
// keeps every task's state; a host only ever holds one sandbox's compute.
#[derive(Clone)]
pub(crate) struct WorkersIsolation {
    fleet: Arc<WorkerFleet>,
    client: LinkClient,
    config: Config,
    placements: Arc<Mutex<HashMap<String, String>>>,
    // The project checkout each sandbox works in, so a snapshot knows what to carry
    // besides the sandbox tree. `None` for a sandbox with no project.
    checkouts: Arc<Mutex<HashMap<String, Option<String>>>>,
    owner: Option<String>,
}

impl WorkersIsolation {
    pub(crate) fn new(fleet: Arc<WorkerFleet>, client: LinkClient, config: &Config) -> Self {
        Self {
            fleet,
            client,
            config: config.clone(),
            placements: Arc::new(Mutex::new(HashMap::new())),
            checkouts: Arc::new(Mutex::new(HashMap::new())),
            owner: None,
        }
    }

    fn placement(&self, id: &str) -> Option<String> {
        self.placements.lock().ok()?.get(id).cloned()
    }

    fn place(&self, id: &str, worker: &str) {
        if let Ok(mut placements) = self.placements.lock() {
            placements.insert(id.to_owned(), worker.to_owned());
        }
    }

    fn remember_checkout(&self, id: &str, checkout: Option<String>) {
        if let Ok(mut checkouts) = self.checkouts.lock() {
            checkouts.insert(id.to_owned(), checkout);
        }
    }

    fn checkout_of(&self, id: &str) -> Option<String> {
        self.checkouts.lock().ok()?.get(id).cloned().flatten()
    }

    async fn sandbox_is_there(&self, worker: &str, id: &str) -> bool {
        matches!(
            self.call_on(worker, WorkerCall::HasSandbox { id: id.to_owned() })
                .await,
            Ok(WorkerReply::Bool(true))
        )
    }

    async fn call_on(&self, worker: &str, call: WorkerCall) -> Result<WorkerReply, IsolationError> {
        let peer = self.fleet.peer(worker).ok_or_else(|| {
            IsolationError::missing_sandbox(format!("isolation worker {worker} is gone"))
        })?;
        let request = WorkerRequest {
            token: self.fleet.token().to_owned(),
            call,
        };
        self.client
            .worker_call(peer, &request)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    async fn provision_on(
        &self,
        worker: &str,
        call: WorkerCall,
        profiles: &[WorkerProfile],
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<WorkerReply, IsolationError> {
        let peer = self.fleet.peer(worker).ok_or_else(|| {
            IsolationError::missing_sandbox(format!("isolation worker {worker} is gone"))
        })?;
        let request = WorkerRequest {
            token: self.fleet.token().to_owned(),
            call,
        };
        let body =
            serde_json::to_vec(profiles).map_err(|error| IsolationError::new(error.to_string()))?;
        tracing::info!(
            worker,
            profile_bytes = body.len(),
            "asking a host for a sandbox"
        );
        let began = Instant::now();
        let answer = self
            .client
            .worker_provision(peer, &request, &body, |says| match says {
                // The operator's line is the host's own words: only it knows which step
                // of a provision it is on.
                WorkerReply::Progress { label } => progress.report(EnvironmentProgress {
                    label,
                    completed: 0,
                    total: 0,
                }),
                // A host's log goes to this machine's journal with the host's name on
                // it, which outlives the host the lines came from.
                WorkerReply::Log { line } => tracing::info!(worker, "{line}"),
                _ => {}
            })
            .await
            .map_err(|error| IsolationError::new(error.to_string()));
        tracing::info!(
            worker,
            elapsed_ms = began.elapsed().as_millis() as u64,
            ok = answer.is_ok(),
            "a host answered"
        );
        answer
    }

    async fn call(&self, id: &str, call: WorkerCall) -> Result<WorkerReply, IsolationError> {
        let worker = self.placement(id).ok_or_else(|| {
            IsolationError::missing_sandbox(format!("sandbox {id} is not on a worker"))
        })?;
        self.call_on(&worker, call).await
    }

    async fn provision_host(&self, id: &str) -> Result<String, IsolationError> {
        let worker = self
            .fleet
            .provision_worker()
            .await
            .map_err(IsolationError::new)?;
        self.place(id, &worker);
        Ok(worker)
    }

    async fn deprovision_host(&self, id: &str) {
        let worker = self.placements.lock().ok().and_then(|mut p| p.remove(id));
        if let Some(worker) = worker {
            self.fleet.deprovision_worker(&worker).await;
        }
    }

    // The archive a parked task left on the master, put back on its new host.
    async fn restore_workspace(
        &self,
        id: &str,
        checkout: Option<&str>,
        snapshot: &str,
    ) -> Result<(), IsolationError> {
        let peer = self.peer_for(id)?;
        let file = AsyncFile::open(snapshot)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let request = WorkerRequest {
            token: self.fleet.token().to_owned(),
            call: WorkerCall::Restore {
                id: id.to_owned(),
                checkout: checkout.map(str::to_owned),
            },
        };
        self.client
            .worker_deliver(peer, &request, file)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    // The host a sandbox lives on, for a call that must reach it directly.
    fn peer_for(&self, id: &str) -> Result<EndpointAddr, IsolationError> {
        let worker = self.placement(id).ok_or_else(|| {
            IsolationError::missing_sandbox(format!("sandbox {id} is not on a worker"))
        })?;
        self.fleet.peer(&worker).ok_or_else(|| {
            IsolationError::missing_sandbox(format!("isolation worker {worker} is gone"))
        })
    }

    // The operator's files for these recipes, gathered as the remote push gathers
    // them, so a host can mount real credentials.
    fn profile_bundles(
        &self,
        recipes: &[ProfileRecipe],
    ) -> Result<Vec<WorkerProfile>, IsolationError> {
        let scope = match &self.owner {
            Some(owner) => self
                .config
                .for_operator(owner)
                .map_err(|error| IsolationError::new(error.to_string()))?,
            None => self.config.clone(),
        };
        let mut profiles = Vec::new();
        for recipe in recipes {
            let bundle = ProfileBundle::of(scope.scoped_data_dir(), &recipe.profile)
                .map_err(|error| IsolationError::new(error.to_string()))?;
            profiles.push(WorkerProfile {
                profile: bundle.profile,
                files: bundle
                    .files
                    .into_iter()
                    .map(|file| (file.path, file.contents))
                    .collect(),
            });
        }
        Ok(profiles)
    }
}

fn pack_tree(source: &Path, tar_path: &Path) -> Result<FileTally, String> {
    let file = fs::File::create(tar_path).map_err(|error| error.to_string())?;
    let mut builder = tar::Builder::new(file);
    builder
        .append_dir_all(".", source)
        .map_err(|error| error.to_string())?;
    builder.finish().map_err(|error| error.to_string())?;
    Ok(count_tree(source))
}

fn count_tree(root: &Path) -> FileTally {
    let mut tally = FileTally::default();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                tally.files += 1;
                tally.bytes += metadata.len();
            }
        }
    }
    tally
}

fn extract_tree(tar_path: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|error| error.to_string())?;
    let file = fs::File::open(tar_path).map_err(|error| error.to_string())?;
    let mut archive = tar::Archive::new(file);
    archive
        .unpack(destination)
        .map_err(|error| error.to_string())
}

fn checkout_guest(project: &Project) -> Option<String> {
    project
        .checkout_path()
        .and_then(|path| guest_absolute(&path).ok())
}

fn expect_ok(reply: WorkerReply) -> Result<(), IsolationError> {
    match reply {
        WorkerReply::Ok => Ok(()),
        WorkerReply::Failed(reason) => Err(IsolationError::new(reason)),
        other => Err(IsolationError::new(format!(
            "unexpected answer from an isolation worker: {other:?}"
        ))),
    }
}

impl IsolationEngine for WorkersIsolation {
    // A worker's host has no host-pressure reading the master can lean on; the fleet
    // caps how many run, and the master's own valve is for its own machine.
    fn pressure(&self) -> Option<Pressure> {
        None
    }

    fn observe_pressure(&self) -> Option<Pressure> {
        None
    }

    fn sandbox_room(&self) -> SandboxRoom {
        let capacity = self.fleet.capacity();
        SandboxRoom {
            offered: Some(SandboxCeiling {
                cpus: 2,
                memory_mib: 4 * 1024,
                disk_mib: 30 * 1024,
            }),
            promised_mib: 0,
            fitting: capacity.ceiling.saturating_sub(capacity.assigned),
        }
    }

    // The master's cores say nothing about how many hosts it may start; `max` is
    // what the fleet is allowed to run.
    fn start_landings(&self) -> usize {
        self.fleet.capacity().ceiling
    }

    fn project_memory_dir(&self, project_id: &str) -> PathBuf {
        // Memory belongs to the master, which owns the task's history; a host is
        // handed it and returns it.
        self.config.project_memory_dir(project_id)
    }

    async fn prepare(
        &self,
        _status: &watch::Sender<IsolationStatus>,
    ) -> Result<(), IsolationError> {
        // There is no base image to build here: each host builds its own sandbox.
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn provision(
        &self,
        id: String,
        task_snapshot: Option<String>,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        vsock: Vec<VsockRoute>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<(), IsolationError> {
        let agent_profile = recipes
            .first()
            .map(|r| r.profile.clone())
            .unwrap_or_default();
        let vcs_profile = recipes
            .get(1)
            .map(|r| r.profile.clone())
            .unwrap_or_default();
        // What a task waits for while a host is made, in the operator's words: the host
        // itself, then the files and software it needs before a sandbox can exist there.
        progress.report(EnvironmentProgress {
            label: "provisioning a worker".to_owned(),
            completed: 0,
            total: 0,
        });
        // The host is fresh, so the operator's profile files travel with the spawn.
        let profiles = self.profile_bundles(&recipes)?;
        let checkout = checkout_guest(&project);
        self.remember_checkout(&id, checkout.clone());
        let worker = self.provision_host(&id).await?;
        let cache = Some(share::cache_path(&project.id));
        progress.report(EnvironmentProgress {
            label: "getting the worker ready".to_owned(),
            completed: 0,
            total: 0,
        });
        let call = WorkerCall::Provision {
            id: id.clone(),
            snapshot: None,
            project,
            owner: self.owner.clone().unwrap_or_default(),
            cache,
            agent_profile,
            vcs_profile,
            vsock,
        };
        if let Err(error) = self
            .provision_on(&worker, call, &profiles, progress)
            .await
            .and_then(expect_ok)
        {
            // The call failed, which is not the same as the sandbox not existing: a
            // connection can break while the host carries on working. Whether the
            // sandbox is there is a fact only the host holds, and asking it is cheaper
            // than failing a task over a lost frame — and cheaper than throwing away a
            // host that has the sandbox a task is waiting for.
            if !self.sandbox_is_there(&worker, &id).await {
                self.deprovision_host(&id).await;
                return Err(error);
            }
            tracing::warn!(
                sandbox = %id,
                worker = %worker,
                %error,
                "the provision's call failed but the sandbox is there"
            );
        }
        progress.report(EnvironmentProgress {
            label: "launching the isolated sandbox".to_owned(),
            completed: 0,
            total: 0,
        });
        // A task that was parked gets its workspace back on the new host.
        if let Some(snapshot) = task_snapshot
            && let Err(error) = self
                .restore_workspace(&id, checkout.as_deref(), &snapshot)
                .await
        {
            self.deprovision_host(&id).await;
            return Err(error);
        }
        Ok(())
    }

    fn for_operator(&self, owner: &str) -> Result<Self, IsolationError> {
        let mut scoped = self.clone();
        scoped.owner = Some(owner.to_owned());
        Ok(scoped)
    }

    async fn credential_shell(
        &self,
        sandbox: CredentialSandbox,
        profile: &str,
        _progress: &dyn EnvironmentProgressSink,
    ) -> Result<String, IsolationError> {
        let worker = self.provision_host(&sandbox.id).await?;
        let profiles = self.profile_bundles(&sandbox.recipes)?;
        let call = WorkerCall::CredentialShell {
            id: sandbox.id.clone(),
            profiles,
            snapshot: sandbox.snapshot,
            vsock: sandbox.vsock,
            profile: profile.to_owned(),
        };
        match self.call_on(&worker, call).await? {
            WorkerReply::Text(command) => Ok(command),
            WorkerReply::Failed(reason) => {
                self.deprovision_host(&sandbox.id).await;
                Err(IsolationError::new(reason))
            }
            other => {
                self.deprovision_host(&sandbox.id).await;
                Err(IsolationError::new(format!(
                    "unexpected answer from an isolation worker: {other:?}"
                )))
            }
        }
    }

    async fn watcher(
        &self,
        id: String,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        _progress: &dyn EnvironmentProgressSink,
    ) -> Result<(), IsolationError> {
        let worker = self.provision_host(&id).await?;

        let profiles = self.profile_bundles(&recipes)?;
        let call = WorkerCall::Watcher {
            id: id.clone(),
            profiles,
            project,
        };
        match self.call_on(&worker, call).await? {
            WorkerReply::Ok => Ok(()),
            WorkerReply::Failed(reason) => {
                self.deprovision_host(&id).await;
                Err(IsolationError::new(reason))
            }
            other => {
                self.deprovision_host(&id).await;
                Err(IsolationError::new(format!(
                    "unexpected answer from an isolation worker: {other:?}"
                )))
            }
        }
    }

    async fn create(
        &self,
        id: String,
        options: SandboxOptions,
        purpose: SandboxPurpose,
    ) -> Result<(), IsolationError> {
        // A sandbox with no project has no checkout to carry across a park.
        self.remember_checkout(&id, None);
        let worker = self.provision_host(&id).await?;

        let call = WorkerCall::Create {
            id: id.clone(),
            options,
            purpose,
        };
        match self.call_on(&worker, call).await.and_then(expect_ok) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.deprovision_host(&id).await;
                Err(error)
            }
        }
    }

    async fn ensure_project(
        &self,
        _project: Project,
        _recipes: Vec<ProfileRecipe>,
        _progress: &dyn EnvironmentProgressSink,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        // Environments are not pre-built here: a host builds its sandbox in
        // `provision`, and there is no snapshot to hand to another host.
        Ok(BaseSandboxSnapshot {
            path: PathBuf::new(),
            created_at_ms: 0,
            os_release: String::new(),
            packages: String::new(),
            setup_command: String::new(),
        })
    }

    async fn ensure_base(
        &self,
        _layers: Vec<BaseSandboxRecipe>,
        _view: LayerView,
        _progress: &dyn EnvironmentProgressSink,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        Ok(BaseSandboxSnapshot {
            path: PathBuf::new(),
            created_at_ms: 0,
            os_release: String::new(),
            packages: String::new(),
            setup_command: String::new(),
        })
    }

    fn base_description(&self) -> Option<EnvironmentDescription> {
        None
    }

    async fn run_command(
        &self,
        id: String,
        command: String,
    ) -> Result<CommandSession, IsolationError> {
        let worker = self.placement(&id).ok_or_else(|| {
            IsolationError::missing_sandbox(format!("sandbox {id} is not on a worker"))
        })?;
        let peer = self.fleet.peer(&worker).ok_or_else(|| {
            IsolationError::missing_sandbox(format!("isolation worker {worker} is gone"))
        })?;
        let request = WorkerRequest {
            token: self.fleet.token().to_owned(),
            call: WorkerCall::RunCommand { id, command },
        };
        self.client
            .worker_command(peer, &request)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    fn project_snapshot(&self, _project: Project, _recipes: Vec<ProfileRecipe>) -> Option<String> {
        None
    }

    async fn credential_reports(
        &self,
        sandbox: CredentialSandbox,
        checks: Vec<CredentialCheck>,
        _progress: &dyn EnvironmentProgressSink,
    ) -> Result<Option<Vec<(String, CredentialReport)>>, IsolationError> {
        let worker = self.provision_host(&sandbox.id).await?;
        let profiles = self.profile_bundles(&sandbox.recipes)?;
        let call = WorkerCall::CredentialReports {
            id: sandbox.id.clone(),
            profiles,
            snapshot: sandbox.snapshot,
            vsock: sandbox.vsock,
            checks,
        };
        match self.call_on(&worker, call).await? {
            WorkerReply::Reports(reports) => Ok(reports),
            WorkerReply::Failed(reason) => {
                self.deprovision_host(&sandbox.id).await;
                Err(IsolationError::new(reason))
            }
            other => {
                self.deprovision_host(&sandbox.id).await;
                Err(IsolationError::new(format!(
                    "unexpected answer from an isolation worker: {other:?}"
                )))
            }
        }
    }

    async fn remove_leftover_sandboxes(&self) {
        // Boxes are destroyed with their sandbox, so there is nothing left here.
    }

    async fn remove(&self, id: String) -> Result<(), IsolationError> {
        let worker = self.placement(&id);
        let result = if let Some(worker) = worker {
            let result = self
                .call_on(&worker, WorkerCall::Remove { id: id.clone() })
                .await
                .and_then(expect_ok);
            self.deprovision_host(&id).await;
            result
        } else {
            Ok(())
        };
        if let Ok(mut checkouts) = self.checkouts.lock() {
            checkouts.remove(&id);
        }
        result
    }

    async fn has_sandbox(&self, id: String) -> bool {
        let Some(worker) = self.placement(&id) else {
            return false;
        };
        matches!(
            self.call_on(&worker, WorkerCall::HasSandbox { id }).await,
            Ok(WorkerReply::Bool(true))
        )
    }

    async fn suspend(
        &self,
        id: String,
        snapshot_name: String,
        snapshots_dir: PathBuf,
    ) -> Result<PathBuf, IsolationError> {
        let destination = snapshots_dir.join(&snapshot_name);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| IsolationError::new(error.to_string()))?;
        }
        let sink = AsyncFile::create(&destination)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let peer = self.peer_for(&id)?;
        let request = WorkerRequest {
            token: self.fleet.token().to_owned(),
            call: WorkerCall::Snapshot {
                id: id.clone(),
                checkout: self.checkout_of(&id),
            },
        };
        // The host is released only once the archive is on the master, so a crash
        // mid-snapshot leaves the task rebuildable rather than silent.
        self.client
            .worker_collect(peer, &request, sink)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        self.deprovision_host(&id).await;
        Ok(destination)
    }

    async fn remove_snapshot(&self, path: PathBuf) -> Result<(), IsolationError> {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(IsolationError::new(error.to_string())),
        }
    }

    async fn guest_regular_file_exists(
        &self,
        id: String,
        path: String,
    ) -> Result<bool, IsolationError> {
        match self
            .call(
                &id,
                WorkerCall::GuestFileExists {
                    id: id.clone(),
                    path,
                },
            )
            .await?
        {
            WorkerReply::Bool(exists) => Ok(exists),
            WorkerReply::Failed(reason) => Err(IsolationError::new(reason)),
            other => Err(IsolationError::new(format!(
                "unexpected answer from an isolation worker: {other:?}"
            ))),
        }
    }

    async fn collect_files(
        &self,
        id: String,
        root: String,
        excluded: Vec<String>,
        destination: PathBuf,
        _progress: &dyn FileTransferSink,
    ) -> Result<FileTally, IsolationError> {
        let peer = self.peer_for(&id)?;
        let tar = NamedTempFile::new().map_err(|error| IsolationError::new(error.to_string()))?;
        let sink = AsyncFile::create(tar.path())
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let request = WorkerRequest {
            token: self.fleet.token().to_owned(),
            call: WorkerCall::CollectFiles { id, root, excluded },
        };
        let (files, bytes) = self
            .client
            .worker_collect(peer, &request, sink)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        extract_tree(tar.path(), &destination).map_err(IsolationError::new)?;
        Ok(FileTally { files, bytes })
    }

    async fn collect_file(
        &self,
        id: String,
        guest_path: String,
        host_path: PathBuf,
    ) -> Result<u64, IsolationError> {
        let peer = self.peer_for(&id)?;
        let sink = AsyncFile::create(&host_path)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let request = WorkerRequest {
            token: self.fleet.token().to_owned(),
            call: WorkerCall::CollectFile { id, guest_path },
        };
        let (_files, bytes) = self
            .client
            .worker_collect(peer, &request, sink)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        Ok(bytes)
    }

    async fn deliver_file(
        &self,
        id: String,
        host_path: PathBuf,
        guest_path: String,
    ) -> Result<(), IsolationError> {
        let peer = self.peer_for(&id)?;
        let file = AsyncFile::open(&host_path)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let request = WorkerRequest {
            token: self.fleet.token().to_owned(),
            call: WorkerCall::DeliverFile { id, guest_path },
        };
        self.client
            .worker_deliver(peer, &request, file)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))
    }

    async fn deliver_files(
        &self,
        id: String,
        source: PathBuf,
        root: String,
        _progress: &dyn FileTransferSink,
    ) -> Result<FileTally, IsolationError> {
        let peer = self.peer_for(&id)?;
        let tar = NamedTempFile::new().map_err(|error| IsolationError::new(error.to_string()))?;
        let tally = pack_tree(&source, tar.path()).map_err(IsolationError::new)?;
        let file = AsyncFile::open(tar.path())
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        let request = WorkerRequest {
            token: self.fleet.token().to_owned(),
            call: WorkerCall::DeliverFiles { id, root },
        };
        self.client
            .worker_deliver(peer, &request, file)
            .await
            .map_err(|error| IsolationError::new(error.to_string()))?;
        Ok(tally)
    }
}
