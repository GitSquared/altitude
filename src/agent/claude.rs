use std::{fs, path::PathBuf, sync::Arc};

use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    config::Config,
    isolation::{CommandEvent, IsolationEngine, SandboxMount, SandboxOptions},
};

use super::{AgentEngine, AgentError, AgentProfile};

#[derive(Clone)]
pub struct ClaudeCode {
    profile: AgentProfile,
    snapshots_dir: PathBuf,
    preparation_lock: Arc<Mutex<()>>,
}

const PREPARATION_SANDBOX: &str = "altitude-claude-setup";
const SNAPSHOT_NAME: &str = "claude-base";
const DEFAULT_SETTINGS: &str = r#"{
  "tui": "fullscreen",
  "theme": "dark",
  "permissions": {
    "defaultMode": "auto"
  }
}
"#;

impl ClaudeCode {
    pub fn new(config: &Config) -> Self {
        Self {
            profile: AgentProfile::new(config.agent_profile_dir("claude")),
            snapshots_dir: config.agent_snapshots_dir("claude"),
            preparation_lock: Arc::new(Mutex::new(())),
        }
    }

    fn base_sandbox_options(&self, snapshot_path: Option<String>) -> SandboxOptions {
        SandboxOptions {
            snapshot_path,
            network_enabled: true,
            mounts: vec![SandboxMount {
                host_path: self.profile.host_config_dir().to_owned(),
                guest_path: "/altitude/claude".to_owned(),
            }],
            environment: vec![
                (
                    "CLAUDE_CONFIG_DIR".to_owned(),
                    "/altitude/claude".to_owned(),
                ),
                ("SHELL".to_owned(), "/bin/sh".to_owned()),
                ("DISABLE_AUTOUPDATER".to_owned(), "1".to_owned()),
            ],
        }
    }

    fn snapshot_path(&self) -> PathBuf {
        self.snapshots_dir.join(SNAPSHOT_NAME)
    }

    fn install_command(&self) -> &'static str {
        "wget -qO /etc/apk/keys/claude-code.rsa.pub https://downloads.claude.ai/keys/claude-code.rsa.pub && \\
         printf '%s\\n' https://downloads.claude.ai/claude-code/apk/stable >> /etc/apk/repositories && \\
         apk add --no-cache claude-code libgcc libstdc++ ripgrep"
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\\"'\\\"'"))
}

impl AgentEngine for ClaudeCode {
    fn sandbox_options(&self, task_snapshot: Option<String>) -> SandboxOptions {
        self.base_sandbox_options(
            task_snapshot.or_else(|| Some(self.snapshot_path().to_string_lossy().into_owned())),
        )
    }

    fn new_conversation_id(&self) -> String {
        Uuid::new_v4().to_string()
    }

    fn command(&self, conversation_id: &str, prompt: &str, resume: bool) -> String {
        if resume {
            format!(
                "exec claude --permission-mode auto --resume {}",
                shell_quote(conversation_id)
            )
        } else {
            format!(
                "exec claude --permission-mode auto --session-id {} {}",
                shell_quote(conversation_id),
                shell_quote(prompt),
            )
        }
    }

    async fn prepare<E: IsolationEngine>(&self, isolation: &E) -> Result<(), AgentError> {
        let _preparation = self.preparation_lock.lock().await;
        tracing::info!(path = %self.profile.host_config_dir().display(), "preparing Claude profile");
        fs::create_dir_all(self.profile.host_config_dir())
            .map_err(|error| AgentError::new(error.to_string()))?;
        let settings_path = self.profile.host_config_dir().join("settings.json");
        if !settings_path.exists() {
            fs::write(settings_path, DEFAULT_SETTINGS)
                .map_err(|error| AgentError::new(error.to_string()))?;
        }
        if self.snapshot_path().exists() {
            return Ok(());
        }

        tracing::info!("building Claude base sandbox snapshot");
        isolation
            .create(
                PREPARATION_SANDBOX.to_owned(),
                self.base_sandbox_options(None),
            )
            .await
            .map_err(|error| AgentError::new(error.to_string()))?;
        let result = async {
            let mut session = isolation
                .run_command(
                    PREPARATION_SANDBOX.to_owned(),
                    self.install_command().to_owned(),
                )
                .await
                .map_err(|error| AgentError::new(error.to_string()))?;
            loop {
                let Some(event) = session.events.recv().await else {
                    return Err(AgentError::new(
                        "Claude setup connection closed without an exit status",
                    ));
                };
                match event {
                    CommandEvent::Output { .. } => {}
                    CommandEvent::Exited(0) => break,
                    CommandEvent::Exited(code) => {
                        return Err(AgentError::new(format!(
                            "Claude setup exited with code {code}"
                        )));
                    }
                    CommandEvent::Failed(error) => return Err(AgentError::new(error)),
                }
            }
            isolation
                .suspend(
                    PREPARATION_SANDBOX.to_owned(),
                    SNAPSHOT_NAME.to_owned(),
                    self.snapshots_dir.clone(),
                )
                .await
                .map_err(|error| AgentError::new(error.to_string()))
        }
        .await;
        if let Err(error) = &result {
            tracing::warn!(error = %error, "Claude base sandbox setup failed");
            if let Err(remove_error) = isolation.remove(PREPARATION_SANDBOX.to_owned()).await {
                tracing::warn!(error = %remove_error, "could not remove failed Claude setup sandbox");
            }
        }
        result?;
        tracing::info!(snapshot = %self.snapshot_path().display(), "Claude base sandbox ready");
        Ok(())
    }
}
