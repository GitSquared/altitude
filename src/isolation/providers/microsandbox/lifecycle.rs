use microsandbox::{Sandbox, sandbox::PullPolicy};

use crate::isolation::{
    IsolationEngine, IsolationError, SandboxCeiling, SandboxOptions, mount_git_cache,
    trust_host_roots,
};

use super::base::BASE_IMAGE;
use super::{MicrosandboxIsolation, output_tail};

impl MicrosandboxIsolation {
    pub(super) async fn create_sandbox(
        &self,
        id: String,
        mut options: SandboxOptions,
        ceiling: SandboxCeiling,
        pull_policy: PullPolicy,
    ) -> Result<(), IsolationError> {
        mount_git_cache(&mut options, &self.config.git_cache_dir());
        let trusted = trust_host_roots(&mut options, self.config.data_dir());
        tracing::debug!(sandbox_id = %id, network_enabled = options.network_enabled, cpus = ceiling.cpus, memory_mib = ceiling.memory_mib, "creating sandbox");
        let result = match options.snapshot_path {
            Some(snapshot_path) => {
                let mut restore = Sandbox::restore(snapshot_path)
                    .name(&id)
                    .cpus(ceiling.cpus)
                    .memory(ceiling.memory_mib);
                if !options.network_enabled {
                    restore = restore.disable_network();
                }
                for mount in options.mounts {
                    restore = restore.volume(mount.guest_path, |volume| {
                        let volume = volume.bind(mount.host_path);
                        let volume = if mount.noexec {
                            volume.noexec()
                        } else {
                            volume
                        };
                        if mount.readonly {
                            volume.readonly()
                        } else {
                            volume
                        }
                    });
                }
                for route in options.vsock {
                    restore = restore.vsock(route.host_socket, route.guest_port);
                }
                match restore.restore().await {
                    Ok(_) if options.environment.is_empty() => Ok(()),
                    Ok(sandbox) => {
                        // Restore preserves captured env; supply this environment's values for future execs.
                        let mut modification = sandbox.modify().restart();
                        for (key, value) in options.environment {
                            modification = modification.env(key, value);
                        }
                        modification.apply().await.map(|_| ())
                    }
                    Err(error) => Err(error),
                }
            }
            None => {
                let mut builder = Sandbox::builder(&id)
                    .cpus(ceiling.cpus)
                    .memory(ceiling.memory_mib)
                    .image(BASE_IMAGE)
                    .root_disk(ceiling.disk_mib)
                    .pull_policy(pull_policy);
                if !options.network_enabled {
                    builder = builder.disable_network();
                }
                for mount in options.mounts {
                    builder = builder.volume(mount.guest_path, |volume| {
                        let volume = volume.bind(mount.host_path);
                        let volume = if mount.noexec {
                            volume.noexec()
                        } else {
                            volume
                        };
                        if mount.readonly {
                            volume.readonly()
                        } else {
                            volume
                        }
                    });
                }
                for route in options.vsock {
                    builder = builder.vsock(route.host_socket, route.guest_port);
                }
                for (key, value) in options.environment {
                    builder = builder.env(key, value);
                }
                builder.create_detached().await.map(|_| ())
            }
        };
        if let Err(error) = result {
            if Sandbox::get(&id).await.is_ok()
                && let Err(cleanup_error) = self.remove(id.clone()).await
            {
                tracing::warn!(sandbox_id = %id, %cleanup_error, "could not remove failed sandbox creation");
            }
            return Err(IsolationError::new(error.to_string()));
        }

        if trusted {
            self.append_host_roots(&id).await;
        }

        tracing::debug!(sandbox_id = %id, "sandbox created");

        Ok(())
    }

    pub(super) async fn append_host_roots(&self, id: &str) {
        // The store is not in every image: a base without it would fail an append
        // that is only a warning, which reads like a broken sandbox.
        let command = "mkdir -p /etc/ssl/certs && cat /etc/altitude/host-roots.pem >> /etc/ssl/certs/ca-certificates.crt";
        match self.run_to_end(id, command).await {
            Ok((0, _)) => tracing::debug!(sandbox_id = %id, "sandbox trusts this machine's roots"),
            Ok((code, output)) => tracing::warn!(
                sandbox_id = %id,
                code,
                output = %output_tail(&output),
                "could not add this machine's roots to the sandbox store"
            ),
            Err(error) => {
                tracing::warn!(sandbox_id = %id, %error, "could not reach the sandbox store")
            }
        }
    }

    pub(super) async fn remove_leftover(&self, id: &str) {
        if Sandbox::get(id).await.is_err() {
            return;
        }
        tracing::debug!(sandbox_id = %id, "removing a sandbox left behind");
        if let Err(error) = self.remove(id.to_owned()).await {
            tracing::warn!(sandbox_id = %id, %error, "could not remove a leftover sandbox");
        }
    }
}
