use std::{
    error::Error as StdError,
    fmt,
    future::Future,
    path::{Path, PathBuf},
};

use sqlx::Type;

use crate::isolation::{AgentBaseSandboxSnapshot, IsolationEngine};

mod claude;

pub use claude::ClaudeCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Type)]
#[sqlx(type_name = "TEXT", rename_all = "lowercase")]
pub enum AgentKind {
    Claude,
}

#[derive(Clone, Debug)]
pub struct AgentProfile {
    host_config_dir: PathBuf,
}

impl AgentProfile {
    pub fn new(host_config_dir: PathBuf) -> Self {
        Self { host_config_dir }
    }

    pub fn host_config_dir(&self) -> &Path {
        &self.host_config_dir
    }
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

/// Describes one agent's durable profile and its provider-specific preparation.
///
/// Agent implementations never read the host user's configuration. They expose the
/// one Altitude-owned directory that may be mounted into their sandboxes instead.
pub trait AgentEngine: Send + Sync {
    fn new_conversation_id(&self) -> String;

    fn command(&self, conversation_id: &str, prompt: &str, resume: bool) -> String;

    /// Prepares agent-owned profile state, then asks isolation to provision the
    /// reusable environment described by the implementation's own recipe.
    fn prepare<E: IsolationEngine>(
        &self,
        isolation: &E,
    ) -> impl Future<Output = Result<AgentBaseSandboxSnapshot, AgentError>> + Send;

    /// Creates one task sandbox, restoring a task snapshot when available or
    /// using the current agent environment for a fresh task.
    fn provision_sandbox<E: IsolationEngine>(
        &self,
        isolation: &E,
        sandbox_id: String,
        task_snapshot: Option<String>,
    ) -> impl Future<Output = Result<(), AgentError>> + Send;
}
