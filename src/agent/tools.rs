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

pub type AgentToolFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, AgentToolError>> + Send + 'a>>;

pub trait AgentToolProvider: Send + Sync {
    fn namespace(&self) -> &'static str;
    fn tools(&self) -> Vec<AgentTool>;
    fn call(&self, name: &str, arguments: Value) -> AgentToolFuture<'_>;
}

#[derive(Clone, Default)]
pub struct AgentToolRegistry {
    providers: Vec<Arc<dyn AgentToolProvider>>,
}

impl AgentToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

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
