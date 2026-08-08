//! Small A2A protocol boundary used by the RustPilot Agent layer.
//!
//! The A2A adapter is deliberately non-streaming. RustPilot exposes the
//! request/response types so a future Tauri command or local server can reuse
//! them without coupling the Agent loop to a web framework.

use std::{error::Error, fmt::Display, future::Future, pin::Pin, sync::Arc, time::Duration};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum A2AStatus {
    InputRequired,
    Completed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct A2AResponse {
    pub status: A2AStatus,
    pub message: String,
    pub is_task_complete: bool,
    pub require_user_input: bool,
}

impl A2AResponse {
    pub fn completed(message: impl Into<String>) -> Self {
        Self {
            status: A2AStatus::Completed,
            message: message.into(),
            is_task_complete: true,
            require_user_input: false,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            status: A2AStatus::Error,
            message: message.into(),
            is_task_complete: true,
            require_user_input: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCapabilities {
    pub streaming: bool,
    #[serde(rename = "pushNotifications")]
    pub push_notifications: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub url: String,
    pub version: String,
    #[serde(rename = "defaultInputModes")]
    pub default_input_modes: Vec<String>,
    #[serde(rename = "defaultOutputModes")]
    pub default_output_modes: Vec<String>,
    pub capabilities: AgentCapabilities,
    #[serde(default)]
    pub skills: Vec<AgentSkill>,
}

impl Default for AgentCard {
    fn default() -> Self {
        Self {
            name: "RustPilot Agent".to_string(),
            description: "A Rust desktop agent with local and remote tools.".to_string(),
            url: "http://127.0.0.1:10000/".to_string(),
            version: "0.1.0".to_string(),
            default_input_modes: vec!["text".to_string(), "text/plain".to_string()],
            default_output_modes: vec!["text".to_string(), "text/plain".to_string()],
            capabilities: AgentCapabilities {
                streaming: false,
                push_notifications: true,
            },
            skills: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct A2ATaskRequest {
    pub jsonrpc: String,
    pub id: String,
    pub method: String,
    pub params: Value,
}

impl A2ATaskRequest {
    pub fn message_send(query: &str, session_id: &str) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Uuid::new_v4().to_string(),
            method: "message/send".to_string(),
            params: json!({
                "message": {
                    "messageId": Uuid::new_v4().to_string(),
                    "role": "user",
                    "parts": [{"kind": "text", "text": query}],
                    "contextId": session_id
                }
            }),
        }
    }
}

#[derive(Debug)]
pub enum A2AError {
    InvalidEndpoint(String),
    InvalidRequest(String),
    Io(std::io::Error),
    Http(reqwest::Error),
    Status(reqwest::StatusCode, String),
    Decode(serde_json::Error),
    Cancelled,
    Timeout,
}

impl Display for A2AError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidEndpoint(value) => write!(formatter, "Invalid A2A endpoint: {value}"),
            Self::InvalidRequest(value) => write!(formatter, "Invalid A2A request: {value}"),
            Self::Io(error) => write!(formatter, "A2A server I/O failed: {error}"),
            Self::Http(error) => write!(formatter, "A2A request failed: {error}"),
            Self::Status(status, body) => write!(formatter, "A2A returned HTTP {status}: {body}"),
            Self::Decode(error) => write!(formatter, "Invalid A2A response: {error}"),
            Self::Cancelled => formatter.write_str("A2A request cancelled."),
            Self::Timeout => formatter.write_str("A2A request timed out."),
        }
    }
}

impl Error for A2AError {}

impl From<std::io::Error> for A2AError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<reqwest::Error> for A2AError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl From<serde_json::Error> for A2AError {
    fn from(error: serde_json::Error) -> Self {
        Self::Decode(error)
    }
}

#[derive(Clone)]
pub struct A2AClient {
    endpoint: String,
    client: Client,
    timeout: Duration,
}

pub type A2AHandlerFuture = Pin<Box<dyn Future<Output = A2AResponse> + Send>>;

pub type A2AHandler =
    Arc<dyn Fn(String, String, CancellationToken) -> A2AHandlerFuture + Send + Sync>;

#[derive(Clone)]
pub struct A2AServer {
    card: AgentCard,
    handler: A2AHandler,
    max_request_bytes: usize,
}

impl std::fmt::Debug for A2AServer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("A2AServer")
            .field("card", &self.card)
            .field("max_request_bytes", &self.max_request_bytes)
            .finish()
    }
}

impl A2AServer {
    pub fn new(card: AgentCard, handler: A2AHandler) -> Self {
        Self {
            card,
            handler,
            max_request_bytes: 1_048_576,
        }
    }

    pub fn card(&self) -> &AgentCard {
        &self.card
    }

    pub fn with_max_request_bytes(mut self, max_request_bytes: usize) -> Self {
        self.max_request_bytes = max_request_bytes.max(1024);
        self
    }

    pub async fn bind(self, address: &str, shutdown: CancellationToken) -> Result<(), A2AError> {
        let listener = TcpListener::bind(address).await?;
        self.serve(listener, shutdown).await
    }

    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: CancellationToken,
    ) -> Result<(), A2AError> {
        let server = Arc::new(self);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let server = Arc::clone(&server);
                    tokio::spawn(async move {
                        if let Err(error) = server.handle_connection(stream).await {
                            tracing::warn!("A2A connection failed: {error}");
                        }
                    });
                }
            }
        }
    }

    async fn handle_connection(&self, mut stream: TcpStream) -> Result<(), A2AError> {
        let request = read_http_request(&mut stream, self.max_request_bytes).await?;
        let response = match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/.well-known/agent.json") | ("GET", "/agent.json") => {
                http_json_response(200, &serde_json::to_value(&self.card)?)
            }
            ("POST", _) => self.handle_message(request.body.as_deref()).await?,
            _ => http_json_response(405, &json!({"error": "Method not allowed"})),
        };
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await?;
        Ok(())
    }

    async fn handle_message(&self, body: Option<&[u8]>) -> Result<String, A2AError> {
        let body =
            body.ok_or_else(|| A2AError::InvalidRequest("Request body is required.".to_string()))?;
        let request: A2ATaskRequest = serde_json::from_slice(body)?;
        if request.jsonrpc != "2.0" || request.method != "message/send" {
            return Ok(http_json_response(
                400,
                &json!({
                    "jsonrpc": "2.0",
                    "id": request.id,
                    "error": {"code": -32601, "message": "Only message/send is supported."}
                }),
            ));
        }
        let query = request
            .params
            .pointer("/message/parts/0/text")
            .and_then(Value::as_str)
            .or_else(|| {
                request
                    .params
                    .pointer("/message/content")
                    .and_then(Value::as_str)
            })
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| A2AError::InvalidRequest("A text message is required.".to_string()))?;
        let session_id = request
            .params
            .pointer("/message/contextId")
            .and_then(Value::as_str)
            .unwrap_or("default")
            .to_string();
        let result = (self.handler)(query.to_string(), session_id, CancellationToken::new()).await;
        let payload = json!({
            "jsonrpc": "2.0",
            "id": request.id,
            "result": {
                "artifacts": [{
                    "parts": [{"kind": "text", "text": result.message}]
                }],
                "status": {
                    "state": if result.is_task_complete { "completed" } else { "input-required" }
                },
                "is_task_complete": result.is_task_complete,
                "require_user_input": result.require_user_input
            }
        });
        Ok(http_json_response(200, &payload))
    }
}

struct HttpRequest {
    method: String,
    path: String,
    body: Option<Vec<u8>>,
}

async fn read_http_request(
    stream: &mut TcpStream,
    max_request_bytes: usize,
) -> Result<HttpRequest, A2AError> {
    let mut bytes = Vec::new();
    let mut header_end = None;
    let mut content_length = 0usize;
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if bytes.len() > max_request_bytes {
            return Err(A2AError::InvalidRequest(
                "Request is too large.".to_string(),
            ));
        }
        if header_end.is_none() {
            if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let end = index + 4;
                header_end = Some(end);
                let headers = String::from_utf8_lossy(&bytes[..index]);
                content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        (name.trim().eq_ignore_ascii_case("content-length"))
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
            }
        }
        if let Some(end) = header_end {
            if bytes.len() >= end.saturating_add(content_length) {
                break;
            }
        }
    }
    let header_end = header_end
        .ok_or_else(|| A2AError::InvalidRequest("HTTP headers were not terminated.".to_string()))?;
    let headers = String::from_utf8_lossy(&bytes[..header_end - 4]);
    let request_line = headers
        .lines()
        .next()
        .ok_or_else(|| A2AError::InvalidRequest("HTTP request line is missing.".to_string()))?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();
    if method.is_empty() || path.is_empty() {
        return Err(A2AError::InvalidRequest(
            "HTTP request line is invalid.".to_string(),
        ));
    }
    let body = (content_length > 0).then(|| {
        bytes[header_end..]
            .get(..content_length)
            .unwrap_or(&bytes[header_end..])
            .to_vec()
    });
    Ok(HttpRequest { method, path, body })
}

fn http_json_response(status: u16, payload: &Value) -> String {
    let body = payload.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

impl std::fmt::Debug for A2AClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("A2AClient")
            .field("endpoint", &self.endpoint)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl A2AClient {
    pub fn new(endpoint: impl Into<String>) -> Result<Self, A2AError> {
        let endpoint = endpoint.into();
        if !(endpoint.starts_with("http://") || endpoint.starts_with("https://")) {
            return Err(A2AError::InvalidEndpoint(endpoint));
        }
        Ok(Self {
            endpoint,
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
            timeout: Duration::from_secs(120),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub async fn invoke(
        &self,
        query: &str,
        session_id: &str,
        cancel: &CancellationToken,
    ) -> Result<A2AResponse, A2AError> {
        let request = A2ATaskRequest::message_send(query, session_id);
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(A2AError::Cancelled),
            result = tokio::time::timeout(self.timeout, self.client.post(&self.endpoint).json(&request).send()) => {
                result.map_err(|_| A2AError::Timeout)??
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(A2AError::Status(status, body));
        }
        parse_a2a_response(response.json::<Value>().await?)
    }
}

fn parse_a2a_response(value: Value) -> Result<A2AResponse, A2AError> {
    let result = value.get("result").unwrap_or(&value);
    let text = result
        .pointer("/artifacts/0/parts/0/text")
        .or_else(|| result.pointer("/content/0/text"))
        .or_else(|| result.get("message"))
        .and_then(Value::as_str)
        .or_else(|| result.get("content").and_then(Value::as_str))
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string());
    if let Some(error) = value.get("error") {
        return Ok(A2AResponse::error(error.to_string()));
    }
    Ok(A2AResponse::completed(text))
}

pub fn default_agent_card() -> AgentCard {
    AgentCard {
        skills: vec![
            AgentSkill {
                id: "rust_python_execute".to_string(),
                name: "Python Execute Tool".to_string(),
                description: "Execute bounded Python code.".to_string(),
                tags: vec!["execute".to_string()],
                examples: vec!["Print a computed result".to_string()],
            },
            AgentSkill {
                id: "rust_browser_use".to_string(),
                name: "Browser Use Tool".to_string(),
                description: "Inspect and interact with a browser session.".to_string(),
                tags: vec!["browser".to_string()],
                examples: vec!["Open a URL and inspect its title".to_string()],
            },
            AgentSkill {
                id: "rust_str_replace_editor".to_string(),
                name: "File Editor Tool".to_string(),
                description: "Inspect and edit repository files with approval.".to_string(),
                tags: vec!["files".to_string()],
                examples: vec!["Replace a verified string in a file".to_string()],
            },
            AgentSkill {
                id: "rust_terminate".to_string(),
                name: "Terminate Tool".to_string(),
                description: "Finish an agent task with a verified result.".to_string(),
                tags: vec!["finish".to_string()],
                examples: vec!["Finish after verifying the result".to_string()],
            },
        ],
        ..AgentCard::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a2a_request_and_card_use_expected_wire_shapes() {
        let request = A2ATaskRequest::message_send("hello", "session-1");
        assert_eq!(request.method, "message/send");
        assert_eq!(request.params["message"]["role"], "user");
        assert!(!default_agent_card().skills.is_empty());
    }

    #[test]
    fn a2a_response_extracts_artifact_text() {
        let response = parse_a2a_response(json!({
            "result": {"artifacts": [{"parts": [{"text": "done"}]}]}
        }))
        .unwrap();
        assert_eq!(response.message, "done");
        assert!(response.is_task_complete);
    }

    #[tokio::test]
    async fn a2a_server_serves_card_and_message_send() {
        let handler: A2AHandler = Arc::new(|query, _session_id, _cancel| {
            Box::pin(async move { A2AResponse::completed(format!("echo: {query}")) })
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let shutdown = CancellationToken::new();
        let server = A2AServer::new(default_agent_card(), handler);
        let server_task = tokio::spawn(server.serve(listener, shutdown.clone()));
        let client = reqwest::Client::new();

        let card = client
            .get(format!("http://{address}/.well-known/agent.json"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(card["name"], "RustPilot Agent");

        let request = A2ATaskRequest::message_send("hello", "session-1");
        let response = client
            .post(format!("http://{address}/"))
            .json(&request)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(
            response["result"]["artifacts"][0]["parts"][0]["text"],
            "echo: hello"
        );

        shutdown.cancel();
        server_task.await.unwrap().unwrap();
    }
}
