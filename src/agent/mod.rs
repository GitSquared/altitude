use crate::isolation::EnvironmentRecipe;
use sqlx::Type;
use std::{error::Error as StdError, fmt};
mod claude;
pub use claude::ClaudeCode;

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

/// Agent-specific commands and environment requirements; isolation owns their lifecycle.
pub trait AgentEngine: Send + Sync {
    fn environment_recipe(&self) -> EnvironmentRecipe;
    fn new_conversation_id(&self) -> String;
    fn command(&self, conversation_id: &str, prompt: &str, resume: bool) -> String;
}
