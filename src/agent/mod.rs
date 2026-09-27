use crate::isolation::EnvironmentRecipe;
use sqlx::Type;
use std::{error::Error as StdError, fmt, path::PathBuf};
mod claude;
pub use claude::{ClaudeCode, ClaudeStream};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
}

#[derive(Debug)]
pub struct AgentError(String);

impl AgentError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl StdError for AgentError {}

/// The persistent files that identify one agent conversation.
///
/// Agents describe their own on-disk layout; isolation validates and removes
/// these entries inside the named profile.
#[derive(Clone, Debug)]
pub struct ConversationFiles {
    pub profile: &'static str,
    pub transcript: PathBuf,
    pub cleanup_entries: Vec<PathBuf>,
}

/// Agent-specific commands and environment requirements; isolation owns their lifecycle.
pub trait AgentEngine: Send + Sync {
    fn environment_recipe(&self) -> EnvironmentRecipe;
    fn new_conversation_id(&self) -> String;
    fn conversation_files(&self, conversation_id: &str) -> ConversationFiles;
}
