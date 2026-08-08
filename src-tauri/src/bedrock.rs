//! Amazon Bedrock Converse compatibility adapter.
//!
//! Bedrock is exposed through an OpenAI-shaped completion surface.
//! This module keeps the conversion isolated and uses the lightweight
//! Converse HTTP endpoint.  A caller may provide an AWS bearer token (or a
//! pre-signed Authorization header) through configuration; no credentials are
//! persisted by RustPilot.

use std::{error::Error, fmt::Display, time::Duration};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BedrockSettings {
    pub model: String,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing)]
    pub bearer_token: Option<String>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default)]
    pub prompt_cache: crate::llm::PromptCacheMode,
}

impl Default for BedrockSettings {
    fn default() -> Self {
        Self {
            model: "anthropic.claude-3-haiku-20240307-v1:0".to_string(),
            region: default_region(),
            endpoint: None,
            bearer_token: None,
            max_tokens: default_max_tokens(),
            temperature: default_temperature(),
            prompt_cache: crate::llm::PromptCacheMode::Auto,
        }
    }
}

#[derive(Debug)]
pub enum BedrockError {
    Http(reqwest::Error),
    Status(reqwest::StatusCode, String),
    Decode(serde_json::Error),
    Cancelled,
    Timeout,
    InvalidInput(String),
}

impl Display for BedrockError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http(error) => write!(formatter, "Bedrock request failed: {error}"),
            Self::Status(status, body) => {
                write!(formatter, "Bedrock returned HTTP {status}: {body}")
            }
            Self::Decode(error) => write!(formatter, "Invalid Bedrock response: {error}"),
            Self::Cancelled => formatter.write_str("Bedrock request cancelled."),
            Self::Timeout => formatter.write_str("Bedrock request timed out."),
            Self::InvalidInput(error) => formatter.write_str(error),
        }
    }
}

impl Error for BedrockError {}

impl From<reqwest::Error> for BedrockError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl From<serde_json::Error> for BedrockError {
    fn from(error: serde_json::Error) -> Self {
        Self::Decode(error)
    }
}

#[derive(Clone)]
pub struct BedrockClient {
    settings: BedrockSettings,
    client: Client,
}

impl std::fmt::Debug for BedrockClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BedrockClient")
            .field("model", &self.settings.model)
            .field("region", &self.settings.region)
            .field(
                "bearer_token_configured",
                &self.settings.bearer_token.is_some(),
            )
            .finish()
    }
}

impl BedrockClient {
    pub fn new(settings: BedrockSettings) -> Result<Self, BedrockError> {
        Ok(Self {
            settings,
            client: Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .build()?,
        })
    }

    pub fn settings(&self) -> &BedrockSettings {
        &self.settings
    }

    pub fn endpoint(&self) -> String {
        self.settings.endpoint.clone().unwrap_or_else(|| {
            format!(
                "https://bedrock-runtime.{}.amazonaws.com/model/{}/converse",
                self.settings.region, self.settings.model
            )
        })
    }

    pub async fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        cancel: &CancellationToken,
    ) -> Result<Value, BedrockError> {
        let (mut system, mut bedrock_messages) = openai_messages_to_bedrock(messages)?;
        if !matches!(
            self.settings.prompt_cache,
            crate::llm::PromptCacheMode::Disabled
        ) {
            append_bedrock_cache_points(messages, &mut system, &mut bedrock_messages);
        }
        let body = json!({
            "system": system,
            "messages": bedrock_messages,
            "inferenceConfig": {
                "temperature": self.settings.temperature,
                "maxTokens": self.settings.max_tokens
            },
            "toolConfig": if tools.is_empty() { Value::Null } else { json!({"tools": openai_tools_to_bedrock(tools)?}) }
        });
        let mut request = self.client.post(self.endpoint()).json(&body);
        if let Some(token) = self
            .settings
            .bearer_token
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            request = request.bearer_auth(token);
        }
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(BedrockError::Cancelled),
            result = tokio::time::timeout(Duration::from_secs(120), request.send()) => {
                result.map_err(|_| BedrockError::Timeout)??
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            return Err(BedrockError::Status(
                status,
                response.text().await.unwrap_or_default(),
            ));
        }
        let payload = response.json::<Value>().await?;
        Ok(bedrock_response_to_openai(&payload))
    }
}

pub fn openai_tools_to_bedrock(tools: &[Value]) -> Result<Vec<Value>, BedrockError> {
    tools
        .iter()
        .filter_map(|tool| tool.get("function"))
        .map(|function| {
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| BedrockError::InvalidInput("Tool name is required.".to_string()))?;
            let parameters = function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            Ok(json!({
                "toolSpec": {
                    "name": name,
                    "description": function.get("description").and_then(Value::as_str).unwrap_or(""),
                    "inputSchema": {"json": parameters}
                }
            }))
        })
        .collect()
}

pub fn openai_messages_to_bedrock(
    messages: &[Value],
) -> Result<(Vec<Value>, Vec<Value>), BedrockError> {
    let mut system = Vec::new();
    let mut converted = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match role {
            "system" => system.push(
                json!({"text": message.get("content").and_then(Value::as_str).unwrap_or("")}),
            ),
            "user" => converted.push(json!({
                "role": "user",
                "content": [{"text": message.get("content").and_then(Value::as_str).unwrap_or("")}]
            })),
            "assistant" => {
                let mut content = Vec::new();
                if let Some(text) = message.get("content").and_then(Value::as_str) {
                    if !text.is_empty() {
                        content.push(json!({"text": text}));
                    }
                }
                if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in tool_calls {
                        let arguments = call
                            .pointer("/function/arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        let input = serde_json::from_str(arguments)
                            .unwrap_or_else(|_| json!({"raw": arguments}));
                        content.push(json!({
                            "toolUse": {
                                "toolUseId": call.get("id").and_then(Value::as_str).unwrap_or("tool-call"),
                                "name": call.pointer("/function/name").and_then(Value::as_str).unwrap_or(""),
                                "input": input
                            }
                        }));
                    }
                }
                converted.push(json!({"role": "assistant", "content": content}));
            }
            "tool" => {
                let tool_use_id = message
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .unwrap_or("tool-call");
                converted.push(json!({
                    "role": "user",
                    "content": [{"toolResult": {
                        "toolUseId": tool_use_id,
                        "content": [{"text": message.get("content").and_then(Value::as_str).unwrap_or("")}]
                    }}]
                }));
            }
            other => {
                return Err(BedrockError::InvalidInput(format!(
                    "Invalid message role: {other}"
                )))
            }
        }
    }
    Ok((system, converted))
}

fn append_bedrock_cache_points(
    original: &[Value],
    system: &mut Vec<Value>,
    converted: &mut [Value],
) {
    let system_breakpoints = original
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .take(2)
        .count();
    if system_breakpoints > 0 {
        let mut decorated = Vec::with_capacity(system.len() + system_breakpoints);
        for (index, block) in system.drain(..).enumerate() {
            decorated.push(block);
            if index < system_breakpoints {
                decorated.push(json!({"cachePoint": {"type": "default"}}));
            }
        }
        *system = decorated;
    }

    let non_system_count = original
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) != Some("system"))
        .count();
    let final_breakpoint_start = non_system_count.saturating_sub(2);
    for (index, message) in converted.iter_mut().enumerate() {
        if index < final_breakpoint_start {
            continue;
        }
        if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
            content.push(json!({"cachePoint": {"type": "default"}}));
        }
    }
}

pub fn bedrock_response_to_openai(response: &Value) -> Value {
    let content_items = response
        .pointer("/output/message/content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for item in content_items {
        if let Some(value) = item.get("text").and_then(Value::as_str) {
            text.push_str(value);
        }
        if let Some(tool_use) = item.get("toolUse") {
            let id = tool_use
                .get("toolUseId")
                .and_then(Value::as_str)
                .unwrap_or("tool-call");
            tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {
                    "name": tool_use.get("name").and_then(Value::as_str).unwrap_or(""),
                    "arguments": serde_json::to_string(tool_use.get("input").unwrap_or(&Value::Null)).unwrap_or_else(|_| "{}".to_string())
                }
            }));
        }
    }
    json!({
        "id": format!("bedrock-{}", uuid::Uuid::new_v4()),
        "object": "chat.completion",
        "created": now_seconds(),
        "choices": [{
            "index": 0,
            "finish_reason": response.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn"),
            "message": {
                "role": response.pointer("/output/message/role").and_then(Value::as_str).unwrap_or("assistant"),
                "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                "tool_calls": if tool_calls.is_empty() { Value::Null } else { Value::Array(tool_calls) }
            }
        }],
        "usage": {
            "prompt_tokens": response.pointer("/usage/inputTokens").and_then(Value::as_u64).unwrap_or(0),
            "completion_tokens": response.pointer("/usage/outputTokens").and_then(Value::as_u64).unwrap_or(0),
            "total_tokens": response.pointer("/usage/totalTokens").and_then(Value::as_u64).unwrap_or(0),
            "prompt_tokens_details": {
                "cached_tokens": response.pointer("/usage/cacheReadInputTokens").and_then(Value::as_u64),
                "cache_write_tokens": response.pointer("/usage/cacheWriteInputTokens").and_then(Value::as_u64)
            }
        }
    })
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn default_region() -> String {
    "us-east-1".to_string()
}
fn default_max_tokens() -> u32 {
    4096
}
fn default_temperature() -> f32 {
    1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bedrock_conversion_preserves_tool_use_ids() {
        let messages = vec![
            json!({"role": "assistant", "content": "", "tool_calls": [{"id": "call-1", "function": {"name": "rust_clock", "arguments": "{}"}}]}),
            json!({"role": "tool", "tool_call_id": "call-1", "content": "now"}),
        ];
        let (_, converted) = openai_messages_to_bedrock(&messages).unwrap();
        assert_eq!(
            converted[1]["content"][0]["toolResult"]["toolUseId"],
            "call-1"
        );
        let response = bedrock_response_to_openai(&json!({
            "output": {"message": {"role": "assistant", "content": [{"toolUse": {"toolUseId": "call-1", "name": "rust_clock", "input": {}}}]}}
        }));
        assert_eq!(
            response["choices"][0]["message"]["tool_calls"][0]["id"],
            "call-1"
        );
    }

    #[test]
    fn bedrock_cache_points_match_message_selection() {
        let messages = vec![
            json!({"role": "system", "content": "one"}),
            json!({"role": "system", "content": "two"}),
            json!({"role": "system", "content": "three"}),
            json!({"role": "user", "content": "first"}),
            json!({"role": "user", "content": "second"}),
            json!({"role": "user", "content": "third"}),
        ];
        let (mut system, mut converted) = openai_messages_to_bedrock(&messages).unwrap();
        append_bedrock_cache_points(&messages, &mut system, &mut converted);

        assert_eq!(
            system
                .iter()
                .filter(|item| item.get("cachePoint").is_some())
                .count(),
            2
        );
        assert!(converted[0]["content"][0].get("cachePoint").is_none());
        assert!(converted[1]["content"][1].get("cachePoint").is_some());
        assert!(converted[2]["content"][1].get("cachePoint").is_some());
    }
}
