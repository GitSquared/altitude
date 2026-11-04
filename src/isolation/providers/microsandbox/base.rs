use std::time::Duration;

use microsandbox::sandbox::PullPolicy;

use crate::isolation::{
    BaseSandboxRecipe, BaseSandboxSnapshot, CommandEvent, EnvironmentProgress,
    EnvironmentProgressSink, IsolationEngine, IsolationError, PackageInstallSink, SandboxOptions,
    SetupStep,
};

use super::packages::Packages;
use super::snapshot::{BaseSandboxSnapshotStore, artifact_name, build_namespace, now_ms};
use super::{MicrosandboxIsolation, output_tail};

pub(super) const BASE_IMAGE: &str = "ubuntu:24.04";

const WORKSPACE_PACKAGES: &[&str] = &[
    "bash",
    "ca-certificates",
    "curl",
    "gnupg",
    "python3",
    "python3-pip",
    "jq",
    "coreutils",
    "findutils",
    "sed",
    "grep",
    "ripgrep",
    "libgcc-s1",
    "libstdc++6",
    "build-essential",
    "pkg-config",
    "libssl-dev",
    "cmake",
    "clang",
    "perl",
    "nodejs",
];

fn workspace_setup() -> String {
    format!(
        "set -eu\n\
         export DEBIAN_FRONTEND=noninteractive\n\
         apt-get update\n\
         apt-get install -y --no-install-recommends ca-certificates curl gnupg\n\
         curl -fsSL https://deb.nodesource.com/setup_22.x -o /tmp/nodesource.sh\n\
         bash /tmp/nodesource.sh\n\
         apt-get install -y --no-install-recommends {}\n\
         cat > /root/README.md <<'ALTITUDE_WORKSPACE_README'\n\
         {}ALTITUDE_WORKSPACE_README\n",
        WORKSPACE_PACKAGES.join(" "),
        include_str!("../assets/workspace.md"),
    )
}

pub(super) const PREPARATION_SANDBOX: &str = "altitude-base-image-setup";

fn description_command() -> String {
    // Asked package by package, because the list is the answer: scanning every
    // installed package would include the image's plumbing, and matching names
    // with a regex would lose ones like `libstdc++6`.
    format!(
        "printf 'os\\t%s\\n' \"$(. /etc/os-release && printf '%s %s' \"$NAME\" \"$VERSION_ID\")\"\nprintf 'packages\\t%s\\n' \"$(for package in {}; do version=$(dpkg-query -W -f='${{Version}}' \"$package\" 2>/dev/null) || continue; [ -n \"$version\" ] || continue; printf '%s-%s\\n' \"$package\" \"$version\"; done | tr '\\n' ' ')\"\n",
        WORKSPACE_PACKAGES.join(" "),
    )
}

impl MicrosandboxIsolation {
    pub(super) async fn ensure_workspace(
        &self,
        installs: Option<&dyn PackageInstallSink>,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        self.ensure_snapshot(
            BaseSandboxRecipe::new(
                "workspace",
                self.config.base_snapshots_dir(),
                Duration::from_secs(72 * 60 * 60),
                vec![SetupStep::new("preparing base packages", workspace_setup())],
            ),
            None,
            None,
            installs,
        )
        .await
    }

    pub(super) async fn ensure_snapshot(
        &self,
        recipe: BaseSandboxRecipe,
        parent: Option<BaseSandboxSnapshot>,
        progress: Option<&dyn EnvironmentProgressSink>,
        installs: Option<&dyn PackageInstallSink>,
    ) -> Result<BaseSandboxSnapshot, IsolationError> {
        let store = BaseSandboxSnapshotStore::new(&recipe);
        let current = store.current()?;
        // A cached child must be rebuilt when its workspace base is newer.
        let parent_refreshed = parent.as_ref().is_some_and(|parent| {
            current
                .as_ref()
                .is_some_and(|current| parent.created_at_ms > current.created_at_ms)
        });
        if !parent_refreshed && !store.needs_refresh(current.as_ref(), &recipe.setup_command()) {
            return Ok(current.expect("a base snapshot was present"));
        }

        let steps: Vec<&SetupStep> = recipe
            .steps
            .iter()
            .filter(|step| !step.command.trim().is_empty())
            .collect();
        let sandbox_id = format!(
            "altitude-{}-{}-setup",
            build_namespace(&recipe.snapshots_dir),
            artifact_name(&recipe.name)
        );
        let created_at_ms = now_ms();
        tracing::debug!(environment = %recipe.name, steps = steps.len(), "building environment base sandbox snapshot");
        self.remove_leftover(&sandbox_id).await;
        let mut options = SandboxOptions::isolated(
            parent.map(|snapshot| snapshot.path.to_string_lossy().into_owned()),
        );
        options.network_enabled = true;
        // An environment build is a sandbox like any other, so it takes the same
        // ceiling: it runs apt and a package manager, which is exactly the work
        // that wants the room.
        let ceiling = self.sandbox_ceiling();
        self.create_sandbox(sandbox_id.clone(), options, ceiling, PullPolicy::Always)
            .await?;
        let result = async {
            for (completed, step) in steps.iter().enumerate() {
                if let Some(progress) = progress {
                    progress.report(EnvironmentProgress {
                        label: step.label.clone(),
                        completed,
                        total: steps.len(),
                    });
                }
                self.run_setup_step(&sandbox_id, step, installs).await?;
            }
            // Asked while the sandbox is still running, and before the snapshot
            // is taken, because it can only be asked of a live machine.
            let (os_release, packages) = self.describe_snapshot(&sandbox_id).await;
            let path = self.suspend(
                sandbox_id.clone(),
                format!("base-{created_at_ms}"),
                recipe.snapshots_dir.clone(),
            ).await?;
            let snapshot = BaseSandboxSnapshot {
                path,
                created_at_ms,
                os_release,
                packages,
                setup_command: recipe.setup_command(),
            };
            store.publish(&snapshot)?;
            for stale_snapshot in store.stale_snapshots().await? {
                tracing::debug!(snapshot = %stale_snapshot.display(), "removing stale environment base sandbox snapshot");
                if let Err(error) = self.remove_snapshot(stale_snapshot).await {
                    tracing::warn!(%error, "could not remove stale environment base sandbox snapshot");
                }
            }
            Ok(snapshot)
        }
        .await;
        if let Err(error) = &result {
            tracing::warn!(environment = %recipe.name, error = %error, "environment base sandbox setup failed");
            if let Err(remove_error) = self.remove(sandbox_id).await {
                tracing::warn!(environment = %recipe.name, error = %remove_error, "could not remove failed environment setup sandbox");
            }
        }
        let snapshot = result?;
        tracing::debug!(environment = %recipe.name, snapshot = %snapshot.path.display(), "environment base sandbox ready");
        Ok(snapshot)
    }

    pub(super) async fn run_setup_step(
        &self,
        sandbox_id: &str,
        step: &SetupStep,
        installs: Option<&dyn PackageInstallSink>,
    ) -> Result<(), IsolationError> {
        let mut output = String::new();
        let mut packages = Packages::default();
        let status = self
            .stream_to_end(sandbox_id, &step.command, |chunk| {
                output.push_str(chunk);
                let Some(installs) = installs else { return };
                if let Some(install) = packages.observe(chunk) {
                    installs.installing(install);
                }
            })
            .await
            .map_err(|error| IsolationError::new(format!("{}: {error}", step.label)))?;
        match status {
            0 => Ok(()),
            code => Err(IsolationError::new(format!(
                "{} exited with code {code}: {}",
                step.label,
                output_tail(&output)
            ))),
        }
    }

    pub(super) async fn run_to_end(
        &self,
        sandbox_id: &str,
        command: &str,
    ) -> Result<(i32, String), IsolationError> {
        let mut output = String::new();
        let status = self
            .stream_to_end(sandbox_id, command, |chunk| output.push_str(chunk))
            .await?;
        Ok((status, output))
    }

    async fn stream_to_end(
        &self,
        sandbox_id: &str,
        command: &str,
        mut on_output: impl FnMut(&str),
    ) -> Result<i32, IsolationError> {
        let mut session = self
            .run_command(sandbox_id.to_owned(), format!("set -eu\n{command}"))
            .await?;
        loop {
            let Some(event) = session.events.recv().await else {
                return Err(IsolationError::new(
                    "a command stopped without an exit status",
                ));
            };
            match event {
                CommandEvent::Output { bytes, .. } => on_output(&String::from_utf8_lossy(&bytes)),
                CommandEvent::Exited(status) => return Ok(status),
                CommandEvent::Failed(error) => return Err(IsolationError::new(error)),
            }
        }
    }

    pub(super) async fn describe_snapshot(&self, sandbox_id: &str) -> (String, String) {
        let unreadable = || {
            tracing::warn!("the sandbox would not describe itself");
            (String::new(), String::new())
        };
        let command = description_command();
        let Ok((0, output)) = self.run_to_end(sandbox_id, &command).await else {
            return unreadable();
        };
        let mut os = String::new();
        let mut packages = String::new();
        for line in output.lines() {
            match line.split_once('\t') {
                Some(("os", value)) => os = value.trim().to_owned(),
                Some(("packages", value)) => packages = value.trim().to_owned(),
                _ => {}
            }
        }
        if os.is_empty() && packages.is_empty() {
            return unreadable();
        }
        (os, packages)
    }
}
