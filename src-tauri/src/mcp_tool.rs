use std::sync::atomic::Ordering;

use serde_json::{json, Value};

use crate::mcp_transport::{open_session, session_notification, session_request};
use crate::tool_registry::{canonical_json, McpToolDefinition};
use crate::{string_argument, truncate_output, AppState};

use crate::mcp_transport::request;
pub(crate) use crate::mcp_transport::McpSession;

pub(crate) fn sanitize_name(value: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    while result.contains("__") {
        result = result.replace("__", "_");
    }
    result.trim_matches('_').to_string()
}

async fn connect(
    state: &AppState,
    server_id: &str,
    transport: &str,
    url: Option<String>,
    command: Option<String>,
    args: Vec<String>,
) -> Result<String, String> {
    let mut session = open_session(server_id, transport, url, command, args).await?;

    let initialize = request("initialize", session.next_id, &json!({}))?;
    session.next_id += 1;
    let initialized_response = session_request(&mut session, initialize).await?;
    if initialized_response.get("error").is_some() {
        return Err(format!("MCP initialize failed: {initialized_response}"));
    }
    let notification = request("initialized", session.next_id, &json!({}))?;
    session.next_id += 1;
    session_notification(&mut session, notification).await?;
    let tools_request = request("list_tools", session.next_id, &json!({}))?;
    session.next_id += 1;
    let tools_response = session_request(&mut session, tools_request).await?;
    if tools_response.get("error").is_some() {
        return Err(format!("MCP tools/list failed: {tools_response}"));
    }
    register_tools(state, server_id, &tools_response)?;
    state
        .mcp_sessions
        .lock()
        .await
        .insert(server_id.to_string(), session);
    Ok(format!(
        "Connected to MCP server '{server_id}' and registered {} tool(s).",
        state
            .mcp_tools
            .read()
            .ok()
            .map(|tools| {
                tools
                    .values()
                    .filter(|tool| tool.server_id == server_id)
                    .count()
            })
            .unwrap_or_default()
    ))
}

pub(crate) fn register_tools(
    state: &AppState,
    server_id: &str,
    response: &Value,
) -> Result<(), String> {
    let tools = response
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "MCP tools/list response has no result.tools array.".to_string())?;

    let server = sanitize_name(server_id);
    let mut incoming = tools
        .iter()
        .map(|tool| {
            let remote_name = tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "MCP tool is missing name.".to_string())?;
            let remote = sanitize_name(remote_name);
            Ok(McpToolDefinition {
                exposed_name: format!("rust_mcp_{server}_{remote}"),
                server_id: server_id.to_string(),
                remote_name: remote_name.to_string(),
                description: tool
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("MCP tool")
                    .to_string(),
                input_schema: tool
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    incoming.sort_by_cached_key(|tool| {
        (
            tool.exposed_name.clone(),
            tool.remote_name.clone(),
            tool.description.clone(),
            serde_json::to_string(&canonical_json(&tool.input_schema)).unwrap_or_default(),
        )
    });
    incoming.dedup_by(|left, right| left.exposed_name == right.exposed_name);

    let mut registry = state
        .mcp_tools
        .write()
        .map_err(|_| "MCP tool registry lock is poisoned".to_string())?;
    let current_count = registry
        .values()
        .filter(|tool| tool.server_id == server_id)
        .count();
    let unchanged = current_count == incoming.len()
        && incoming.iter().all(|tool| {
            registry
                .get(&tool.exposed_name)
                .map(|current| current == tool)
                .unwrap_or(false)
        });
    if unchanged {
        return Ok(());
    }
    registry.retain(|_, tool| tool.server_id != server_id);
    for tool in incoming {
        registry.insert(tool.exposed_name.clone(), tool);
    }
    state.mcp_tools_revision.fetch_add(1, Ordering::AcqRel);
    if let Ok(mut cache) = state.tool_definition_cache.write() {
        *cache = None;
    }
    Ok(())
}

pub(crate) async fn run_dynamic(
    state: &AppState,
    name: &str,
    arguments: &Value,
) -> Result<String, String> {
    let definition = state
        .mcp_tools
        .read()
        .map_err(|_| "MCP tool registry lock is poisoned".to_string())?
        .get(name)
        .cloned()
        .ok_or_else(|| format!("Unknown dynamic MCP tool: {name}"))?;
    let mut sessions = state.mcp_sessions.lock().await;
    let session = sessions
        .get_mut(&definition.server_id)
        .ok_or_else(|| format!("MCP server '{}' is not connected.", definition.server_id))?;
    let request = request(
        "call_tool",
        session.next_id,
        &json!({
            "tool_name": definition.remote_name,
            "arguments": arguments
        }),
    )?;
    session.next_id += 1;
    let response = session_request(session, request).await?;
    if response.get("error").is_some() {
        return Err(format!("MCP tool failed: {response}"));
    }
    Ok(truncate_output(
        &serde_json::to_string_pretty(&response).unwrap_or_else(|_| response.to_string()),
    ))
}

pub(crate) async fn run(state: &AppState, arguments: &Value) -> Result<String, String> {
    let action = string_argument(arguments, "action").unwrap_or_else(|| "list_tools".to_string());
    let server_id = string_argument(arguments, "server_id")
        .or_else(|| string_argument(arguments, "url"))
        .or_else(|| string_argument(arguments, "command"))
        .unwrap_or_else(|| "default".to_string());
    match action.as_str() {
        "connect" | "list_tools" => {
            let session_exists = state.mcp_sessions.lock().await.contains_key(&server_id);
            if !session_exists {
                let transport =
                    string_argument(arguments, "transport").unwrap_or_else(|| "http".to_string());
                let url = string_argument(arguments, "url");
                let command = string_argument(arguments, "command");
                let args = arguments
                    .get("args")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                connect(state, &server_id, &transport, url, command, args).await?;
            } else {
                let response = {
                    let mut sessions = state.mcp_sessions.lock().await;
                    let session = sessions
                        .get_mut(&server_id)
                        .ok_or_else(|| "MCP session disappeared.".to_string())?;
                    let request = request("list_tools", session.next_id, &json!({}))?;
                    session.next_id += 1;
                    session_request(session, request).await?
                };
                register_tools(state, &server_id, &response)?;
            }
            let mut tools = state
                .mcp_tools
                .read()
                .map_err(|_| "MCP tool registry lock is poisoned".to_string())?
                .values()
                .filter(|tool| tool.server_id == server_id)
                .map(|tool| json!({"name": tool.exposed_name, "remote_name": tool.remote_name, "description": tool.description, "input_schema": tool.input_schema}))
                .collect::<Vec<_>>();
            tools.sort_by(|left, right| {
                left.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .cmp(
                        right
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
            });
            Ok(truncate_output(
                &serde_json::to_string_pretty(&json!({
                    "server_id": server_id,
                    "tools": tools
                }))
                .unwrap_or_default(),
            ))
        }
        "call_tool" => {
            let name = string_argument(arguments, "tool_name")
                .ok_or_else(|| "tool_name is required for call_tool".to_string())?;
            if name.starts_with("rust_mcp_") {
                return run_dynamic(
                    state,
                    &name,
                    arguments.get("arguments").unwrap_or(&json!({})),
                )
                .await;
            }
            if !state.mcp_sessions.lock().await.contains_key(&server_id) {
                return Err(format!("MCP server '{server_id}' is not connected."));
            }
            let mut sessions = state.mcp_sessions.lock().await;
            let session = sessions
                .get_mut(&server_id)
                .ok_or_else(|| "MCP session disappeared.".to_string())?;
            let request = request("call_tool", session.next_id, arguments)?;
            session.next_id += 1;
            let response = session_request(session, request).await?;
            Ok(truncate_output(
                &serde_json::to_string_pretty(&response).unwrap_or_else(|_| response.to_string()),
            ))
        }
        "disconnect" => {
            state.mcp_sessions.lock().await.remove(&server_id);
            let mut changed = false;
            if let Ok(mut tools) = state.mcp_tools.write() {
                let before = tools.len();
                tools.retain(|_, tool| tool.server_id != server_id);
                changed = tools.len() != before;
            }
            if changed {
                state.mcp_tools_revision.fetch_add(1, Ordering::AcqRel);
                if let Ok(mut cache) = state.tool_definition_cache.write() {
                    *cache = None;
                }
            }
            Ok(format!("Disconnected MCP server '{server_id}'."))
        }
        _ => Err(format!("Unsupported MCP action: {action}")),
    }
}
