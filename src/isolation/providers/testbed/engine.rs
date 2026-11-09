use std::{fs, io, io::ErrorKind, path::PathBuf};

use tokio::sync::watch;

use crate::{
    isolation::{
        BaseSandboxRecipe, BaseSandboxSnapshot, CommandSession, CredentialCheck, CredentialReport,
        CredentialSandbox, EnvironmentDescription, EnvironmentProgressSink, FileTally,
        FileTransferSink, IsolationEngine, IsolationError, IsolationStatus, Pressure,
        ProfileRecipe, SandboxCeiling, SandboxOptions, SandboxPurpose, VsockRoute,
        private_directory,
        sandbox::{self, ProvisionRequest},
    },
    project::Project,
};

use super::{
    LAYER_MARKER, MountRecord, SandboxRecord, TestbedIsolation, check_environment, files, machine,
};

impl IsolationEngine for TestbedIsolation {
    fn pressure(&self) -> Option<Pressure> {
        self.host_pressure
            .lock()
            .expect("pressure lock poisoned")
            .pressure()
    }

    fn observe_pressure(&self) -> Option<Pressure> {
        self.host_pressure
            .lock()
            .expect("pressure lock poisoned")
            .observe()
    }

    fn sandbox_ceiling(&self) -> SandboxCeiling {
        self.host_pressure
            .lock()
            .expect("pressure lock poisoned")
            .ceiling()
    }

    fn project_memory_dir(&self, project_id: &str) -> PathBuf {
        self.config.project_memory_dir(project_id)
    }

    async fn prepare(
        &self,
        _status: &watch::Sender<IsolationStatus>,
    ) -> Result<(), IsolationError> {
        check_environment()?;
        private_directory(&self.config.isolation_data_dir())?;
        private_directory(&self.sandboxes_dir())?;
        // Nothing was running before `prepare`, so every sandbox directory is a
        // previous process's and can go.
        self.remove_leftover_sandboxes().await;
        tracing::warn!(
            "testbed isolation is active: sandboxes are chroots over this machine, not microVMs"
        );
        Ok(())
    }

    async fn create(
        &self,
        id: String,
        options: SandboxOptions,
        _purpose: SandboxPurpose,
    ) -> Result<(), IsolationError> {
        let directory = self.sandbox_dir(&id);
        private_directory(&directory)?;
        let root = self.sandbox_root(&id);
        if root.exists() {
            fs::remove_dir_all(&root).map_err(io_error)?;
        }
        machine::prepare_root(&root)?;
        let mounts = options
            .mounts
            .into_iter()
            .map(|mount| MountRecord {
                host_path: mount.host_path,
                guest_path: mount.guest_path,
                readonly: mount.readonly,
            })
            .collect();
        self.write_record(
            &id,
            &SandboxRecord {
                mounts,
                environment: options.environment,
            },
        )
    }

    async fn run_command(
        &self,
        id: String,
        command: String,
    ) -> Result<CommandSession, IsolationError> {
        let root = self.sandbox_root(&id);
        if !root.is_dir() {
            return Err(IsolationError::missing_sandbox(format!(
                "this testbed sandbox is not on disk: {id}"
            )));
        }
        let record = self.read_record(&id)?;
        machine::command_session(root, record, command)
    }

    async fn ensure_project(
        &self,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        sandbox::ensure_project(self, &self.config, project, recipes, progress).await
    }

    async fn ensure_base(
        &self,
        layers: Vec<BaseSandboxRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        // The machine is the base, so there is no workspace image to build: a layer
        // is only its commands, run once here.
        let mut snapshot = self.host_base_snapshot();
        for layer in layers {
            snapshot = self.ensure_layer(layer, progress).await?;
        }
        Ok(snapshot)
    }

    async fn provision(
        &self,
        id: String,
        task_snapshot: Option<String>,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        vsock: Vec<VsockRoute>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<(), IsolationError> {
        sandbox::provision(
            self,
            &self.config,
            ProvisionRequest {
                id,
                snapshot: task_snapshot,
                project,
                recipes,
                vsock,
            },
            progress,
        )
        .await
    }

    async fn credential_shell(
        &self,
        sandbox: CredentialSandbox,
        profile: &str,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<String, IsolationError> {
        sandbox::credential_shell(self, &self.config, sandbox, profile, progress).await
    }

    async fn watcher(
        &self,
        id: String,
        project: Project,
        recipes: Vec<ProfileRecipe>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<(), IsolationError> {
        sandbox::watcher(self, &self.config, id, project, recipes, progress).await
    }

    async fn credential_reports(
        &self,
        sandbox: CredentialSandbox,
        checks: Vec<CredentialCheck>,
        progress: &dyn EnvironmentProgressSink,
    ) -> Result<Option<Vec<(String, CredentialReport)>>, IsolationError> {
        sandbox::credential_reports(self, &self.config, sandbox, checks, progress).await
    }

    fn project_snapshot(&self, project: Project, recipes: Vec<ProfileRecipe>) -> Option<String> {
        let recipe = sandbox::environment_top_layer(&self.config, project, recipes).ok()??;
        let marker = recipe.snapshots_dir.join(LAYER_MARKER);
        marker
            .is_file()
            .then(|| marker.to_string_lossy().into_owned())
    }

    fn base_description(&self) -> Option<EnvironmentDescription> {
        // There is no base image: the machine is the base. Reporting one would
        // claim it was built, and there is nothing Altitude built to describe.
        None
    }

    async fn remove(&self, id: String) -> Result<(), IsolationError> {
        let directory = self.sandbox_dir(&id);
        match fs::remove_dir_all(&directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(error)),
        }
    }

    async fn has_sandbox(&self, id: String) -> bool {
        self.exists(&id)
    }

    async fn remove_leftover_sandboxes(&self) {
        let entries = match fs::read_dir(self.sandboxes_dir()) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => return,
            Err(error) => {
                tracing::warn!(%error, "could not read the testbed sandboxes");
                return;
            }
        };
        for entry in entries.flatten() {
            tracing::debug!(sandbox = %entry.file_name().to_string_lossy(), "removing a testbed sandbox left behind");
            if let Err(error) = fs::remove_dir_all(entry.path()) {
                tracing::warn!(%error, path = %entry.path().display(), "could not remove a testbed sandbox left behind");
            }
        }
    }

    async fn suspend(
        &self,
        id: String,
        snapshot_name: String,
        snapshots_dir: PathBuf,
    ) -> Result<PathBuf, IsolationError> {
        if !self.exists(&id) {
            return Err(IsolationError::missing_sandbox(format!(
                "this testbed sandbox is not on disk: {id}"
            )));
        }
        private_directory(&snapshots_dir)?;
        let path = snapshots_dir.join(snapshot_name);
        if let Some(parent) = path.parent() {
            private_directory(parent)?;
        }
        // There is nothing to capture: a testbed sandbox's files are the machine's,
        // which outlive it. The marker only says that a snapshot is recorded, so a
        // resume can tell the agent it was paused and restored.
        fs::write(&path, b"testbed\n").map_err(io_error)?;
        self.remove(id).await?;
        Ok(path)
    }

    async fn remove_snapshot(&self, path: PathBuf) -> Result<(), IsolationError> {
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => match fs::remove_dir_all(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
                Err(error) => Err(io_error(error)),
            },
            Err(error) if error.kind() == ErrorKind::IsADirectory => {
                fs::remove_dir_all(&path).map_err(io_error)
            }
            Err(error) => Err(io_error(error)),
        }
    }

    async fn guest_regular_file_exists(
        &self,
        id: String,
        path: String,
    ) -> Result<bool, IsolationError> {
        let record = self.read_record(&id)?;
        let host = self.host_path(&id, &record, &path);
        Ok(files::regular_file(&host))
    }

    async fn collect_files(
        &self,
        id: String,
        root: String,
        excluded: Vec<String>,
        destination: PathBuf,
        progress: &dyn FileTransferSink,
    ) -> Result<FileTally, IsolationError> {
        let record = self.read_record(&id)?;
        let source = self.host_path(&id, &record, &root);
        files::copy_out(&source, &root, &excluded, &destination, progress)
    }

    async fn collect_file(
        &self,
        id: String,
        guest_path: String,
        host_path: PathBuf,
    ) -> Result<u64, IsolationError> {
        let record = self.read_record(&id)?;
        let source = self.host_path(&id, &record, &guest_path);
        files::copy_file(&source, &host_path)
    }

    async fn deliver_file(
        &self,
        id: String,
        host_path: PathBuf,
        guest_path: String,
    ) -> Result<(), IsolationError> {
        let record = self.read_record(&id)?;
        let target = self.host_path(&id, &record, &guest_path);
        files::copy_file(&host_path, &target)?;
        Ok(())
    }

    async fn deliver_files(
        &self,
        id: String,
        source: PathBuf,
        root: String,
        progress: &dyn FileTransferSink,
    ) -> Result<FileTally, IsolationError> {
        let record = self.read_record(&id)?;
        let target = self.host_path(&id, &record, &root);
        files::copy_in(&source, &target, progress)
    }
}

fn io_error(error: io::Error) -> IsolationError {
    IsolationError::new(error.to_string())
}
