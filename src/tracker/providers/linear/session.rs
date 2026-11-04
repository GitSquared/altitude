use std::{collections::HashMap, path::Path};

use super::auth;
use super::{CLEANUP_TIMEOUT, REQUIRED_TOOLS, TOOL_TIMEOUT, tool_result};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, Tool},
    service::RunningService,
};
use serde_json::{Map, Value};
use tokio::time::timeout;

use crate::tracker::TrackerError;

pub(super) struct Session {
    pub(super) client: RunningService<RoleClient, ()>,
    tools: HashMap<String, Tool>,
}

impl Session {
    pub(super) async fn start(profile: &Path) -> Result<Self, TrackerError> {
        let client = auth::authorized_client(profile).await?;
        let transport = auth::transport(client);
        let mut client = ().serve(transport).await.map_err(|_| {
            TrackerError::new(
                "could not connect to Linear; sign in again from Settings → Task tracker",
            )
        })?;

        let tools = match client.list_all_tools().await {
            Ok(tools) => tools,
            Err(_) => {
                let _ = client.close_with_timeout(CLEANUP_TIMEOUT).await;
                return Err(TrackerError::new("could not read the Linear tool list"));
            }
        };
        let tools: HashMap<String, Tool> = tools
            .into_iter()
            .map(|tool| (tool.name.to_string(), tool))
            .collect();
        let missing: Vec<&str> = REQUIRED_TOOLS
            .iter()
            .copied()
            .filter(|name| !tools.contains_key(*name))
            .collect();
        if !missing.is_empty() {
            let _ = client.close_with_timeout(CLEANUP_TIMEOUT).await;
            return Err(TrackerError::new(format!(
                "the Linear MCP server is missing required tools: {}",
                missing.join(", ")
            )));
        }
        Ok(Self { client, tools })
    }

    pub(super) fn has(&self, tool: &str) -> bool {
        self.tools.contains_key(tool)
    }

    pub(super) fn accepts(&self, tool: &str, argument: &str) -> bool {
        self.tools
            .get(tool)
            .and_then(|tool| tool.input_schema.get("properties"))
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.contains_key(argument))
    }

    pub(super) async fn call(
        &self,
        tool: &str,
        arguments: Map<String, Value>,
    ) -> Result<Value, TrackerError> {
        if !self.has(tool) {
            return Err(TrackerError::new(format!(
                "the Linear MCP server does not offer the {tool} tool"
            )));
        }
        if let Some(argument) = arguments
            .keys()
            .find(|argument| !self.accepts(tool, argument))
        {
            return Err(TrackerError::new(format!(
                "Linear's {tool} tool does not accept {argument}; the integration needs updating"
            )));
        }
        let params = CallToolRequestParams::new(tool.to_owned()).with_arguments(arguments);
        let result = timeout(TOOL_TIMEOUT, self.client.call_tool(params))
            .await
            .map_err(|_| TrackerError::new("Linear did not answer in time"))?
            .map_err(|_| TrackerError::new("the Linear connection dropped"))?;
        tool_result(result)
    }
}
