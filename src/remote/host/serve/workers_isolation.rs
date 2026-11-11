use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use tokio::sync::watch;

use crate::{
    config::Config,
    isolation::{
        BaseSandboxRecipe, BaseSandboxSnapshot, CommandSession, CredentialCheck, CredentialReport,
        CredentialSandbox, EnvironmentDescription, EnvironmentProgressSink, FileTally,
        FileTransferSink, IsolationEngine, IsolationError, IsolationStatus, Pressure,
        ProfileRecipe, SandboxCeiling, SandboxOptions, SandboxPurpose, SandboxRoom, VsockRoute,
        worker_protocol::{WorkerCall, WorkerReply, WorkerRequest},
    },
    project::Project,
    remote::LinkClient,
};

use super::workers::WorkerFleet;

// The isolation provider for a remote that has no sandbox runtime of its own: it
// spawns a disposable box per sandbox and speaks to it over the link. The master
// keeps every task's state; a box only ever holds one sandbox's compute.
#[derive(Clone)]
pub(crate) struct WorkersIsolation {
    fleet: Arc<WorkerFleet>,
    client: LinkClient,
    config: Config,
    placements: Arc<Mutex<HashMap<String, String>>>,
    owner: Option<String>,
}

impl WorkersIsolation {
    pub(crate) fn new(fleet: Arc<WorkerFleet>, client: LinkClient, config: &Config) -> Self {
        Self {
            fleet,
            client,
            config: config.clone(),
            placements: Arc::new(Mutex::new(HashMap::new())),
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

    async fn call(&self, id: &str, call: WorkerCall) -> Result<WorkerReply, IsolationError> {
        let worker = self.placement(id).ok_or_else(|| {
            IsolationError::missing_sandbox(format!("sandbox {id} is not on a worker"))
        })?;
        self.call_on(&worker, call).await
    }

    async fn lease(&self, id: &str) -> Result<String, IsolationError> {
        let worker = self.fleet.lease().await.map_err(IsolationError::new)?;
        self.place(id, &worker);
        Ok(worker)
    }

    async fn release(&self, id: &str) {
        let worker = self.placements.lock().ok().and_then(|mut p| p.remove(id));
        if let Some(worker) = worker {
            self.fleet.release(&worker).await;
        }
    }
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
    // A worker's box has no host-pressure reading the master can lean on; the fleet
    // caps how many run, and the master's own valve is for its own machine.
    fn pressure(&self) -> Option<Pressure> {
        None
    }

    fn observe_pressure(&self) -> Option<Pressure> {
        None
    }

    fn sandbox_room(&self) -> SandboxRoom {
        let (connected, leased, ceiling) = self.fleet.capacity();
        let _ = connected;
        SandboxRoom {
            offered: Some(SandboxCeiling {
                cpus: 2,
                memory_mib: 4 * 1024,
                disk_mib: 30 * 1024,
            }),
            promised_mib: 0,
            fitting: ceiling.saturating_sub(leased),
        }
    }

    fn project_memory_dir(&self, project_id: &str) -> PathBuf {
        // Memory belongs to the master, which owns the task's history; a box is
        // handed it and returns it.
        self.config.project_memory_dir(project_id)
    }

    async fn prepare(
        &self,
        _status: &watch::Sender<IsolationStatus>,
    ) -> Result<(), IsolationError> {
        // There is no base image to build here: each box builds its own sandbox.
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn provision(
        &self,
        id: String,
        _task_snapshot: Option<String>,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        vsock: Vec<VsockRoute>,
        _progress: &dyn EnvironmentProgressSink,
    ) -> Result<(), IsolationError> {
        // A box is fresh, so a snapshot from an earlier box is gone with it; the
        // task rebuilds from the repository. The recipes' profile ids are all the
        // box needs to rebuild them.
        let agent_profile = recipes
            .first()
            .map(|r| r.profile.clone())
            .unwrap_or_default();
        let vcs_profile = recipes
            .get(1)
            .map(|r| r.profile.clone())
            .unwrap_or_default();
        let worker = self.lease(&id).await?;
        let call = WorkerCall::Provision {
            id: id.clone(),
            snapshot: None,
            project,
            agent_profile,
            vcs_profile,
            vsock,
        };
        match self.call_on(&worker, call).await.and_then(expect_ok) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.release(&id).await;
                Err(error)
            }
        }
    }

    fn for_operator(&self, owner: &str) -> Result<Self, IsolationError> {
        let mut scoped = self.clone();
        scoped.owner = Some(owner.to_owned());
        Ok(scoped)
    }

    async fn credential_shell(
        &self,
        _sandbox: CredentialSandbox,
        _profile: &str,
        _progress: &dyn EnvironmentProgressSink,
    ) -> Result<String, IsolationError> {
        Err(IsolationError::new(
            "the workers isolation provider cannot run a credential shell yet",
        ))
    }

    async fn watcher(
        &self,
        _id: String,
        _project: Project,
        _recipes: Vec<ProfileRecipe>,
        _progress: &dyn EnvironmentProgressSink,
    ) -> Result<(), IsolationError> {
        Err(IsolationError::new(
            "the workers isolation provider cannot run a watcher yet",
        ))
    }

    async fn create(
        &self,
        id: String,
        options: SandboxOptions,
        purpose: SandboxPurpose,
    ) -> Result<(), IsolationError> {
        let worker = self.lease(&id).await?;
        let call = WorkerCall::Create {
            id: id.clone(),
            options,
            purpose,
        };
        match self.call_on(&worker, call).await.and_then(expect_ok) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.release(&id).await;
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
        // Environments are not pre-built here: a box builds its sandbox in
        // `provision`, and there is no snapshot to hand to another box.
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
        _sandbox: CredentialSandbox,
        _checks: Vec<CredentialCheck>,
        _progress: &dyn EnvironmentProgressSink,
    ) -> Result<Option<Vec<(String, CredentialReport)>>, IsolationError> {
        Err(IsolationError::new(
            "the workers isolation provider cannot run credential checks yet",
        ))
    }

    async fn remove_leftover_sandboxes(&self) {
        // Boxes are destroyed with their sandbox, so there is nothing left here.
    }

    async fn remove(&self, id: String) -> Result<(), IsolationError> {
        let worker = self.placement(&id);
        if let Some(worker) = worker {
            let result = self
                .call_on(&worker, WorkerCall::Remove { id: id.clone() })
                .await
                .and_then(expect_ok);
            self.release(&id).await;
            result
        } else {
            Ok(())
        }
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
        _snapshot_name: String,
        _snapshots_dir: PathBuf,
    ) -> Result<PathBuf, IsolationError> {
        // A box cannot be snapshotted for a later box to restore; destroying it and
        // saying so is how the task learns to rebuild from the repository.
        self.release(&id).await;
        Err(IsolationError::missing_sandbox(format!(
            "sandbox {id} lives on a box that is now gone"
        )))
    }

    async fn remove_snapshot(&self, _path: PathBuf) -> Result<(), IsolationError> {
        Ok(())
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
        _id: String,
        _root: String,
        _excluded: Vec<String>,
        _destination: PathBuf,
        _progress: &dyn FileTransferSink,
    ) -> Result<FileTally, IsolationError> {
        Err(IsolationError::new(
            "the workers isolation provider cannot collect files yet",
        ))
    }

    async fn collect_file(
        &self,
        _id: String,
        _guest_path: String,
        _host_path: PathBuf,
    ) -> Result<u64, IsolationError> {
        Err(IsolationError::new(
            "the workers isolation provider cannot collect a file yet",
        ))
    }

    async fn deliver_file(
        &self,
        _id: String,
        _host_path: PathBuf,
        _guest_path: String,
    ) -> Result<(), IsolationError> {
        Err(IsolationError::new(
            "the workers isolation provider cannot deliver a file yet",
        ))
    }

    async fn deliver_files(
        &self,
        _id: String,
        _source: PathBuf,
        _root: String,
        _progress: &dyn FileTransferSink,
    ) -> Result<FileTally, IsolationError> {
        Err(IsolationError::new(
            "the workers isolation provider cannot deliver files yet",
        ))
    }
}
