use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::BufReader;

use crate::{agent, attachments, llm};

pub(crate) fn default_agent_name() -> String {
    "RustPilot Manus".to_string()
}

pub(crate) fn default_agent_kind() -> String {
    "manus".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Planning,
    Executing,
    Verifying,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepPhase {
    Plan,
    Act,
    Verify,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    NotStarted,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMemoryEntry {
    pub id: String,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_opaque: Option<String>,
    pub created_at: i64,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub tool_names: Vec<String>,
    #[serde(default)]
    pub tool_calls: Vec<agent::MessageToolCall>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base64_image: Option<String>,
    #[serde(default)]
    pub attachments: Vec<attachments::AttachmentRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub(crate) struct ContextState {
    #[serde(default)]
    pub(crate) generation: u64,
    #[serde(default)]
    pub(crate) active_compaction_id: Option<String>,
    #[serde(default)]
    pub(crate) last_compaction_id: Option<String>,
    #[serde(default)]
    pub(crate) surface_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPlanStep {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: PlanStepStatus,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentPlan {
    pub id: String,
    pub title: String,
    pub steps: Vec<AgentPlanStep>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolDefinition {
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct BrowserSession {
    pub(crate) current_url: String,
    pub(crate) title: String,
    pub(crate) html: String,
    pub(crate) history: Vec<String>,
    pub(crate) history_index: usize,
    pub(crate) scroll_y: i64,
    pub(crate) typed_values: HashMap<String, String>,
    #[serde(default)]
    pub(crate) tabs: Vec<BrowserTab>,
    #[serde(default)]
    pub(crate) active_tab_id: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct BrowserTab {
    pub(crate) id: usize,
    pub(crate) url: String,
    pub(crate) title: String,
}

pub(crate) struct PersistentShell {
    pub(crate) child: tokio::process::Child,
    pub(crate) stdin: tokio::process::ChildStdin,
    pub(crate) stdout: BufReader<tokio::process::ChildStdout>,
    pub(crate) cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantPart {
    Reasoning {
        id: String,
        start: usize,
        end: usize,
    },
    Text {
        id: String,
        start: usize,
        end: usize,
    },
    Tool {
        id: String,
        index: usize,
        call_id: String,
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskMessage {
    pub id: String,
    pub task_id: String,
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_opaque: Option<String>,
    pub created_at: i64,
    pub streaming: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<AssistantPart>,
    #[serde(default)]
    pub tool_calls: Vec<agent::MessageToolCall>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub base64_image: Option<String>,
    #[serde(default)]
    pub attachments: Vec<attachments::AttachmentRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStep {
    pub id: String,
    pub task_id: String,
    pub phase: StepPhase,
    pub title: String,
    pub detail: Option<String>,
    pub status: StepStatus,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub task_id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_tool_call_id: Option<String>,
    pub status: ToolCallStatus,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_ms: Option<u64>,
    pub result: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub task_id: String,
    pub tool_call_id: String,
    pub status: ToolCallStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub duration_ms: Option<u64>,
}

pub(crate) struct ToolInvocation {
    pub(crate) model_tool_call_id: Option<String>,
    pub(crate) name: String,
    pub(crate) arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub id: String,
    pub task_id: String,
    pub tool_name: String,
    pub reason: String,
    pub details: String,
    pub created_at: i64,
    pub status: String,
    #[serde(default)]
    pub rememberable: bool,
    #[serde(default)]
    pub remember_action: Option<String>,
    #[serde(default)]
    pub remember_pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub prompt: String,
    #[serde(default)]
    pub workspace: String,
    pub status: AgentStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub demo_mode: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<llm::ReasoningEffort>,
    #[serde(default)]
    pub archived: bool,
    #[serde(default = "default_agent_name")]
    pub agent_name: String,
    #[serde(default = "default_agent_kind")]
    pub agent_kind: String,
    pub messages: Vec<TaskMessage>,
    #[serde(default)]
    pub memory: Vec<AgentMemoryEntry>,
    #[serde(default)]
    pub(crate) context: ContextState,
    #[serde(default)]
    pub plans: Vec<AgentPlan>,
    #[serde(default)]
    pub active_plan_id: Option<String>,
    pub steps: Vec<AgentStep>,
    pub tool_calls: Vec<ToolCall>,
    pub approval_requests: Vec<ApprovalRequest>,
    #[serde(default)]
    pub llm_usage: llm::TokenUsage,
    pub final_answer: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub event_seq: i64,
    #[serde(skip)]
    pub(crate) persistence_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub workspace: String,
    pub status: AgentStatus,
    pub updated_at: i64,
    pub demo_mode: bool,
    #[serde(default)]
    pub archived: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSummary {
    pub id: String,
    pub directory: String,
    pub name: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub task_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatusEvent {
    pub task_id: String,
    pub status: AgentStatus,
    pub updated_at: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCompletedEvent {
    pub task_id: String,
    pub final_answer: String,
    pub demo_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskFailedEvent {
    pub task_id: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCancelledEvent {
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlanEvent {
    pub task_id: String,
    pub plan: AgentPlan,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    Confirm,
    Guarded,
}

impl Default for ApprovalMode {
    fn default() -> Self {
        Self::Guarded
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalRule {
    #[serde(default)]
    pub workspace: String,
    pub action: String,
    pub resource: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSettings {
    pub api_base_url: String,
    pub model: String,
    #[serde(skip_serializing, skip_deserializing)]
    pub api_key: Option<String>,
    pub max_steps: u32,
    pub timeout_secs: u64,
    #[serde(default)]
    pub prompt_cache: llm::PromptCacheMode,
    #[serde(default)]
    pub reasoning_effort: Option<llm::ReasoningEffort>,
    #[serde(default)]
    pub approval_mode: ApprovalMode,
    #[serde(default)]
    pub(crate) approval_rules: Vec<ApprovalRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsInput {
    pub api_base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub max_steps: Option<u32>,
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub prompt_cache: Option<llm::PromptCacheMode>,
    #[serde(default)]
    pub approval_mode: Option<ApprovalMode>,
    #[serde(default)]
    pub clear_approval_rules: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsView {
    pub api_base_url: String,
    pub model: String,
    pub api_key_configured: bool,
    pub max_steps: u32,
    pub timeout_secs: u64,
    pub prompt_cache: llm::PromptCacheMode,
    pub approval_mode: ApprovalMode,
    pub remembered_approvals: usize,
    pub demo_mode: bool,
    pub available_tools: Vec<AgentToolDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct PersistedSettings {
    pub(crate) api_base_url: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) max_steps: Option<u32>,
    pub(crate) timeout_secs: Option<u64>,
    pub(crate) prompt_cache: Option<llm::PromptCacheMode>,
    pub(crate) approval_mode: Option<ApprovalMode>,
    #[serde(default)]
    pub(crate) approval_rules: Vec<ApprovalRule>,
}
