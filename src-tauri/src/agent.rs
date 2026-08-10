//! Agent primitives shared by the desktop runtime.
//!
//! Agent state, chat messages, bounded memory, ReAct decisions, and tool-call
//! execution stay separate from the desktop UI. These boundaries let a
//! persisted task resume without reconstructing an agent from display-only
//! messages.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DEFAULT_MAX_AGENT_STEPS: u32 = 100;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    None,
    #[default]
    Auto,
    Required,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum AgentState {
    #[default]
    Idle,
    Running,
    Finished,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageToolCall {
    pub id: String,
    #[serde(rename = "type", default = "default_function_type")]
    pub call_type: String,
    pub function: FunctionCall,
}

fn default_function_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<MessageToolCall>>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub base64_image: Option<String>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Some(content.into()),
            tool_calls: None,
            name: None,
            tool_call_id: None,
            base64_image: None,
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Some(content.into()),
            tool_calls: None,
            name: None,
            tool_call_id: None,
            base64_image: None,
        }
    }

    pub fn assistant(content: Option<String>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: None,
            name: None,
            tool_call_id: None,
            base64_image: None,
        }
    }

    pub fn assistant_with_tools(content: Option<String>, tool_calls: Vec<MessageToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content,
            tool_calls: Some(tool_calls),
            name: None,
            tool_call_id: None,
            base64_image: None,
        }
    }

    pub fn tool(
        content: impl Into<String>,
        name: impl Into<String>,
        tool_call_id: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: Some(content.into()),
            tool_calls: None,
            name: Some(name.into()),
            tool_call_id: Some(tool_call_id.into()),
            base64_image: None,
        }
    }

    pub fn user_message(content: impl Into<String>) -> Self {
        Self::user(content)
    }

    pub fn system_message(content: impl Into<String>) -> Self {
        Self::system(content)
    }

    pub fn assistant_message(content: Option<String>) -> Self {
        Self::assistant(content)
    }

    pub fn tool_message(
        content: impl Into<String>,
        name: impl Into<String>,
        tool_call_id: impl Into<String>,
    ) -> Self {
        Self::tool(content, name, tool_call_id)
    }

    pub fn from_tool_calls(content: Option<String>, tool_calls: Vec<MessageToolCall>) -> Self {
        Self::assistant_with_tools(content, tool_calls)
    }

    pub fn to_openai_value(&self) -> Value {
        let mut value = json!({
            "role": serde_json::to_string(&self.role)
                .unwrap_or_else(|_| "\"user\"".to_string())
                .trim_matches('"')
        });
        if let Some(content) = &self.content {
            value["content"] = Value::String(content.clone());
        }
        if let Some(tool_calls) = &self.tool_calls {
            value["tool_calls"] = serde_json::to_value(tool_calls).unwrap_or_else(|_| json!([]));
        }
        if let Some(name) = &self.name {
            value["name"] = Value::String(name.clone());
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            value["tool_call_id"] = Value::String(tool_call_id.clone());
        }
        if let Some(image) = &self.base64_image {
            value["base64_image"] = Value::String(image.clone());
        }
        value
    }

    pub fn to_dict(&self) -> Value {
        self.to_openai_value()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Memory {
    #[serde(default)]
    pub messages: VecDeque<Message>,
    #[serde(default = "default_memory_limit")]
    pub max_messages: usize,
}

fn default_memory_limit() -> usize {
    100
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            messages: VecDeque::new(),
            max_messages: default_memory_limit(),
        }
    }
}

impl Memory {
    pub fn add_message(&mut self, message: Message) {
        self.messages.push_back(message);
        self.trim();
    }

    pub fn add_messages<I>(&mut self, messages: I)
    where
        I: IntoIterator<Item = Message>,
    {
        self.messages.extend(messages);
        self.trim();
    }

    pub fn clear(&mut self) {
        self.messages.clear();
    }

    pub fn recent(&self, count: usize) -> impl Iterator<Item = &Message> {
        let skip = self.messages.len().saturating_sub(count);
        self.messages.iter().skip(skip)
    }

    pub fn get_recent_messages(&self, count: usize) -> Vec<Message> {
        self.recent(count).cloned().collect()
    }

    pub fn to_dict_list(&self) -> Vec<Value> {
        self.to_openai_messages()
    }

    pub fn to_openai_messages(&self) -> Vec<Value> {
        self.messages.iter().map(Message::to_openai_value).collect()
    }

    fn trim(&mut self) {
        while self.messages.len() > self.max_messages {
            self.messages.pop_front();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseAgentRuntime {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub next_step_prompt: String,
    #[serde(default)]
    pub state: AgentState,
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    #[serde(default)]
    pub current_step: u32,
    #[serde(default)]
    pub memory: Memory,
    #[serde(default = "default_duplicate_threshold")]
    pub duplicate_threshold: usize,
}

fn default_max_steps() -> u32 {
    DEFAULT_MAX_AGENT_STEPS
}

fn default_duplicate_threshold() -> usize {
    2
}

impl BaseAgentRuntime {
    pub fn new(name: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: String::new(),
            system_prompt: system_prompt.into(),
            next_step_prompt: String::new(),
            state: AgentState::Idle,
            max_steps: default_max_steps(),
            current_step: 0,
            memory: Memory::default(),
            duplicate_threshold: default_duplicate_threshold(),
        }
    }

    pub fn initialize_agent(&mut self) -> &mut Self {
        if self.memory.max_messages == 0 {
            self.memory.max_messages = default_memory_limit();
        }
        if self.max_steps == 0 {
            self.max_steps = default_max_steps();
        }
        self
    }

    pub async fn state_context<F, Fut, T>(
        &mut self,
        new_state: AgentState,
        operation: F,
    ) -> Result<T, String>
    where
        F: FnOnce(&mut Self) -> Fut,
        Fut: Future<Output = Result<T, String>>,
    {
        let previous_state = self.state;
        self.state = new_state;
        let result = operation(self).await;
        if result.is_err() {
            self.state = AgentState::Error;
        }
        self.state = previous_state;
        result
    }

    pub fn begin(&mut self) -> Result<(), String> {
        if self.state != AgentState::Idle {
            return Err(format!("Cannot run agent from state: {:?}", self.state));
        }
        self.state = AgentState::Running;
        Ok(())
    }

    pub fn update_memory(
        &mut self,
        role: Role,
        content: impl Into<String>,
        base64_image: Option<String>,
        name: Option<String>,
        tool_call_id: Option<String>,
    ) {
        let mut message = match role {
            Role::System => Message::system(content),
            Role::User => Message::user(content),
            Role::Assistant => Message::assistant(Some(content.into())),
            Role::Tool => Message::tool(
                content,
                name.unwrap_or_default(),
                tool_call_id.unwrap_or_default(),
            ),
        };
        message.base64_image = base64_image;
        self.memory.add_message(message);
    }

    pub fn messages(&self) -> Vec<Message> {
        self.memory.messages.iter().cloned().collect()
    }

    pub fn replace_messages(&mut self, messages: Vec<Message>) {
        self.memory.messages = messages.into_iter().collect();
        self.memory.trim();
    }

    pub fn next_step(&mut self) -> Result<u32, String> {
        if self.state != AgentState::Running {
            return Err("Agent is not running.".to_string());
        }
        if self.current_step >= self.max_steps {
            self.state = AgentState::Finished;
            return Err(format!("Reached max steps ({})", self.max_steps));
        }
        self.current_step += 1;
        Ok(self.current_step)
    }

    pub fn finish(&mut self) {
        self.state = AgentState::Finished;
    }

    pub fn fail(&mut self) {
        self.state = AgentState::Error;
    }

    pub fn is_stuck(&self) -> bool {
        let Some(last) = self.memory.messages.back() else {
            return false;
        };
        let Some(content) = last.content.as_deref().filter(|value| !value.is_empty()) else {
            return false;
        };
        let duplicates = self
            .memory
            .messages
            .iter()
            .rev()
            .skip(1)
            .filter(|message| {
                message.role == Role::Assistant && message.content.as_deref() == Some(content)
            })
            .count();
        duplicates >= self.duplicate_threshold
    }

    pub fn handle_stuck_state(&mut self) {
        let prompt =
            "Observed duplicate responses. Change strategy and avoid repeating ineffective paths.";
        self.next_step_prompt = if self.next_step_prompt.is_empty() {
            prompt.to_string()
        } else {
            format!("{prompt}\n{}", self.next_step_prompt)
        };
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallAgentRuntime {
    pub base: BaseAgentRuntime,
    #[serde(default)]
    pub tool_choice: ToolChoice,
    #[serde(default)]
    pub special_tool_names: Vec<String>,
    #[serde(default)]
    pub tool_calls: Vec<MessageToolCall>,
    #[serde(default)]
    pub max_observe: Option<usize>,
}

impl ToolCallAgentRuntime {
    pub fn new(name: impl Into<String>, system_prompt: impl Into<String>) -> Self {
        let mut base = BaseAgentRuntime::new(name, system_prompt);
        base.max_steps = DEFAULT_MAX_AGENT_STEPS;
        Self {
            base,
            tool_choice: ToolChoice::Auto,
            special_tool_names: vec!["terminate".to_string()],
            tool_calls: Vec::new(),
            max_observe: None,
        }
    }

    pub fn set_response(
        &mut self,
        content: Option<String>,
        tool_calls: Vec<MessageToolCall>,
    ) -> bool {
        self.tool_calls = tool_calls.clone();
        let should_continue = match self.tool_choice {
            ToolChoice::None => content.as_deref().is_some_and(|value| !value.is_empty()),
            ToolChoice::Required => !tool_calls.is_empty() || content.is_some(),
            ToolChoice::Auto => {
                !tool_calls.is_empty() || content.as_deref().is_some_and(|v| !v.is_empty())
            }
        };
        self.base
            .memory
            .add_message(Message::assistant_with_tools(content, tool_calls));
        should_continue
    }

    pub fn is_special_tool(&self, name: &str) -> bool {
        self.special_tool_names.iter().any(|item| item == name)
    }

    pub fn should_finish_execution(&self, name: &str) -> bool {
        self.is_special_tool(name)
    }

    pub fn observed(&self, output: &str) -> String {
        self.max_observe
            .map(|limit| output.chars().take(limit).collect())
            .unwrap_or_else(|| output.to_string())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentKind {
    Manus,
    Browser,
    DataAnalysis,
    Swe,
    Mcp,
    SandboxManus,
}

impl AgentKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manus => "manus",
            Self::Browser => "browser",
            Self::DataAnalysis => "data_analysis",
            Self::Swe => "swe",
            Self::Mcp => "mcp",
            Self::SandboxManus => "sandbox_manus",
        }
    }
}

pub fn prompt_profile(kind: AgentKind, workspace: &str) -> (String, String) {
    let system = match kind {
        AgentKind::Browser => format!(
            "You are a browser automation agent. Inspect the current URL, tabs, interactive elements, and page content before acting. Use indexed browser actions, verify after state-changing actions, and never invent an element index. Workspace: {workspace}."
        ),
        AgentKind::DataAnalysis => format!(
            "You are a data analysis agent. Inspect the dataset, compute evidence-backed summaries, create a real chart artifact when requested, and state limitations. Workspace: {workspace}."
        ),
        AgentKind::Swe => format!(
            "You are an autonomous programmer working directly in a repository. Use rust_code read/list/glob/grep/status/diff to inspect only the relevant files, use rust_code apply_patch or replace for precise edits, and use rust_bash for tests or build checks. Reproduce the issue, make the smallest verified edit, inspect the diff, and run the narrowest relevant checks before answering. Workspace: {workspace}."
        ),
        AgentKind::Mcp => "You are an MCP agent. Inspect available server tools first, validate arguments against their live schemas, recover from tool errors, and stop when the request is verified.".to_string(),
        AgentKind::SandboxManus => format!(
            "You are a general-purpose agent operating inside a task-scoped sandbox. Keep paths inside the sandbox, use browser, shell, file, and vision tools deliberately, and verify results. Sandbox: {workspace}."
        ),
        AgentKind::Manus => format!(
            "You are RustPilot Manus, an all-capable desktop assistant. Select the smallest useful tool or sequence for programming, retrieval, file processing, browser work, and human approval. Verify every result and never claim an action that has no tool evidence. Workspace: {workspace}."
        ),
    };
    let next = "Based on the current state, select the most appropriate rust_ tool or provide a verified answer. For complex work, keep a concise plan and execute one meaningful action at a time. Use rust_terminate when complete or irrecoverably blocked.";
    (system, next.to_string())
}

pub fn parse_agent_kind(value: &str) -> Option<AgentKind> {
    match value {
        "manus" => Some(AgentKind::Manus),
        "browser" => Some(AgentKind::Browser),
        "data_analysis" => Some(AgentKind::DataAnalysis),
        "swe" => Some(AgentKind::Swe),
        "mcp" => Some(AgentKind::Mcp),
        "sandbox_manus" => Some(AgentKind::SandboxManus),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanningFlowRuntime {
    pub plan_id: String,
    pub current_step_index: Option<usize>,
    #[serde(default)]
    pub executors: HashMap<String, AgentKind>,
}

impl PlanningFlowRuntime {
    pub fn new(plan_id: impl Into<String>) -> Self {
        Self {
            plan_id: plan_id.into(),
            current_step_index: None,
            executors: HashMap::new(),
        }
    }

    pub fn executor_for(&self, marker: Option<&str>, fallback: AgentKind) -> AgentKind {
        marker
            .and_then(|value| self.executors.get(value).copied())
            .or_else(|| self.executors.values().next().copied())
            .unwrap_or(fallback)
    }

    pub fn next_active_step<T, F>(&mut self, steps: &[T], mut active: F) -> Option<usize>
    where
        F: FnMut(&T) -> bool,
    {
        let index = steps.iter().position(&mut active);
        self.current_step_index = index;
        index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_is_bounded_and_replayable() {
        let mut memory = Memory {
            max_messages: 2,
            ..Memory::default()
        };
        memory.add_message(Message::user("one"));
        memory.add_message(Message::assistant_with_tools(
            Some("thinking".to_string()),
            vec![MessageToolCall {
                id: "call-1".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "rust_clock".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
        ));
        memory.add_message(Message::tool("now", "rust_clock", "call-1"));
        assert_eq!(memory.messages.len(), 2);
        assert_eq!(memory.messages[0].role, Role::Assistant);
        assert!(memory.to_openai_messages()[0]["tool_calls"].is_array());
    }

    #[test]
    fn base_agent_detects_duplicate_assistant_responses() {
        let mut agent = BaseAgentRuntime::new("test", "system");
        agent
            .memory
            .add_message(Message::assistant(Some("same".to_string())));
        agent
            .memory
            .add_message(Message::assistant(Some("same".to_string())));
        agent
            .memory
            .add_message(Message::assistant(Some("same".to_string())));
        assert!(agent.is_stuck());
        agent.handle_stuck_state();
        assert!(agent.next_step_prompt.contains("Change strategy"));
    }

    #[tokio::test]
    async fn state_context_restores_state_and_marks_errors() {
        let mut agent = BaseAgentRuntime::new("test", "system");
        agent.initialize_agent();
        assert_eq!(agent.memory.max_messages, 100);
        assert_eq!(agent.max_steps, DEFAULT_MAX_AGENT_STEPS);
        let result = agent
            .state_context(AgentState::Running, |_agent| async { Ok::<_, String>(7) })
            .await
            .unwrap();
        assert_eq!(result, 7);
        assert_eq!(agent.state, AgentState::Idle);

        let error = agent
            .state_context(AgentState::Running, |_agent| async {
                Err::<(), _>("failed".to_string())
            })
            .await
            .unwrap_err();
        assert_eq!(error, "failed");
        assert_eq!(agent.state, AgentState::Idle);
    }

    #[test]
    fn specialized_prompt_profiles_cover_all_agents() {
        for kind in [
            AgentKind::Manus,
            AgentKind::Browser,
            AgentKind::DataAnalysis,
            AgentKind::Swe,
            AgentKind::Mcp,
            AgentKind::SandboxManus,
        ] {
            let (system, next) = prompt_profile(kind, ".");
            assert!(!system.is_empty());
            assert!(next.contains("rust_"));
        }
    }
}
