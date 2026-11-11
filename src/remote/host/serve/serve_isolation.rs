use std::{future::Future, path::PathBuf};

use tokio::sync::watch;

use crate::{
    isolation::{
        BaseSandboxRecipe, BaseSandboxSnapshot, CommandSession, CredentialCheck, CredentialReport,
        CredentialSandbox, EnvironmentDescription, EnvironmentProgressSink, FileTally,
        FileTransferSink, IsolationEngine, IsolationError, IsolationStatus, MicrosandboxIsolation,
        Pressure, ProfileRecipe, SandboxOptions, SandboxPurpose, SandboxRoom, VsockRoute,
    },
    project::Project,
};

use super::workers_isolation::WorkersIsolation;

// The isolation engine a remote serves with: its own microVMs, or the disposable
// boxes it provisions across a network. One enum so `OwnTasks` and the task loop
// do not have to be generic; every method hands the call to the chosen engine.
#[derive(Clone)]
pub(crate) enum ServeIsolation {
    Microsandbox(MicrosandboxIsolation),
    Workers(WorkersIsolation),
}

// An `async fn` in the impl cannot carry the trait's `+ Send`, so the bodies stay
// `impl Future` returning one `async move` per method.
#[allow(clippy::manual_async_fn)]
impl IsolationEngine for ServeIsolation {
    fn pressure(&self) -> Option<Pressure> {
        match self {
            Self::Microsandbox(engine) => engine.pressure(),
            Self::Workers(engine) => engine.pressure(),
        }
    }

    fn observe_pressure(&self) -> Option<Pressure> {
        match self {
            Self::Microsandbox(engine) => engine.observe_pressure(),
            Self::Workers(engine) => engine.observe_pressure(),
        }
    }

    fn sandbox_room(&self) -> SandboxRoom {
        match self {
            Self::Microsandbox(engine) => engine.sandbox_room(),
            Self::Workers(engine) => engine.sandbox_room(),
        }
    }

    fn project_memory_dir(&self, project_id: &str) -> PathBuf {
        match self {
            Self::Microsandbox(engine) => engine.project_memory_dir(project_id),
            Self::Workers(engine) => engine.project_memory_dir(project_id),
        }
    }

    fn provision(
        &self,
        id: String,
        task_snapshot: Option<String>,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        vsock: Vec<VsockRoute>,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => {
                    engine
                        .provision(id, task_snapshot, project, recipes, vsock, progress)
                        .await
                }
                Self::Workers(engine) => {
                    engine
                        .provision(id, task_snapshot, project, recipes, vsock, progress)
                        .await
                }
            }
        }
    }

    fn for_operator(&self, owner: &str) -> Result<Self, IsolationError> {
        Ok(match self {
            Self::Microsandbox(engine) => Self::Microsandbox(engine.for_operator(owner)?),
            Self::Workers(engine) => Self::Workers(engine.for_operator(owner)?),
        })
    }

    fn credential_shell(
        &self,
        sandbox: CredentialSandbox,
        profile: &str,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<String, IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => {
                    engine.credential_shell(sandbox, profile, progress).await
                }
                Self::Workers(engine) => engine.credential_shell(sandbox, profile, progress).await,
            }
        }
    }

    fn watcher(
        &self,
        id: String,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.watcher(id, project, recipes, progress).await,
                Self::Workers(engine) => engine.watcher(id, project, recipes, progress).await,
            }
        }
    }

    fn prepare(
        &self,
        status: &watch::Sender<IsolationStatus>,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.prepare(status).await,
                Self::Workers(engine) => engine.prepare(status).await,
            }
        }
    }

    fn create(
        &self,
        id: String,
        options: SandboxOptions,
        purpose: SandboxPurpose,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.create(id, options, purpose).await,
                Self::Workers(engine) => engine.create(id, options, purpose).await,
            }
        }
    }

    fn ensure_project(
        &self,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<BaseSandboxSnapshot, IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => {
                    engine.ensure_project(project, recipes, progress).await
                }
                Self::Workers(engine) => engine.ensure_project(project, recipes, progress).await,
            }
        }
    }

    fn ensure_base(
        &self,
        layers: Vec<BaseSandboxRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<BaseSandboxSnapshot, IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.ensure_base(layers, progress).await,
                Self::Workers(engine) => engine.ensure_base(layers, progress).await,
            }
        }
    }

    fn base_description(&self) -> Option<EnvironmentDescription> {
        match self {
            Self::Microsandbox(engine) => engine.base_description(),
            Self::Workers(engine) => engine.base_description(),
        }
    }

    fn run_command(
        &self,
        id: String,
        command: String,
    ) -> impl Future<Output = Result<CommandSession, IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.run_command(id, command).await,
                Self::Workers(engine) => engine.run_command(id, command).await,
            }
        }
    }

    fn project_snapshot(&self, project: Project, recipes: Vec<ProfileRecipe>) -> Option<String> {
        match self {
            Self::Microsandbox(engine) => engine.project_snapshot(project, recipes),
            Self::Workers(engine) => engine.project_snapshot(project, recipes),
        }
    }

    fn credential_reports(
        &self,
        sandbox: CredentialSandbox,
        checks: Vec<CredentialCheck>,
        progress: &dyn EnvironmentProgressSink,
    ) -> impl Future<Output = Result<Option<Vec<(String, CredentialReport)>>, IsolationError>> + Send
    {
        async move {
            match self {
                Self::Microsandbox(engine) => {
                    engine.credential_reports(sandbox, checks, progress).await
                }
                Self::Workers(engine) => engine.credential_reports(sandbox, checks, progress).await,
            }
        }
    }

    fn remove_leftover_sandboxes(&self) -> impl Future<Output = ()> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.remove_leftover_sandboxes().await,
                Self::Workers(engine) => engine.remove_leftover_sandboxes().await,
            }
        }
    }

    fn remove(&self, id: String) -> impl Future<Output = Result<(), IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.remove(id).await,
                Self::Workers(engine) => engine.remove(id).await,
            }
        }
    }

    fn has_sandbox(&self, id: String) -> impl Future<Output = bool> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.has_sandbox(id).await,
                Self::Workers(engine) => engine.has_sandbox(id).await,
            }
        }
    }

    fn suspend(
        &self,
        id: String,
        snapshot_name: String,
        snapshots_dir: PathBuf,
    ) -> impl Future<Output = Result<PathBuf, IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => {
                    engine.suspend(id, snapshot_name, snapshots_dir).await
                }
                Self::Workers(engine) => engine.suspend(id, snapshot_name, snapshots_dir).await,
            }
        }
    }

    fn remove_snapshot(
        &self,
        path: PathBuf,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.remove_snapshot(path).await,
                Self::Workers(engine) => engine.remove_snapshot(path).await,
            }
        }
    }

    fn guest_regular_file_exists(
        &self,
        id: String,
        path: String,
    ) -> impl Future<Output = Result<bool, IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.guest_regular_file_exists(id, path).await,
                Self::Workers(engine) => engine.guest_regular_file_exists(id, path).await,
            }
        }
    }

    fn collect_files(
        &self,
        id: String,
        root: String,
        excluded: Vec<String>,
        destination: PathBuf,
        progress: &dyn FileTransferSink,
    ) -> impl Future<Output = Result<FileTally, IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => {
                    engine
                        .collect_files(id, root, excluded, destination, progress)
                        .await
                }
                Self::Workers(engine) => {
                    engine
                        .collect_files(id, root, excluded, destination, progress)
                        .await
                }
            }
        }
    }

    fn collect_file(
        &self,
        id: String,
        guest_path: String,
        host_path: PathBuf,
    ) -> impl Future<Output = Result<u64, IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.collect_file(id, guest_path, host_path).await,
                Self::Workers(engine) => engine.collect_file(id, guest_path, host_path).await,
            }
        }
    }

    fn deliver_file(
        &self,
        id: String,
        host_path: PathBuf,
        guest_path: String,
    ) -> impl Future<Output = Result<(), IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => engine.deliver_file(id, host_path, guest_path).await,
                Self::Workers(engine) => engine.deliver_file(id, host_path, guest_path).await,
            }
        }
    }

    fn deliver_files(
        &self,
        id: String,
        source: PathBuf,
        root: String,
        progress: &dyn FileTransferSink,
    ) -> impl Future<Output = Result<FileTally, IsolationError>> + Send {
        async move {
            match self {
                Self::Microsandbox(engine) => {
                    engine.deliver_files(id, source, root, progress).await
                }
                Self::Workers(engine) => engine.deliver_files(id, source, root, progress).await,
            }
        }
    }
}
