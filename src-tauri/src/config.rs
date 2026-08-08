//! Lightweight configuration models for the desktop runtime.
//!
//! Configuration files may provide defaults, but secrets are intentionally
//! resolved from environment variables or kept in memory by the desktop
//! settings command.  `mcp.json` is supported for MCP server settings.

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::llm::LlmSettings;

pub const PROJECT_CONFIG_DIR: &str = "config";
pub const MCP_CONFIG_FILE: &str = "mcp.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxySettings {
    pub server: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchSettings {
    #[serde(default = "default_search_engine")]
    pub engine: String,
    #[serde(default = "default_fallback_engines")]
    pub fallback_engines: Vec<String>,
    #[serde(default = "default_retry_delay")]
    pub retry_delay: u64,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_language")]
    pub lang: String,
    #[serde(default = "default_country")]
    pub country: String,
}

impl Default for SearchSettings {
    fn default() -> Self {
        Self {
            engine: default_search_engine(),
            fallback_engines: default_fallback_engines(),
            retry_delay: default_retry_delay(),
            max_retries: default_max_retries(),
            lang: default_language(),
            country: default_country(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct RunflowSettings {
    #[serde(default)]
    pub use_data_analysis_agent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserSettings {
    #[serde(default)]
    pub headless: bool,
    #[serde(default = "default_true")]
    pub disable_security: bool,
    #[serde(default)]
    pub extra_chromium_args: Vec<String>,
    #[serde(default)]
    pub chrome_instance_path: Option<String>,
    #[serde(default)]
    pub wss_url: Option<String>,
    #[serde(default)]
    pub cdp_url: Option<String>,
    #[serde(default)]
    pub proxy: Option<ProxySettings>,
    #[serde(default = "default_content_length")]
    pub max_content_length: usize,
}

impl Default for BrowserSettings {
    fn default() -> Self {
        Self {
            headless: false,
            disable_security: true,
            extra_chromium_args: Vec::new(),
            chrome_instance_path: None,
            wss_url: None,
            cdp_url: None,
            proxy: None,
            max_content_length: default_content_length(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SandboxSettings {
    #[serde(default)]
    pub use_sandbox: bool,
    #[serde(default = "default_sandbox_image")]
    pub image: String,
    #[serde(default = "default_work_dir")]
    pub work_dir: String,
    #[serde(default = "default_memory_limit")]
    pub memory_limit: String,
    #[serde(default = "default_cpu_limit")]
    pub cpu_limit: f32,
    #[serde(default = "default_sandbox_timeout")]
    pub timeout: u64,
    #[serde(default)]
    pub network_enabled: bool,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            use_sandbox: false,
            image: default_sandbox_image(),
            work_dir: default_work_dir(),
            memory_limit: default_memory_limit(),
            cpu_limit: default_cpu_limit(),
            timeout: default_sandbox_timeout(),
            network_enabled: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaytonaSettings {
    #[serde(default, skip_serializing)]
    pub daytona_api_key: String,
    #[serde(default = "default_daytona_server_url")]
    pub daytona_server_url: Option<String>,
    #[serde(default = "default_daytona_target")]
    pub daytona_target: Option<String>,
    #[serde(default = "default_sandbox_image_name")]
    pub sandbox_image_name: Option<String>,
    #[serde(default = "default_sandbox_entrypoint")]
    pub sandbox_entrypoint: Option<String>,
    #[serde(
        rename = "VNC_password",
        default = "default_vnc_password",
        skip_serializing
    )]
    pub vnc_password: Option<String>,
}

impl Default for DaytonaSettings {
    fn default() -> Self {
        Self {
            daytona_api_key: String::new(),
            daytona_server_url: default_daytona_server_url(),
            daytona_target: default_daytona_target(),
            sandbox_image_name: default_sandbox_image_name(),
            sandbox_entrypoint: default_sandbox_entrypoint(),
            vnc_password: default_vnc_password(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpServerConfig {
    pub r#type: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpSettings {
    #[serde(default = "default_server_reference")]
    pub server_reference: String,
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl Default for McpSettings {
    fn default() -> Self {
        Self {
            server_reference: default_server_reference(),
            servers: BTreeMap::new(),
        }
    }
}

impl McpSettings {
    pub fn load_server_config(root: &Path) -> Result<BTreeMap<String, McpServerConfig>, String> {
        let path = root.join(PROJECT_CONFIG_DIR).join(MCP_CONFIG_FILE);
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let contents = fs::read_to_string(&path)
            .map_err(|error| format!("Unable to read MCP config {}: {error}", path.display()))?;
        let value: Value = serde_json::from_str(&contents)
            .map_err(|error| format!("Unable to parse MCP config {}: {error}", path.display()))?;
        let Some(servers) = value.get("mcpServers").and_then(Value::as_object) else {
            return Ok(BTreeMap::new());
        };
        let mut parsed = BTreeMap::new();
        for (server_id, server) in servers {
            let server_type = server
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("stdio")
                .to_string();
            let args = server
                .get("args")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(ToString::to_string)
                        .collect()
                })
                .unwrap_or_default();
            parsed.insert(
                server_id.clone(),
                McpServerConfig {
                    r#type: server_type,
                    url: server
                        .get("url")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    command: server
                        .get("command")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                    args,
                },
            );
        }
        Ok(parsed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub llm: BTreeMap<String, LlmSettings>,
    #[serde(default)]
    pub sandbox: SandboxSettings,
    #[serde(default)]
    pub browser_config: Option<BrowserSettings>,
    #[serde(default)]
    pub search_config: Option<SearchSettings>,
    #[serde(default)]
    pub mcp_config: McpSettings,
    #[serde(default)]
    pub run_flow_config: RunflowSettings,
    #[serde(default)]
    pub daytona_config: DaytonaSettings,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            llm: BTreeMap::from([("default".to_string(), LlmSettings::default())]),
            sandbox: SandboxSettings::default(),
            browser_config: None,
            search_config: None,
            mcp_config: McpSettings::default(),
            run_flow_config: RunflowSettings::default(),
            daytona_config: DaytonaSettings::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub app: AppConfig,
    pub project_root: PathBuf,
    pub workspace_root: PathBuf,
}

impl Config {
    pub fn load(project_root: impl Into<PathBuf>) -> Result<Self, String> {
        let project_root = project_root.into();
        let config_dir = project_root.join(PROJECT_CONFIG_DIR);
        let config_path = ["config.toml", "config.example.toml"]
            .iter()
            .map(|name| config_dir.join(name))
            .find(|path| path.exists());
        let mut app =
            if let Some(path) = config_path {
                parse_toml_config(&fs::read_to_string(&path).map_err(|error| {
                    format!("Unable to read config {}: {error}", path.display())
                })?)?
            } else {
                AppConfig::default()
            };
        app.mcp_config.servers = McpSettings::load_server_config(&project_root)?;
        apply_environment(&mut app);
        let workspace_root = env::var_os("RUSTPILOT_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|| project_root.join("workspace"));
        Ok(Self {
            app,
            project_root,
            workspace_root,
        })
    }

    pub fn from_env() -> Result<Self, String> {
        let project_root = env::var_os("RUSTPILOT_PROJECT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::load(project_root)
    }

    pub fn llm(&self, name: &str) -> LlmSettings {
        self.app
            .llm
            .get(name)
            .or_else(|| self.app.llm.get("default"))
            .cloned()
            .unwrap_or_default()
    }
}

fn parse_toml_config(contents: &str) -> Result<AppConfig, String> {
    let raw: toml::Value =
        toml::from_str(contents).map_err(|error| format!("Invalid TOML config: {error}"))?;
    let mut app = AppConfig::default();
    if let Some(llm) = raw.get("llm").and_then(toml::Value::as_table) {
        let base = llm_settings_from_toml(llm, &LlmSettings::default());
        app.llm.insert("default".to_string(), base.clone());
        for (name, profile) in llm {
            if let Some(table) = profile.as_table() {
                app.llm
                    .insert(name.clone(), llm_settings_from_toml(table, &base));
            }
        }
    }
    if let Some(value) = raw.get("browser") {
        app.browser_config = value
            .clone()
            .try_into()
            .map(Some)
            .map_err(|error| format!("Invalid browser config: {error}"))?;
    }
    if let Some(value) = raw.get("search") {
        app.search_config = value
            .clone()
            .try_into()
            .map(Some)
            .map_err(|error| format!("Invalid search config: {error}"))?;
    }
    if let Some(value) = raw.get("sandbox") {
        app.sandbox = value
            .clone()
            .try_into()
            .map_err(|error| format!("Invalid sandbox config: {error}"))?;
    }
    if let Some(value) = raw.get("runflow") {
        app.run_flow_config = value
            .clone()
            .try_into()
            .map_err(|error| format!("Invalid runflow config: {error}"))?;
    }
    if let Some(value) = raw.get("daytona") {
        app.daytona_config = value
            .clone()
            .try_into()
            .map_err(|error| format!("Invalid daytona config: {error}"))?;
    }
    if let Some(value) = raw.get("mcp") {
        app.mcp_config = value
            .clone()
            .try_into()
            .map_err(|error| format!("Invalid MCP config: {error}"))?;
    }
    Ok(app)
}

fn llm_settings_from_toml(
    table: &toml::map::Map<String, toml::Value>,
    fallback: &LlmSettings,
) -> LlmSettings {
    let mut settings = fallback.clone();
    if let Some(value) = table.get("model").and_then(toml::Value::as_str) {
        settings.model = value.to_string();
    }
    if let Some(value) = table.get("base_url").and_then(toml::Value::as_str) {
        settings.base_url = value.to_string();
    }
    if let Some(value) = table.get("api_key").and_then(toml::Value::as_str) {
        settings.api_key = value.to_string();
    }
    if let Some(value) = table.get("max_tokens").and_then(toml::Value::as_integer) {
        settings.max_tokens = value.max(1) as u32;
    }
    if let Some(value) = table
        .get("max_input_tokens")
        .and_then(toml::Value::as_integer)
    {
        settings.max_input_tokens = (value > 0).then_some(value as usize);
    }
    if let Some(value) = table.get("temperature").and_then(toml::Value::as_float) {
        settings.temperature = value as f32;
    }
    if let Some(value) = table.get("api_type").and_then(toml::Value::as_str) {
        settings.api_type = value.to_string();
    }
    if let Some(value) = table.get("api_version").and_then(toml::Value::as_str) {
        settings.api_version = value.to_string();
    }
    if let Some(value) = table.get("prompt_cache").and_then(toml::Value::as_str) {
        if let Some(mode) = crate::llm::PromptCacheMode::parse(value) {
            settings.prompt_cache = mode;
        }
    }
    settings
}

fn apply_environment(app: &mut AppConfig) {
    let default = app.llm.entry("default".to_string()).or_default();
    if let Some(value) = env_value("RUSTPILOT_MODEL") {
        default.model = value;
    }
    if let Some(value) = env_value("RUSTPILOT_API_BASE_URL") {
        default.base_url = value.trim_end_matches('/').to_string();
    }
    if let Some(value) = env_value("RUSTPILOT_API_KEY").or_else(|| env_value("OPENAI_API_KEY")) {
        default.api_key = value;
    }
    if let Some(value) = env_value("RUSTPILOT_PROMPT_CACHE")
        .and_then(|value| crate::llm::PromptCacheMode::parse(&value))
    {
        default.prompt_cache = value;
    }
    if let Some(value) = env_value("DAYTONA_API_KEY") {
        app.daytona_config.daytona_api_key = value;
    }
}

fn env_value(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn default_true() -> bool {
    true
}
fn default_search_engine() -> String {
    "Google".to_string()
}
fn default_fallback_engines() -> Vec<String> {
    vec![
        "DuckDuckGo".to_string(),
        "Baidu".to_string(),
        "Bing".to_string(),
    ]
}
fn default_retry_delay() -> u64 {
    60
}
fn default_max_retries() -> u32 {
    3
}
fn default_language() -> String {
    "en".to_string()
}
fn default_country() -> String {
    "us".to_string()
}
fn default_content_length() -> usize {
    2000
}
fn default_sandbox_image() -> String {
    "python:3.12-slim".to_string()
}
fn default_work_dir() -> String {
    "/workspace".to_string()
}
fn default_memory_limit() -> String {
    "512m".to_string()
}
fn default_cpu_limit() -> f32 {
    1.0
}
fn default_sandbox_timeout() -> u64 {
    300
}
fn default_daytona_server_url() -> Option<String> {
    Some("https://app.daytona.io/api".to_string())
}
fn default_daytona_target() -> Option<String> {
    Some("us".to_string())
}
fn default_sandbox_image_name() -> Option<String> {
    Some("whitezxj/sandbox:0.1.0".to_string())
}
fn default_sandbox_entrypoint() -> Option<String> {
    Some("/usr/bin/supervisord -n -c /etc/supervisor/conf.d/supervisord.conf".to_string())
}
fn default_vnc_password() -> Option<String> {
    Some("123456".to_string())
}
fn default_server_reference() -> String {
    "app.mcp.server".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_config_shape() {
        let config = Config::load(".").unwrap();
        assert!(config.llm("default").max_tokens > 0);
        assert_eq!(config.app.sandbox.work_dir, "/workspace");
        assert_eq!(config.app.search_config, None);
    }

    #[test]
    fn environment_overrides_are_applied_without_persisting_secrets() {
        let previous = env::var("RUSTPILOT_MODEL").ok();
        env::set_var("RUSTPILOT_MODEL", "test-model");
        let config = Config::load(".").unwrap();
        if let Some(value) = previous {
            env::set_var("RUSTPILOT_MODEL", value);
        } else {
            env::remove_var("RUSTPILOT_MODEL");
        }
        assert_eq!(config.llm("default").model, "test-model");
    }
}
