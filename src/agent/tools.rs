//! The capabilities Altitude's subsystems offer to the agents they run, and the
//! registry that binds them for one agent.
//!
//! The names point in both directions on purpose. An [`AgentToolProvider`] is a
//! subsystem — `tasks`, and later `vcs` or anything else with state an agent
//! should reach — and an [`AgentTool`] is one thing it offers that agent. Each
//! provider declares the namespace its tools live in, so a tool's name says
//! which subsystem answers it.
//!
//! Nothing here knows MCP: the protocol in [`crate::agent::mcp`] reads an
//! [`AgentToolRegistry`] and names no tool itself.

use serde_json::Value;
use std::{error::Error, fmt, future::Future, pin::Pin, sync::Arc};

#[derive(Clone, Debug)]
pub struct AgentTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

impl AgentTool {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
        }
    }
}

/// Why a tool call could not be answered.
///
/// This is the agent's problem, not the server's: it travels back as the tool's
/// result so the model can read it and correct itself.
#[derive(Debug)]
pub struct AgentToolError(String);

impl AgentToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for AgentToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Error for AgentToolError {}

/// The result of a tool call, boxed so one registry can hold providers of
/// different concrete types behind one trait.
pub type AgentToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, AgentToolError>> + Send + 'a>>;

/// A subsystem's capabilities for agents.
///
/// Implemented beside the state the tools act on, so a subsystem owns its own
/// agent surface. Implementations are shared across an agent's connections, so
/// they must be safe to call from several turns at once.
pub trait AgentToolProvider: Send + Sync {
    /// The subsystem these tools belong to. Every tool is named
    /// `<namespace>_<action>`.
    fn namespace(&self) -> &'static str;
    fn tools(&self) -> Vec<AgentTool>;
    fn call(&self, name: &str, arguments: Value) -> AgentToolFuture<'_>;
}

/// Every subsystem's agent tools, looked up by tool name.
///
/// This is what the MCP server serves and what the binding point assembles. It
/// holds providers, not tools: a call is routed to whichever subsystem declared
/// the name.
#[derive(Clone, Default)]
pub struct AgentToolRegistry {
    providers: Vec<Arc<dyn AgentToolProvider>>,
}

impl AgentToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind a subsystem's agent tools.
    ///
    /// The subsystem owns its names; the one rule is that each begins with the
    /// namespace it declares, which is checked in development builds.
    pub fn register(&mut self, provider: impl AgentToolProvider + 'static) -> &mut Self {
        let namespace = provider.namespace();
        debug_assert!(
            provider
                .tools()
                .iter()
                .all(|tool| tool.name.starts_with(&format!("{namespace}_"))),
            "every tool a provider declares belongs to its namespace"
        );
        self.providers.push(Arc::new(provider));
        self
    }

    pub fn tools(&self) -> Vec<AgentTool> {
        self.providers
            .iter()
            .flat_map(|provider| provider.tools())
            .collect()
    }

    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value, AgentToolError> {
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.tools().iter().any(|tool| tool.name == name))
            .ok_or_else(|| AgentToolError::new(format!("there is no tool named {name}")))?;
        provider.call(name, arguments).await
    }
}
