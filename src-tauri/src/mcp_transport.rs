//! Small MCP transport layer shared by the dynamic tool registry.

use std::{collections::HashMap, pin::Pin, sync::Arc, time::Duration};

use futures_util::{Stream, StreamExt};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{oneshot, Mutex as AsyncMutex},
    task::JoinHandle,
    time::timeout,
};

use crate::{string_argument, truncate_output};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MCP_CLIENT_VERSION: &str = "0.1.0";
const MCP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MCP_SSE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_SSE_EVENT_BYTES: usize = 4 * 1024 * 1024;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>, String>> + Send>>;
pub(crate) type PendingResponses =
    Arc<AsyncMutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

pub(crate) struct McpStdioSession {
    pub(crate) _child: Child,
    pub(crate) stdin: ChildStdin,
    pub(crate) stdout: BufReader<ChildStdout>,
}

pub(crate) struct McpSseSession {
    pub(crate) pending: PendingResponses,
    stream_task: JoinHandle<()>,
}

impl Drop for McpSseSession {
    fn drop(&mut self) {
        self.stream_task.abort();
    }
}

pub(crate) struct McpSession {
    pub(crate) _server_id: String,
    pub(crate) transport: String,
    pub(crate) endpoint: Option<String>,
    pub(crate) client: Option<Client>,
    pub(crate) stdio: Option<McpStdioSession>,
    pub(crate) sse: Option<McpSseSession>,
    pub(crate) next_id: u64,
}

pub(crate) fn request(action: &str, request_id: u64, arguments: &Value) -> Result<Value, String> {
    match action {
        "initialize" => Ok(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": "rustpilot", "version": MCP_CLIENT_VERSION}
            }
        })),
        "initialized" => Ok(json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        })),
        "list_tools" => Ok(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "tools/list",
            "params": {}
        })),
        "call_tool" => {
            let tool_name = string_argument(arguments, "tool_name")
                .ok_or_else(|| "tool_name is required for call_tool".to_string())?;
            let tool_arguments = arguments
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            Ok(json!({
                "jsonrpc": "2.0",
                "id": request_id,
                "method": "tools/call",
                "params": {"name": tool_name, "arguments": tool_arguments}
            }))
        }
        _ => Err(format!("Unsupported MCP action: {action}")),
    }
}

pub(crate) fn parse_response(body: &str) -> Result<Value, String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return Err("MCP returned an empty response.".to_string());
    }
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Ok(value);
    }

    let mut last_json = None;
    let mut last_response = None;
    for payload in sse_payloads(body) {
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            if value.get("id").is_some() || value.get("error").is_some() {
                last_response = Some(value.clone());
            }
            last_json = Some(value);
        }
    }
    last_response
        .or(last_json)
        .ok_or_else(|| "MCP returned neither JSON nor an SSE data payload.".to_string())
}

pub(crate) async fn open_session(
    server_id: &str,
    transport: &str,
    url: Option<String>,
    command: Option<String>,
    args: Vec<String>,
) -> Result<McpSession, String> {
    let transport = transport.to_ascii_lowercase();
    if !matches!(transport.as_str(), "http" | "sse" | "stdio") {
        return Err(format!("Unsupported MCP transport: {transport}"));
    }

    if transport == "stdio" {
        let command = command.ok_or_else(|| "stdio MCP transport requires command".to_string())?;
        let mut child = Command::new(&command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("Unable to start MCP stdio server: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "MCP stdio stdin is unavailable.".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "MCP stdio stdout is unavailable.".to_string())?;
        return Ok(McpSession {
            _server_id: server_id.to_string(),
            transport,
            endpoint: None,
            client: None,
            stdio: Some(McpStdioSession {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
            sse: None,
            next_id: 1,
        });
    }

    let endpoint = url.ok_or_else(|| "HTTP/SSE MCP transport requires url".to_string())?;
    let client = build_client()?;
    let (endpoint, sse) = if transport == "sse" {
        let (message_endpoint, stream) = discover_sse_endpoint(&client, &endpoint).await?;
        let pending = Arc::new(AsyncMutex::new(HashMap::new()));
        let stream_task = tokio::spawn(read_sse_stream(stream, pending.clone()));
        (
            message_endpoint,
            Some(McpSseSession {
                pending,
                stream_task,
            }),
        )
    } else {
        (endpoint, None)
    };

    Ok(McpSession {
        _server_id: server_id.to_string(),
        transport,
        endpoint: Some(endpoint),
        client: Some(client),
        stdio: None,
        sse,
        next_id: 1,
    })
}

pub(crate) async fn session_request(
    session: &mut McpSession,
    request: Value,
) -> Result<Value, String> {
    if session.transport.eq_ignore_ascii_case("stdio") {
        return stdio_request(session, request).await;
    }

    let client = session
        .client
        .as_ref()
        .cloned()
        .ok_or_else(|| "MCP HTTP session has no client.".to_string())?;
    let endpoint = session
        .endpoint
        .as_deref()
        .ok_or_else(|| "MCP HTTP session has no endpoint.".to_string())?;
    let request_id = request_id_key(&request);
    let receiver = if let (Some(sse), Some(request_id)) = (&session.sse, request_id.as_ref()) {
        let (sender, receiver) = oneshot::channel();
        sse.pending.lock().await.insert(request_id.clone(), sender);
        Some((request_id.clone(), receiver))
    } else {
        None
    };

    let result = post_json(&client, endpoint, &request).await;
    let (status, body) = match result {
        Ok(response) => response,
        Err(error) => {
            remove_pending(session, receiver.as_ref().map(|(id, _)| id)).await;
            return Err(error);
        }
    };
    if !status.is_success() {
        remove_pending(session, receiver.as_ref().map(|(id, _)| id)).await;
        return Err(format!("MCP HTTP {status}: {}", truncate_output(&body)));
    }
    if !body.trim().is_empty() {
        remove_pending(session, receiver.as_ref().map(|(id, _)| id)).await;
        return parse_response(&body);
    }

    let Some((request_id, receiver)) = receiver else {
        return Err("MCP returned an empty response.".to_string());
    };
    match timeout(MCP_REQUEST_TIMEOUT, receiver).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err(format!(
            "MCP SSE response channel closed for request {request_id}."
        )),
        Err(_) => {
            remove_pending(session, Some(&request_id)).await;
            Err(format!("MCP request {request_id} timed out."))
        }
    }
}

pub(crate) async fn session_notification(
    session: &mut McpSession,
    notification: Value,
) -> Result<(), String> {
    if session.transport.eq_ignore_ascii_case("stdio") {
        let stdio = session
            .stdio
            .as_mut()
            .ok_or_else(|| "MCP stdio session is not connected.".to_string())?;
        write_stdio(stdio, &notification).await
    } else {
        let client = session
            .client
            .as_ref()
            .cloned()
            .ok_or_else(|| "MCP HTTP session has no client.".to_string())?;
        let endpoint = session
            .endpoint
            .as_deref()
            .ok_or_else(|| "MCP HTTP session has no endpoint.".to_string())?;
        let (status, body) = post_json(&client, endpoint, &notification).await?;
        if !status.is_success() {
            return Err(format!(
                "MCP notification HTTP {status}: {}",
                truncate_output(&body)
            ));
        }
        Ok(())
    }
}

async fn stdio_request(session: &mut McpSession, request: Value) -> Result<Value, String> {
    let stdio = session
        .stdio
        .as_mut()
        .ok_or_else(|| "MCP stdio session is not connected.".to_string())?;
    write_stdio(stdio, &request).await?;
    loop {
        let mut line = String::new();
        let bytes = stdio
            .stdout
            .read_line(&mut line)
            .await
            .map_err(|error| format!("Unable to read MCP stdio server: {error}"))?;
        if bytes == 0 {
            return Err("MCP stdio server exited without a response.".to_string());
        }
        if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
            if value.get("id").is_some() || value.get("error").is_some() {
                return Ok(value);
            }
        }
    }
}

async fn write_stdio(stdio: &mut McpStdioSession, value: &Value) -> Result<(), String> {
    stdio
        .stdin
        .write_all(format!("{}\n", value).as_bytes())
        .await
        .map_err(|error| format!("Unable to write to MCP stdio server: {error}"))?;
    stdio
        .stdin
        .flush()
        .await
        .map_err(|error| format!("Unable to flush MCP stdio server: {error}"))
}

fn build_client() -> Result<Client, String> {
    Client::builder()
        .user_agent("RustPilot/0.1 MCP client")
        .connect_timeout(Duration::from_secs(10))
        .timeout(MCP_REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("Unable to create MCP client: {error}"))
}

async fn post_json(
    client: &Client,
    endpoint: &str,
    request: &Value,
) -> Result<(StatusCode, String), String> {
    let response = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .json(request)
        .send()
        .await
        .map_err(|error| format!("MCP HTTP request failed: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Unable to read MCP response: {error}"))?;
    Ok((status, body))
}

async fn discover_sse_endpoint(client: &Client, url: &str) -> Result<(String, ByteStream), String> {
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await
        .map_err(|error| format!("MCP SSE connection failed: {error}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "MCP SSE connection returned HTTP {}.",
            response.status()
        ));
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !content_type.contains("text/event-stream") {
        return Err("MCP SSE endpoint did not return text/event-stream.".to_string());
    }

    let mut stream: ByteStream = Box::pin(response.bytes_stream().map(|chunk| {
        chunk
            .map(|bytes| bytes.to_vec())
            .map_err(|error| error.to_string())
    }));
    let mut buffer = Vec::new();
    loop {
        let chunk = timeout(MCP_SSE_CONNECT_TIMEOUT, stream.next())
            .await
            .map_err(|_| "Timed out waiting for the MCP SSE endpoint event.".to_string())?
            .ok_or_else(|| "MCP SSE stream ended before sending an endpoint event.".to_string())?
            .map_err(|error| format!("Unable to read MCP SSE endpoint event: {error}"))?;
        buffer.extend_from_slice(&chunk);
        if buffer.len() > MAX_SSE_EVENT_BYTES {
            return Err("MCP SSE endpoint event exceeded the 4 MiB limit.".to_string());
        }
        while let Some(block) = take_sse_block(&mut buffer) {
            let Some((event, data)) = parse_sse_block(&block) else {
                continue;
            };
            if event.as_deref() != Some("endpoint") {
                continue;
            }
            let endpoint = resolve_endpoint(url, data.trim())?;
            return Ok((endpoint, stream));
        }
    }
}

async fn read_sse_stream(mut stream: ByteStream, pending: PendingResponses) {
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        buffer.extend_from_slice(&chunk);
        if buffer.len() > MAX_SSE_EVENT_BYTES {
            buffer.clear();
            continue;
        }
        while let Some(block) = take_sse_block(&mut buffer) {
            dispatch_sse_block(&block, &pending).await;
        }
    }
    if !buffer.is_empty() {
        dispatch_sse_block(&buffer, &pending).await;
    }
    let mut pending = pending.lock().await;
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err("MCP SSE stream closed.".to_string()));
    }
}

async fn dispatch_sse_block(block: &[u8], pending: &PendingResponses) {
    let Some((_, data)) = parse_sse_block(block) else {
        return;
    };
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(data) else {
        return;
    };
    let Some(request_id) = request_id_key(&value) else {
        return;
    };
    if let Some(sender) = pending.lock().await.remove(&request_id) {
        let _ = sender.send(Ok(value));
    }
}

async fn remove_pending(session: &McpSession, request_id: Option<&String>) {
    let (Some(sse), Some(request_id)) = (&session.sse, request_id) else {
        return;
    };
    sse.pending.lock().await.remove(request_id);
}

fn request_id_key(value: &Value) -> Option<String> {
    let id = value.get("id")?;
    match id {
        Value::String(value) => Some(format!("s:{value}")),
        Value::Number(value) => Some(format!("n:{value}")),
        _ => None,
    }
}

fn resolve_endpoint(base: &str, endpoint: &str) -> Result<String, String> {
    if let Ok(endpoint) = reqwest::Url::parse(endpoint) {
        return Ok(endpoint.to_string());
    }
    reqwest::Url::parse(base)
        .and_then(|base| base.join(endpoint))
        .map(|url| url.to_string())
        .map_err(|error| format!("MCP SSE endpoint is not a valid URL: {error}"))
}

fn sse_payloads(body: &str) -> Vec<String> {
    let mut payloads = Vec::new();
    let mut block = String::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            if let Some((_, data)) = parse_sse_block(block.as_bytes()) {
                payloads.push(data);
            }
            block.clear();
        } else {
            block.push_str(line);
            block.push('\n');
        }
    }
    if !block.is_empty() {
        if let Some((_, data)) = parse_sse_block(block.as_bytes()) {
            payloads.push(data);
        }
    }
    payloads.extend(
        body.lines()
            .filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok())
            .map(|value| value.to_string()),
    );
    payloads
}

fn take_sse_block(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let boundary = (0..buffer.len()).find_map(|index| {
        let first_len = line_ending_len(buffer, index)?;
        let second_index = index + first_len;
        let second_len = line_ending_len(buffer, second_index)?;
        Some((index, first_len + second_len))
    });
    let (end, delimiter_len) = boundary?;
    let block = buffer[..end].to_vec();
    buffer.drain(..end + delimiter_len);
    Some(block)
}

fn line_ending_len(buffer: &[u8], index: usize) -> Option<usize> {
    match *buffer.get(index)? {
        b'\n' => Some(1),
        b'\r' if buffer.get(index + 1) == Some(&b'\n') => Some(2),
        // A trailing CR may be the first half of a CRLF split across chunks.
        b'\r' if index + 1 < buffer.len() => Some(1),
        _ => None,
    }
}

fn parse_sse_block(block: &[u8]) -> Option<(Option<String>, String)> {
    let text = std::str::from_utf8(block).ok()?;
    let mut event = None;
    let mut data = Vec::new();
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_string());
        } else if let Some(value) = line.strip_prefix("data:") {
            data.push(value.strip_prefix(' ').unwrap_or(value).to_string());
        }
    }
    (!data.is_empty()).then(|| (event, data.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::{parse_response, parse_sse_block, request, take_sse_block};
    use serde_json::json;

    #[test]
    fn parses_multiline_sse_json() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":1,\ndata: \"result\":{\"tools\":[]}}\n\n";
        let value = parse_response(body).expect("SSE response should parse");
        assert_eq!(
            value
                .pointer("/result/tools")
                .and_then(|value| value.as_array())
                .map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn accepts_json_rpc_json_body() {
        let value = parse_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
            .expect("JSON response should parse");
        assert_eq!(value.get("id"), Some(&json!(1)));
    }

    #[test]
    fn preserves_sse_event_boundaries_across_chunks() {
        let mut buffer = b"event: endpoint\ndata: /message\n\r".to_vec();
        assert!(take_sse_block(&mut buffer).is_none());
        buffer.extend_from_slice(b"\n");
        let block = take_sse_block(&mut buffer).expect("event should complete");
        assert_eq!(
            parse_sse_block(&block).expect("event should parse").1,
            "/message"
        );
    }

    #[test]
    fn builds_initialize_request() {
        let value = request("initialize", 7, &json!({})).expect("request should build");
        assert_eq!(value.get("id"), Some(&json!(7)));
        assert_eq!(
            value.pointer("/params/protocolVersion"),
            Some(&json!("2024-11-05"))
        );
    }
}
