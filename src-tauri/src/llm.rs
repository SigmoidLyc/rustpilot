//! OpenAI-compatible LLM runtime used by RustPilot agents.
//!
//! This layer is independent of Tauri events. A desktop UI, CLI runner, or
//! protocol adapter can consume the same streaming provider.

use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::agent::{FunctionCall, Message, MessageToolCall, Role, ToolChoice};

pub const REASONING_MODELS: &[&str] = &["o1", "o3-mini"];
pub const MULTIMODAL_MODELS: &[&str] = &[
    "gpt-4-vision-preview",
    "gpt-4o",
    "gpt-4o-mini",
    "claude-3-opus-20240229",
    "claude-3-sonnet-20240229",
    "claude-3-haiku-20240307",
];

const LLM_MAX_ATTEMPTS: usize = 3;
const LLM_RETRY_DELAY_SECS: u64 = 5;
const MAX_PROVIDER_DETAIL_CHARS: usize = 4096;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheMode {
    Auto,
    Enabled,
    Disabled,
}

impl Default for PromptCacheMode {
    fn default() -> Self {
        Self::Auto
    }
}

impl PromptCacheMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "on" | "enabled" | "enable" | "true" => Some(Self::Enabled),
            "off" | "disabled" | "disable" | "false" => Some(Self::Disabled),
            _ => None,
        }
    }

    fn enabled(self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmSettings {
    pub model: String,
    pub base_url: String,
    #[serde(default, skip_serializing)]
    pub api_key: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub max_input_tokens: Option<usize>,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
    #[serde(default)]
    pub api_type: String,
    #[serde(default)]
    pub api_version: String,
    #[serde(default)]
    pub prompt_cache: PromptCacheMode,
    #[serde(skip)]
    pub session_id: Option<String>,
    #[serde(skip)]
    pub tool_schema_hash: Option<String>,
}

fn default_max_tokens() -> u32 {
    4096
}

fn default_temperature() -> f32 {
    1.0
}

fn append_utf8_bytes(buffer: &mut String, pending: &mut Vec<u8>, bytes: &[u8]) {
    pending.extend_from_slice(bytes);
    loop {
        match std::str::from_utf8(pending) {
            Ok(text) => {
                buffer.push_str(text);
                pending.clear();
                break;
            }
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0 {
                    buffer.push_str(&String::from_utf8_lossy(&pending[..valid_up_to]));
                }
                if let Some(error_len) = error.error_len() {
                    buffer.push('\u{fffd}');
                    pending.drain(..valid_up_to + error_len);
                    continue;
                }
                pending.drain(..valid_up_to);
                break;
            }
        }
    }
}

impl Default for LlmSettings {
    fn default() -> Self {
        Self {
            model: "gpt-4o-mini".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            max_tokens: default_max_tokens(),
            max_input_tokens: None,
            temperature: default_temperature(),
            api_type: "openai".to_string(),
            api_version: String::new(),
            prompt_cache: PromptCacheMode::Auto,
            session_id: None,
            tool_schema_hash: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct TokenUsage {
    #[serde(default)]
    pub total_input_tokens: usize,
    #[serde(default)]
    pub total_completion_tokens: usize,
    #[serde(default)]
    pub total_cached_input_tokens: usize,
    #[serde(default)]
    pub total_cache_write_tokens: usize,
    #[serde(default)]
    pub cache_hit_count: usize,
    #[serde(default)]
    pub cache_write_count: usize,
}

impl TokenUsage {
    pub fn record(
        &mut self,
        input_tokens: usize,
        completion_tokens: usize,
        cached_input_tokens: Option<usize>,
        cache_write_tokens: Option<usize>,
    ) {
        self.total_input_tokens = self.total_input_tokens.saturating_add(input_tokens);
        self.total_completion_tokens = self
            .total_completion_tokens
            .saturating_add(completion_tokens);
        if let Some(tokens) = cached_input_tokens.filter(|tokens| *tokens > 0) {
            self.total_cached_input_tokens = self.total_cached_input_tokens.saturating_add(tokens);
            self.cache_hit_count = self.cache_hit_count.saturating_add(1);
        }
        if let Some(tokens) = cache_write_tokens.filter(|tokens| *tokens > 0) {
            self.total_cache_write_tokens = self.total_cache_write_tokens.saturating_add(tokens);
            self.cache_write_count = self.cache_write_count.saturating_add(1);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenLimitExceeded {
    pub message: String,
}

impl Display for TokenLimitExceeded {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TokenLimitExceeded {}

#[derive(Debug)]
pub enum LlmError {
    MissingApiKey,
    InvalidInput(String),
    TokenLimit(TokenLimitExceeded),
    Http(reqwest::Error),
    HttpStatus {
        status: reqwest::StatusCode,
        body: String,
    },
    Decode(serde_json::Error),
    Upstream(String),
    Cancelled,
    Timeout,
    EmptyResponse {
        details: String,
    },
}

impl Display for LlmError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingApiKey => formatter.write_str("No API key is configured."),
            Self::InvalidInput(message) => formatter.write_str(message),
            Self::TokenLimit(error) => formatter.write_str(&error.message),
            Self::Http(error) => write!(formatter, "LLM request failed: {error}"),
            Self::HttpStatus { status, body } => {
                write!(formatter, "LLM returned HTTP {status}: {body}")
            }
            Self::Decode(error) => write!(formatter, "Invalid LLM response: {error}"),
            Self::Upstream(message) => write!(formatter, "LLM provider error: {message}"),
            Self::Cancelled => formatter.write_str("LLM request cancelled."),
            Self::Timeout => formatter.write_str("LLM request timed out."),
            Self::EmptyResponse { details } => {
                write!(formatter, "Empty response from LLM: {details}")
            }
        }
    }
}

impl std::error::Error for LlmError {}

impl From<reqwest::Error> for LlmError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error)
    }
}

impl From<serde_json::Error> for LlmError {
    fn from(error: serde_json::Error) -> Self {
        Self::Decode(error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Completion {
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<MessageToolCall>,
    #[serde(default)]
    pub prompt_tokens: Option<usize>,
    #[serde(default)]
    pub completion_tokens: Option<usize>,
    #[serde(default)]
    pub cached_input_tokens: Option<usize>,
    #[serde(default)]
    pub cache_write_tokens: Option<usize>,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

impl Completion {
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// Ordered provider deltas used by the agent runtime and UI projection.
///
/// The legacy completion fields remain the canonical model history. These
/// events only describe how that completion arrived, so callers can render
/// text and tool input in the same order without changing model behavior.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    TextDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
}

struct StreamAttemptError {
    error: LlmError,
    output_started: bool,
}

fn stream_attempt_error(error: LlmError, completion: &Completion) -> StreamAttemptError {
    StreamAttemptError {
        error,
        output_started: completion_has_output(completion),
    }
}

fn completion_has_output(completion: &Completion) -> bool {
    !completion.content.trim().is_empty() || completion.has_tool_calls()
}

fn empty_completion_error(completion: &Completion) -> LlmError {
    let finish_reason = completion.finish_reason.as_deref().unwrap_or("unknown");
    LlmError::EmptyResponse {
        details: format!(
            "The provider returned no content or tool calls (finish_reason: {finish_reason})."
        ),
    }
}

fn provider_detail(value: &Value) -> String {
    let serialized = serde_json::to_string(value)
        .unwrap_or_else(|_| "The provider returned an unreadable error payload.".to_string());
    serialized.chars().take(MAX_PROVIDER_DETAIL_CHARS).collect()
}

fn provider_error(value: &Value) -> Option<LlmError> {
    let error = value.get("error").filter(|error| !error.is_null());
    let is_error_event = value.get("type").and_then(Value::as_str) == Some("error");
    if error.is_some() || is_error_event {
        return Some(LlmError::Upstream(provider_detail(value)));
    }
    None
}

fn is_retryable_upstream_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "429",
        "500",
        "502",
        "503",
        "504",
        "524",
        "rate limit",
        "rate_limit",
        "too many requests",
        "overloaded",
        "service unavailable",
        "server error",
        "server_error",
        "temporarily unavailable",
        "temporarily_unavailable",
        "timeout",
        "timed out",
        "try again",
        "connection reset",
        "network error",
    ]
    .iter()
    .any(|pattern| message.contains(pattern))
}

fn is_retryable_error(error: &LlmError) -> bool {
    match error {
        LlmError::Http(_)
        | LlmError::Decode(_)
        | LlmError::Timeout
        | LlmError::EmptyResponse { .. } => true,
        LlmError::HttpStatus { status, .. } => {
            matches!(status.as_u16(), 408 | 425 | 429) || status.is_server_error()
        }
        LlmError::Upstream(message) => is_retryable_upstream_message(message),
        LlmError::MissingApiKey
        | LlmError::InvalidInput(_)
        | LlmError::TokenLimit(_)
        | LlmError::Cancelled => false,
    }
}

async fn wait_for_retry(
    attempt: usize,
    error: &LlmError,
    cancel: &CancellationToken,
) -> Result<(), LlmError> {
    tracing::warn!(
        attempt = attempt + 1,
        max_attempts = LLM_MAX_ATTEMPTS,
        error = %error,
        "LLM request failed; retrying after delay"
    );
    tokio::select! {
        _ = cancel.cancelled() => Err(LlmError::Cancelled),
        _ = tokio::time::sleep(Duration::from_secs(LLM_RETRY_DELAY_SECS)) => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCounter;

impl TokenCounter {
    pub const BASE_MESSAGE_TOKENS: usize = 4;
    pub const FORMAT_TOKENS: usize = 2;
    pub const LOW_DETAIL_IMAGE_TOKENS: usize = 85;
    pub const HIGH_DETAIL_TILE_TOKENS: usize = 170;
    pub const MAX_SIZE: usize = 2048;
    pub const HIGH_DETAIL_TARGET_SHORT_SIDE: usize = 768;
    pub const TILE_SIZE: usize = 512;

    pub fn count_text(text: &str) -> usize {
        if text.is_empty() {
            0
        } else {
            // This conservative estimate keeps the desktop binary light while
            // still enforcing input limits.
            text.chars().count().div_ceil(4).max(1)
        }
    }

    pub fn count_image(image_item: &Value) -> usize {
        let detail = image_item
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("medium");
        if detail == "low" {
            return Self::LOW_DETAIL_IMAGE_TOKENS;
        }
        if let Some(dimensions) = image_item.get("dimensions").and_then(Value::as_array) {
            if let (Some(width), Some(height)) = (
                dimensions.first().and_then(Value::as_u64),
                dimensions.get(1).and_then(Value::as_u64),
            ) {
                return Self::high_detail_tokens(width as usize, height as usize);
            }
        }
        if detail == "high" {
            Self::high_detail_tokens(1024, 1024)
        } else {
            1024
        }
    }

    pub fn high_detail_tokens(mut width: usize, mut height: usize) -> usize {
        width = width.max(1);
        height = height.max(1);
        if width > Self::MAX_SIZE || height > Self::MAX_SIZE {
            let scale = Self::MAX_SIZE as f64 / width.max(height) as f64;
            width = (width as f64 * scale) as usize;
            height = (height as f64 * scale) as usize;
        }
        let scale = Self::HIGH_DETAIL_TARGET_SHORT_SIDE as f64 / width.min(height) as f64;
        let scaled_width = (width as f64 * scale).ceil() as usize;
        let scaled_height = (height as f64 * scale).ceil() as usize;
        let tiles =
            scaled_width.div_ceil(Self::TILE_SIZE) * scaled_height.div_ceil(Self::TILE_SIZE);
        tiles * Self::HIGH_DETAIL_TILE_TOKENS + Self::LOW_DETAIL_IMAGE_TOKENS
    }

    pub fn count_content(content: &Value) -> usize {
        match content {
            Value::String(text) => Self::count_text(text),
            Value::Array(items) => items
                .iter()
                .map(|item| {
                    if let Some(text) = item.as_str() {
                        Self::count_text(text)
                    } else if item.get("text").is_some() {
                        Self::count_text(item.get("text").and_then(Value::as_str).unwrap_or(""))
                    } else if item.get("image_url").is_some() {
                        Self::count_image(item)
                    } else {
                        0
                    }
                })
                .sum(),
            _ => 0,
        }
    }

    pub fn count_tool_calls(tool_calls: &[Value]) -> usize {
        tool_calls
            .iter()
            .filter_map(|call| call.get("function"))
            .map(|function| {
                Self::count_text(function.get("name").and_then(Value::as_str).unwrap_or(""))
                    + Self::count_text(
                        function
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    )
            })
            .sum()
    }

    pub fn count_messages(messages: &[Value]) -> usize {
        Self::FORMAT_TOKENS
            + messages
                .iter()
                .map(|message| {
                    let mut tokens = Self::BASE_MESSAGE_TOKENS
                        + Self::count_text(
                            message.get("role").and_then(Value::as_str).unwrap_or(""),
                        );
                    if let Some(content) = message.get("content") {
                        tokens += Self::count_content(content);
                    }
                    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
                        tokens += Self::count_tool_calls(tool_calls);
                    }
                    tokens +=
                        Self::count_text(message.get("name").and_then(Value::as_str).unwrap_or(""));
                    tokens += Self::count_text(
                        message
                            .get("tool_call_id")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    );
                    tokens
                })
                .sum::<usize>()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CacheProvider {
    OpenAi,
    OpenRouter,
    Anthropic,
    Bedrock,
    Copilot,
    Alibaba,
    Gateway,
    OpenAiCompatible,
}

impl CacheProvider {
    fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::OpenRouter => "openrouter",
            Self::Anthropic => "anthropic",
            Self::Bedrock => "bedrock",
            Self::Copilot => "copilot",
            Self::Alibaba => "alibaba",
            Self::Gateway => "gateway",
            Self::OpenAiCompatible => "openai-compatible",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExtensionCapability {
    Unknown,
    Supported,
    Unsupported,
}

#[derive(Debug, Clone, Copy)]
struct ExtensionCapabilities {
    prompt_cache: ExtensionCapability,
    stream_usage: ExtensionCapability,
}

impl Default for ExtensionCapabilities {
    fn default() -> Self {
        Self {
            prompt_cache: ExtensionCapability::Unknown,
            stream_usage: ExtensionCapability::Unknown,
        }
    }
}

static EXTENSION_CAPABILITIES: OnceLock<Mutex<HashMap<String, ExtensionCapabilities>>> =
    OnceLock::new();

fn extension_capabilities() -> &'static Mutex<HashMap<String, ExtensionCapabilities>> {
    EXTENSION_CAPABILITIES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn provider_for_settings(settings: &LlmSettings) -> CacheProvider {
    let base_url = settings.base_url.to_ascii_lowercase();
    if base_url.contains("openrouter.ai") {
        return CacheProvider::OpenRouter;
    }
    if base_url.contains("anthropic.com") {
        return CacheProvider::Anthropic;
    }
    if base_url.contains("amazonaws.com") || base_url.contains("bedrock") {
        return CacheProvider::Bedrock;
    }
    if base_url.contains("githubcopilot") || base_url.contains("copilot") {
        return CacheProvider::Copilot;
    }
    if base_url.contains("alibabacloud")
        || base_url.contains("dashscope")
        || base_url.contains("aliyuncs")
    {
        return CacheProvider::Alibaba;
    }
    if base_url.contains("gateway") {
        return CacheProvider::Gateway;
    }

    match settings.api_type.trim().to_ascii_lowercase().as_str() {
        "openrouter" => CacheProvider::OpenRouter,
        "anthropic" | "claude" => CacheProvider::Anthropic,
        "bedrock" | "amazon-bedrock" => CacheProvider::Bedrock,
        "copilot" | "github-copilot" => CacheProvider::Copilot,
        "alibaba" | "alibaba-cn" | "dashscope" => CacheProvider::Alibaba,
        "gateway" => CacheProvider::Gateway,
        "openai-compatible" | "compatible" => CacheProvider::OpenAiCompatible,
        "openai" => CacheProvider::OpenAi,
        _ if base_url.contains("api.openai.com") => CacheProvider::OpenAi,
        _ => CacheProvider::OpenAiCompatible,
    }
}

fn openai_compatible_key_supported(settings: &LlmSettings) -> bool {
    let api_type = settings.api_type.trim().to_ascii_lowercase();
    let base_url = settings.base_url.to_ascii_lowercase();
    ["deepinfra", "cerebras", "xai", "mistral", "venice", "azure"]
        .iter()
        .any(|value| api_type.contains(value) || base_url.contains(value))
}

fn capability_key(settings: &LlmSettings, provider: CacheProvider) -> String {
    let raw = format!(
        "{}|{}|{}",
        provider.as_str(),
        settings.base_url.trim_end_matches('/'),
        settings.model
    );
    normalize_cache_key(&raw).unwrap_or_else(|| provider.as_str().to_string())
}

fn read_capabilities(key: &str) -> ExtensionCapabilities {
    extension_capabilities()
        .lock()
        .ok()
        .and_then(|capabilities| capabilities.get(key).copied())
        .unwrap_or_default()
}

fn update_capabilities(key: &str, update: impl FnOnce(&mut ExtensionCapabilities)) {
    if let Ok(mut capabilities) = extension_capabilities().lock() {
        if capabilities.len() >= 32 && !capabilities.contains_key(key) {
            if let Some(first) = capabilities.keys().next().cloned() {
                capabilities.remove(&first);
            }
        }
        let entry = capabilities.entry(key.to_string()).or_default();
        update(entry);
    }
}

fn normalize_cache_key(value: &str) -> Option<String> {
    let key = value.trim();
    if key.is_empty() {
        return None;
    }
    Some(key.chars().take(256).collect())
}

pub fn prompt_cache_key(
    session_id: Option<&str>,
    tool_schema_hash: Option<&str>,
) -> Option<String> {
    let session_id = session_id.and_then(normalize_cache_key)?;
    let session_id = session_id.strip_prefix("rustpilot:").unwrap_or(&session_id);
    let session_id = session_id.strip_prefix("task_").unwrap_or(session_id);
    let session_id = session_id.chars().take(38).collect::<String>();
    let key = tool_schema_hash
        .map(|hash| {
            let hash = hash.chars().take(16).collect::<String>();
            format!("rp:{session_id}:tools:{hash}")
        })
        .unwrap_or_else(|| format!("rp:{session_id}"));
    normalize_cache_key(&key)
}

fn cache_breakpoint_indices(messages: &[Value]) -> Vec<usize> {
    let mut indices = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| message.get("role").and_then(Value::as_str) == Some("system"))
        .map(|(index, _)| index)
        .take(2)
        .collect::<Vec<_>>();
    for (index, message) in messages.iter().enumerate().rev() {
        if message.get("role").and_then(Value::as_str) != Some("system") {
            indices.push(index);
            if indices.len() >= 4 {
                break;
            }
        }
    }
    indices.sort_unstable();
    indices.dedup();
    indices
}

fn cache_marker(provider: CacheProvider) -> Option<(&'static str, Value)> {
    match provider {
        CacheProvider::Anthropic
        | CacheProvider::OpenRouter
        | CacheProvider::Alibaba
        | CacheProvider::OpenAiCompatible => Some(("cache_control", json!({"type": "ephemeral"}))),
        CacheProvider::Copilot => Some(("copilot_cache_control", json!({"type": "ephemeral"}))),
        CacheProvider::OpenAi | CacheProvider::Bedrock | CacheProvider::Gateway => None,
    }
}

fn mark_message_cache_breakpoint(message: &mut Value, provider: CacheProvider) -> bool {
    let Some((key, marker)) = cache_marker(provider) else {
        return false;
    };
    let Some(object) = message.as_object_mut() else {
        return false;
    };
    let Some(content) = object.get_mut("content") else {
        return false;
    };
    match content {
        Value::String(text) => {
            let text = std::mem::take(text);
            let mut part = serde_json::Map::new();
            part.insert("type".to_string(), Value::String("text".to_string()));
            part.insert("text".to_string(), Value::String(text));
            part.insert(key.to_string(), marker);
            *content = Value::Array(vec![Value::Object(part)]);
            true
        }
        Value::Array(parts) => parts
            .iter_mut()
            .rev()
            .find_map(|part| {
                let part_object = part.as_object_mut()?;
                let part_type = part_object.get("type").and_then(Value::as_str);
                if matches!(
                    part_type,
                    Some("tool-approval-request") | Some("tool-approval-response")
                ) {
                    return None;
                }
                part_object.insert(key.to_string(), marker.clone());
                Some(())
            })
            .is_some(),
        _ => false,
    }
}

fn value_contains_key(value: &Value, keys: &[&str]) -> bool {
    match value {
        Value::Object(object) => {
            keys.iter().any(|key| object.contains_key(*key))
                || object.values().any(|value| value_contains_key(value, keys))
        }
        Value::Array(values) => values.iter().any(|value| value_contains_key(value, keys)),
        _ => false,
    }
}

fn body_has_prompt_cache_fields(body: &Value) -> bool {
    value_contains_key(
        body,
        &[
            "prompt_cache_key",
            "promptCacheKey",
            "session_id",
            "cache_control",
            "cachePoint",
            "copilot_cache_control",
            "gateway",
        ],
    )
}

fn body_has_stream_usage_extension(body: &Value) -> bool {
    body.pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn is_extension_error(status: reqwest::StatusCode, body: &str) -> bool {
    if !matches!(status.as_u16(), 400 | 404 | 422) {
        return false;
    }
    let body = body.to_ascii_lowercase();
    body.contains("cache_control")
        || body.contains("cachepoint")
        || body.contains("prompt_cache")
        || body.contains("promptcache")
        || body.contains("session_id")
        || body.contains("copilot_cache")
        || body.contains("gateway")
        || body.contains("stream_options")
        || body.contains("include_usage")
}

fn extension_error_kinds(body: &str) -> (bool, bool) {
    let body = body.to_ascii_lowercase();
    let prompt = body.contains("cache_control")
        || body.contains("cachepoint")
        || body.contains("prompt_cache")
        || body.contains("promptcache")
        || body.contains("session_id")
        || body.contains("copilot_cache")
        || body.contains("gateway");
    let usage = body.contains("stream_options") || body.contains("include_usage");
    (prompt, usage)
}

#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    settings: LlmSettings,
    client: Client,
    usage: Arc<Mutex<TokenUsage>>,
}

impl std::fmt::Debug for OpenAiCompatibleClient {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenAiCompatibleClient")
            .field("model", &self.settings.model)
            .field("base_url", &self.settings.base_url)
            .field(
                "api_key_configured",
                &!self.settings.api_key.trim().is_empty(),
            )
            .finish()
    }
}

impl OpenAiCompatibleClient {
    pub fn new(settings: LlmSettings) -> Result<Self, LlmError> {
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            settings,
            client,
            usage: Arc::new(Mutex::new(TokenUsage::default())),
        })
    }

    pub fn settings(&self) -> &LlmSettings {
        &self.settings
    }

    pub fn usage(&self) -> TokenUsage {
        self.usage
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default()
    }

    pub fn supports_images(&self) -> bool {
        MULTIMODAL_MODELS.contains(&self.settings.model.as_str())
    }

    pub fn completion_url(base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else if base.ends_with("/v1") {
            format!("{base}/chat/completions")
        } else {
            format!("{base}/v1/chat/completions")
        }
    }

    pub fn format_messages(
        messages: &[Message],
        supports_images: bool,
    ) -> Result<Vec<Value>, LlmError> {
        let mut formatted = Vec::new();
        for message in messages {
            let mut value = message.to_dict();
            if supports_images {
                if let Some(image) = value.get("base64_image").and_then(Value::as_str) {
                    let mut items = match value.get("content").and_then(Value::as_str) {
                        Some(text) => vec![json!({"type": "text", "text": text})],
                        None => Vec::new(),
                    };
                    items.push(json!({
                        "type": "image_url",
                        "image_url": {"url": format!("data:image/jpeg;base64,{image}")}
                    }));
                    value["content"] = Value::Array(items);
                }
            }
            if let Some(object) = value.as_object_mut() {
                object.remove("base64_image");
            }
            let role = value.get("role").and_then(Value::as_str).unwrap_or("");
            if !matches!(role, "system" | "user" | "assistant" | "tool") {
                return Err(LlmError::InvalidInput(format!("Invalid role: {role}")));
            }
            if value.get("content").is_some() || value.get("tool_calls").is_some() {
                formatted.push(value);
            }
        }
        Ok(formatted)
    }

    pub fn check_token_limit(&self, input_tokens: usize) -> Result<(), LlmError> {
        if let Some(max) = self.settings.max_input_tokens {
            let used = self.usage().total_input_tokens;
            if used.saturating_add(input_tokens) > max {
                return Err(LlmError::TokenLimit(TokenLimitExceeded {
                    message: format!(
                        "Request may exceed input token limit (Current: {used}, Needed: {input_tokens}, Max: {max})"
                    ),
                }));
            }
        }
        Ok(())
    }

    fn record_usage(
        &self,
        input: usize,
        completion: usize,
        cached_input_tokens: Option<usize>,
        cache_write_tokens: Option<usize>,
    ) {
        if let Ok(mut usage) = self.usage.lock() {
            usage.record(input, completion, cached_input_tokens, cache_write_tokens);
        }
    }

    pub fn record_completion_usage(&self, completion: &Completion, fallback_input: usize) {
        self.record_usage(
            completion.prompt_tokens.unwrap_or(fallback_input),
            completion
                .completion_tokens
                .unwrap_or_else(|| TokenCounter::count_text(&completion.content)),
            completion.cached_input_tokens,
            completion.cache_write_tokens,
        );
    }

    pub async fn ask(
        &self,
        messages: &[Message],
        system_messages: &[Message],
        stream: bool,
        temperature: Option<f32>,
        cancel: &CancellationToken,
    ) -> Result<String, LlmError> {
        let mut all = system_messages.to_vec();
        all.extend_from_slice(messages);
        let formatted = Self::format_messages(&all, self.supports_images())?;
        let input_tokens = TokenCounter::count_messages(&formatted);
        self.check_token_limit(input_tokens)?;
        let completion = if stream {
            self.stream(
                &formatted,
                &[],
                ToolChoice::Auto,
                temperature,
                cancel,
                |_| Ok(()),
            )
            .await?
        } else {
            self.complete(&formatted, &[], ToolChoice::Auto, temperature, cancel)
                .await?
        };
        if completion.content.trim().is_empty() {
            return Err(empty_completion_error(&completion));
        }
        Ok(completion.content)
    }

    pub async fn ask_with_images(
        &self,
        messages: &[Message],
        images: &[Value],
        system_messages: &[Message],
        stream: bool,
        temperature: Option<f32>,
        cancel: &CancellationToken,
    ) -> Result<String, LlmError> {
        if !self.supports_images() {
            return Err(LlmError::InvalidInput(format!(
                "Model {} does not support images.",
                self.settings.model
            )));
        }
        let Some(last) = messages.last() else {
            return Err(LlmError::InvalidInput(
                "The last message must be from the user.".to_string(),
            ));
        };
        if last.role != Role::User {
            return Err(LlmError::InvalidInput(
                "The last message must be from the user.".to_string(),
            ));
        }
        let mut value = last.to_dict();
        let text = value.get("content").and_then(Value::as_str).unwrap_or("");
        let mut content = vec![json!({"type": "text", "text": text})];
        for image in images {
            if let Some(url) = image.as_str() {
                content.push(json!({"type": "image_url", "image_url": {"url": url}}));
            } else if image.get("url").is_some() {
                content.push(json!({"type": "image_url", "image_url": image}));
            } else if image.get("image_url").is_some() {
                content.push(image.clone());
            } else {
                return Err(LlmError::InvalidInput(format!(
                    "Unsupported image format: {image}"
                )));
            }
        }
        value["content"] = Value::Array(content);
        let mut all = system_messages
            .iter()
            .map(Message::to_dict)
            .collect::<Vec<_>>();
        all.extend(messages[..messages.len() - 1].iter().map(Message::to_dict));
        all.push(value);
        let input_tokens = TokenCounter::count_messages(&all);
        self.check_token_limit(input_tokens)?;
        let completion = if stream {
            self.stream(&all, &[], ToolChoice::Auto, temperature, cancel, |_| Ok(()))
                .await?
        } else {
            self.complete(&all, &[], ToolChoice::Auto, temperature, cancel)
                .await?
        };
        if completion.content.trim().is_empty() {
            return Err(empty_completion_error(&completion));
        }
        Ok(completion.content)
    }

    pub async fn ask_tool(
        &self,
        messages: &[Message],
        system_messages: &[Message],
        tools: &[Value],
        tool_choice: ToolChoice,
        temperature: Option<f32>,
        cancel: &CancellationToken,
    ) -> Result<Completion, LlmError> {
        let mut all = system_messages.to_vec();
        all.extend_from_slice(messages);
        let formatted = Self::format_messages(&all, self.supports_images())?;
        let input_tokens = TokenCounter::count_messages(&formatted)
            + tools
                .iter()
                .map(|tool| TokenCounter::count_text(&tool.to_string()))
                .sum::<usize>();
        self.check_token_limit(input_tokens)?;
        let completion = self
            .complete(&formatted, tools, tool_choice, temperature, cancel)
            .await?;
        Ok(completion)
    }

    pub async fn complete(
        &self,
        messages: &[Value],
        tools: &[Value],
        tool_choice: ToolChoice,
        temperature: Option<f32>,
        cancel: &CancellationToken,
    ) -> Result<Completion, LlmError> {
        for attempt in 0..LLM_MAX_ATTEMPTS {
            let body = self.request_body(messages, tools, tool_choice, temperature, false);
            let result = match self
                .send_with_extension_fallback(
                    body,
                    || {
                        self.request_body_without_extensions(
                            messages,
                            tools,
                            tool_choice,
                            temperature,
                            false,
                        )
                    },
                    cancel,
                )
                .await
            {
                Err(error) => Err(error),
                Ok(response) if !response.status().is_success() => {
                    Err(response_error(response).await)
                }
                Ok(response) => response
                    .json::<Value>()
                    .await
                    .map_err(LlmError::Http)
                    .and_then(|payload| {
                        let completion = completion_from_response(&payload)?;
                        if completion_has_output(&completion) {
                            Ok(completion)
                        } else {
                            Err(empty_completion_error(&completion))
                        }
                    }),
            };

            match result {
                Ok(completion) => {
                    self.record_completion_usage(
                        &completion,
                        TokenCounter::count_messages(messages)
                            + tools
                                .iter()
                                .map(|tool| TokenCounter::count_text(&tool.to_string()))
                                .sum::<usize>(),
                    );
                    return Ok(completion);
                }
                Err(error) if attempt + 1 < LLM_MAX_ATTEMPTS && is_retryable_error(&error) => {
                    wait_for_retry(attempt, &error, cancel).await?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("LLM retry loop must return within its attempt limit")
    }

    pub async fn complete_with_response_format(
        &self,
        messages: &[Value],
        response_format: Option<Value>,
        cancel: &CancellationToken,
    ) -> Result<Completion, LlmError> {
        for attempt in 0..LLM_MAX_ATTEMPTS {
            let mut body = self.request_body(messages, &[], ToolChoice::None, None, false);
            if let Some(response_format) = response_format.clone() {
                body["response_format"] = response_format;
            }
            let result = match self
                .send_with_extension_fallback(
                    body,
                    || {
                        let mut fallback = self.request_body_without_extensions(
                            messages,
                            &[],
                            ToolChoice::None,
                            None,
                            false,
                        );
                        if let Some(response_format) = response_format.clone() {
                            fallback["response_format"] = response_format;
                        }
                        fallback
                    },
                    cancel,
                )
                .await
            {
                Err(error) => Err(error),
                Ok(response) if !response.status().is_success() => {
                    Err(response_error(response).await)
                }
                Ok(response) => response
                    .json::<Value>()
                    .await
                    .map_err(LlmError::Http)
                    .and_then(|payload| {
                        let completion = completion_from_response(&payload)?;
                        if completion_has_output(&completion) {
                            Ok(completion)
                        } else {
                            Err(empty_completion_error(&completion))
                        }
                    }),
            };

            match result {
                Ok(completion) => {
                    self.record_completion_usage(
                        &completion,
                        TokenCounter::count_messages(messages),
                    );
                    return Ok(completion);
                }
                Err(error) if attempt + 1 < LLM_MAX_ATTEMPTS && is_retryable_error(&error) => {
                    wait_for_retry(attempt, &error, cancel).await?;
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("LLM retry loop must return within its attempt limit")
    }

    pub async fn stream<F>(
        &self,
        messages: &[Value],
        tools: &[Value],
        tool_choice: ToolChoice,
        temperature: Option<f32>,
        cancel: &CancellationToken,
        mut on_event: F,
    ) -> Result<Completion, LlmError>
    where
        F: FnMut(&StreamEvent) -> Result<(), LlmError>,
    {
        for attempt in 0..LLM_MAX_ATTEMPTS {
            match self
                .stream_attempt(
                    messages,
                    tools,
                    tool_choice,
                    temperature,
                    cancel,
                    &mut on_event,
                )
                .await
            {
                Ok(completion) => {
                    self.record_completion_usage(
                        &completion,
                        TokenCounter::count_messages(messages)
                            + tools
                                .iter()
                                .map(|tool| TokenCounter::count_text(&tool.to_string()))
                                .sum::<usize>(),
                    );
                    return Ok(completion);
                }
                Err(failure)
                    if !failure.output_started
                        && attempt + 1 < LLM_MAX_ATTEMPTS
                        && is_retryable_error(&failure.error) =>
                {
                    wait_for_retry(attempt, &failure.error, cancel).await?;
                }
                Err(failure) => return Err(failure.error),
            }
        }
        unreachable!("LLM retry loop must return within its attempt limit")
    }

    async fn stream_attempt<F>(
        &self,
        messages: &[Value],
        tools: &[Value],
        tool_choice: ToolChoice,
        temperature: Option<f32>,
        cancel: &CancellationToken,
        on_event: &mut F,
    ) -> Result<Completion, StreamAttemptError>
    where
        F: FnMut(&StreamEvent) -> Result<(), LlmError>,
    {
        let mut completion = Completion {
            content: String::new(),
            tool_calls: Vec::new(),
            prompt_tokens: None,
            completion_tokens: None,
            cached_input_tokens: None,
            cache_write_tokens: None,
            finish_reason: None,
        };
        let body = self.request_body(messages, tools, tool_choice, temperature, true);
        let response = self
            .send_with_extension_fallback(
                body,
                || {
                    self.request_body_without_extensions(
                        messages,
                        tools,
                        tool_choice,
                        temperature,
                        true,
                    )
                },
                cancel,
            )
            .await
            .map_err(|error| stream_attempt_error(error, &completion))?;
        if !response.status().is_success() {
            return Err(stream_attempt_error(
                response_error(response).await,
                &completion,
            ));
        }
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut pending_utf8 = Vec::new();
        let timeout = Duration::from_secs(120);
        let mut done = false;
        while !done {
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    return Err(stream_attempt_error(LlmError::Cancelled, &completion));
                }
                result = tokio::time::timeout(timeout, stream.next()) => {
                    match result {
                        Ok(next) => next,
                        Err(_) => {
                            return Err(stream_attempt_error(LlmError::Timeout, &completion));
                        }
                    }
                }
            };
            let Some(chunk) = next else {
                break;
            };
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    return Err(stream_attempt_error(LlmError::Http(error), &completion));
                }
            };
            append_utf8_bytes(&mut buffer, &mut pending_utf8, &chunk);
            while let Some(newline) = buffer.find('\n') {
                let line = buffer[..newline].trim_end_matches('\r').to_string();
                buffer.drain(..=newline);
                done = match consume_sse_line(&line, &mut completion, on_event) {
                    Ok(done) => done,
                    Err(error) => {
                        return Err(stream_attempt_error(error, &completion));
                    }
                };
                if done {
                    break;
                }
            }
        }
        if !pending_utf8.is_empty() {
            buffer.push_str(&String::from_utf8_lossy(&pending_utf8));
        }
        if !buffer.trim().is_empty() {
            if let Err(error) = consume_sse_line(buffer.trim(), &mut completion, on_event) {
                return Err(stream_attempt_error(error, &completion));
            }
        }
        if !completion_has_output(&completion) {
            return Err(stream_attempt_error(
                empty_completion_error(&completion),
                &completion,
            ));
        }
        Ok(completion)
    }

    fn request_body(
        &self,
        messages: &[Value],
        tools: &[Value],
        tool_choice: ToolChoice,
        temperature: Option<f32>,
        stream: bool,
    ) -> Value {
        self.request_body_with_extensions(messages, tools, tool_choice, temperature, stream, true)
    }

    fn request_body_without_extensions(
        &self,
        messages: &[Value],
        tools: &[Value],
        tool_choice: ToolChoice,
        temperature: Option<f32>,
        stream: bool,
    ) -> Value {
        self.request_body_with_extensions(messages, tools, tool_choice, temperature, stream, false)
    }

    fn request_body_with_extensions(
        &self,
        messages: &[Value],
        tools: &[Value],
        tool_choice: ToolChoice,
        temperature: Option<f32>,
        stream: bool,
        apply_extensions: bool,
    ) -> Value {
        let mut body = json!({
            "model": self.settings.model,
            "messages": messages,
            "stream": stream,
            "tool_choice": tool_choice_name(tool_choice),
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools.to_vec());
        }
        if REASONING_MODELS.contains(&self.settings.model.as_str()) {
            body["max_completion_tokens"] = json!(self.settings.max_tokens);
        } else {
            body["max_tokens"] = json!(self.settings.max_tokens);
            body["temperature"] = json!(temperature.unwrap_or(self.settings.temperature));
        }
        if apply_extensions {
            self.apply_prompt_cache(&mut body, stream);
        }
        body
    }

    fn apply_prompt_cache(&self, body: &mut Value, stream: bool) {
        if !self.settings.prompt_cache.enabled() {
            return;
        }
        let provider = provider_for_settings(&self.settings);
        let capability_key = capability_key(&self.settings, provider);
        let capabilities = read_capabilities(&capability_key);
        let cache_key = prompt_cache_key(
            self.settings.session_id.as_deref(),
            self.settings.tool_schema_hash.as_deref(),
        );

        if capabilities.prompt_cache != ExtensionCapability::Unsupported {
            match provider {
                CacheProvider::OpenAi => {
                    if let Some(cache_key) = cache_key.as_deref() {
                        body["prompt_cache_key"] = Value::String(cache_key.to_string());
                    }
                }
                CacheProvider::OpenRouter => {
                    if let Some(cache_key) = cache_key.as_deref() {
                        body["session_id"] = Value::String(cache_key.to_string());
                    }
                    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
                        for index in cache_breakpoint_indices(messages) {
                            mark_message_cache_breakpoint(&mut messages[index], provider);
                        }
                    }
                }
                CacheProvider::Anthropic | CacheProvider::Alibaba | CacheProvider::Copilot => {
                    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
                        for index in cache_breakpoint_indices(messages) {
                            mark_message_cache_breakpoint(&mut messages[index], provider);
                        }
                    }
                }
                CacheProvider::OpenAiCompatible => {
                    if matches!(self.settings.prompt_cache, PromptCacheMode::Enabled)
                        || openai_compatible_key_supported(&self.settings)
                    {
                        if let Some(cache_key) = cache_key.as_deref() {
                            body["prompt_cache_key"] = Value::String(cache_key.to_string());
                        }
                    }
                    if matches!(self.settings.prompt_cache, PromptCacheMode::Enabled) {
                        if let Some(messages) =
                            body.get_mut("messages").and_then(Value::as_array_mut)
                        {
                            for index in cache_breakpoint_indices(messages) {
                                mark_message_cache_breakpoint(&mut messages[index], provider);
                            }
                        }
                    }
                }
                CacheProvider::Gateway => {
                    body["gateway"] = json!({"caching": "auto"});
                }
                CacheProvider::Bedrock => {}
            }
        }

        if stream
            && capabilities.stream_usage != ExtensionCapability::Unsupported
            && matches!(
                provider,
                CacheProvider::OpenAi
                    | CacheProvider::OpenRouter
                    | CacheProvider::Gateway
                    | CacheProvider::Alibaba
                    | CacheProvider::OpenAiCompatible
            )
        {
            body["stream_options"] = json!({"include_usage": true});
        }
    }

    async fn send_with_extension_fallback<F>(
        &self,
        body: Value,
        fallback: F,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, LlmError>
    where
        F: FnOnce() -> Value,
    {
        let has_prompt_cache = body_has_prompt_cache_fields(&body);
        let has_stream_usage = body_has_stream_usage_extension(&body);
        let response = self.send(body, cancel).await?;
        if response.status().is_success() || !(has_prompt_cache || has_stream_usage) {
            if response.status().is_success() {
                let provider = provider_for_settings(&self.settings);
                let key = capability_key(&self.settings, provider);
                update_capabilities(&key, |capabilities| {
                    if has_prompt_cache {
                        capabilities.prompt_cache = ExtensionCapability::Supported;
                    }
                    if has_stream_usage {
                        capabilities.stream_usage = ExtensionCapability::Supported;
                    }
                });
            }
            return Ok(response);
        }

        let status = response.status();
        let error_body = response
            .text()
            .await
            .unwrap_or_else(|_| "No error body returned.".to_string());
        let explicit_extension_error = is_extension_error(status, &error_body);
        let validation_error_with_extensions =
            matches!(status.as_u16(), 400 | 422) && (has_prompt_cache || has_stream_usage);
        if !explicit_extension_error && !validation_error_with_extensions {
            return Err(LlmError::HttpStatus {
                status,
                body: error_body,
            });
        }

        let (mut prompt_cache_error, mut stream_usage_error) = extension_error_kinds(&error_body);
        if validation_error_with_extensions && !explicit_extension_error {
            prompt_cache_error = has_prompt_cache;
            stream_usage_error = has_stream_usage;
        }
        let provider = provider_for_settings(&self.settings);
        let key = capability_key(&self.settings, provider);
        update_capabilities(&key, |capabilities| {
            if prompt_cache_error {
                capabilities.prompt_cache = ExtensionCapability::Unsupported;
            }
            if stream_usage_error {
                capabilities.stream_usage = ExtensionCapability::Unsupported;
            }
        });
        self.send(fallback(), cancel).await
    }

    async fn send(
        &self,
        body: Value,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, LlmError> {
        if self.settings.api_key.trim().is_empty() {
            return Err(LlmError::MissingApiKey);
        }
        tokio::select! {
            _ = cancel.cancelled() => Err(LlmError::Cancelled),
            result = tokio::time::timeout(
                Duration::from_secs(120),
                self.client
                    .post(Self::completion_url(&self.settings.base_url))
                    .bearer_auth(self.settings.api_key.trim())
                    .json(&body)
                    .send(),
            ) => match result {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(error)) => Err(LlmError::Http(error)),
                Err(_) => Err(LlmError::Timeout),
            }
        }
    }
}

#[allow(clippy::upper_case_acronyms)]
pub type LLM = OpenAiCompatibleClient;

pub fn tool_choice_name(choice: ToolChoice) -> &'static str {
    match choice {
        ToolChoice::None => "none",
        ToolChoice::Auto => "auto",
        ToolChoice::Required => "required",
    }
}

fn completion_from_response(payload: &Value) -> Result<Completion, LlmError> {
    if let Some(error) = provider_error(payload) {
        return Err(error);
    }
    let choice = payload
        .pointer("/choices/0")
        .ok_or_else(|| LlmError::EmptyResponse {
            details: format!(
                "The provider payload contained no choices: {}",
                provider_detail(payload)
            ),
        })?;
    let message = choice.get("message").cloned().unwrap_or_else(|| json!({}));
    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut completion = Completion {
        content,
        tool_calls: parse_tool_calls(message.get("tool_calls")),
        prompt_tokens: None,
        completion_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        finish_reason: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    };
    if let Some(usage) = payload.get("usage") {
        apply_usage(&mut completion, usage);
    }
    Ok(completion)
}

fn usage_number(usage: &Value, paths: &[&str]) -> Option<usize> {
    paths
        .iter()
        .find_map(|path| usage.pointer(path).and_then(Value::as_u64))
        .map(|value| value as usize)
}

fn usage_sum(usage: &Value, paths: &[&str]) -> Option<usize> {
    let mut total = 0usize;
    let mut found = false;
    for path in paths {
        if let Some(value) = usage.pointer(path).and_then(Value::as_u64) {
            total = total.saturating_add(value as usize);
            found = true;
        }
    }
    found.then_some(total)
}

fn apply_usage(completion: &mut Completion, usage: &Value) {
    completion.prompt_tokens = usage_number(usage, &["/prompt_tokens", "/input_tokens"]);
    completion.completion_tokens = usage_number(usage, &["/completion_tokens", "/output_tokens"]);
    completion.cached_input_tokens = usage_number(
        usage,
        &[
            "/prompt_tokens_details/cached_tokens",
            "/prompt_tokens_details/cache_read_tokens",
            "/input_tokens_details/cached_tokens",
            "/input_tokens_details/cache_read_tokens",
            "/input_token_details/cache_read_tokens",
            "/cached_tokens",
            "/cache_read_input_tokens",
        ],
    );
    completion.cache_write_tokens = usage_number(
        usage,
        &[
            "/prompt_tokens_details/cache_write_tokens",
            "/prompt_tokens_details/cache_creation_input_tokens",
            "/input_tokens_details/cache_write_tokens",
            "/input_tokens_details/cache_creation_input_tokens",
            "/input_token_details/cache_write_tokens",
            "/cache_write_tokens",
            "/cache_creation_input_tokens",
            "/cache_creation_tokens",
        ],
    )
    .or_else(|| {
        usage_sum(
            usage,
            &[
                "/cache_creation/ephemeral_5m_input_tokens",
                "/cache_creation/ephemeral_1h_input_tokens",
            ],
        )
    });
}

fn parse_tool_calls(value: Option<&Value>) -> Vec<MessageToolCall> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|call| MessageToolCall {
            id: call
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            call_type: call
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("function")
                .to_string(),
            function: FunctionCall {
                name: call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}")
                    .to_string(),
            },
        })
        .collect()
}

async fn response_error(response: reqwest::Response) -> LlmError {
    let status = response.status();
    let body = response
        .text()
        .await
        .unwrap_or_else(|_| "No error body returned.".to_string());
    LlmError::HttpStatus { status, body }
}

fn consume_sse_line<F>(
    line: &str,
    completion: &mut Completion,
    on_event: &mut F,
) -> Result<bool, LlmError>
where
    F: FnMut(&StreamEvent) -> Result<(), LlmError>,
{
    let data = match line.strip_prefix("data:") {
        Some(data) => data.trim(),
        None if line.trim_start().starts_with('{') => line.trim(),
        None => return Ok(false),
    };
    if data == "[DONE]" {
        return Ok(true);
    }
    if data.is_empty() {
        return Ok(false);
    }
    let chunk: Value = serde_json::from_str(data)?;
    if let Some(error) = provider_error(&chunk) {
        return Err(error);
    }
    if let Some(reason) = chunk
        .pointer("/choices/0/finish_reason")
        .and_then(Value::as_str)
    {
        completion.finish_reason = Some(reason.to_string());
    }
    if let Some(content) = chunk
        .pointer("/choices/0/delta/content")
        .and_then(Value::as_str)
    {
        completion.content.push_str(content);
        on_event(&StreamEvent::TextDelta(content.to_string()))?;
    }
    if let Some(deltas) = chunk
        .pointer("/choices/0/delta/tool_calls")
        .and_then(Value::as_array)
    {
        for delta in deltas {
            let index = delta.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            while completion.tool_calls.len() <= index {
                completion.tool_calls.push(MessageToolCall {
                    id: String::new(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: String::new(),
                        arguments: String::new(),
                    },
                });
            }
            let call = &mut completion.tool_calls[index];
            if let Some(id) = delta.get("id").and_then(Value::as_str) {
                call.id.push_str(id);
            }
            if let Some(name) = delta.pointer("/function/name").and_then(Value::as_str) {
                call.function.name.push_str(name);
            }
            if let Some(arguments) = delta.pointer("/function/arguments").and_then(Value::as_str) {
                call.function.arguments.push_str(arguments);
            }
            on_event(&StreamEvent::ToolCallDelta {
                index,
                id: delta.get("id").and_then(Value::as_str).map(str::to_string),
                name: delta
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                arguments: delta
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })?;
        }
    }
    if let Some(usage) = chunk.get("usage") {
        apply_usage(completion, usage);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counter_matches_image_rules() {
        assert_eq!(TokenCounter::count_image(&json!({"detail": "low"})), 85);
        assert_eq!(TokenCounter::high_detail_tokens(1024, 1024), 765);
        assert!(
            TokenCounter::count_messages(&[json!({
                "role": "user",
                "content": "hello"
            })]) > 0
        );
    }

    #[test]
    fn message_formatting_removes_private_image_field() {
        let mut message = Message::user("look");
        message.base64_image = Some("abc".to_string());
        let formatted = OpenAiCompatibleClient::format_messages(&[message], true).unwrap();
        assert!(formatted[0].get("base64_image").is_none());
        assert_eq!(formatted[0]["content"][1]["type"], "image_url");
    }

    #[test]
    fn completion_url_and_sse_tool_deltas_are_stable() {
        assert_eq!(
            OpenAiCompatibleClient::completion_url("http://localhost:1/v1"),
            "http://localhost:1/v1/chat/completions"
        );
        let mut completion = Completion {
            content: String::new(),
            tool_calls: vec![],
            prompt_tokens: None,
            completion_tokens: None,
            cached_input_tokens: None,
            cache_write_tokens: None,
            finish_reason: None,
        };
        let mut events = Vec::new();
        consume_sse_line(
            r#"data: {"choices":[{"delta":{"content":"hi","tool_calls":[{"index":0,"id":"c1","function":{"name":"rust_clock","arguments":"{}"}}]}}]}"#,
            &mut completion,
            &mut |event| {
                events.push(event.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta("hi".to_string()),
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("c1".to_string()),
                    name: Some("rust_clock".to_string()),
                    arguments: Some("{}".to_string()),
                },
            ]
        );
        assert_eq!(completion.tool_calls[0].function.name, "rust_clock");
    }

    #[test]
    fn utf8_stream_chunks_preserve_multibyte_text() {
        let source = "你好，RustPilot";
        let mut buffer = String::new();
        let mut pending = Vec::new();
        for byte in source.as_bytes().chunks(2) {
            append_utf8_bytes(&mut buffer, &mut pending, byte);
        }
        if !pending.is_empty() {
            buffer.push_str(&String::from_utf8_lossy(&pending));
        }
        assert_eq!(buffer, source);
    }

    #[test]
    fn prompt_cache_adds_openai_key_and_stream_usage_without_local_state() {
        let mut settings = LlmSettings::default();
        settings.model = "cache-test-openai".to_string();
        settings.base_url = "https://api.openai.test/v1".to_string();
        settings.api_key = "test-key".to_string();
        settings.session_id = Some("rustpilot:task-1".to_string());
        settings.tool_schema_hash = Some("0123456789abcdef".to_string());
        let client = OpenAiCompatibleClient::new(settings).unwrap();
        let body = client.request_body(
            &[json!({"role": "system", "content": "stable"})],
            &[],
            ToolChoice::Auto,
            None,
            true,
        );
        assert_eq!(body["prompt_cache_key"], "rp:task-1:tools:0123456789abcdef");
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert!(body.get("cache_control").is_none());
    }

    #[test]
    fn prompt_cache_key_changes_only_when_schema_changes() {
        let base = prompt_cache_key(Some("rustpilot:task-1"), None).unwrap();
        let first = prompt_cache_key(Some("rustpilot:task-1"), Some("schema-a")).unwrap();
        let second = prompt_cache_key(Some("rustpilot:task-1"), Some("schema-b")).unwrap();
        assert_eq!(base, "rp:task-1");
        assert_eq!(first, "rp:task-1:tools:schema-a");
        assert_ne!(first, second);
        assert_eq!(
            first,
            prompt_cache_key(Some("rustpilot:task-1"), Some("schema-a")).unwrap()
        );
        let long_hash = "a".repeat(64);
        let long_key = prompt_cache_key(Some("rustpilot:task-1"), Some(&long_hash)).unwrap();
        assert!(long_key.len() <= 64);
    }

    #[test]
    fn prompt_cache_disabled_does_not_change_wire_body() {
        let mut settings = LlmSettings::default();
        settings.model = "cache-test-disabled".to_string();
        settings.prompt_cache = PromptCacheMode::Disabled;
        settings.session_id = Some("rustpilot:task-2".to_string());
        let client = OpenAiCompatibleClient::new(settings).unwrap();
        let body = client.request_body(
            &[json!({"role": "system", "content": "stable"})],
            &[],
            ToolChoice::Auto,
            None,
            true,
        );
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("session_id").is_none());
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn openrouter_breakpoints_match_opencode_message_selection() {
        let mut settings = LlmSettings::default();
        settings.model = "cache-test-openrouter".to_string();
        settings.base_url = "https://openrouter.ai/api/v1".to_string();
        settings.api_type = "openrouter".to_string();
        settings.session_id = Some("rustpilot:task-3".to_string());
        let client = OpenAiCompatibleClient::new(settings).unwrap();
        let messages = vec![
            json!({"role": "system", "content": "system-1"}),
            json!({"role": "system", "content": "system-2"}),
            json!({"role": "system", "content": "system-3"}),
            json!({"role": "user", "content": "user-1"}),
            json!({"role": "assistant", "content": "assistant-1"}),
            json!({"role": "user", "content": "user-2"}),
            json!({"role": "user", "content": "user-3"}),
        ];
        let body = client.request_body(&messages, &[], ToolChoice::Auto, None, false);
        assert_eq!(body["session_id"], "rp:task-3");
        for index in [0, 1, 5, 6] {
            assert_eq!(
                body["messages"][index]["content"][0]["cache_control"]["type"],
                "ephemeral"
            );
        }
        for index in [2, 3, 4] {
            assert!(body["messages"][index]["content"].is_string());
        }
    }

    #[test]
    fn usage_parser_handles_openai_and_anthropic_cache_fields() {
        let openai = completion_from_response(&json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 80, "cache_write_tokens": 20}
            }
        }))
        .unwrap();
        assert_eq!(openai.cached_input_tokens, Some(80));
        assert_eq!(openai.cache_write_tokens, Some(20));

        let anthropic = completion_from_response(&json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 5,
                "cache_read_input_tokens": 70,
                "cache_creation_input_tokens": 30
            }
        }))
        .unwrap();
        assert_eq!(anthropic.prompt_tokens, Some(100));
        assert_eq!(anthropic.completion_tokens, Some(5));
        assert_eq!(anthropic.cached_input_tokens, Some(70));
        assert_eq!(anthropic.cache_write_tokens, Some(30));
    }

    #[test]
    fn provider_errors_are_not_rewritten_as_empty_responses() {
        let error = completion_from_response(&json!({
            "error": {
                "message": "upstream overloaded",
                "type": "server_error",
                "code": "temporarily_unavailable"
            }
        }))
        .unwrap_err();
        assert!(matches!(error, LlmError::Upstream(_)));
        assert!(error.to_string().contains("upstream overloaded"));

        let empty = completion_from_response(&json!({"choices": []})).unwrap_err();
        assert!(matches!(empty, LlmError::EmptyResponse { .. }));
        assert!(empty.to_string().contains("no choices"));
    }

    #[test]
    fn retry_policy_matches_transient_provider_failures() {
        assert!(is_retryable_error(&LlmError::EmptyResponse {
            details: "no choices".to_string(),
        }));
        assert!(is_retryable_error(&LlmError::HttpStatus {
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            body: "try again".to_string(),
        }));
        assert!(is_retryable_error(&LlmError::HttpStatus {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: "rate limited".to_string(),
        }));
        assert!(!is_retryable_error(&LlmError::HttpStatus {
            status: reqwest::StatusCode::UNAUTHORIZED,
            body: "invalid key".to_string(),
        }));
        assert!(is_retryable_error(&LlmError::Upstream(
            r#"{"error":{"type":"server_error","message":"temporarily unavailable"}}"#.to_string()
        )));
        assert!(!is_retryable_error(&LlmError::Upstream(
            "invalid prompt".to_string()
        )));
        assert_eq!(LLM_MAX_ATTEMPTS, 3);
        assert_eq!(LLM_RETRY_DELAY_SECS, 5);
    }

    #[test]
    fn sse_provider_errors_are_preserved() {
        let mut completion = Completion {
            content: String::new(),
            tool_calls: Vec::new(),
            prompt_tokens: None,
            completion_tokens: None,
            cached_input_tokens: None,
            cache_write_tokens: None,
            finish_reason: None,
        };
        let error = consume_sse_line(
            r#"data: {"type":"error","error":{"message":"provider rejected request"}}"#,
            &mut completion,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(error, LlmError::Upstream(_)));
        assert!(error.to_string().contains("provider rejected request"));

        let plain_json_error = consume_sse_line(
            r#"{"error":{"message":"gateway rejected request"}}"#,
            &mut completion,
            &mut |_| Ok(()),
        )
        .unwrap_err();
        assert!(matches!(plain_json_error, LlmError::Upstream(_)));
        assert!(plain_json_error
            .to_string()
            .contains("gateway rejected request"));
    }

    #[test]
    fn stream_usage_is_parsed_without_content_deltas() {
        let mut completion = Completion {
            content: String::new(),
            tool_calls: Vec::new(),
            prompt_tokens: None,
            completion_tokens: None,
            cached_input_tokens: None,
            cache_write_tokens: None,
            finish_reason: None,
        };
        consume_sse_line(
            r#"data: {"usage":{"prompt_tokens":40,"completion_tokens":4,"prompt_tokens_details":{"cached_tokens":32,"cache_write_tokens":8}}}"#,
            &mut completion,
            &mut |_| Ok(()),
        )
        .unwrap();
        assert_eq!(completion.cached_input_tokens, Some(32));
        assert_eq!(completion.cache_write_tokens, Some(8));
    }

    #[test]
    fn token_usage_accumulates_cache_metrics_separately() {
        let mut usage = TokenUsage::default();
        usage.record(100, 10, Some(80), Some(20));
        usage.record(50, 5, None, None);
        assert_eq!(usage.total_input_tokens, 150);
        assert_eq!(usage.total_cached_input_tokens, 80);
        assert_eq!(usage.total_cache_write_tokens, 20);
        assert_eq!(usage.cache_hit_count, 1);
        assert_eq!(usage.cache_write_count, 1);
    }
}
