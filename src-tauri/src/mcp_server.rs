//! Lightweight MCP server boundary.
//!
//! This implements the useful subset of an MCP wrapper without pulling an HTTP
//! framework into the desktop binary. Stdio JSON-RPC is enough for local MCP
//! clients and keeps registration/testability independent of Tauri.

use std::{collections::BTreeMap, io, sync::Arc};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::tool::{BaseTool, ToolCollection};

#[derive(Clone, Default)]
pub struct McpServer {
    pub name: String,
    pub version: String,
    pub tools: ToolCollection,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("McpServer")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("tools", &self.tools.names())
            .finish()
    }
}

impl McpServer {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version: "0.1.0".to_string(),
            tools: ToolCollection::default(),
        }
    }

    pub fn register_tool(&mut self, tool: Arc<dyn BaseTool>) -> bool {
        self.tools.add(tool)
    }

    pub fn register_tools(&mut self, tools: impl IntoIterator<Item = Arc<dyn BaseTool>>) -> usize {
        self.tools.add_many(tools)
    }

    pub fn list_tools(&self) -> Vec<Value> {
        self.tools
            .to_params()
            .into_iter()
            .filter_map(|param| {
                let function = param.get("function")?;
                Some(json!({
                    "name": function.get("name")?,
                    "description": function.get("description").cloned().unwrap_or(Value::Null),
                    "inputSchema": function.get("parameters").cloned().unwrap_or_else(|| json!({"type": "object"}))
                }))
            })
            .collect()
    }

    pub async fn handle_request(
        &self,
        request: Value,
        cancel: &CancellationToken,
    ) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let response = match method {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": self.name, "version": self.version}
            }),
            "notifications/initialized" => return None,
            "ping" => json!({}),
            "tools/list" => json!({"tools": self.list_tools()}),
            "tools/call" => {
                let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                if name.is_empty() {
                    json!({"error": {"code": -32602, "message": "tools/call requires name"}})
                } else {
                    let result = tokio::select! {
                        _ = cancel.cancelled() => return Some(error_response(id, -32800, "Request cancelled.")),
                        result = self.tools.execute(name, arguments) => result,
                    };
                    json!({
                        "content": [{"type": "text", "text": result.text()}],
                        "isError": !result.is_success()
                    })
                }
            }
            _ => {
                return Some(error_response(
                    id,
                    -32601,
                    &format!("Method not found: {method}"),
                ))
            }
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": response}))
    }

    pub async fn run_stdio(&self, cancel: &CancellationToken) -> io::Result<()> {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut stdout = tokio::io::stdout();
        let mut line = String::new();
        loop {
            line.clear();
            let read = tokio::select! {
                _ = cancel.cancelled() => break,
                result = reader.read_line(&mut line) => result?,
            };
            if read == 0 {
                break;
            }
            let request = match serde_json::from_str::<Value>(line.trim()) {
                Ok(value) => value,
                Err(error) => {
                    let response = error_response(None, -32700, &format!("Parse error: {error}"));
                    stdout
                        .write_all(format!("{}\n", response).as_bytes())
                        .await?;
                    stdout.flush().await?;
                    continue;
                }
            };
            if let Some(response) = self.handle_request(request, cancel).await {
                stdout
                    .write_all(format!("{}\n", response).as_bytes())
                    .await?;
                stdout.flush().await?;
            }
        }
        Ok(())
    }
}

fn error_response(id: Option<Value>, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

pub fn tool_schema_map(server: &McpServer) -> BTreeMap<String, Value> {
    server
        .list_tools()
        .into_iter()
        .filter_map(|tool| {
            Some((
                tool.get("name")?.as_str()?.to_string(),
                tool.get("inputSchema")?.clone(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{FunctionTool, ToolDefinition, ToolResult};

    fn echo_tool() -> Arc<dyn BaseTool> {
        Arc::new(FunctionTool::new(
            ToolDefinition::new("rust_echo", "Echo input", json!({"type": "object"})),
            |arguments| async move { Ok(ToolResult::success(arguments)) },
        ))
    }

    #[tokio::test]
    async fn mcp_server_lists_and_executes_registered_tools() {
        let mut server = McpServer::new("rustpilot");
        assert!(server.register_tool(echo_tool()));
        assert_eq!(tool_schema_map(&server).len(), 1);
        let response = server
            .handle_request(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": "rust_echo", "arguments": {"ok": true}}
                }),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("ok"));
    }

    #[tokio::test]
    async fn mcp_notifications_do_not_emit_responses() {
        let server = McpServer::new("rustpilot");
        assert!(server
            .handle_request(
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                &CancellationToken::new()
            )
            .await
            .is_none());
    }
}
