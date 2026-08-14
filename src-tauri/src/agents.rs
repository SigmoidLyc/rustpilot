//! Agent specializations.
//!
//! The desktop runner can use the same profiles directly, while the concrete
//! Tauri dispatcher remains responsible for approval dialogs and OS handles.
//! Keeping these types explicit preserves the specialization boundaries without
//! introducing a heavy dependency-injection framework.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    agent::{prompt_profile, AgentKind, DEFAULT_MAX_AGENT_STEPS},
    react::ReActAgentRuntime,
    tool::ToolCollection,
};

pub(crate) const MCP_DYNAMIC_TOOL_PREFIX: &str = "rust_mcp_";
pub(crate) const TERMINATE_TOOL_NAME: &str = "rust_terminate";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSpec {
    pub key: String,
    pub name: String,
    pub description: String,
    pub kind: AgentKind,
    pub system_prompt: String,
    pub next_step_prompt: String,
    pub max_steps: u32,
    pub max_observe: Option<usize>,
    pub tool_names: Vec<String>,
    pub special_tool_names: Vec<String>,
    pub uses_browser_context: bool,
    pub uses_mcp: bool,
    pub uses_sandbox: bool,
}

impl AgentSpec {
    pub fn for_kind(kind: AgentKind, workspace: &str) -> Self {
        let (system_prompt, next_step_prompt) = prompt_profile(kind, workspace);
        let (key, name, description, max_steps, max_observe, tool_names, browser, mcp, sandbox) =
            match kind {
                AgentKind::Manus => (
                    "manus",
                    "Manus",
                    "A versatile agent that can solve tasks with local, browser, data, and MCP tools.",
                    DEFAULT_MAX_AGENT_STEPS,
                    Some(10_000),
                    vec![
                        "rust_python_execute",
                        "rust_browser_use",
                        "rust_str_replace_editor",
                        "rust_ask_human",
                        "rust_mcp",
                        "rust_terminate",
                    ],
                    true,
                    true,
                    false,
                ),
                AgentKind::Browser => (
                    "browser",
                    "browser",
                    "A browser agent that can inspect and control web pages.",
                    DEFAULT_MAX_AGENT_STEPS,
                    Some(10_000),
                    vec!["rust_browser_use", "rust_terminate"],
                    true,
                    false,
                    false,
                ),
                AgentKind::DataAnalysis => (
                    "data_analysis",
                    "Data_Analysis",
                    "An analytical agent for Python execution, data preparation, and charts.",
                    DEFAULT_MAX_AGENT_STEPS,
                    Some(15_000),
                    vec![
                        "rust_python_execute",
                        "rust_visualization_preparation",
                        "rust_data_visualization",
                        "rust_data_analysis",
                        "rust_terminate",
                    ],
                    false,
                    false,
                    false,
                ),
                AgentKind::Swe => (
                    "swe",
                    "swe",
                    "An autonomous programmer that edits and verifies a repository.",
                    DEFAULT_MAX_AGENT_STEPS,
                    None,
                    vec!["rust_code", "rust_bash", "rust_terminate"],
                    false,
                    false,
                    false,
                ),
                AgentKind::Mcp => (
                    "mcp_agent",
                    "mcp_agent",
                    "An agent that connects to an MCP server and uses its live tools.",
                    DEFAULT_MAX_AGENT_STEPS,
                    None,
                    vec!["rust_mcp", "rust_terminate"],
                    false,
                    true,
                    false,
                ),
                AgentKind::SandboxManus => (
                    "sandbox_manus",
                    "SandboxManus",
                    "A general-purpose agent using task-scoped sandbox tools and MCP.",
                    DEFAULT_MAX_AGENT_STEPS,
                    Some(10_000),
                    vec![
                        "rust_ask_human",
                        "rust_sandbox_files",
                        "rust_sandbox_shell",
                        "rust_sandbox_browser",
                        "rust_sandbox_vision",
                        "rust_mcp",
                        "rust_terminate",
                    ],
                    true,
                    true,
                    true,
                ),
            };
        Self {
            key: key.to_string(),
            name: name.to_string(),
            description: description.to_string(),
            kind,
            system_prompt,
            next_step_prompt: if matches!(kind, AgentKind::Swe) {
                String::new()
            } else {
                next_step_prompt
            },
            max_steps,
            max_observe,
            tool_names: tool_names.into_iter().map(str::to_string).collect(),
            special_tool_names: vec!["rust_terminate".to_string()],
            uses_browser_context: browser,
            uses_mcp: mcp,
            uses_sandbox: sandbox,
        }
    }

    pub fn all(workspace: &str) -> Vec<Self> {
        [
            AgentKind::Manus,
            AgentKind::Browser,
            AgentKind::DataAnalysis,
            AgentKind::Swe,
            AgentKind::Mcp,
            AgentKind::SandboxManus,
        ]
        .into_iter()
        .map(|kind| Self::for_kind(kind, workspace))
        .collect()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.key.trim().is_empty() || self.name.trim().is_empty() {
            return Err("Agent key and name must not be empty.".to_string());
        }
        if self.max_steps == 0 {
            return Err("Agent max_steps must be greater than zero.".to_string());
        }
        if self
            .tool_names
            .iter()
            .chain(self.special_tool_names.iter())
            .any(|name| !name.starts_with("rust_"))
        {
            return Err("All agent tools must use the rust_ prefix.".to_string());
        }
        Ok(())
    }

    pub(crate) fn allows_tool(&self, name: &str) -> bool {
        if name == TERMINATE_TOOL_NAME {
            return true;
        }
        if name.starts_with(MCP_DYNAMIC_TOOL_PREFIX) {
            return self.uses_mcp;
        }
        if name.starts_with("rust_sandbox_") && !self.uses_sandbox {
            return false;
        }
        if name == "rust_mcp" && !self.uses_mcp {
            return false;
        }
        self.tool_names.iter().any(|tool| tool == name)
            || self.special_tool_names.iter().any(|tool| tool == name)
    }
}

pub trait AgentProfile {
    fn spec(&self) -> &AgentSpec;
    fn runtime(&self) -> &ReActAgentRuntime;
    fn runtime_mut(&mut self) -> &mut ReActAgentRuntime;

    fn kind(&self) -> AgentKind {
        self.spec().kind
    }

    fn name(&self) -> &str {
        &self.spec().name
    }

    fn description(&self) -> &str {
        &self.spec().description
    }
}

fn runtime_for(spec: &AgentSpec, tools: ToolCollection) -> ReActAgentRuntime {
    let mut runtime = ReActAgentRuntime::new(spec.name.clone(), spec.system_prompt.clone(), tools);
    runtime.tool_agent.base.description = spec.description.clone();
    runtime.tool_agent.base.next_step_prompt = spec.next_step_prompt.clone();
    runtime.tool_agent.base.max_steps = spec.max_steps;
    runtime.tool_agent.max_observe = spec.max_observe;
    runtime.tool_agent.special_tool_names = spec.special_tool_names.clone();
    runtime
}

#[derive(Debug, Clone)]
pub struct ManusAgent {
    pub spec: AgentSpec,
    pub runtime: ReActAgentRuntime,
    pub connected_servers: BTreeMap<String, String>,
    pub initialized: bool,
}

impl ManusAgent {
    pub fn new(workspace: &str, tools: ToolCollection) -> Self {
        let spec = AgentSpec::for_kind(AgentKind::Manus, workspace);
        Self {
            runtime: runtime_for(&spec, tools),
            spec,
            connected_servers: BTreeMap::new(),
            initialized: false,
        }
    }

    pub fn connect_mcp_server(&mut self, server_id: &str, endpoint: &str) {
        self.connected_servers
            .insert(server_id.to_string(), endpoint.to_string());
        self.initialized = true;
    }

    pub fn disconnect_mcp_server(&mut self, server_id: Option<&str>) {
        if let Some(server_id) = server_id {
            self.connected_servers.remove(server_id);
        } else {
            self.connected_servers.clear();
        }
        self.initialized = !self.connected_servers.is_empty();
    }
}

impl AgentProfile for ManusAgent {
    fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    fn runtime(&self) -> &ReActAgentRuntime {
        &self.runtime
    }

    fn runtime_mut(&mut self) -> &mut ReActAgentRuntime {
        &mut self.runtime
    }
}

#[derive(Debug, Clone)]
pub struct BrowserContextHelper {
    pub current_base64_image: Option<String>,
}

impl BrowserContextHelper {
    pub fn new() -> Self {
        Self {
            current_base64_image: None,
        }
    }

    pub fn format_next_step_prompt(&self, state: Option<&Value>) -> String {
        let mut prompt = String::from("Inspect the current browser state before acting.");
        if let Some(state) = state {
            if let Some(url) = state.get("url").and_then(Value::as_str) {
                prompt.push_str(&format!("\nURL: {url}"));
            }
            if let Some(title) = state.get("title").and_then(Value::as_str) {
                prompt.push_str(&format!("\nTitle: {title}"));
            }
            if let Some(tabs) = state.get("tabs").and_then(Value::as_array) {
                prompt.push_str(&format!("\n{} tab(s) available.", tabs.len()));
            }
            if let Some(elements) = state.get("interactive_elements").and_then(Value::as_array) {
                prompt.push_str(&format!(
                    "\n{} indexed element(s) available.",
                    elements.len()
                ));
            }
        }
        prompt
    }

    pub fn set_image(&mut self, image: Option<String>) {
        self.current_base64_image = image;
    }

    pub fn take_image(&mut self) -> Option<String> {
        self.current_base64_image.take()
    }
}

impl Default for BrowserContextHelper {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
pub struct BrowserAgent {
    pub spec: AgentSpec,
    pub runtime: ReActAgentRuntime,
    pub browser_context_helper: BrowserContextHelper,
}

impl BrowserAgent {
    pub fn new(workspace: &str, tools: ToolCollection) -> Self {
        let spec = AgentSpec::for_kind(AgentKind::Browser, workspace);
        Self {
            runtime: runtime_for(&spec, tools),
            spec,
            browser_context_helper: BrowserContextHelper::new(),
        }
    }

    pub fn update_browser_prompt(&mut self, state: Option<&Value>) {
        self.runtime.tool_agent.base.next_step_prompt =
            self.browser_context_helper.format_next_step_prompt(state);
    }
}

impl AgentProfile for BrowserAgent {
    fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    fn runtime(&self) -> &ReActAgentRuntime {
        &self.runtime
    }

    fn runtime_mut(&mut self) -> &mut ReActAgentRuntime {
        &mut self.runtime
    }
}

#[derive(Debug, Clone)]
pub struct DataAnalysisAgent {
    pub spec: AgentSpec,
    pub runtime: ReActAgentRuntime,
}

impl DataAnalysisAgent {
    pub fn new(workspace: &str, tools: ToolCollection) -> Self {
        let spec = AgentSpec::for_kind(AgentKind::DataAnalysis, workspace);
        Self {
            runtime: runtime_for(&spec, tools),
            spec,
        }
    }
}

impl AgentProfile for DataAnalysisAgent {
    fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    fn runtime(&self) -> &ReActAgentRuntime {
        &self.runtime
    }

    fn runtime_mut(&mut self) -> &mut ReActAgentRuntime {
        &mut self.runtime
    }
}

#[derive(Debug, Clone)]
pub struct SweAgent {
    pub spec: AgentSpec,
    pub runtime: ReActAgentRuntime,
}

impl SweAgent {
    pub fn new(workspace: &str, tools: ToolCollection) -> Self {
        let spec = AgentSpec::for_kind(AgentKind::Swe, workspace);
        Self {
            runtime: runtime_for(&spec, tools),
            spec,
        }
    }
}

impl AgentProfile for SweAgent {
    fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    fn runtime(&self) -> &ReActAgentRuntime {
        &self.runtime
    }

    fn runtime_mut(&mut self) -> &mut ReActAgentRuntime {
        &mut self.runtime
    }
}

#[derive(Debug, Clone)]
pub struct McpAgent {
    pub spec: AgentSpec,
    pub runtime: ReActAgentRuntime,
    pub connection_type: String,
    pub tool_schemas: BTreeMap<String, Value>,
    pub refresh_tools_interval: u32,
}

impl McpAgent {
    pub fn new(workspace: &str, tools: ToolCollection) -> Self {
        let spec = AgentSpec::for_kind(AgentKind::Mcp, workspace);
        Self {
            runtime: runtime_for(&spec, tools),
            spec,
            connection_type: "stdio".to_string(),
            tool_schemas: BTreeMap::new(),
            refresh_tools_interval: 5,
        }
    }

    pub fn initialize(&mut self, connection_type: &str) -> Result<(), String> {
        if !matches!(connection_type, "stdio" | "sse" | "http") {
            return Err(format!(
                "Unsupported MCP connection type: {connection_type}"
            ));
        }
        self.connection_type = connection_type.to_string();
        Ok(())
    }

    pub fn refresh_tools(
        &mut self,
        schemas: BTreeMap<String, Value>,
    ) -> (Vec<String>, Vec<String>) {
        let previous = self.tool_schemas.keys().cloned().collect::<Vec<_>>();
        let current = schemas.keys().cloned().collect::<Vec<_>>();
        let added = current
            .iter()
            .filter(|name| !self.tool_schemas.contains_key(*name))
            .cloned()
            .collect();
        let removed = previous
            .iter()
            .filter(|name| !schemas.contains_key(*name))
            .cloned()
            .collect();
        self.tool_schemas = schemas;
        (added, removed)
    }
}

impl AgentProfile for McpAgent {
    fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    fn runtime(&self) -> &ReActAgentRuntime {
        &self.runtime
    }

    fn runtime_mut(&mut self) -> &mut ReActAgentRuntime {
        &mut self.runtime
    }
}

#[derive(Debug, Clone)]
pub struct SandboxManusAgent {
    pub spec: AgentSpec,
    pub runtime: ReActAgentRuntime,
    pub sandbox_links: BTreeMap<String, BTreeMap<String, String>>,
    pub connected_servers: BTreeMap<String, String>,
    pub initialized: bool,
}

impl SandboxManusAgent {
    pub fn new(workspace: &str, tools: ToolCollection) -> Self {
        let spec = AgentSpec::for_kind(AgentKind::SandboxManus, workspace);
        Self {
            runtime: runtime_for(&spec, tools),
            spec,
            sandbox_links: BTreeMap::new(),
            connected_servers: BTreeMap::new(),
            initialized: false,
        }
    }

    pub fn add_sandbox_link(&mut self, sandbox_id: &str, kind: &str, url: &str) {
        self.sandbox_links
            .entry(sandbox_id.to_string())
            .or_default()
            .insert(kind.to_string(), url.to_string());
        self.initialized = true;
    }

    pub fn delete_sandbox(&mut self, sandbox_id: &str) -> bool {
        self.sandbox_links.remove(sandbox_id).is_some()
    }
}

impl AgentProfile for SandboxManusAgent {
    fn spec(&self) -> &AgentSpec {
        &self.spec
    }

    fn runtime(&self) -> &ReActAgentRuntime {
        &self.runtime
    }

    fn runtime_mut(&mut self) -> &mut ReActAgentRuntime {
        &mut self.runtime
    }
}

pub enum AgentInstance {
    Manus(ManusAgent),
    Browser(BrowserAgent),
    DataAnalysis(DataAnalysisAgent),
    Swe(SweAgent),
    Mcp(McpAgent),
    SandboxManus(SandboxManusAgent),
}

impl std::fmt::Debug for AgentInstance {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentInstance")
            .field("spec", self.spec())
            .finish()
    }
}

impl AgentInstance {
    pub fn create(kind: AgentKind, workspace: &str, tools: ToolCollection) -> Self {
        match kind {
            AgentKind::Manus => Self::Manus(ManusAgent::new(workspace, tools)),
            AgentKind::Browser => Self::Browser(BrowserAgent::new(workspace, tools)),
            AgentKind::DataAnalysis => Self::DataAnalysis(DataAnalysisAgent::new(workspace, tools)),
            AgentKind::Swe => Self::Swe(SweAgent::new(workspace, tools)),
            AgentKind::Mcp => Self::Mcp(McpAgent::new(workspace, tools)),
            AgentKind::SandboxManus => Self::SandboxManus(SandboxManusAgent::new(workspace, tools)),
        }
    }

    pub fn spec(&self) -> &AgentSpec {
        match self {
            Self::Manus(agent) => agent.spec(),
            Self::Browser(agent) => agent.spec(),
            Self::DataAnalysis(agent) => agent.spec(),
            Self::Swe(agent) => agent.spec(),
            Self::Mcp(agent) => agent.spec(),
            Self::SandboxManus(agent) => agent.spec(),
        }
    }

    pub fn runtime(&self) -> &ReActAgentRuntime {
        match self {
            Self::Manus(agent) => agent.runtime(),
            Self::Browser(agent) => agent.runtime(),
            Self::DataAnalysis(agent) => agent.runtime(),
            Self::Swe(agent) => agent.runtime(),
            Self::Mcp(agent) => agent.runtime(),
            Self::SandboxManus(agent) => agent.runtime(),
        }
    }

    pub fn runtime_mut(&mut self) -> &mut ReActAgentRuntime {
        match self {
            Self::Manus(agent) => agent.runtime_mut(),
            Self::Browser(agent) => agent.runtime_mut(),
            Self::DataAnalysis(agent) => agent.runtime_mut(),
            Self::Swe(agent) => agent.runtime_mut(),
            Self::Mcp(agent) => agent.runtime_mut(),
            Self::SandboxManus(agent) => agent.runtime_mut(),
        }
    }
}

pub type Manus = ManusAgent;
pub type DataAnalysis = DataAnalysisAgent;
pub type SWEAgent = SweAgent;
pub type MCPAgent = McpAgent;
pub type SandboxManus = SandboxManusAgent;

pub struct AgentFactory;

impl AgentFactory {
    pub fn create(kind: AgentKind, workspace: &str, tools: ToolCollection) -> AgentInstance {
        AgentInstance::create(kind, workspace, tools)
    }

    pub fn create_from_name(name: &str, workspace: &str, tools: ToolCollection) -> AgentInstance {
        let kind = crate::agent::parse_agent_kind(name).unwrap_or(AgentKind::Manus);
        Self::create(kind, workspace, tools)
    }

    pub fn specs(workspace: &str) -> Vec<AgentSpec> {
        AgentSpec::all(workspace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specialized_defaults_match_agent_profiles() {
        let specs = AgentSpec::all(".");
        assert_eq!(specs.len(), 6);
        assert_eq!(
            specs
                .iter()
                .find(|spec| spec.kind == AgentKind::Manus)
                .unwrap()
                .max_steps,
            DEFAULT_MAX_AGENT_STEPS
        );
        assert_eq!(
            specs
                .iter()
                .find(|spec| spec.kind == AgentKind::DataAnalysis)
                .unwrap()
                .max_observe,
            Some(15_000)
        );
        assert_eq!(
            specs
                .iter()
                .find(|spec| spec.kind == AgentKind::Swe)
                .unwrap()
                .next_step_prompt,
            ""
        );
        for spec in specs {
            spec.validate().unwrap();
        }
    }

    #[test]
    fn mcp_refresh_reports_added_and_removed_tools() {
        let mut agent = McpAgent::new(".", ToolCollection::default());
        let (added, removed) =
            agent.refresh_tools(BTreeMap::from([("weather".to_string(), Value::Null)]));
        assert_eq!(added, vec!["weather"]);
        assert!(removed.is_empty());
        let (added, removed) = agent.refresh_tools(BTreeMap::new());
        assert!(added.is_empty());
        assert_eq!(removed, vec!["weather"]);
    }
}
