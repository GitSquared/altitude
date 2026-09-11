use super::AgentEngine;
use crate::isolation::{DefaultFile, EnvironmentRecipe, GuestPath, PersistentDirectory};
use uuid::Uuid;

#[derive(Clone)]
pub struct ClaudeCode;

const DEFAULT_SETTINGS: &str = r#"{
  "tui": "fullscreen",
  "theme": "dark",
  "permissions": {
    "defaultMode": "auto"
  }
}
"#;

impl ClaudeCode {
    fn install_command() -> &'static str {
        "wget -qO /etc/apk/keys/claude-code.rsa.pub https://downloads.claude.ai/keys/claude-code.rsa.pub && \\
         printf '%s\\n' https://downloads.claude.ai/claude-code/apk/stable >> /etc/apk/repositories && \\
         apk upgrade --no-cache && \\
         apk add --no-cache claude-code libgcc libstdc++ ripgrep"
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\\"'\\\"'"))
}

impl AgentEngine for ClaudeCode {
    fn environment_recipe(&self) -> EnvironmentRecipe {
        EnvironmentRecipe {
            profile: "agents/claude",
            setup_command: Self::install_command(),
            directories: vec![PersistentDirectory {
                name: "",
                guest_path: GuestPath::Absolute("/altitude/claude"),
                defaults: vec![DefaultFile {
                    path: "settings.json",
                    contents: DEFAULT_SETTINGS,
                }],
            }],
            environment: vec![
                ("CLAUDE_CONFIG_DIR", "/altitude/claude"),
                ("SHELL", "/bin/sh"),
                ("DISABLE_AUTOUPDATER", "1"),
            ],
        }
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
}
