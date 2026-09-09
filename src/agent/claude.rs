use std::{fs, time::Duration};
use uuid::Uuid;

use crate::{
    config::Config,
    isolation::{
        AgentBaseSandboxRecipe, AgentBaseSandboxSnapshot, ImagePullPolicy, IsolationEngine,
        SandboxMount, SandboxOptions,
    },
};

use super::{AgentEngine, AgentError, AgentProfile};

#[derive(Clone)]
pub struct ClaudeCode {
    profile: AgentProfile,
    base_recipe: AgentBaseSandboxRecipe,
}

const BASE_REFRESH_AFTER: Duration = Duration::from_secs(72 * 60 * 60);
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
            base_recipe: AgentBaseSandboxRecipe::new(
                "claude",
                config.agent_snapshots_dir("claude"),
                BASE_REFRESH_AFTER,
                Self::install_command(),
            ),
        }
    }

    fn task_sandbox_options(&self, snapshot_path: Option<String>) -> SandboxOptions {
        SandboxOptions {
            snapshot_path,
            image_pull_policy: ImagePullPolicy::Never,
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

    fn install_command() -> &'static str {
        "wget -qO /etc/apk/keys/claude-code.rsa.pub https://downloads.claude.ai/keys/claude-code.rsa.pub && \\
         printf '%s\\n' https://downloads.claude.ai/claude-code/apk/stable >> /etc/apk/repositories && \\
         apk upgrade --no-cache && \\
         apk add --no-cache claude-code libgcc libstdc++ ripgrep"
    }

    fn base_recipe(&self) -> AgentBaseSandboxRecipe {
        self.base_recipe.clone()
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\\"'\\\"'"))
}

impl AgentEngine for ClaudeCode {
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

    async fn prepare<E: IsolationEngine>(
        &self,
        isolation: &E,
    ) -> Result<AgentBaseSandboxSnapshot, AgentError> {
        tracing::info!(path = %self.profile.host_config_dir().display(), "preparing Claude profile");
        fs::create_dir_all(self.profile.host_config_dir())
            .map_err(|error| AgentError::new(error.to_string()))?;
        let settings_path = self.profile.host_config_dir().join("settings.json");
        if !settings_path.exists() {
            fs::write(settings_path, DEFAULT_SETTINGS)
                .map_err(|error| AgentError::new(error.to_string()))?;
        }
        isolation
            .ensure_base(self.base_recipe())
            .await
            .map_err(|error| AgentError::new(error.to_string()))
    }

    async fn provision_sandbox<E: IsolationEngine>(
        &self,
        isolation: &E,
        sandbox_id: String,
        task_snapshot: Option<String>,
    ) -> Result<(), AgentError> {
        let snapshot_path = match task_snapshot {
            Some(snapshot_path) => snapshot_path,
            None => self
                .prepare(isolation)
                .await?
                .path
                .to_string_lossy()
                .into_owned(),
        };
        isolation
            .create(sandbox_id, self.task_sandbox_options(Some(snapshot_path)))
            .await
            .map_err(|error| AgentError::new(error.to_string()))
    }
}
