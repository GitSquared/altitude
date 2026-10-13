//! The MCP server that gives one agent the tools its subsystems registered.
//!
//! Altitude speaks MCP over whatever byte stream reaches an agent — a Unix
//! socket bridged to a sandbox by virtio-vsock — so this module knows nothing
//! about sandboxes, tasks, or which tools exist. It binds an
//! [`AgentToolRegistry`] to the protocol and names no tool itself.

use serde_json::{Value, json};
use std::io;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

use super::tools::AgentToolRegistry;

/// The guest port an agent's bridge dials on the host. Fixed because every
/// sandbox reaches the host the same way; the host socket behind it is the
/// per-agent part.
///
/// The other half of this contract is `ALTITUDE_MCP_PORT` in
/// `src/agent/assets/bridge.py`, which defaults to the same number.
pub const AGENT_CHANNEL_PORT: u32 = 5000;

/// The protocol revisions Altitude answers, newest first.
///
/// A client that asks for one of these gets it back; one that asks for anything
/// else is offered the newest, which is the protocol's own negotiation.
const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// Serve one agent's MCP session until the stream closes.
pub async fn serve_agent<R, W>(
    registry: AgentToolRegistry,
    reader: R,
    mut writer: W,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // A notification has no id and wants no answer.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let payload = match dispatch(&registry, method, &params).await {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": error.code, "message": error.message }
            }),
        };
        let mut encoded = serde_json::to_string(&payload).expect("a JSON value serializes");
        encoded.push('\n');
        writer.write_all(encoded.as_bytes()).await?;
        writer.flush().await?;
    }
    Ok(())
}

struct RpcError {
    code: i32,
    message: String,
}

impl RpcError {
    fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("Altitude does not handle {method}"),
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }
}

async fn dispatch(
    registry: &AgentToolRegistry,
    method: &str,
    params: &Value,
) -> Result<Value, RpcError> {
    match method {
        "initialize" => Ok(initialize(params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list(registry)),
        "tools/call" => Ok(tools_call(registry, params).await),
        // Advertised as unsupported, but answered empty so a client that asks
        // anyway is not left with an error it cannot act on.
        "resources/list" => Ok(json!({ "resources": [] })),
        "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        other => Err(RpcError::method_not_found(other)),
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let version = if PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        PROTOCOL_VERSIONS[0]
    };
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "altitude", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Altitude owns this task. Its tools read and change the task's own state; the operator is usually away and reaches you through signals."
    })
}

fn tools_list(registry: &AgentToolRegistry) -> Value {
    let tools: Vec<Value> = registry
        .tools()
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "description": tool.description,
                "inputSchema": tool.input_schema,
            })
        })
        .collect();
    json!({ "tools": tools })
}

async fn tools_call(registry: &AgentToolRegistry, params: &Value) -> Value {
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        let error = RpcError::invalid_params("tools/call needs a name");
        return json!({
            "content": [{ "type": "text", "text": error.message }],
            "isError": true
        });
    };
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    match registry.call(name, arguments).await {
        Ok(value) => json!({
            "content": [{ "type": "text", "text": value.to_string() }],
            "structuredContent": value,
            "isError": false
        }),
        Err(error) => json!({
            "content": [{ "type": "text", "text": error.to_string() }],
            "isError": true
        }),
    }
}
