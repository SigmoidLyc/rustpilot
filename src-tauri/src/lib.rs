use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicU64,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, State};
use tokio::sync::{oneshot, Mutex as AsyncMutex};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

pub mod agent;
pub mod agents;
pub mod attachments;
pub mod bedrock;
pub mod coding;
pub mod config;
pub mod flow;
pub mod llm;
pub mod mcp_server;
pub mod path_guard;
pub mod protocol;
pub mod react;
pub mod schema;
pub mod task_events;
pub mod task_persistence;
pub mod task_storage;
pub mod tool;

pub(crate) mod browser_tool;
pub(crate) mod computer_tool;
pub(crate) mod data_tool;
pub(crate) mod editor_tool;
pub(crate) mod file_tool;
pub(crate) mod http_tool;
pub(crate) mod mcp_tool;
mod mcp_transport;
mod models;
pub(crate) mod planning_tool;
mod project_store;
pub(crate) mod runtime_tool;
pub(crate) mod shell_tool;
mod tool_catalog;
mod tool_dispatch;
mod tool_policy;
mod tool_registry;

#[cfg(test)]
use browser_tool::{html_links, html_text, html_title};
use browser_tool::{run_browser_tool, run_crawl_tool, run_web_search_tool};
#[cfg(test)]
use models::default_agent_kind;
use models::{
    default_agent_name, BrowserSession, BrowserTab, PersistedSettings, PersistentShell,
    ToolInvocation,
};
pub use models::{
    AgentMemoryEntry, AgentPlan, AgentPlanStep, AgentSettings, AgentStatus, AgentStep,
    AgentToolDefinition, ApprovalMode, ApprovalRequest, ApprovalRule, AssistantPart,
    PlanStepStatus, ProjectSummary, SettingsInput, SettingsView, StepPhase, StepStatus, Task,
    TaskCancelledEvent, TaskCompletedEvent, TaskFailedEvent, TaskMessage, TaskPlanEvent,
    TaskStatusEvent, TaskSummary, ToolCall, ToolCallStatus, ToolResult,
};
#[cfg(test)]
use planning_tool::format_plan;
#[cfg(test)]
use tool_catalog::tool_definitions;
#[cfg(test)]
use tool_policy::is_high_risk;
use tool_policy::{
    approval_details, approval_reason, external_path_requested, needs_approval, rule_for,
    sanitize_rules,
};
#[cfg(test)]
use tool_registry::tool_schema_hash;
use tool_registry::{
    available_tool_views, tool_definitions_for_state, McpToolDefinition, ToolDefinitionCache,
};

const SETTINGS_FILE: &str = "settings.json";
const MAX_OUTPUT_CHARS: usize = 16_000;
const MAX_MEMORY_ENTRIES: usize = 100;
const APPROVAL_TIMEOUT_SECS: u64 = 300;
const API_KEY_REQUIRED_MESSAGE: &str = "Configure an API key in Settings before sending a task.";
const INTERNAL_WORKSPACE_ARGUMENT: &str = "_rustpilot_workspace";
pub(crate) const INTERNAL_ATTACHMENT_READ_PATHS_ARGUMENT: &str = "_rustpilot_attachment_read_paths";

pub(crate) fn ensure_writable_directory(
    preferred: PathBuf,
    fallback_name: &str,
) -> Result<PathBuf, String> {
    let use_fallback = |reason: String| {
        let fallback = env::temp_dir().join("RustPilot").join(fallback_name);
        fs::create_dir_all(&fallback).map_err(|fallback_error| {
            format!(
                "Unable to write to {} ({reason}) or create fallback {} ({fallback_error})",
                preferred.display(),
                fallback.display()
            )
        })?;
        warn!(
            preferred = %preferred.display(),
            fallback = %fallback.display(),
            error = %reason,
            "Using writable fallback directory"
        );
        Ok(fallback)
    };

    if let Err(error) = fs::create_dir_all(&preferred) {
        return use_fallback(error.to_string());
    }

    let probe_path = preferred.join(format!(".rustpilot-write-test-{}", std::process::id()));
    match fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&probe_path)
    {
        Ok(_) => {
            let _ = fs::remove_file(probe_path);
            Ok(preferred)
        }
        Err(primary_error) => use_fallback(primary_error.to_string()),
    }
}

#[cfg(test)]
use task_events::{apply_persisted_stream_event, PersistedStreamEvent};
pub use task_events::{TaskEvent, TaskEventPage};
#[cfg(test)]
use task_persistence::{
    commit_task_writes, insert_stream_event, insert_task_event, merge_pending_task_writes,
    project_task_writes, PendingTaskWrite, PendingTaskWrites, ProjectedTaskChanges,
};
use task_persistence::{PendingTaskEvent, TaskPersistence};
#[cfg(test)]
use task_storage::{
    insert_task_state, legacy_task_backup_path, legacy_task_temp_path, load_legacy_task_records,
    open_task_database, task_database_path, LEGACY_TASK_FILE,
};
use task_storage::{load_task_store, LoadedTaskStore};

#[derive(Clone)]
pub struct AppState {
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    task_persistence: TaskPersistence,
    event_cursors: Arc<RwLock<HashMap<String, i64>>>,
    event_floors: Arc<RwLock<HashMap<String, i64>>>,
    running: Arc<RwLock<HashMap<String, CancellationToken>>>,
    approval_waiters: Arc<RwLock<HashMap<String, ApprovalWaiter>>>,
    settings: Arc<RwLock<AgentSettings>>,
    storage_dir: Arc<RwLock<Option<PathBuf>>>,
    projects: Arc<RwLock<project_store::ProjectStore>>,
    project_store_path: Arc<RwLock<Option<PathBuf>>>,
    persist_lock: Arc<Mutex<()>>,
    edit_history: Arc<Mutex<HashMap<String, VecDeque<String>>>>,
    shell_sessions: Arc<AsyncMutex<HashMap<String, PersistentShell>>>,
    browser_sessions: Arc<Mutex<HashMap<String, BrowserSession>>>,
    mcp_sessions: Arc<AsyncMutex<HashMap<String, mcp_tool::McpSession>>>,
    mcp_tools: Arc<RwLock<HashMap<String, McpToolDefinition>>>,
    mcp_tools_revision: Arc<AtomicU64>,
    tool_definition_cache: ToolDefinitionCache,
}

struct ApprovalWaiter {
    task_id: String,
    sender: oneshot::Sender<bool>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            task_persistence: TaskPersistence::new(),
            event_cursors: Arc::new(RwLock::new(HashMap::new())),
            event_floors: Arc::new(RwLock::new(HashMap::new())),
            running: Arc::new(RwLock::new(HashMap::new())),
            approval_waiters: Arc::new(RwLock::new(HashMap::new())),
            settings: Arc::new(RwLock::new(default_settings())),
            storage_dir: Arc::new(RwLock::new(None)),
            projects: Arc::new(RwLock::new(project_store::ProjectStore::default())),
            project_store_path: Arc::new(RwLock::new(None)),
            persist_lock: Arc::new(Mutex::new(())),
            edit_history: Arc::new(Mutex::new(HashMap::new())),
            shell_sessions: Arc::new(AsyncMutex::new(HashMap::new())),
            browser_sessions: Arc::new(Mutex::new(HashMap::new())),
            mcp_sessions: Arc::new(AsyncMutex::new(HashMap::new())),
            mcp_tools: Arc::new(RwLock::new(HashMap::new())),
            mcp_tools_revision: Arc::new(AtomicU64::new(0)),
            tool_definition_cache: Arc::new(RwLock::new(None)),
        }
    }

    fn initialize(&self, app: &AppHandle) -> Result<(), String> {
        let data_dir = match env::var_os("RUSTPILOT_DATA_DIR") {
            Some(path) => PathBuf::from(path),
            None => app
                .path()
                .app_data_dir()
                .map_err(|error| format!("Unable to locate app data directory: {error}"))?,
        };
        let data_dir = ensure_writable_directory(data_dir, "data")?;

        {
            let mut storage_dir = self
                .storage_dir
                .write()
                .map_err(|_| "Storage lock is poisoned".to_string())?;
            *storage_dir = Some(data_dir.clone());
        }
        let projects_path = data_dir.join(project_store::PROJECTS_FILE);
        let loaded_projects = project_store::ProjectStore::load(&projects_path);
        let migrate_projects = loaded_projects.is_none();
        {
            let mut projects = self
                .projects
                .write()
                .map_err(|_| "Project lock is poisoned".to_string())?;
            *projects = loaded_projects.unwrap_or_default();
        }
        {
            let mut path = self
                .project_store_path
                .write()
                .map_err(|_| "Project storage lock is poisoned".to_string())?;
            *path = Some(projects_path);
        }

        let settings_path = data_dir.join(SETTINGS_FILE);
        let persisted_settings = fs::read_to_string(settings_path)
            .ok()
            .and_then(|contents| serde_json::from_str::<PersistedSettings>(&contents).ok())
            .unwrap_or_default();

        {
            let mut settings = self
                .settings
                .write()
                .map_err(|_| "Settings lock is poisoned".to_string())?;
            if let Some(api_base_url) = persisted_settings.api_base_url {
                if !api_base_url.trim().is_empty() {
                    settings.api_base_url = api_base_url;
                }
            }
            if let Some(model) = persisted_settings.model {
                if !model.trim().is_empty() {
                    settings.model = model;
                }
            }
            if let Some(max_steps) = persisted_settings.max_steps {
                settings.max_steps = normalize_max_steps(max_steps);
            }
            if let Some(timeout_secs) = persisted_settings.timeout_secs {
                settings.timeout_secs = timeout_secs.clamp(5, 120);
            }
            if let Some(prompt_cache) = persisted_settings.prompt_cache {
                settings.prompt_cache = prompt_cache;
            }
            if let Some(approval_mode) = persisted_settings.approval_mode {
                settings.approval_mode = approval_mode;
            }
            settings.approval_rules = sanitize_rules(persisted_settings.approval_rules);
            if let Some(api_base_url) = first_env_value(&["RUSTPILOT_API_BASE_URL"]) {
                settings.api_base_url = api_base_url.trim_end_matches('/').to_string();
            }
            if let Some(model) = first_env_value(&["RUSTPILOT_MODEL"]) {
                settings.model = model;
            }
            if let Some(prompt_cache) = first_env_value(&["RUSTPILOT_PROMPT_CACHE"])
                .and_then(|value| llm::PromptCacheMode::parse(&value))
            {
                settings.prompt_cache = prompt_cache;
            }
            settings.api_key = first_env_value(&["RUSTPILOT_API_KEY", "OPENAI_API_KEY"]);
        }

        let LoadedTaskStore {
            tasks: mut loaded_tasks,
            event_bytes,
            event_cursors,
            event_floors,
            connection,
        } = load_task_store(&data_dir)?;
        {
            let mut cursors = self
                .event_cursors
                .write()
                .map_err(|_| "Task event cursor lock is poisoned".to_string())?;
            *cursors = event_cursors.clone();
        }
        {
            let mut floors = self
                .event_floors
                .write()
                .map_err(|_| "Task event floor lock is poisoned".to_string())?;
            *floors = event_floors.clone();
        }
        let durable_tasks = loaded_tasks.clone();
        let mut repaired_task_ids = Vec::new();
        {
            let mut tasks = self
                .tasks
                .write()
                .map_err(|_| "Task lock is poisoned".to_string())?;
            for (task_id, task) in loaded_tasks.iter_mut() {
                let mut changed = repair_task_record(task);
                if matches!(
                    task.status,
                    AgentStatus::Planning
                        | AgentStatus::Executing
                        | AgentStatus::Verifying
                        | AgentStatus::WaitingApproval
                ) {
                    task.status = AgentStatus::Failed;
                    task.error =
                        Some("The task was interrupted before the app closed.".to_string());
                    touch_task(task);
                    changed = true;
                }
                if changed {
                    repaired_task_ids.push(task_id.clone());
                }
                tasks.insert(task.id.clone(), task.clone());
            }
        }

        {
            let mut projects = self
                .projects
                .write()
                .map_err(|_| "Project lock is poisoned".to_string())?;
            let mut changed = false;
            if migrate_projects {
                // Pre-project-store task history had no explicit open/closed state.
                // Seed it once, then preserve user close decisions across restarts.
                let timestamp = now();
                let current = workspace_root();
                if current.is_dir() {
                    changed |= projects.open(&current, timestamp);
                }
                let mut task_workspaces = loaded_tasks
                    .values()
                    .filter(|task| Path::new(&task.workspace).is_dir())
                    .collect::<Vec<_>>();
                task_workspaces.sort_by_key(|task| Reverse(task.updated_at));
                for task in task_workspaces {
                    changed |= projects.open(Path::new(&task.workspace), timestamp);
                }
            }
            drop(projects);
            if changed {
                self.persist_projects()?;
            }
        }

        self.task_persistence.start(
            connection,
            durable_tasks,
            event_bytes,
            app.clone(),
            Arc::clone(&self.event_cursors),
            Arc::clone(&self.event_floors),
        )?;
        for task_id in repaired_task_ids {
            self.persist_task(&task_id)?;
        }
        info!("RustPilot state initialized");
        Ok(())
    }

    fn persist_task(&self, task_id: &str) -> Result<(), String> {
        let tasks = self
            .tasks
            .read()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get(task_id)
            .cloned()
            .ok_or_else(|| "Task not found".to_string())?;
        self.task_persistence.enqueue_upsert(task)
    }

    fn persist_stream_event(
        &self,
        task_id: &str,
        message_id: &str,
        revision: u64,
        event: &llm::StreamEvent,
    ) -> Result<(), String> {
        self.task_persistence
            .enqueue_stream(task_id, message_id, revision, event)
    }

    fn persist_task_event<T: Serialize>(
        &self,
        task_id: &str,
        event: &str,
        payload: T,
    ) -> Result<(), String> {
        let revision = self
            .tasks
            .read()
            .map_err(|_| "Task lock is poisoned".to_string())?
            .get(task_id)
            .map(|task| task.persistence_revision)
            .ok_or_else(|| "Task not found".to_string())?;
        let payload = serde_json::to_value(payload)
            .map_err(|error| format!("Unable to encode task event {event}: {error}"))?;
        self.task_persistence
            .enqueue_task_event(task_id, revision, event.to_string(), payload)
    }

    fn persist_deleted_task(
        &self,
        task_id: &str,
        revision: u64,
        summary: TaskSummary,
    ) -> Result<(), String> {
        let payload = serde_json::to_value(summary)
            .map_err(|error| format!("Unable to encode deleted task event: {error}"))?;
        self.task_persistence.enqueue_delete(
            task_id,
            revision,
            PendingTaskEvent::Task {
                revision,
                event: "task_deleted".to_string(),
                payload,
            },
        )
    }

    fn persist_settings(&self) -> Result<(), String> {
        let data_dir = self
            .storage_dir
            .read()
            .map_err(|_| "Storage lock is poisoned".to_string())?
            .clone();
        let Some(data_dir) = data_dir else {
            return Ok(());
        };
        let _guard = self
            .persist_lock
            .lock()
            .map_err(|_| "Persistence lock is poisoned".to_string())?;
        let settings = self
            .settings
            .read()
            .map_err(|_| "Settings lock is poisoned".to_string())?;
        let safe_settings = PersistedSettings {
            api_base_url: Some(settings.api_base_url.clone()),
            model: Some(settings.model.clone()),
            max_steps: Some(settings.max_steps),
            timeout_secs: Some(settings.timeout_secs),
            prompt_cache: Some(settings.prompt_cache),
            approval_mode: Some(settings.approval_mode),
            approval_rules: settings.approval_rules.clone(),
        };
        let contents = serde_json::to_string_pretty(&safe_settings)
            .map_err(|error| format!("Unable to encode settings: {error}"))?;
        fs::write(data_dir.join(SETTINGS_FILE), contents)
            .map_err(|error| format!("Unable to persist settings: {error}"))
    }

    fn persist_projects(&self) -> Result<(), String> {
        let path = self
            .project_store_path
            .read()
            .map_err(|_| "Project storage lock is poisoned".to_string())?
            .clone();
        let Some(path) = path else {
            return Ok(());
        };
        let projects = self
            .projects
            .read()
            .map_err(|_| "Project lock is poisoned".to_string())?
            .clone();
        let _guard = self
            .persist_lock
            .lock()
            .map_err(|_| "Persistence lock is poisoned".to_string())?;
        project_store::persist(&path, &projects)
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
enum AgentError {
    Cancelled,
    Message(String),
}

impl From<String> for AgentError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

fn default_settings() -> AgentSettings {
    AgentSettings {
        api_base_url: "https://api.openai.com/v1".to_string(),
        model: "gpt-4o-mini".to_string(),
        api_key: None,
        max_steps: agent::DEFAULT_MAX_AGENT_STEPS,
        timeout_secs: 45,
        prompt_cache: llm::PromptCacheMode::Auto,
        approval_mode: ApprovalMode::Guarded,
        approval_rules: Vec::new(),
    }
}

fn normalize_max_steps(max_steps: u32) -> u32 {
    max_steps.max(1)
}

fn first_env_value(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        env::var(name)
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
    })
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn mark_task_revision(task: &mut Task) {
    task.persistence_revision = task.persistence_revision.saturating_add(1);
}

fn touch_task(task: &mut Task) {
    mark_task_revision(task);
    task.updated_at = now();
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4())
}

fn make_title(prompt: &str) -> String {
    let normalized = prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title: String = normalized.chars().take(56).collect();
    if normalized.chars().count() > 56 {
        title.push_str("...");
    }
    if title.is_empty() {
        "New task".to_string()
    } else {
        title
    }
}

fn infer_agent_kind(prompt: &str) -> String {
    let lower = prompt.to_lowercase();
    if lower.contains("\u{6570}\u{636e}")
        || lower.contains("\u{5206}\u{6790}")
        || lower.contains("\u{56fe}\u{8868}")
        || lower.contains("\u{7edf}\u{8ba1}")
    {
        return "data_analysis".to_string();
    }
    if lower.contains("\u{4ee3}\u{7801}")
        || lower.contains("\u{4fee}\u{590d}")
        || lower.contains("\u{6d4b}\u{8bd5}")
        || lower.contains("\u{9519}\u{8bef}")
    {
        return "swe".to_string();
    }
    if lower.contains("\u{6d4f}\u{89c8}\u{5668}")
        || lower.contains("\u{7f51}\u{9875}")
        || lower.contains("\u{7f51}\u{7ad9}")
    {
        return "browser".to_string();
    }
    if lower.contains("csv") || lower.contains("data") || lower.contains("chart") {
        return "data_analysis".to_string();
    }
    if lower.contains("code") || lower.contains("bug") {
        return "swe".to_string();
    }
    if lower.contains("browser") || lower.contains("website") {
        return "browser".to_string();
    }
    if lower.contains("mcp") {
        return "mcp".to_string();
    }
    "manus".to_string()
}

fn emit_task_event<T: Serialize>(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    event: &str,
    payload: T,
) {
    let _ = app;
    if let Err(error) = state.persist_task_event(task_id, event, payload) {
        warn!(task_id = %task_id, event, "Unable to queue task event: {error}");
    }
}

fn queue_current_plan(state: &AppState, task_id: &str) -> Result<(), String> {
    let plan = state
        .tasks
        .read()
        .ok()
        .and_then(|tasks| tasks.get(task_id).cloned())
        .and_then(|task| {
            task.active_plan_id
                .as_deref()
                .and_then(|id| task.plans.iter().find(|plan| plan.id == id).cloned())
        });
    if let Some(plan) = plan {
        state.persist_task_event(
            task_id,
            "task_plan",
            TaskPlanEvent {
                task_id: task_id.to_string(),
                plan,
            },
        )?;
    }
    Ok(())
}

fn emit_current_plan(app: &AppHandle, state: &AppState, task_id: &str) {
    let _ = app;
    if let Err(error) = queue_current_plan(state, task_id) {
        warn!(task_id = %task_id, "Unable to queue task plan event: {error}");
    }
}

fn task_snapshot(state: &AppState, task_id: &str) -> Result<Task, String> {
    let mut task = state
        .tasks
        .read()
        .map_err(|_| "Task lock is poisoned".to_string())?
        .get(task_id)
        .cloned()
        .ok_or_else(|| "Task not found".to_string())?;
    if let Some(cursor) = state
        .event_cursors
        .read()
        .map_err(|_| "Task event cursor lock is poisoned".to_string())?
        .get(task_id)
        .copied()
    {
        task.event_seq = task.event_seq.max(cursor);
    }
    Ok(task)
}

fn api_key_configured(state: &AppState) -> Result<bool, String> {
    Ok(state
        .settings
        .read()
        .map_err(|_| "Settings lock is poisoned".to_string())?
        .api_key
        .is_some())
}

fn memory_from_task_messages(messages: &[TaskMessage]) -> Vec<AgentMemoryEntry> {
    messages
        .iter()
        .filter(|message| {
            matches!(message.role.as_str(), "user" | "assistant" | "tool")
                && !(message.role == "assistant"
                    && message.content.trim().is_empty()
                    && message.tool_calls.is_empty())
        })
        .map(|message| AgentMemoryEntry {
            id: message.id.clone(),
            role: message.role.clone(),
            content: message.content.clone(),
            created_at: message.created_at,
            tool_call_id: message.tool_call_id.clone(),
            tool_names: message
                .name
                .as_ref()
                .map(|name| vec![name.clone()])
                .unwrap_or_default(),
            tool_calls: message.tool_calls.clone(),
            name: message.name.clone(),
            base64_image: message.base64_image.clone(),
            attachments: message.attachments.clone(),
        })
        .collect()
}

fn recovered_tool_result(call: &agent::MessageToolCall) -> AgentMemoryEntry {
    AgentMemoryEntry {
        id: new_id("memory"),
        role: "tool".to_string(),
        content: format!(
            "[RustPilot] No result was recorded for the previous `{}` call because that run ended before the tool completed. Treat it as unsuccessful and continue from the available evidence.",
            call.function.name
        ),
        created_at: now(),
        tool_call_id: Some(call.id.clone()),
        tool_names: vec![call.function.name.clone()],
        tool_calls: Vec::new(),
        name: Some(call.function.name.clone()),
        base64_image: None,
        attachments: Vec::new(),
    }
}

fn normalize_memory_entries(entries: &[AgentMemoryEntry]) -> (Vec<AgentMemoryEntry>, bool) {
    let mut normalized = Vec::new();
    let mut changed = false;
    let mut index = 0;

    while index < entries.len() {
        let entry = &entries[index];
        match entry.role.as_str() {
            "user" => {
                normalized.push(entry.clone());
                index += 1;
            }
            "assistant" => {
                let mut calls = Vec::new();
                let mut call_ids = HashSet::new();
                for call in &entry.tool_calls {
                    if call.id.trim().is_empty()
                        || call.function.name.trim().is_empty()
                        || !call_ids.insert(call.id.clone())
                    {
                        changed = true;
                        continue;
                    }
                    calls.push(call.clone());
                }

                let mut assistant = entry.clone();
                if assistant.tool_calls != calls {
                    assistant.tool_calls = calls.clone();
                    changed = true;
                }
                normalized.push(assistant);
                index += 1;

                if calls.is_empty() {
                    continue;
                }

                let mut candidates = Vec::new();
                while index < entries.len() && entries[index].role == "tool" {
                    candidates.push(entries[index].clone());
                    index += 1;
                }

                let mut used_candidates = HashSet::new();
                for call in &calls {
                    let exact =
                        candidates
                            .iter()
                            .enumerate()
                            .find(|(candidate_index, candidate)| {
                                !used_candidates.contains(candidate_index)
                                    && candidate.tool_call_id.as_deref() == Some(call.id.as_str())
                            });
                    let named =
                        candidates
                            .iter()
                            .enumerate()
                            .find(|(candidate_index, candidate)| {
                                !used_candidates.contains(candidate_index)
                                    && (candidate.name.as_deref()
                                        == Some(call.function.name.as_str())
                                        || candidate
                                            .tool_names
                                            .iter()
                                            .any(|name| name == &call.function.name))
                            });
                    let positional = (candidates.len() == calls.len()).then(|| {
                        candidates
                            .iter()
                            .enumerate()
                            .find(|(candidate_index, _)| !used_candidates.contains(candidate_index))
                    });
                    let matched = exact.or(named).or_else(|| positional.flatten());

                    if let Some((candidate_index, candidate)) = matched {
                        used_candidates.insert(candidate_index);
                        let mut result = candidate.clone();
                        if result.tool_call_id.as_deref() != Some(call.id.as_str()) {
                            result.tool_call_id = Some(call.id.clone());
                            changed = true;
                        }
                        normalized.push(result);
                    } else {
                        normalized.push(recovered_tool_result(call));
                        changed = true;
                    }
                }
                if used_candidates.len() != candidates.len() {
                    changed = true;
                }
            }
            "tool" => {
                // A tool response without its assistant request cannot be replayed safely.
                changed = true;
                index += 1;
            }
            _ => {
                changed = true;
                index += 1;
            }
        }
    }

    (normalized, changed)
}

fn split_memory_turns(entries: &[AgentMemoryEntry]) -> Vec<Vec<AgentMemoryEntry>> {
    let mut turns = Vec::new();
    let mut current = Vec::new();
    for entry in entries {
        if entry.role == "user" && !current.is_empty() {
            turns.push(std::mem::take(&mut current));
        }
        current.push(entry.clone());
    }
    if !current.is_empty() {
        turns.push(current);
    }
    turns
}

fn reduce_latest_turn(turn: &[AgentMemoryEntry], max_entries: usize) -> Vec<AgentMemoryEntry> {
    if turn.len() <= max_entries {
        return turn.to_vec();
    }
    let user_count = turn.iter().take_while(|entry| entry.role == "user").count();
    if user_count == 0 {
        return Vec::new();
    }
    if user_count >= max_entries {
        return vec![turn[user_count - 1].clone()];
    }

    let mut blocks = Vec::new();
    let mut current = Vec::new();
    for entry in &turn[user_count..] {
        if entry.role == "assistant" && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        current.push(entry.clone());
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let mut selected = Vec::new();
    let mut used = user_count;
    for block in blocks.iter().rev() {
        if used + block.len() > max_entries {
            break;
        }
        selected.push(block.clone());
        used += block.len();
    }
    selected.reverse();

    let mut result = turn[..user_count].to_vec();
    for block in selected {
        result.extend(block);
    }
    result
}

fn trim_memory_to_budget(entries: &mut Vec<AgentMemoryEntry>, max_entries: usize) {
    if max_entries == 0 {
        entries.clear();
        return;
    }
    if entries.len() <= max_entries {
        return;
    }

    let turns = split_memory_turns(entries);
    let mut selected_turns = Vec::new();
    let mut used = 0;
    for turn in turns.iter().rev() {
        if turn.len() <= max_entries.saturating_sub(used) {
            used += turn.len();
            selected_turns.push(turn.clone());
            continue;
        }
        if selected_turns.is_empty() {
            selected_turns.push(reduce_latest_turn(turn, max_entries));
        }
        break;
    }
    selected_turns.reverse();
    *entries = selected_turns.into_iter().flatten().collect();
}

fn normalize_memory_for_context(entries: &[AgentMemoryEntry]) -> (Vec<AgentMemoryEntry>, bool) {
    let (normalized, mut changed) = normalize_memory_entries(entries);
    let mut bounded = normalized.clone();
    trim_memory_to_budget(&mut bounded, MAX_MEMORY_ENTRIES);
    if bounded != normalized {
        changed = true;
    }
    (bounded, changed)
}

fn recovered_tool_result_count(entries: &[AgentMemoryEntry]) -> usize {
    entries
        .iter()
        .filter(|entry| {
            entry.role == "tool"
                && entry
                    .content
                    .starts_with("[RustPilot] No result was recorded")
        })
        .count()
}

fn clear_streaming_message_flags(messages: &mut [TaskMessage]) -> bool {
    let mut changed = false;
    for message in messages {
        if message.streaming {
            message.streaming = false;
            changed = true;
        }
    }
    changed
}

fn repair_task_record(task: &mut Task) -> bool {
    let mut changed = clear_streaming_message_flags(&mut task.messages);
    let workspace = normalized_task_workspace(&task.workspace);
    if task.workspace != workspace {
        task.workspace = workspace;
        changed = true;
    }
    let source = if task.memory.is_empty() {
        let recovered = memory_from_task_messages(&task.messages);
        if recovered.is_empty() && !task.prompt.trim().is_empty() {
            vec![AgentMemoryEntry {
                id: new_id("memory"),
                role: "user".to_string(),
                content: task.prompt.clone(),
                created_at: task.created_at,
                tool_call_id: None,
                tool_names: Vec::new(),
                tool_calls: Vec::new(),
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            }]
        } else {
            recovered
        }
    } else {
        task.memory.clone()
    };
    let (mut memory, memory_changed) = normalize_memory_for_context(&source);
    changed |= memory_changed;
    // A crash can persist the display-side tool result just before the model
    // memory write. Prefer that evidence when it repairs more interrupted calls.
    if !task.messages.is_empty() && recovered_tool_result_count(&memory) > 0 {
        let display_source = memory_from_task_messages(&task.messages);
        let (display_memory, _) = normalize_memory_for_context(&display_source);
        if recovered_tool_result_count(&display_memory) < recovered_tool_result_count(&memory) {
            memory = display_memory;
            changed = true;
        }
    }
    if changed || memory != task.memory {
        task.memory = memory;
        touch_task(task);
        true
    } else {
        false
    }
}

fn normalized_task_workspace(raw: &str) -> String {
    let fallback = workspace_root();
    let requested = if raw.trim().is_empty() {
        fallback
    } else {
        PathBuf::from(raw)
    };
    let resolved = requested
        .canonicalize()
        .ok()
        .filter(|path| path.is_dir())
        .unwrap_or_else(workspace_root);
    project_store::display_directory(&resolved)
}

fn task_workspace(task: &Task) -> PathBuf {
    PathBuf::from(&task.workspace)
}

pub(crate) fn workspace_root_for_arguments(arguments: &Value) -> PathBuf {
    arguments
        .get(INTERNAL_WORKSPACE_ARGUMENT)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(workspace_root)
}

fn task_attachment_read_paths(state: &AppState, task: &Task) -> Result<Vec<String>, String> {
    let attachments = task
        .messages
        .iter()
        .flat_map(|message| message.attachments.iter())
        .collect::<Vec<_>>();
    if attachments.is_empty() {
        return Ok(Vec::new());
    }
    let data_dir = attachment_data_directory(state)?;
    attachments
        .into_iter()
        .map(|attachment| {
            attachments::read_path(&data_dir, attachment)
                .map(|path| path.to_string_lossy().into_owned())
        })
        .collect()
}

fn with_task_workspace(
    arguments: &Value,
    workspace: &Path,
    attachment_read_paths: &[String],
) -> Value {
    let mut enriched = arguments.clone();
    if let Some(object) = enriched.as_object_mut() {
        object.insert(
            INTERNAL_WORKSPACE_ARGUMENT.to_string(),
            Value::String(workspace.display().to_string()),
        );
        object.remove(INTERNAL_ATTACHMENT_READ_PATHS_ARGUMENT);
        if !attachment_read_paths.is_empty() {
            object.insert(
                INTERNAL_ATTACHMENT_READ_PATHS_ARGUMENT.to_string(),
                Value::Array(
                    attachment_read_paths
                        .iter()
                        .cloned()
                        .map(Value::String)
                        .collect(),
                ),
            );
        }
    }
    enriched
}

fn repair_task_memory(state: &AppState, task_id: &str) -> Result<Vec<AgentMemoryEntry>, String> {
    let (memory, changed) = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        let changed = repair_task_record(task);
        (task.memory.clone(), changed)
    };
    if changed {
        state.persist_task(task_id)?;
    }
    Ok(memory)
}

fn record_memory(
    state: &AppState,
    task_id: &str,
    role: &str,
    content: String,
    tool_call_id: Option<String>,
    tool_names: Vec<String>,
) -> Result<(), String> {
    record_memory_full(
        state,
        task_id,
        MemoryRecord {
            role: role.to_string(),
            content,
            tool_call_id,
            tool_names,
            tool_calls: Vec::new(),
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        },
    )
}

struct MemoryRecord {
    role: String,
    content: String,
    tool_call_id: Option<String>,
    tool_names: Vec<String>,
    tool_calls: Vec<agent::MessageToolCall>,
    name: Option<String>,
    base64_image: Option<String>,
    attachments: Vec<attachments::AttachmentRef>,
}

fn record_memory_full(state: &AppState, task_id: &str, record: MemoryRecord) -> Result<(), String> {
    let mut tasks = state
        .tasks
        .write()
        .map_err(|_| "Task lock is poisoned".to_string())?;
    let task = tasks
        .get_mut(task_id)
        .ok_or_else(|| "Task not found".to_string())?;
    task.memory.push(AgentMemoryEntry {
        id: new_id("memory"),
        role: record.role,
        content: record.content,
        created_at: now(),
        tool_call_id: record.tool_call_id,
        tool_names: record.tool_names,
        tool_calls: record.tool_calls,
        name: record.name,
        base64_image: record.base64_image,
        attachments: record.attachments,
    });
    trim_memory_to_budget(&mut task.memory, MAX_MEMORY_ENTRIES);
    touch_task(task);
    drop(tasks);
    state.persist_task(task_id)
}

fn ensure_default_plan(state: &AppState, task_id: &str) -> Result<AgentPlan, String> {
    let mut tasks = state
        .tasks
        .write()
        .map_err(|_| "Task lock is poisoned".to_string())?;
    let task = tasks
        .get_mut(task_id)
        .ok_or_else(|| "Task not found".to_string())?;
    if let Some(active_id) = task.active_plan_id.clone() {
        if let Some(plan) = task.plans.iter().find(|plan| plan.id == active_id) {
            return Ok(plan.clone());
        }
    }
    let titles = match task.agent_kind.as_str() {
        "data_analysis" => [
            "Inspect the dataset and define the analysis",
            "Compute summaries and prepare a visualization",
            "Verify the evidence and report limitations",
        ],
        "swe" => [
            "Inspect the repository and reproduce the issue",
            "Implement the smallest verified change",
            "Run checks and report the result",
        ],
        "browser" => [
            "Open the relevant pages and gather evidence",
            "Act on the selected page state",
            "Verify sources and summarize findings",
        ],
        _ => [
            "Understand the request and choose an execution path",
            "Use the smallest useful tools to gather evidence",
            "Verify tool results and compose the answer",
        ],
    };
    let plan = AgentPlan {
        id: new_id("plan"),
        title: format!("{} execution plan", task.agent_name),
        steps: titles
            .iter()
            .map(|title| AgentPlanStep {
                id: new_id("plan_step"),
                title: (*title).to_string(),
                description: (*title).to_string(),
                status: PlanStepStatus::NotStarted,
                notes: String::new(),
            })
            .collect(),
        created_at: now(),
        updated_at: now(),
    };
    task.active_plan_id = Some(plan.id.clone());
    task.plans.push(plan.clone());
    touch_task(task);
    drop(tasks);
    state.persist_task(task_id)?;
    queue_current_plan(state, task_id)?;
    Ok(plan)
}

fn set_plan_step_status(
    state: &AppState,
    task_id: &str,
    index: usize,
    status: PlanStepStatus,
    note: Option<String>,
) -> Result<(), String> {
    let mut tasks = state
        .tasks
        .write()
        .map_err(|_| "Task lock is poisoned".to_string())?;
    let task = tasks
        .get_mut(task_id)
        .ok_or_else(|| "Task not found".to_string())?;
    let Some(active_id) = task.active_plan_id.clone() else {
        return Ok(());
    };
    if let Some(plan) = task.plans.iter_mut().find(|plan| plan.id == active_id) {
        if let Some(step) = plan.steps.get_mut(index) {
            step.status = status;
            if let Some(note) = note {
                step.notes = note;
            }
            plan.updated_at = now();
            touch_task(task);
        }
    }
    drop(tasks);
    state.persist_task(task_id)?;
    queue_current_plan(state, task_id)
}

fn set_status(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    status: AgentStatus,
    error: Option<String>,
) -> Result<(), String> {
    let event = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.status = status.clone();
        touch_task(task);
        task.error = error.clone();
        TaskStatusEvent {
            task_id: task_id.to_string(),
            status,
            updated_at: task.updated_at,
            error,
        }
    };
    state.persist_task(task_id)?;
    emit_task_event(app, state, task_id, "task_status", event);
    emit_current_plan(app, state, task_id);
    Ok(())
}

fn set_final_answer(state: &AppState, task_id: &str, answer: String) -> Result<(), String> {
    {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.final_answer = Some(answer);
        touch_task(task);
    }
    state.persist_task(task_id)
}

fn add_message(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    role: &str,
    content: String,
    streaming: bool,
) -> Result<TaskMessage, String> {
    let message = TaskMessage {
        id: new_id("msg"),
        task_id: task_id.to_string(),
        role: role.to_string(),
        content,
        created_at: now(),
        streaming,
        parts: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
        base64_image: None,
        attachments: Vec::new(),
    };
    {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.messages.push(message.clone());
        touch_task(task);
    }
    state.persist_task(task_id)?;
    emit_task_event(app, state, task_id, "task_message", message.clone());
    Ok(message)
}

fn add_tool_message(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    name: &str,
    model_tool_call_id: &str,
    content: String,
) -> Result<TaskMessage, String> {
    let message = TaskMessage {
        id: new_id("msg"),
        task_id: task_id.to_string(),
        role: "tool".to_string(),
        content,
        created_at: now(),
        streaming: false,
        parts: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: Some(model_tool_call_id.to_string()),
        name: Some(name.to_string()),
        base64_image: None,
        attachments: Vec::new(),
    };
    {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.messages.push(message.clone());
        touch_task(task);
    }
    state.persist_task(task_id)?;
    emit_task_event(app, state, task_id, "task_message", message.clone());
    Ok(message)
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

fn rebuild_assistant_parts(message: &mut TaskMessage) {
    if message.role != "assistant" {
        return;
    }
    let mut parts = Vec::with_capacity(
        (if message.content.is_empty() { 0 } else { 1 }) + message.tool_calls.len(),
    );
    let content_end = utf16_len(&message.content);
    if content_end > 0 {
        parts.push(AssistantPart::Text {
            id: format!("{}:text", message.id),
            start: 0,
            end: content_end,
        });
    }
    for (index, call) in message.tool_calls.iter().enumerate() {
        parts.push(AssistantPart::Tool {
            id: format!("{}:tool:{index}", message.id),
            index,
            call_id: call.id.clone(),
            name: call.function.name.clone(),
        });
    }
    message.parts = parts;
}

fn ensure_assistant_parts(message: &mut TaskMessage) {
    if message.parts.is_empty() {
        rebuild_assistant_parts(message);
    }
}

fn apply_stream_event(message: &mut TaskMessage, event: &llm::StreamEvent) {
    match event {
        llm::StreamEvent::TextDelta(delta) if !delta.is_empty() => {
            ensure_assistant_parts(message);
            let start = match message.parts.last() {
                Some(AssistantPart::Text { end, .. }) => *end,
                _ => utf16_len(&message.content),
            };
            let end = start + utf16_len(delta);
            message.content.push_str(delta);
            if let Some(AssistantPart::Text { end: part_end, .. }) = message.parts.last_mut() {
                if *part_end == start {
                    *part_end = end;
                    return;
                }
            }
            message.parts.push(AssistantPart::Text {
                id: new_id("part"),
                start,
                end,
            });
        }
        llm::StreamEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments: _,
        } => {
            ensure_assistant_parts(message);
            let call_id = id
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("")
                .to_string();
            let part = message.parts.iter_mut().find(|part| {
                matches!(part, AssistantPart::Tool { index: part_index, .. } if part_index == index)
            });
            if let Some(AssistantPart::Tool {
                call_id: current_call_id,
                name: current_name,
                ..
            }) = part
            {
                if !call_id.is_empty() {
                    if current_call_id.starts_with("stream:") {
                        *current_call_id = call_id;
                    } else if current_call_id.as_str() != call_id
                        && !current_call_id.ends_with(call_id.as_str())
                    {
                        current_call_id.push_str(&call_id);
                    }
                }
                if let Some(name) = name.as_deref().filter(|value| !value.is_empty()) {
                    current_name.push_str(name);
                }
                return;
            }
            message.parts.push(AssistantPart::Tool {
                id: new_id("part"),
                index: *index,
                call_id: if call_id.is_empty() {
                    format!("stream:{index}")
                } else {
                    call_id
                },
                name: name.clone().unwrap_or_default(),
            });
        }
        _ => {}
    }
}

#[cfg(test)]
fn append_stream_message(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    message_id: &str,
    delta: &str,
) -> Result<(), String> {
    append_stream_event(
        app,
        state,
        task_id,
        message_id,
        &llm::StreamEvent::TextDelta(delta.to_string()),
    )
}

fn append_stream_event(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    message_id: &str,
    event: &llm::StreamEvent,
) -> Result<(), String> {
    if matches!(event, llm::StreamEvent::TextDelta(delta) if delta.is_empty()) {
        return Ok(());
    }
    let (message, revision) = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        let message =
            if let Some(message) = task.messages.iter_mut().find(|item| item.id == message_id) {
                message.streaming = true;
                apply_stream_event(message, event);
                message.clone()
            } else {
                let mut message = TaskMessage {
                    id: message_id.to_string(),
                    task_id: task_id.to_string(),
                    role: "assistant".to_string(),
                    content: String::new(),
                    created_at: now(),
                    streaming: true,
                    parts: Vec::new(),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    name: None,
                    base64_image: None,
                    attachments: Vec::new(),
                };
                apply_stream_event(&mut message, event);
                task.messages.push(message.clone());
                message
            };
        mark_task_revision(task);
        (message, task.persistence_revision)
    };
    state.persist_stream_event(task_id, message_id, revision, event)?;
    let _ = app;
    let _ = message;
    Ok(())
}

fn finish_stream_message(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    message_id: &str,
) -> Result<(), String> {
    let message = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        if let Some(message) = task.messages.iter_mut().find(|item| item.id == message_id) {
            message.streaming = false;
            let snapshot = message.clone();
            mark_task_revision(task);
            Some(snapshot)
        } else {
            None
        }
    };
    state.persist_task(task_id)?;
    if let Some(message) = message {
        emit_task_event(app, state, task_id, "task_message", message);
    }
    Ok(())
}

fn attach_tool_calls_to_last_message(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    message_id: &str,
    calls: &[ChatToolCall],
) -> Result<(), String> {
    if calls.is_empty() {
        return Ok(());
    }
    let memory_calls = calls
        .iter()
        .map(|call| agent::MessageToolCall {
            id: call.id.clone(),
            call_type: call.call_type.clone(),
            function: agent::FunctionCall {
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            },
        })
        .collect::<Vec<_>>();
    let message = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        let message = if let Some(message) =
            task.messages.iter_mut().find(|item| item.id == message_id)
        {
            message.tool_calls = memory_calls;
            if message.parts.is_empty() {
                rebuild_assistant_parts(message);
            } else {
                for (index, call) in calls.iter().enumerate() {
                    let part = message.parts.iter_mut().find(|part| {
                        matches!(part, AssistantPart::Tool { index: part_index, .. } if *part_index == index)
                    });
                    if let Some(AssistantPart::Tool { call_id, name, .. }) = part {
                        *call_id = call.id.clone();
                        *name = call.function.name.clone();
                    } else {
                        let part_id = format!("{}:tool:{index}", message.id);
                        message.parts.push(AssistantPart::Tool {
                            id: part_id,
                            index,
                            call_id: call.id.clone(),
                            name: call.function.name.clone(),
                        });
                    }
                }
            }
            message.clone()
        } else {
            let mut message = TaskMessage {
                id: message_id.to_string(),
                task_id: task_id.to_string(),
                role: "assistant".to_string(),
                content: String::new(),
                created_at: now(),
                streaming: false,
                parts: Vec::new(),
                tool_calls: memory_calls,
                tool_call_id: None,
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            };
            rebuild_assistant_parts(&mut message);
            task.messages.push(message.clone());
            message
        };
        mark_task_revision(task);
        message
    };
    state.persist_task(task_id)?;
    emit_task_event(app, state, task_id, "task_message", message);
    Ok(())
}

fn add_step(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    phase: StepPhase,
    title: &str,
    detail: Option<String>,
) -> Result<String, String> {
    let step = AgentStep {
        id: new_id("step"),
        task_id: task_id.to_string(),
        phase,
        title: title.to_string(),
        detail,
        status: StepStatus::Running,
        started_at: now(),
        ended_at: None,
        duration_ms: None,
    };
    {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.steps.push(step.clone());
        touch_task(task);
    }
    state.persist_task(task_id)?;
    emit_task_event(app, state, task_id, "task_step", step.clone());
    Ok(step.id)
}

fn finish_step(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    step_id: &str,
    status: StepStatus,
    detail: Option<String>,
) -> Result<(), String> {
    let step = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        let step = task
            .steps
            .iter_mut()
            .find(|item| item.id == step_id)
            .ok_or_else(|| "Step not found".to_string())?;
        step.status = status;
        step.ended_at = Some(now());
        step.duration_ms =
            Some((step.ended_at.unwrap_or_default() - step.started_at).max(0) as u64);
        if detail.is_some() {
            step.detail = detail;
        }
        let snapshot = step.clone();
        touch_task(task);
        snapshot
    };
    state.persist_task(task_id)?;
    emit_task_event(app, state, task_id, "task_step", step);
    Ok(())
}

fn finish_active_steps(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    status: StepStatus,
    detail: String,
) {
    let active_ids = state
        .tasks
        .read()
        .ok()
        .and_then(|tasks| tasks.get(task_id).cloned())
        .map(|task| {
            task.steps
                .into_iter()
                .filter(|step| step.status == StepStatus::Running)
                .map(|step| step.id)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for step_id in active_ids {
        let _ = finish_step(
            app,
            state,
            task_id,
            &step_id,
            status.clone(),
            Some(detail.clone()),
        );
    }
}

fn add_tool_call(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    name: &str,
    arguments: Value,
    model_tool_call_id: Option<String>,
) -> Result<String, String> {
    let call = ToolCall {
        id: new_id("tool"),
        task_id: task_id.to_string(),
        name: name.to_string(),
        arguments,
        model_tool_call_id,
        status: ToolCallStatus::Running,
        started_at: now(),
        ended_at: None,
        duration_ms: None,
        result: None,
        error: None,
    };
    {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.tool_calls.push(call.clone());
        touch_task(task);
    }
    state.persist_task(task_id)?;
    emit_task_event(app, state, task_id, "task_tool_call", call.clone());
    Ok(call.id)
}

fn finish_tool_call(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    tool_call_id: &str,
    status: ToolCallStatus,
    output: Option<String>,
    error: Option<String>,
) -> Result<ToolResult, String> {
    let (call, duration_ms) = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        let call = task
            .tool_calls
            .iter_mut()
            .find(|item| item.id == tool_call_id)
            .ok_or_else(|| "Tool call not found".to_string())?;
        call.status = status.clone();
        call.ended_at = Some(now());
        call.duration_ms =
            Some((call.ended_at.unwrap_or_default() - call.started_at).max(0) as u64);
        call.result = output.clone();
        call.error = error.clone();
        let duration_ms = call.duration_ms;
        let snapshot = call.clone();
        touch_task(task);
        (snapshot, duration_ms)
    };
    let result = ToolResult {
        id: new_id("result"),
        task_id: task_id.to_string(),
        tool_call_id: tool_call_id.to_string(),
        status,
        output,
        error,
        duration_ms,
    };
    state.persist_task(task_id)?;
    emit_task_event(app, state, task_id, "task_tool_call", call);
    emit_task_event(app, state, task_id, "task_tool_result", result.clone());
    emit_current_plan(app, state, task_id);
    Ok(result)
}

fn add_approval_request(
    app: &AppHandle,
    state: &AppState,
    request: ApprovalRequest,
) -> Result<(), String> {
    {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(&request.task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.approval_requests.push(request.clone());
        touch_task(task);
    }
    state.persist_task(&request.task_id)?;
    emit_task_event(
        app,
        state,
        &request.task_id,
        "task_approval_required",
        request.clone(),
    );
    Ok(())
}

fn update_approval_status(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    approval_id: &str,
    status: &str,
) {
    let mut updated_request = None;
    if let Ok(mut tasks) = state.tasks.write() {
        if let Some(task) = tasks.get_mut(task_id) {
            if let Some(request) = task
                .approval_requests
                .iter_mut()
                .find(|item| item.id == approval_id)
            {
                request.status = status.to_string();
                updated_request = Some(request.clone());
                touch_task(task);
            }
        }
    }
    let _ = state.persist_task(task_id);
    if let Some(request) = updated_request {
        emit_task_event(app, state, task_id, "task_approval_updated", request);
    }
}

enum ApprovalOutcome {
    Approved,
    Rejected,
    Expired,
}

async fn wait_for_approval(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    tool_name: &str,
    arguments: &Value,
    cancel: &CancellationToken,
) -> Result<ApprovalOutcome, AgentError> {
    let approval_id = new_id("approval");
    let remembered_rule = rule_for(tool_name, arguments);
    let request = ApprovalRequest {
        id: approval_id.clone(),
        task_id: task_id.to_string(),
        tool_name: tool_name.to_string(),
        reason: approval_reason(tool_name, arguments),
        details: approval_details(tool_name, arguments),
        created_at: now(),
        status: "pending".to_string(),
        rememberable: remembered_rule.is_some(),
        remember_action: remembered_rule.as_ref().map(|rule| rule.action.clone()),
        remember_pattern: remembered_rule.map(|rule| rule.resource),
    };
    let (sender, receiver) = oneshot::channel();
    state
        .approval_waiters
        .write()
        .map_err(|_| AgentError::Message("Approval lock is poisoned".to_string()))?
        .insert(
            approval_id.clone(),
            ApprovalWaiter {
                task_id: task_id.to_string(),
                sender,
            },
        );
    set_status(app, state, task_id, AgentStatus::WaitingApproval, None).map_err(|error| {
        state
            .approval_waiters
            .write()
            .ok()
            .map(|mut waiters| waiters.remove(&approval_id));
        AgentError::Message(error)
    })?;
    if let Err(error) = add_approval_request(app, state, request) {
        state
            .approval_waiters
            .write()
            .ok()
            .map(|mut waiters| waiters.remove(&approval_id));
        return Err(AgentError::Message(error));
    }

    let decision = tokio::select! {
        _ = cancel.cancelled() => {
            state.approval_waiters.write().ok().map(|mut waiters| waiters.remove(&approval_id));
            return Err(AgentError::Cancelled);
        }
        result = tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), receiver) => {
            match result {
                Ok(Ok(approved)) => {
                    if approved { ApprovalOutcome::Approved } else { ApprovalOutcome::Rejected }
                }
                Ok(Err(_)) => ApprovalOutcome::Rejected,
                Err(_) => ApprovalOutcome::Expired,
            }
        }
    };

    state
        .approval_waiters
        .write()
        .map_err(|_| AgentError::Message("Approval lock is poisoned".to_string()))?
        .remove(&approval_id);
    let status = match decision {
        ApprovalOutcome::Approved => "approved",
        ApprovalOutcome::Rejected => "rejected",
        ApprovalOutcome::Expired => "expired",
    };
    update_approval_status(app, state, task_id, &approval_id, status);
    set_status(app, state, task_id, AgentStatus::Executing, None).map_err(AgentError::Message)?;
    Ok(decision)
}

fn string_argument(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn truncate_output(output: &str) -> String {
    let mut chars = output.chars();
    let result: String = chars.by_ref().take(MAX_OUTPUT_CHARS).collect();
    if chars.next().is_some() {
        format!("{result}\n[output truncated]")
    } else {
        result
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or_default() as u32;
        let third = chunk.get(2).copied().unwrap_or_default() as u32;
        let value = (first << 16) | (second << 8) | third;
        output.push(TABLE[((value >> 18) & 0x3f) as usize] as char);
        output.push(TABLE[((value >> 12) & 0x3f) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[((value >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(value & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn workspace_root() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn sandbox_root_for_task(task_id: &str, workspace: &Path) -> Result<PathBuf, String> {
    let requested = format!(".rustpilot/sandboxes/{task_id}");
    let root = path_guard::resolve_scoped_path(workspace, &requested)?;
    fs::create_dir_all(&root).map_err(|error| format!("Unable to create task sandbox: {error}"))?;
    path_guard::resolve_scoped_path(workspace, &root.to_string_lossy())
}

fn sandbox_path_for_task(task_id: &str, raw: &str, workspace: &Path) -> Result<PathBuf, String> {
    let root = sandbox_root_for_task(task_id, workspace)?;
    path_guard::resolve_scoped_path(&root, raw)
}

async fn run_chat_completion_tool(
    state: &AppState,
    task_id: &str,
    arguments: &Value,
    settings: &AgentSettings,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let messages = arguments
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "messages is required for rust_create_chat_completion".to_string())?;
    let client = llm::OpenAiCompatibleClient::new(llm_settings_for_task(settings, task_id, None))
        .map_err(|error| error.to_string())?;
    let completion = client
        .complete_with_response_format(&messages, arguments.get("response_format").cloned(), cancel)
        .await
        .map_err(|error| error.to_string())?;
    record_task_llm_usage(
        state,
        task_id,
        &completion,
        llm::TokenCounter::count_messages(&messages),
    )?;
    Ok(if completion.content.trim().is_empty() {
        serde_json::to_string(&completion.tool_calls).unwrap_or_else(|_| "".to_string())
    } else {
        completion.content
    })
}

async fn terminate_shell_session(state: &AppState, task_id: &str, name: &str, arguments: &Value) {
    if !matches!(name, "rust_bash" | "rust_sandbox_shell") {
        return;
    }
    let session_id =
        string_argument(arguments, "session_id").unwrap_or_else(|| "default".to_string());
    let workspace = workspace_root_for_arguments(arguments);
    let key = shell_tool::session_key(
        task_id,
        &session_id,
        (name == "rust_sandbox_shell").then_some("sandbox"),
        &workspace,
    );
    state.shell_sessions.lock().await.remove(&key);
}

async fn run_tool(
    state: &AppState,
    task_id: &str,
    name: &str,
    arguments: &Value,
    settings: &AgentSettings,
    cancel: &CancellationToken,
    external_path_approved: bool,
) -> Result<String, AgentError> {
    if !name.starts_with("rust_") {
        return Err(AgentError::Message(
            "Tool names must use the rust_ prefix.".to_string(),
        ));
    }
    let timeout_secs = settings.timeout_secs.clamp(5, 120);
    tokio::select! {
        _ = cancel.cancelled() => {
            terminate_shell_session(state, task_id, name, arguments).await;
            Err(AgentError::Cancelled)
        },
        result = tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            tool_dispatch::run(
                state,
                task_id,
                name,
                arguments,
                settings,
                cancel,
                external_path_approved,
            ),
        ) => match result {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(error)) => Err(AgentError::Message(error)),
            Err(_) => {
                terminate_shell_session(state, task_id, name, arguments).await;
                Err(AgentError::Message(format!("{name} timed out after {timeout_secs}s")))
            },
        }
    }
}

async fn perform_tool(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    invocation: ToolInvocation,
    settings: &AgentSettings,
    cancel: &CancellationToken,
) -> Result<ToolResult, AgentError> {
    let name = invocation.name.as_str();
    let arguments = &invocation.arguments;
    let tool_call_id = add_tool_call(
        app,
        state,
        task_id,
        name,
        arguments.clone(),
        invocation.model_tool_call_id.clone(),
    )
    .map_err(AgentError::Message)?;
    // UI execution ids and model tool-call ids have different lifecycles.
    // Conversation memory must retain the id returned by the model.
    let memory_tool_call_id = invocation
        .model_tool_call_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&tool_call_id)
        .to_string();

    let task = task_snapshot(state, task_id).map_err(AgentError::Message)?;
    let task_workspace = task_workspace(&task);
    let attachment_read_paths =
        task_attachment_read_paths(state, &task).map_err(AgentError::Message)?;
    let task_arguments = with_task_workspace(arguments, &task_workspace, &attachment_read_paths);
    let (approval_mode, approval_rules) = state
        .settings
        .read()
        .map(|settings| (settings.approval_mode, settings.approval_rules.clone()))
        .map_err(|_| AgentError::Message("Settings lock is poisoned".to_string()))?;
    // Persistent shells retain state between calls, so policy must inspect the
    // session's actual cwd instead of trusting an optional argument from the model.
    let policy_arguments = match name {
        "rust_bash" | "rust_sandbox_shell" => {
            let sandbox_prefix = (name == "rust_sandbox_shell").then_some("sandbox");
            let cwd = shell_tool::effective_cwd(
                state,
                task_id,
                &task_arguments,
                sandbox_prefix,
                &task_workspace,
            )
            .await
            .map_err(AgentError::Message)?;
            let mut normalized = task_arguments.clone();
            if let Some(object) = normalized.as_object_mut() {
                object.insert(
                    "cwd".to_string(),
                    Value::String(cwd.to_string_lossy().into_owned()),
                );
            }
            normalized
        }
        _ => task_arguments.clone(),
    };
    let requires_approval = needs_approval(approval_mode, &approval_rules, name, &policy_arguments);
    let mut external_path_approved =
        external_path_requested(name, &policy_arguments) && !requires_approval;
    if requires_approval {
        let approval =
            match wait_for_approval(app, state, task_id, name, &policy_arguments, cancel).await {
                Ok(approval) => approval,
                Err(error) => {
                    let (status, message) = match &error {
                        AgentError::Cancelled => (
                            ToolCallStatus::Cancelled,
                            "Tool execution cancelled while waiting for approval.".to_string(),
                        ),
                        AgentError::Message(message) => (
                            ToolCallStatus::Failed,
                            format!("Approval failed: {message}"),
                        ),
                    };
                    finish_tool_call(
                        app,
                        state,
                        task_id,
                        &tool_call_id,
                        status,
                        None,
                        Some(message.clone()),
                    )
                    .map_err(AgentError::Message)?;
                    let _ = add_tool_message(
                        app,
                        state,
                        task_id,
                        name,
                        &memory_tool_call_id,
                        format!("{name} failed:\n{message}"),
                    );
                    record_memory(
                        state,
                        task_id,
                        "tool",
                        message,
                        Some(memory_tool_call_id.clone()),
                        vec![name.to_string()],
                    )
                    .map_err(AgentError::Message)?;
                    return Err(error);
                }
            };
        match approval {
            ApprovalOutcome::Approved => {
                external_path_approved = external_path_requested(name, &policy_arguments);
            }
            ApprovalOutcome::Rejected => {
                let result = finish_tool_call(
                    app,
                    state,
                    task_id,
                    &tool_call_id,
                    ToolCallStatus::Failed,
                    None,
                    Some("Operation was declined by the user.".to_string()),
                )
                .map_err(AgentError::Message)?;
                let _ = add_tool_message(
                    app,
                    state,
                    task_id,
                    name,
                    &memory_tool_call_id,
                    format!("{name}: operation declined"),
                );
                record_memory(
                    state,
                    task_id,
                    "tool",
                    "Operation was declined by the user.".to_string(),
                    Some(memory_tool_call_id.clone()),
                    vec![name.to_string()],
                )
                .map_err(AgentError::Message)?;
                return Ok(result);
            }
            ApprovalOutcome::Expired => {
                let result = finish_tool_call(
                    app,
                    state,
                    task_id,
                    &tool_call_id,
                    ToolCallStatus::Failed,
                    None,
                    Some("Approval request expired.".to_string()),
                )
                .map_err(AgentError::Message)?;
                let _ = add_tool_message(
                    app,
                    state,
                    task_id,
                    name,
                    &memory_tool_call_id,
                    format!("{name}: approval expired"),
                );
                record_memory(
                    state,
                    task_id,
                    "tool",
                    "Approval request expired.".to_string(),
                    Some(memory_tool_call_id.clone()),
                    vec![name.to_string()],
                )
                .map_err(AgentError::Message)?;
                return Ok(result);
            }
        }
    }

    let execution = run_tool(
        state,
        task_id,
        name,
        &task_arguments,
        settings,
        cancel,
        external_path_approved,
    )
    .await;
    match execution {
        Ok(output) => {
            let result = finish_tool_call(
                app,
                state,
                task_id,
                &tool_call_id,
                ToolCallStatus::Completed,
                Some(output.clone()),
                None,
            )
            .map_err(AgentError::Message)?;
            let _ = add_tool_message(
                app,
                state,
                task_id,
                name,
                &memory_tool_call_id,
                format!("{name} completed:\n{}", truncate_output(&output)),
            );
            record_memory(
                state,
                task_id,
                "tool",
                truncate_output(&output),
                Some(memory_tool_call_id.clone()),
                vec![name.to_string()],
            )
            .map_err(AgentError::Message)?;
            Ok(result)
        }
        Err(AgentError::Cancelled) => {
            finish_tool_call(
                app,
                state,
                task_id,
                &tool_call_id,
                ToolCallStatus::Cancelled,
                None,
                Some("Tool execution cancelled.".to_string()),
            )
            .map_err(AgentError::Message)?;
            let _ = add_tool_message(
                app,
                state,
                task_id,
                name,
                &memory_tool_call_id,
                format!("{name} cancelled."),
            );
            record_memory(
                state,
                task_id,
                "tool",
                "Tool execution cancelled.".to_string(),
                Some(memory_tool_call_id),
                vec![name.to_string()],
            )
            .map_err(AgentError::Message)?;
            Err(AgentError::Cancelled)
        }
        Err(AgentError::Message(error)) => {
            let result = finish_tool_call(
                app,
                state,
                task_id,
                &tool_call_id,
                ToolCallStatus::Failed,
                None,
                Some(error.clone()),
            )
            .map_err(AgentError::Message)?;
            let _ = add_tool_message(
                app,
                state,
                task_id,
                name,
                &memory_tool_call_id,
                format!("{name} failed:\n{error}"),
            );
            record_memory(
                state,
                task_id,
                "tool",
                error.clone(),
                Some(memory_tool_call_id.clone()),
                vec![name.to_string()],
            )
            .map_err(AgentError::Message)?;
            Ok(result)
        }
    }
}

#[cfg(test)]
#[allow(dead_code)]
async fn stream_demo_message(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    content: &str,
    cancel: &CancellationToken,
) -> Result<String, AgentError> {
    let message_id = new_id("msg");
    for chunk in content.chars().collect::<Vec<_>>().chunks(4) {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let delta: String = chunk.iter().collect();
        append_stream_message(app, state, task_id, &message_id, &delta)
            .map_err(AgentError::Message)?;
        tokio::select! {
            _ = cancel.cancelled() => return Err(AgentError::Cancelled),
            _ = tokio::time::sleep(Duration::from_millis(35)) => {}
        }
    }
    finish_stream_message(app, state, task_id, &message_id).map_err(AgentError::Message)?;
    Ok(message_id)
}

#[cfg(test)]
fn demo_contains_any(prompt: &str, terms: &[&str]) -> bool {
    terms.iter().any(|term| prompt.contains(term))
}

#[cfg(test)]
fn demo_capability_request(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    demo_contains_any(
        &lower,
        &[
            "what can you do",
            "what are you able to do",
            "capabilities",
            "help",
        ],
    )
}

#[cfg(test)]
fn demo_requested_time(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    demo_contains_any(&lower, &["time", "date", "clock"])
}

#[cfg(test)]
fn demo_requested_files(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    demo_contains_any(
        &lower,
        &[
            "file",
            "files",
            "folder",
            "directory",
            "project",
            "repository",
            "repo",
            "code",
        ],
    )
}

#[cfg(test)]
fn demo_requested_search(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    demo_contains_any(&lower, &["find", "search", "research", "web", "lookup"])
}

#[cfg(test)]
fn demo_requested_shell(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    demo_contains_any(&lower, &["shell", "terminal", "command", "run command"])
}

#[cfg(test)]
fn demo_requested_write(prompt: &str) -> bool {
    let lower = prompt.to_ascii_lowercase();
    demo_contains_any(&lower, &["write", "create file", "save"])
}

#[cfg(test)]
fn demo_first_url(prompt: &str) -> Option<String> {
    prompt
        .split_whitespace()
        .find(|word| word.starts_with("http://") || word.starts_with("https://"))
        .map(|word| word.trim_matches(|character: char| ",.;!?".contains(character)))
        .filter(|url| !url.is_empty())
        .map(ToString::to_string)
}

#[cfg(test)]
fn demo_tool_calls(prompt: &str) -> Vec<(String, Value)> {
    let lower = prompt.to_ascii_lowercase();
    if demo_capability_request(prompt) {
        return Vec::new();
    }

    let mut calls = Vec::new();
    if demo_requested_time(prompt) {
        calls.push(("rust_clock".to_string(), json!({"format": "unix_millis"})));
    }
    if demo_requested_files(prompt) {
        calls.push((
            "rust_files".to_string(),
            json!({"operation": "list", "path": "."}),
        ));
    }
    if let Some(url) = demo_first_url(prompt) {
        calls.push(("rust_http".to_string(), json!({"url": url})));
    }
    if demo_requested_search(prompt) {
        calls.push((
            "rust_web_search".to_string(),
            json!({"query": prompt, "num_results": 5, "fetch_content": false}),
        ));
    }
    if demo_requested_shell(prompt) {
        calls.push((
            "rust_shell".to_string(),
            json!({"command": "echo RustPilot demo shell request"}),
        ));
    }
    if demo_requested_write(prompt) {
        calls.push((
            "rust_files".to_string(),
            json!({
                "operation": "write",
                "path": "rustpilot-demo.txt",
                "content": "Created by RustPilot Demo mode."
            }),
        ));
    }
    if calls.is_empty() && lower.contains("inspect") {
        calls.push((
            "rust_files".to_string(),
            json!({"operation": "list", "path": "."}),
        ));
    }
    calls
}

#[cfg(test)]
fn demo_result_summary(name: &str, output: &str, _cjk: bool) -> String {
    match name {
        "rust_clock" => "Read the local time. The raw value is available in Rust Trace.".to_string(),
        "rust_files" if output.lines().any(|line| line.starts_with("Wrote file: ")) => output
            .lines()
            .find(|line| line.starts_with("Wrote file: "))
            .unwrap_or("File written.")
            .to_string(),
        "rust_files" => {
            let count = output
                .lines()
                .filter(|line| line.starts_with("file") || line.starts_with("dir "))
                .count();
            format!(
                "Inspected the workspace and found {count} files and directories. The full list is in Rust Trace."
            )
        }
        "rust_shell" => {
            let exit_code = output
                .lines()
                .find_map(|line| line.strip_prefix("exit_code: "))
                .unwrap_or("unknown");
            format!("The command completed with exit code {exit_code}.")
        }
        "rust_http" => {
            let status = output.lines().next().unwrap_or("HTTP response received");
            format!(
                "Completed the network request ({status}). The response body is in Rust Trace."
            )
        }
        "rust_web_search" => {
            "Completed the web search. Candidate sources are available in Rust Trace for verification."
                .to_string()
        }
        _ => format!("{name} returned a result and it passed the basic verification."),
    }
}

#[cfg(test)]
fn demo_answer(_prompt: &str, results: &[(String, ToolResult)]) -> String {
    if results.is_empty() {
        return "I understood the request. Demo mode only calls a local tool when it is actually needed, then returns a concise conclusion.".to_string();
    }

    let summaries = results
        .iter()
        .map(|(name, result)| match (&result.output, &result.error) {
            (_, Some(error)) => format!("{name} failed: {error}"),
            (Some(output), _) => demo_result_summary(name, output, false),
            _ => format!("{name} returned no usable result."),
        })
        .collect::<Vec<_>>();
    format!("The task is complete.\n\n{}", summaries.join("\n"))
}

#[cfg(test)]
#[allow(dead_code)]
async fn run_demo(
    app: &AppHandle,
    state: &AppState,
    task: &Task,
    settings: &AgentSettings,
    cancel: &CancellationToken,
) -> Result<String, AgentError> {
    ensure_default_plan(state, &task.id).map_err(AgentError::Message)?;
    set_plan_step_status(
        state,
        &task.id,
        0,
        PlanStepStatus::InProgress,
        Some("Demo planner selected a bounded execution path.".to_string()),
    )
    .map_err(AgentError::Message)?;
    set_status(app, state, &task.id, AgentStatus::Planning, None).map_err(AgentError::Message)?;
    let plan_step = add_step(
        app,
        state,
        &task.id,
        StepPhase::Plan,
        "Plan",
        Some("Turn the request into a small, observable execution plan.".to_string()),
    )
    .map_err(AgentError::Message)?;
    if task.prompt.to_lowercase().contains("fail") || task.prompt.contains("澶辫触") {
        finish_step(
            app,
            state,
            &task.id,
            &plan_step,
            StepStatus::Failed,
            Some("The demo failure scenario was requested by the task.".to_string()),
        )
        .map_err(AgentError::Message)?;
        set_plan_step_status(
            state,
            &task.id,
            0,
            PlanStepStatus::Blocked,
            Some("The explicit demo failure scenario stopped planning.".to_string()),
        )
        .map_err(AgentError::Message)?;
        return Err(AgentError::Message(
            "Demo failure scenario requested. Retry the task without the word 'fail' to continue."
                .to_string(),
        ));
    }
    finish_step(
        app,
        state,
        &task.id,
        &plan_step,
        StepStatus::Completed,
        Some("Plan ready.".to_string()),
    )
    .map_err(AgentError::Message)?;
    set_plan_step_status(
        state,
        &task.id,
        0,
        PlanStepStatus::Completed,
        Some("Plan ready.".to_string()),
    )
    .map_err(AgentError::Message)?;
    set_plan_step_status(state, &task.id, 1, PlanStepStatus::InProgress, None)
        .map_err(AgentError::Message)?;

    set_status(app, state, &task.id, AgentStatus::Executing, None).map_err(AgentError::Message)?;
    let act_step = add_step(
        app,
        state,
        &task.id,
        StepPhase::Act,
        "Act",
        Some("Run the smallest useful local tools for this request.".to_string()),
    )
    .map_err(AgentError::Message)?;

    let calls = demo_tool_calls(&task.prompt);

    let mut results = Vec::new();
    for (name, arguments) in calls {
        let result = perform_tool(
            app,
            state,
            &task.id,
            ToolInvocation {
                model_tool_call_id: None,
                name: name.clone(),
                arguments,
            },
            settings,
            cancel,
        )
        .await?;
        results.push((name, result));
    }
    finish_step(
        app,
        state,
        &task.id,
        &act_step,
        StepStatus::Completed,
        Some(if results.is_empty() {
            "No local tools were needed.".to_string()
        } else {
            format!("{} relevant tool call(s) recorded.", results.len())
        }),
    )
    .map_err(AgentError::Message)?;
    set_plan_step_status(
        state,
        &task.id,
        1,
        PlanStepStatus::Completed,
        Some(if results.is_empty() {
            "The request was answered directly.".to_string()
        } else {
            format!("{} relevant result(s) returned.", results.len())
        }),
    )
    .map_err(AgentError::Message)?;
    set_plan_step_status(state, &task.id, 2, PlanStepStatus::InProgress, None)
        .map_err(AgentError::Message)?;

    set_status(app, state, &task.id, AgentStatus::Verifying, None).map_err(AgentError::Message)?;
    let verify_step = add_step(
        app,
        state,
        &task.id,
        StepPhase::Verify,
        "Verify",
        Some("Check each tool result before composing the answer.".to_string()),
    )
    .map_err(AgentError::Message)?;
    let answer = demo_answer(&task.prompt, &results);
    stream_demo_message(app, state, &task.id, &answer, cancel).await?;
    record_memory(
        state,
        &task.id,
        "assistant",
        answer.clone(),
        None,
        results.iter().map(|(name, _)| name.clone()).collect(),
    )
    .map_err(AgentError::Message)?;
    finish_step(
        app,
        state,
        &task.id,
        &verify_step,
        StepStatus::Completed,
        Some(
            "The result was summarized for the user; raw evidence remains available in Rust Trace."
                .to_string(),
        ),
    )
    .map_err(AgentError::Message)?;
    set_plan_step_status(
        state,
        &task.id,
        2,
        PlanStepStatus::Completed,
        Some("The user-facing answer was verified and summarized.".to_string()),
    )
    .map_err(AgentError::Message)?;
    Ok(answer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ChatToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing)]
    attachments: Vec<attachments::AttachmentRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ChatFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatFunction {
    name: String,
    arguments: String,
}

#[derive(Debug)]
struct Completion {
    message_id: String,
    content: String,
    tool_calls: Vec<ChatToolCall>,
}

fn memory_to_chat_messages(memory: &[AgentMemoryEntry]) -> Vec<ChatMessage> {
    memory
        .iter()
        .filter_map(|entry| {
            let role = match entry.role.as_str() {
                "user" | "assistant" | "tool" => entry.role.clone(),
                _ => return None,
            };
            Some(ChatMessage {
                role,
                content: if entry.role == "tool" || !entry.content.is_empty() {
                    Some(entry.content.clone())
                } else {
                    None
                },
                tool_calls: (!entry.tool_calls.is_empty()).then_some(
                    entry
                        .tool_calls
                        .iter()
                        .map(|call| ChatToolCall {
                            id: call.id.clone(),
                            call_type: call.call_type.clone(),
                            function: ChatFunction {
                                name: call.function.name.clone(),
                                arguments: call.function.arguments.clone(),
                            },
                        })
                        .collect(),
                ),
                tool_call_id: entry.tool_call_id.clone(),
                name: entry.name.clone(),
                attachments: entry.attachments.clone(),
            })
        })
        .collect()
}

fn chat_message_value(
    message: &ChatMessage,
    data_dir: &Path,
    supports_images: bool,
) -> Result<Value, String> {
    let mut value = serde_json::to_value(message)
        .map_err(|error| format!("Unable to encode chat message: {error}"))?;
    if message.attachments.is_empty() {
        return Ok(value);
    }
    if message.role != "user" {
        return Err("Only user messages can contain attachments.".to_string());
    }

    let contains_image = message
        .attachments
        .iter()
        .any(|attachment| attachments::is_image(&attachment.mime));
    if contains_image && !supports_images {
        let image_name = message
            .attachments
            .iter()
            .find(|attachment| attachments::is_image(&attachment.mime))
            .map(|attachment| attachment.name.as_str())
            .unwrap_or("image");
        return Err(format!(
            "The configured model does not support image attachments. Remove {image_name} or choose a vision model."
        ));
    }

    if !contains_image {
        let mut text = message.content.clone().unwrap_or_default();
        for attachment in &message.attachments {
            let bytes = attachments::read(data_dir, attachment)?;
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(&attachment_prompt_text(data_dir, attachment, &bytes));
        }
        value["content"] = Value::String(text);
        return Ok(value);
    }

    let mut content = Vec::with_capacity(message.attachments.len() + 1);
    if let Some(text) = message.content.as_deref().filter(|text| !text.is_empty()) {
        content.push(json!({"type": "text", "text": text}));
    }
    for attachment in &message.attachments {
        let bytes = attachments::read(data_dir, attachment)?;
        if attachments::is_image(&attachment.mime) {
            content.push(json!({
                "type": "image_url",
                "image_url": {
                    "url": format!("data:{};base64,{}", attachment.mime, base64_encode(&bytes))
                }
            }));
            continue;
        }
        content.push(json!({
            "type": "text",
            "text": attachment_prompt_text(data_dir, attachment, &bytes)
        }));
    }
    value["content"] = Value::Array(content);
    Ok(value)
}

fn attachment_prompt_text(
    data_dir: &Path,
    attachment: &attachments::AttachmentRef,
    bytes: &[u8],
) -> String {
    if attachments::is_text(&attachment.mime, &attachment.name) {
        let text_bytes = &bytes[..bytes.len().min(attachments::MAX_TEXT_CONTEXT_BYTES)];
        let mut text = String::from_utf8_lossy(text_bytes).into_owned();
        if bytes.len() > text_bytes.len() {
            text.push_str("\n[attachment text truncated]");
        }
        let local_path = data_dir.join(&attachment.storage_key);
        return format!(
            "[Attached file: {} | {} | {} bytes]\n{}\n[Full attachment path: {}]",
            attachment.name,
            attachment.mime,
            attachment.size,
            text,
            local_path.display()
        );
    }

    let local_path = data_dir.join(&attachment.storage_key);
    format!(
        "[Attached file: {} | {} | {} bytes]\nThis file is stored locally at {}. Use the file tools if you need to inspect its raw contents.",
        attachment.name,
        attachment.mime,
        attachment.size,
        local_path.display()
    )
}

fn validate_chat_message_context(messages: &[ChatMessage]) -> Result<(), String> {
    let mut pending_tool_calls = HashSet::new();
    for message in messages {
        if !pending_tool_calls.is_empty() && message.role != "tool" {
            return Err(
                "Conversation context is invalid: a tool response is missing before the next message."
                    .to_string(),
            );
        }
        match message.role.as_str() {
            "assistant" => {
                if let Some(calls) = &message.tool_calls {
                    for call in calls {
                        if call.id.trim().is_empty() || !pending_tool_calls.insert(call.id.clone())
                        {
                            return Err(
                                "Conversation context is invalid: assistant tool-call ids are empty or duplicated."
                                    .to_string(),
                            );
                        }
                    }
                }
            }
            "tool" => {
                let Some(tool_call_id) = message.tool_call_id.as_deref() else {
                    return Err(
                        "Conversation context is invalid: a tool response has no tool_call_id."
                            .to_string(),
                    );
                };
                if !pending_tool_calls.remove(tool_call_id) {
                    return Err(format!(
                        "Conversation context is invalid: tool response `{tool_call_id}` has no matching assistant tool call."
                    ));
                }
            }
            "system" | "user" => {}
            _ => {
                return Err("Conversation context contains an unsupported message role.".to_string())
            }
        }
    }
    if pending_tool_calls.is_empty() {
        Ok(())
    } else {
        Err(
            "Conversation context is invalid: the last assistant tool call has no result."
                .to_string(),
        )
    }
}

fn llm_settings_for_task(
    settings: &AgentSettings,
    task_id: &str,
    tool_schema_hash: Option<&str>,
) -> llm::LlmSettings {
    let mut llm_settings = llm::LlmSettings {
        model: settings.model.clone(),
        base_url: settings.api_base_url.clone(),
        api_key: settings.api_key.clone().unwrap_or_default(),
        prompt_cache: settings.prompt_cache,
        ..llm::LlmSettings::default()
    };
    llm_settings.session_id = Some(format!("rustpilot:{task_id}"));
    llm_settings.tool_schema_hash = tool_schema_hash.map(str::to_string);
    llm_settings
}

fn record_task_llm_usage(
    state: &AppState,
    task_id: &str,
    completion: &llm::Completion,
    fallback_input: usize,
) -> Result<(), String> {
    {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.llm_usage.record(
            completion.prompt_tokens.unwrap_or(fallback_input),
            completion
                .completion_tokens
                .unwrap_or_else(|| llm::TokenCounter::count_text(&completion.content)),
            completion.cached_input_tokens,
            completion.cache_write_tokens,
        );
        touch_task(task);
    }
    state.persist_task(task_id)
}

async fn stream_openai(
    app: &AppHandle,
    state: &AppState,
    task_id: &str,
    settings: &AgentSettings,
    messages: &[ChatMessage],
    cancel: &CancellationToken,
) -> Result<Completion, AgentError> {
    validate_chat_message_context(messages).map_err(AgentError::Message)?;
    let tools = tool_definitions_for_state(state);
    let client = llm::OpenAiCompatibleClient::new(llm_settings_for_task(
        settings,
        task_id,
        Some(tools.schema_hash.as_ref()),
    ))
    .map_err(|error| AgentError::Message(error.to_string()))?;
    let data_dir = state
        .storage_dir
        .read()
        .map_err(|_| AgentError::Message("Storage lock is poisoned".to_string()))?
        .clone()
        .ok_or_else(|| AgentError::Message("Attachment storage is not initialized.".to_string()))?;
    let request_messages = messages
        .iter()
        .map(|message| chat_message_value(message, &data_dir, client.supports_images()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(AgentError::Message)?;
    // Publish the assistant turn before waiting for the first token so the UI can show progress.
    let message = add_message(app, state, task_id, "assistant", String::new(), true)
        .map_err(AgentError::Message)?;
    let message_id = message.id;
    let completion = match client
        .stream(
            &request_messages,
            tools.definitions.as_slice(),
            agent::ToolChoice::Auto,
            None,
            cancel,
            |event| {
                append_stream_event(app, state, task_id, &message_id, event)
                    .map_err(llm::LlmError::InvalidInput)
            },
        )
        .await
    {
        Ok(completion) => completion,
        Err(error) => {
            // Do not leave a visible "working" turn behind after a failed or cancelled stream.
            let _ = finish_stream_message(app, state, task_id, &message_id);
            return Err(match error {
                llm::LlmError::Cancelled => AgentError::Cancelled,
                other => AgentError::Message(other.to_string()),
            });
        }
    };
    finish_stream_message(app, state, task_id, &message_id).map_err(AgentError::Message)?;
    record_task_llm_usage(
        state,
        task_id,
        &completion,
        llm::TokenCounter::count_messages(&request_messages)
            + tools
                .definitions
                .iter()
                .map(|tool| llm::TokenCounter::count_text(&tool.to_string()))
                .sum::<usize>(),
    )
    .map_err(AgentError::Message)?;
    Ok(Completion {
        message_id,
        content: completion.content,
        tool_calls: completion
            .tool_calls
            .into_iter()
            .map(|call| ChatToolCall {
                id: call.id,
                call_type: call.call_type,
                function: ChatFunction {
                    name: call.function.name,
                    arguments: call.function.arguments,
                },
            })
            .collect(),
    })
}

fn system_prompt_parts(agent_kind: &str, workspace: &str) -> (String, String) {
    let kind = agent::parse_agent_kind(agent_kind).unwrap_or(agent::AgentKind::Manus);
    let spec = agents::AgentSpec::for_kind(kind, workspace);
    let policy = format!(
        "{}\n\nThe desktop runtime handles high-risk approvals. Keep every argument valid JSON, bound observations, recover from tool errors, and never claim a tool ran unless its result is present. Before each non-trivial tool call, give the user one concise factual progress sentence; after the result, state the useful implication before choosing the next action. Do not reveal chain-of-thought or invent progress that the tool result cannot support. The persisted conversation history is authoritative: treat the newest user message as a continuation of the prior exchange unless the user explicitly asks to restart, and use prior messages and tool results instead of reintroducing yourself or resetting the task. Write the final response for the user, not for the execution log: answer in the user's language, lead with the conclusion, and keep it concise. Never paste raw JSON, full directory listings, timestamps, stack traces, or tool logs unless the user explicitly asks for them. Summarize evidence and mention only the paths or values that matter. For capability questions, answer directly without calling unrelated tools. Internal planning and tool-call messages are not user-facing.",
        spec.next_step_prompt
    );
    (spec.system_prompt, policy)
}

async fn run_real(
    app: &AppHandle,
    state: &AppState,
    task: &Task,
    settings: &AgentSettings,
    cancel: &CancellationToken,
) -> Result<String, AgentError> {
    let task_memory = repair_task_memory(state, &task.id).map_err(AgentError::Message)?;
    ensure_default_plan(state, &task.id).map_err(AgentError::Message)?;
    set_plan_step_status(
        state,
        &task.id,
        0,
        PlanStepStatus::InProgress,
        Some("The ReAct agent is deciding how to solve the request.".to_string()),
    )
    .map_err(AgentError::Message)?;
    let (system_header, system_policy) = system_prompt_parts(&task.agent_kind, &task.workspace);
    let system_prompt = format!("{system_header}\n\n{system_policy}");
    let system_messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: Some(system_header),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            attachments: Vec::new(),
        },
        ChatMessage {
            role: "system".to_string(),
            content: Some(system_policy),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            attachments: Vec::new(),
        },
    ];
    let system_message_count = system_messages.len();
    let mut messages = system_messages;
    if task_memory.is_empty() {
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: Some(task.prompt.clone()),
            tool_calls: None,
            tool_call_id: None,
            name: None,
            attachments: Vec::new(),
        });
    } else {
        messages.extend(memory_to_chat_messages(&task_memory));
    }
    let plan_step = add_step(
        app,
        state,
        &task.id,
        StepPhase::Plan,
        "Plan",
        Some("Ask the configured model to plan the task and choose tools.".to_string()),
    )
    .map_err(AgentError::Message)?;
    let agent_kind = agent::parse_agent_kind(&task.agent_kind).unwrap_or(agent::AgentKind::Manus);
    let agent_spec = agents::AgentSpec::for_kind(agent_kind, &task.workspace);
    let max_steps = normalize_max_steps(settings.max_steps);
    let mut runtime = agent::ToolCallAgentRuntime::new(task.agent_name.clone(), system_prompt);
    runtime.base.description = agent_spec.description;
    runtime.base.next_step_prompt = agent_spec.next_step_prompt;
    runtime.base.max_steps = max_steps;
    runtime.base.memory.max_messages = MAX_MEMORY_ENTRIES;
    runtime.max_observe = agent_spec.max_observe;
    runtime.special_tool_names = agent_spec.special_tool_names;
    runtime.base.begin().map_err(AgentError::Message)?;
    let mut latest_answer = String::new();
    let mut repeated_signatures: HashMap<String, u32> = HashMap::new();

    for round in 0..max_steps {
        if cancel.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        runtime.base.next_step().map_err(AgentError::Message)?;
        let step_id = if round == 0 {
            set_status(app, state, &task.id, AgentStatus::Planning, None)
                .map_err(AgentError::Message)?;
            plan_step.clone()
        } else {
            set_status(app, state, &task.id, AgentStatus::Verifying, None)
                .map_err(AgentError::Message)?;
            add_step(
                app,
                state,
                &task.id,
                StepPhase::Verify,
                "Verify",
                Some("Review tool evidence and decide whether more action is needed.".to_string()),
            )
            .map_err(AgentError::Message)?
        };
        let mut completion =
            stream_openai(app, state, &task.id, settings, &messages, cancel).await?;
        let tool_call_count = completion.tool_calls.len();
        completion
            .tool_calls
            .retain(|call| !call.id.trim().is_empty() && !call.function.name.trim().is_empty());
        if tool_call_count > 0
            && completion.tool_calls.is_empty()
            && completion.content.trim().is_empty()
        {
            return Err(AgentError::Message(
                "The model returned a tool call without a valid id or function name.".to_string(),
            ));
        }
        attach_tool_calls_to_last_message(
            app,
            state,
            &task.id,
            &completion.message_id,
            &completion.tool_calls,
        )
        .map_err(AgentError::Message)?;
        if !completion.content.trim().is_empty() {
            latest_answer = completion.content.clone();
        }
        let memory_tool_calls = completion
            .tool_calls
            .iter()
            .map(|call| agent::MessageToolCall {
                id: call.id.clone(),
                call_type: call.call_type.clone(),
                function: agent::FunctionCall {
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                },
            })
            .collect::<Vec<_>>();
        runtime.set_response(
            (!completion.content.is_empty()).then_some(completion.content.clone()),
            memory_tool_calls.clone(),
        );
        record_memory_full(
            state,
            &task.id,
            MemoryRecord {
                role: "assistant".to_string(),
                content: completion.content.clone(),
                tool_call_id: None,
                tool_names: completion
                    .tool_calls
                    .iter()
                    .map(|call| call.function.name.clone())
                    .collect(),
                tool_calls: memory_tool_calls,
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            },
        )
        .map_err(AgentError::Message)?;

        let signature = format!(
            "{}|{}",
            completion.content.trim(),
            completion
                .tool_calls
                .iter()
                .map(|call| format!("{}:{}", call.function.name, call.function.arguments))
                .collect::<Vec<_>>()
                .join("|")
        );
        let repetitions = repeated_signatures.entry(signature).or_insert(0);
        *repetitions += 1;
        if *repetitions >= 3 && !completion.tool_calls.is_empty() {
            let stuck_message = "The agent detected a repeated tool-call pattern and stopped to avoid an infinite loop.";
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: Some(stuck_message.to_string()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
                attachments: Vec::new(),
            });
            record_memory(
                state,
                &task.id,
                "system",
                stuck_message.to_string(),
                None,
                Vec::new(),
            )
            .map_err(AgentError::Message)?;
            finish_step(
                app,
                state,
                &task.id,
                &step_id,
                StepStatus::Failed,
                Some(stuck_message.to_string()),
            )
            .map_err(AgentError::Message)?;
            return Err(AgentError::Message(stuck_message.to_string()));
        }

        let assistant_message = ChatMessage {
            role: "assistant".to_string(),
            content: (!completion.content.is_empty()).then_some(completion.content.clone()),
            tool_calls: (!completion.tool_calls.is_empty())
                .then_some(completion.tool_calls.clone()),
            tool_call_id: None,
            name: None,
            attachments: Vec::new(),
        };
        messages.push(assistant_message);

        if completion.tool_calls.is_empty() {
            finish_step(
                app,
                state,
                &task.id,
                &step_id,
                StepStatus::Completed,
                Some("The response did not request another tool.".to_string()),
            )
            .map_err(AgentError::Message)?;
            if latest_answer.is_empty() {
                latest_answer =
                    "The model completed the run without a streamed final message.".to_string();
                add_message(
                    app,
                    state,
                    &task.id,
                    "assistant",
                    latest_answer.clone(),
                    false,
                )
                .map_err(AgentError::Message)?;
                record_memory(
                    state,
                    &task.id,
                    "assistant",
                    latest_answer.clone(),
                    None,
                    Vec::new(),
                )
                .map_err(AgentError::Message)?;
            }
            runtime.base.finish();
            set_plan_step_status(
                state,
                &task.id,
                0,
                PlanStepStatus::Completed,
                Some("The model produced a plan and final response.".to_string()),
            )
            .map_err(AgentError::Message)?;
            set_plan_step_status(
                state,
                &task.id,
                2,
                PlanStepStatus::Completed,
                Some("No further tool call was requested.".to_string()),
            )
            .map_err(AgentError::Message)?;
            return Ok(latest_answer);
        }

        finish_step(
            app,
            state,
            &task.id,
            &step_id,
            StepStatus::Completed,
            Some(format!(
                "{} tool call{} requested.",
                completion.tool_calls.len(),
                if completion.tool_calls.len() == 1 {
                    ""
                } else {
                    "s"
                }
            )),
        )
        .map_err(AgentError::Message)?;
        if round == 0 {
            set_plan_step_status(
                state,
                &task.id,
                0,
                PlanStepStatus::Completed,
                Some("The model selected the first tool actions.".to_string()),
            )
            .map_err(AgentError::Message)?;
        }
        set_plan_step_status(
            state,
            &task.id,
            1,
            PlanStepStatus::InProgress,
            Some(format!(
                "Executing {} requested tool call(s).",
                completion.tool_calls.len()
            )),
        )
        .map_err(AgentError::Message)?;
        set_status(app, state, &task.id, AgentStatus::Executing, None)
            .map_err(AgentError::Message)?;
        let act_step = add_step(
            app,
            state,
            &task.id,
            StepPhase::Act,
            "Act",
            Some("Execute requested tools with timeout and approval checks.".to_string()),
        )
        .map_err(AgentError::Message)?;
        let mut termination_answer: Option<String> = None;
        let mut termination_failure: Option<String> = None;
        for tool_call in &completion.tool_calls {
            if tool_call.id.is_empty() || tool_call.function.name.is_empty() {
                continue;
            }
            let arguments = serde_json::from_str::<Value>(&tool_call.function.arguments)
                .unwrap_or_else(|_| json!({"raw": tool_call.function.arguments}));
            let result = perform_tool(
                app,
                state,
                &task.id,
                ToolInvocation {
                    model_tool_call_id: Some(tool_call.id.clone()),
                    name: tool_call.function.name.clone(),
                    arguments: arguments.clone(),
                },
                settings,
                cancel,
            )
            .await?;
            if tool_call.function.name == "rust_terminate" {
                let status = arguments
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("success");
                if status == "failure" {
                    termination_failure = Some(
                        arguments
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("The agent terminated with failure.")
                            .to_string(),
                    );
                } else if let Some(output) = result.output.clone() {
                    termination_answer = Some(output);
                }
            }
            let tool_content = match (&result.output, &result.error) {
                (Some(output), _) => truncate_output(output),
                (_, Some(error)) => format!("Tool error: {error}"),
                _ => "Tool returned no output.".to_string(),
            };
            runtime.base.memory.add_message(agent::Message::tool(
                tool_content.clone(),
                tool_call.function.name.clone(),
                tool_call.id.clone(),
            ));
            messages.push(ChatMessage {
                role: "tool".to_string(),
                content: Some(tool_content),
                tool_calls: None,
                tool_call_id: Some(tool_call.id.clone()),
                name: Some(tool_call.function.name.clone()),
                attachments: Vec::new(),
            });
        }
        finish_step(
            app,
            state,
            &task.id,
            &act_step,
            StepStatus::Completed,
            Some("Tool results recorded.".to_string()),
        )
        .map_err(AgentError::Message)?;
        set_plan_step_status(
            state,
            &task.id,
            1,
            PlanStepStatus::Completed,
            Some("Tool results recorded in memory.".to_string()),
        )
        .map_err(AgentError::Message)?;
        set_plan_step_status(state, &task.id, 2, PlanStepStatus::InProgress, None)
            .map_err(AgentError::Message)?;
        // Rebuild the next request from durable memory after every tool batch. This
        // keeps a long-running task bounded without splitting an assistant/tool pair.
        let refreshed_memory = repair_task_memory(state, &task.id).map_err(AgentError::Message)?;
        messages.truncate(system_message_count);
        if refreshed_memory.is_empty() {
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: Some(task.prompt.clone()),
                tool_calls: None,
                tool_call_id: None,
                name: None,
                attachments: Vec::new(),
            });
        } else {
            messages.extend(memory_to_chat_messages(&refreshed_memory));
        }
        if let Some(error) = termination_failure {
            runtime.base.fail();
            return Err(AgentError::Message(error));
        }
        if let Some(answer) = termination_answer {
            runtime.base.finish();
            return Ok(answer);
        }
    }

    runtime.base.fail();
    Err(AgentError::Message(format!(
        "The agent reached the maximum of {} steps before a final answer.",
        max_steps
    )))
}

async fn run_agent(app: AppHandle, state: AppState, task_id: String, cancel: CancellationToken) {
    let task = match task_snapshot(&state, &task_id) {
        Ok(task) => task,
        Err(error) => {
            error!("Unable to start task {task_id}: {error}");
            return;
        }
    };
    let settings = match state.settings.read() {
        Ok(settings) => settings.clone(),
        Err(_) => {
            let message = "Settings lock is poisoned.".to_string();
            let _ = set_status(
                &app,
                &state,
                &task_id,
                AgentStatus::Failed,
                Some(message.clone()),
            );
            emit_task_event(
                &app,
                &state,
                &task_id,
                "task_failed",
                TaskFailedEvent {
                    task_id: task_id.clone(),
                    error: message,
                },
            );
            return;
        }
    };
    let result = if settings.api_key.is_some() {
        run_real(&app, &state, &task, &settings, &cancel).await
    } else {
        Err(AgentError::Message(API_KEY_REQUIRED_MESSAGE.to_string()))
    };

    state
        .running
        .write()
        .ok()
        .map(|mut running| running.remove(&task.id));
    match result {
        Ok(answer) if !cancel.is_cancelled() => {
            let _ = set_final_answer(&state, &task.id, answer.clone());
            let _ = set_status(&app, &state, &task.id, AgentStatus::Completed, None);
            emit_task_event(
                &app,
                &state,
                &task.id,
                "task_completed",
                TaskCompletedEvent {
                    task_id: task.id.clone(),
                    final_answer: answer,
                    demo_mode: task.demo_mode,
                },
            );
        }
        Ok(_) | Err(AgentError::Cancelled) => {
            finish_active_steps(
                &app,
                &state,
                &task.id,
                StepStatus::Cancelled,
                "Task cancelled.".to_string(),
            );
            let _ = set_status(&app, &state, &task.id, AgentStatus::Cancelled, None);
            emit_task_event(
                &app,
                &state,
                &task.id,
                "task_cancelled",
                TaskCancelledEvent {
                    task_id: task.id.clone(),
                },
            );
        }
        Err(AgentError::Message(error_message)) => {
            finish_active_steps(
                &app,
                &state,
                &task.id,
                StepStatus::Failed,
                error_message.clone(),
            );
            let _ = add_message(
                &app,
                &state,
                &task.id,
                "system",
                format!("Agent failed: {error_message}"),
                false,
            );
            let _ = set_status(
                &app,
                &state,
                &task.id,
                AgentStatus::Failed,
                Some(error_message.clone()),
            );
            emit_task_event(
                &app,
                &state,
                &task.id,
                "task_failed",
                TaskFailedEvent {
                    task_id: task.id.clone(),
                    error: error_message,
                },
            );
        }
    }
}

fn start_task(app: &AppHandle, state: &AppState, task_id: String) {
    let cancel = CancellationToken::new();
    if let Ok(mut running) = state.running.write() {
        running.insert(task_id.clone(), cancel.clone());
    }
    let app = app.clone();
    let state = state.clone();
    tauri::async_runtime::spawn(async move {
        run_agent(app, state, task_id, cancel).await;
    });
}

fn settings_view(settings: &AgentSettings, state: Option<&AppState>) -> SettingsView {
    SettingsView {
        api_base_url: settings.api_base_url.clone(),
        model: settings.model.clone(),
        api_key_configured: settings.api_key.is_some(),
        max_steps: settings.max_steps,
        timeout_secs: settings.timeout_secs,
        prompt_cache: settings.prompt_cache,
        approval_mode: settings.approval_mode,
        remembered_approvals: settings.approval_rules.len(),
        demo_mode: settings.api_key.is_none(),
        available_tools: available_tool_views(state),
    }
}

fn task_summary(task: &Task) -> TaskSummary {
    TaskSummary {
        id: task.id.clone(),
        title: task.title.clone(),
        workspace: task.workspace.clone(),
        status: task.status.clone(),
        updated_at: task.updated_at,
        demo_mode: task.demo_mode,
        archived: task.archived,
        error: task.error.clone(),
    }
}

fn task_summaries(state: &AppState, archived: bool) -> Result<Vec<TaskSummary>, String> {
    let tasks = state
        .tasks
        .read()
        .map_err(|_| "Task lock is poisoned".to_string())?;
    let mut summaries: Vec<TaskSummary> = tasks
        .values()
        .filter(|task| task.archived == archived)
        .map(task_summary)
        .collect();
    summaries.sort_by_key(|task| Reverse(task.updated_at));
    Ok(summaries)
}

fn project_task_counts(state: &AppState) -> HashMap<String, usize> {
    state
        .tasks
        .read()
        .map(|tasks| {
            let mut counts = HashMap::with_capacity(tasks.len());
            for task in tasks.values() {
                *counts
                    .entry(project_store::path_key(Path::new(&task.workspace)))
                    .or_insert(0) += 1;
            }
            counts
        })
        .unwrap_or_default()
}

fn project_summary(
    record: &project_store::ProjectRecord,
    task_counts: &HashMap<String, usize>,
) -> ProjectSummary {
    let directory = PathBuf::from(&record.directory);
    let key = project_store::path_key(&directory);
    ProjectSummary {
        task_count: task_counts.get(&key).copied().unwrap_or_default(),
        id: key,
        directory: record.directory.clone(),
        name: project_store::display_name(&directory),
        created_at: record.created_at,
        updated_at: record.updated_at,
    }
}

fn project_summaries(
    state: &AppState,
    recently_closed: bool,
) -> Result<Vec<ProjectSummary>, String> {
    let projects = state
        .projects
        .read()
        .map_err(|_| "Project lock is poisoned".to_string())?;
    let records = if recently_closed {
        projects.recently_closed.clone()
    } else {
        projects.open.clone()
    };
    drop(projects);
    let task_counts = project_task_counts(state);
    Ok(records
        .iter()
        .map(|record| project_summary(record, &task_counts))
        .collect())
}

fn open_project_internal(state: &AppState, raw_path: &str) -> Result<ProjectSummary, String> {
    let directory = project_store::normalize_directory(raw_path)?;
    let timestamp = now();
    let changed = {
        let mut projects = state
            .projects
            .write()
            .map_err(|_| "Project lock is poisoned".to_string())?;
        projects.open(&directory, timestamp)
    };
    if changed {
        state.persist_projects()?;
    }
    let record = state
        .projects
        .read()
        .map_err(|_| "Project lock is poisoned".to_string())?;
    let record = record
        .open
        .iter()
        .find(|record| {
            project_store::path_key(Path::new(&record.directory))
                == project_store::path_key(&directory)
        })
        .cloned()
        .ok_or_else(|| "Project was not recorded after opening.".to_string())?;
    let task_counts = project_task_counts(state);
    Ok(project_summary(&record, &task_counts))
}

#[tauri::command(rename_all = "camelCase")]
async fn list_tasks(state: State<'_, AppState>) -> Result<Vec<TaskSummary>, String> {
    task_summaries(&state, false)
}

#[tauri::command(rename_all = "camelCase")]
async fn list_archived_tasks(state: State<'_, AppState>) -> Result<Vec<TaskSummary>, String> {
    task_summaries(&state, true)
}

#[tauri::command(rename_all = "camelCase")]
async fn list_projects(state: State<'_, AppState>) -> Result<Vec<ProjectSummary>, String> {
    project_summaries(&state, false)
}

#[tauri::command(rename_all = "camelCase")]
async fn list_recent_projects(state: State<'_, AppState>) -> Result<Vec<ProjectSummary>, String> {
    project_summaries(&state, true)
}

#[tauri::command(rename_all = "camelCase")]
async fn open_project(state: State<'_, AppState>, path: String) -> Result<ProjectSummary, String> {
    open_project_internal(&state, &path)
}

#[tauri::command(rename_all = "camelCase")]
async fn pick_project(
    state: State<'_, AppState>,
    kind: String,
) -> Result<Option<ProjectSummary>, String> {
    let Some(path) = project_store::pick_path(&kind)? else {
        return Ok(None);
    };
    open_project_internal(&state, &path.to_string_lossy()).map(Some)
}

#[tauri::command(rename_all = "camelCase")]
async fn close_project(state: State<'_, AppState>, directory: String) -> Result<bool, String> {
    let directory = project_store::normalize_directory(&directory)?;
    let changed = {
        let mut projects = state
            .projects
            .write()
            .map_err(|_| "Project lock is poisoned".to_string())?;
        projects.close(&directory, now())
    };
    if changed {
        state.persist_projects()?;
    }
    Ok(changed)
}

#[tauri::command(rename_all = "camelCase")]
async fn touch_project(state: State<'_, AppState>, directory: String) -> Result<bool, String> {
    let directory = project_store::normalize_directory(&directory)?;
    let changed = {
        let mut projects = state
            .projects
            .write()
            .map_err(|_| "Project lock is poisoned".to_string())?;
        projects.touch(&directory, now())
    };
    if changed {
        state.persist_projects()?;
    }
    Ok(changed)
}

#[tauri::command(rename_all = "camelCase")]
async fn get_task(state: State<'_, AppState>, task_id: String) -> Result<Task, String> {
    task_snapshot(&state, &task_id)
}

#[tauri::command(rename_all = "camelCase")]
async fn get_task_events(
    state: State<'_, AppState>,
    task_id: String,
    after: Option<i64>,
) -> Result<TaskEventPage, String> {
    let data_dir = state
        .storage_dir
        .read()
        .map_err(|_| "Storage lock is poisoned".to_string())?
        .clone()
        .ok_or_else(|| "Task storage is not initialized.".to_string())?;
    tauri::async_runtime::spawn_blocking(move || task_events::read_page(&data_dir, &task_id, after))
        .await
        .map_err(|error| format!("Unable to read task events: {error}"))?
}

fn attachment_data_directory(state: &AppState) -> Result<PathBuf, String> {
    state
        .storage_dir
        .read()
        .map_err(|_| "Storage lock is poisoned".to_string())?
        .clone()
        .ok_or_else(|| "Attachment storage is not initialized.".to_string())
}

fn task_title(prompt: &str, attachment_count: usize) -> String {
    if !prompt.trim().is_empty() {
        return make_title(prompt);
    }
    if attachment_count == 1 {
        "Attached file".to_string()
    } else {
        format!("{} attached files", attachment_count)
    }
}

fn store_task_attachments(
    state: &AppState,
    task_id: &str,
    encoded_inputs: &[attachments::AttachmentInput],
    path_inputs: &[attachments::AttachmentPathInput],
) -> Result<Vec<attachments::AttachmentRef>, String> {
    if encoded_inputs.is_empty() && path_inputs.is_empty() {
        return Ok(Vec::new());
    }
    let data_dir = attachment_data_directory(state)?;
    let references =
        attachments::store_inputs_and_paths(&data_dir, task_id, encoded_inputs, path_inputs)?;
    let model = match state.settings.read() {
        Ok(settings) => settings.model.clone(),
        Err(_) => {
            attachments::remove_refs(&data_dir, &references);
            return Err("Settings lock is poisoned".to_string());
        }
    };
    if references
        .iter()
        .any(|reference| attachments::is_image(&reference.mime))
        && !llm::model_supports_images(&model)
    {
        attachments::remove_refs(&data_dir, &references);
        return Err(format!(
            "The configured model does not support image attachments. Remove the images or choose a vision model. Current model: {model}"
        ));
    }
    Ok(references)
}

fn remove_stored_attachments(state: &AppState, references: &[attachments::AttachmentRef]) {
    if references.is_empty() {
        return;
    }
    if let Ok(data_dir) = attachment_data_directory(state) {
        attachments::remove_refs(&data_dir, references);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentPreview {
    pub mime: String,
    pub data_url: String,
}

#[tauri::command(rename_all = "camelCase")]
async fn get_attachment_preview(
    state: State<'_, AppState>,
    task_id: String,
    attachment_id: String,
) -> Result<AttachmentPreview, String> {
    let task = task_snapshot(&state, &task_id)?;
    let attachment = task
        .messages
        .iter()
        .flat_map(|message| message.attachments.iter())
        .find(|attachment| attachment.id == attachment_id)
        .cloned()
        .ok_or_else(|| "Attachment not found.".to_string())?;
    if !attachments::is_image(&attachment.mime) {
        return Err("Only image attachments can be previewed.".to_string());
    }
    let bytes = attachments::read(&attachment_data_directory(&state)?, &attachment)?;
    Ok(AttachmentPreview {
        mime: attachment.mime.clone(),
        data_url: format!("data:{};base64,{}", attachment.mime, base64_encode(&bytes)),
    })
}

fn create_task_internal(
    app: &AppHandle,
    state: &AppState,
    prompt: String,
    attachment_inputs: Vec<attachments::AttachmentInput>,
    attachment_paths: Vec<attachments::AttachmentPathInput>,
    workspace_raw: Option<String>,
) -> Result<Task, String> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() && attachment_inputs.is_empty() && attachment_paths.is_empty() {
        return Err("Add a prompt or attach at least one file.".to_string());
    }
    if !api_key_configured(state)? {
        return Err(API_KEY_REQUIRED_MESSAGE.to_string());
    }
    let fallback_workspace = workspace_root();
    let workspace = match workspace_raw.as_deref() {
        Some(raw) => project_store::normalize_directory(raw)?,
        None => project_store::normalize_directory(&fallback_workspace.to_string_lossy())?,
    };
    open_project_internal(state, &workspace.to_string_lossy())?;
    let demo_mode = false;
    let task_id = new_id("task");
    let attachment_refs =
        store_task_attachments(state, &task_id, &attachment_inputs, &attachment_paths)?;
    let created_at = now();
    let task = Task {
        id: task_id.clone(),
        title: task_title(&prompt, attachment_refs.len()),
        prompt: prompt.clone(),
        workspace: project_store::display_directory(&workspace),
        status: AgentStatus::Idle,
        created_at,
        updated_at: created_at,
        demo_mode,
        archived: false,
        agent_name: default_agent_name(),
        agent_kind: infer_agent_kind(&prompt),
        messages: vec![TaskMessage {
            id: new_id("msg"),
            task_id: task_id.clone(),
            role: "user".to_string(),
            content: prompt.clone(),
            created_at,
            streaming: false,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            base64_image: None,
            attachments: attachment_refs.clone(),
        }],
        memory: vec![AgentMemoryEntry {
            id: new_id("memory"),
            role: "user".to_string(),
            content: prompt.clone(),
            created_at,
            tool_call_id: None,
            tool_names: Vec::new(),
            tool_calls: Vec::new(),
            name: None,
            base64_image: None,
            attachments: attachment_refs.clone(),
        }],
        plans: Vec::new(),
        active_plan_id: None,
        steps: Vec::new(),
        tool_calls: Vec::new(),
        approval_requests: Vec::new(),
        llm_usage: llm::TokenUsage::default(),
        final_answer: None,
        error: None,
        event_seq: 0,
        persistence_revision: 1,
    };
    let insert_result = state
        .tasks
        .write()
        .map_err(|_| "Task lock is poisoned".to_string());
    let mut tasks = match insert_result {
        Ok(tasks) => tasks,
        Err(error) => {
            remove_stored_attachments(state, &attachment_refs);
            return Err(error);
        }
    };
    tasks.insert(task_id.clone(), task.clone());
    drop(tasks);

    let task = match task_snapshot(state, &task_id) {
        Ok(task) => task,
        Err(error) => {
            if let Ok(mut tasks) = state.tasks.write() {
                tasks.remove(&task_id);
            }
            remove_stored_attachments(state, &attachment_refs);
            return Err(error);
        }
    };
    if let Err(error) = state.persist_task(&task_id) {
        if let Ok(mut tasks) = state.tasks.write() {
            tasks.remove(&task_id);
        }
        remove_stored_attachments(state, &attachment_refs);
        return Err(error);
    }
    emit_task_event(app, state, &task_id, "task_created", task.clone());
    emit_task_event(
        app,
        state,
        &task_id,
        "task_message",
        task.messages.first().cloned().unwrap_or(TaskMessage {
            id: new_id("msg"),
            task_id: task_id.clone(),
            role: "user".to_string(),
            content: String::new(),
            created_at,
            streaming: false,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        }),
    );
    start_task(app, state, task_id);
    Ok(task)
}

#[tauri::command(rename_all = "camelCase")]
async fn create_task(
    app: AppHandle,
    state: State<'_, AppState>,
    prompt: String,
    attachment_inputs: Option<Vec<attachments::AttachmentInput>>,
    attachment_paths: Option<Vec<attachments::AttachmentPathInput>>,
    workspace: Option<String>,
) -> Result<Task, String> {
    create_task_internal(
        &app,
        &state,
        prompt,
        attachment_inputs.unwrap_or_default(),
        attachment_paths.unwrap_or_default(),
        workspace,
    )
}

fn continue_task_internal(
    app: &AppHandle,
    state: &AppState,
    task_id: String,
    prompt: String,
    attachment_inputs: Vec<attachments::AttachmentInput>,
    attachment_paths: Vec<attachments::AttachmentPathInput>,
) -> Result<Task, String> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() && attachment_inputs.is_empty() && attachment_paths.is_empty() {
        return Err("Add a prompt or attach at least one file.".to_string());
    }
    if !api_key_configured(state)? {
        return Err(API_KEY_REQUIRED_MESSAGE.to_string());
    }
    if state
        .running
        .read()
        .map_err(|_| "Running task lock is poisoned".to_string())?
        .contains_key(&task_id)
    {
        return Err(
            "Wait for the current task to finish before sending another message.".to_string(),
        );
    }

    {
        let tasks = state
            .tasks
            .read()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get(&task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        if task.archived {
            return Err(
                "Restore the archived task before continuing the conversation.".to_string(),
            );
        }
    }

    let attachment_refs =
        store_task_attachments(state, &task_id, &attachment_inputs, &attachment_paths)?;
    let created_at = now();
    let message = TaskMessage {
        id: new_id("msg"),
        task_id: task_id.clone(),
        role: "user".to_string(),
        content: prompt.clone(),
        created_at,
        streaming: false,
        parts: Vec::new(),
        tool_calls: Vec::new(),
        tool_call_id: None,
        name: None,
        base64_image: None,
        attachments: attachment_refs.clone(),
    };
    let memory = AgentMemoryEntry {
        id: new_id("memory"),
        role: "user".to_string(),
        content: prompt,
        created_at,
        tool_call_id: None,
        tool_names: Vec::new(),
        tool_calls: Vec::new(),
        name: None,
        base64_image: None,
        attachments: attachment_refs,
    };
    let update_result = (|| {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(&task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        if task.archived {
            return Err(
                "Restore the archived task before continuing the conversation.".to_string(),
            );
        }
        // Repair legacy and interrupted tool turns before appending a new user turn.
        // The next user message must never appear before an unresolved tool response.
        repair_task_record(task);
        task.status = AgentStatus::Idle;
        task.error = None;
        task.final_answer = None;
        task.demo_mode = false;
        task.plans.clear();
        task.active_plan_id = None;
        task.messages.push(message.clone());
        task.memory.push(memory);
        trim_memory_to_budget(&mut task.memory, MAX_MEMORY_ENTRIES);
        touch_task(task);
        Ok::<(), String>(())
    })();
    if let Err(error) = update_result {
        remove_stored_attachments(state, &message.attachments);
        return Err(error);
    }
    let task = task_snapshot(state, &task_id)?;
    state.persist_task(&task_id)?;
    emit_task_event(
        app,
        state,
        &task_id,
        "task_status",
        TaskStatusEvent {
            task_id: task_id.clone(),
            status: AgentStatus::Idle,
            updated_at: task.updated_at,
            error: None,
        },
    );
    emit_task_event(app, state, &task_id, "task_message", message);
    start_task(app, state, task_id);
    Ok(task)
}

#[tauri::command(rename_all = "camelCase")]
async fn continue_task(
    app: AppHandle,
    state: State<'_, AppState>,
    task_id: String,
    prompt: String,
    attachment_inputs: Option<Vec<attachments::AttachmentInput>>,
    attachment_paths: Option<Vec<attachments::AttachmentPathInput>>,
) -> Result<Task, String> {
    continue_task_internal(
        &app,
        &state,
        task_id,
        prompt,
        attachment_inputs.unwrap_or_default(),
        attachment_paths.unwrap_or_default(),
    )
}

#[tauri::command(rename_all = "camelCase")]
async fn stop_task(state: State<'_, AppState>, task_id: String) -> Result<bool, String> {
    let running = state
        .running
        .read()
        .map_err(|_| "Running task lock is poisoned".to_string())?;
    if let Some(cancel) = running.get(&task_id) {
        cancel.cancel();
        Ok(true)
    } else {
        Ok(false)
    }
}

#[tauri::command(rename_all = "camelCase")]
async fn retry_task(
    app: AppHandle,
    state: State<'_, AppState>,
    task_id: String,
) -> Result<Task, String> {
    if state
        .running
        .read()
        .map_err(|_| "Running task lock is poisoned".to_string())?
        .contains_key(&task_id)
    {
        return Err("Stop the running task before retrying.".to_string());
    }
    if !api_key_configured(&state)? {
        return Err(API_KEY_REQUIRED_MESSAGE.to_string());
    }
    let demo_mode = false;
    let (_updated_task, retry_message) = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(&task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.status = AgentStatus::Idle;
        task.error = None;
        task.final_answer = None;
        task.demo_mode = demo_mode;
        task.archived = false;
        let initial_attachments = task
            .messages
            .iter()
            .find(|message| message.role == "user")
            .map(|message| message.attachments.clone())
            .unwrap_or_default();
        task.memory = vec![AgentMemoryEntry {
            id: new_id("memory"),
            role: "user".to_string(),
            content: task.prompt.clone(),
            created_at: now(),
            tool_call_id: None,
            tool_names: Vec::new(),
            tool_calls: Vec::new(),
            name: None,
            base64_image: None,
            attachments: initial_attachments,
        }];
        task.plans.clear();
        task.active_plan_id = None;
        task.steps.clear();
        task.tool_calls.clear();
        task.approval_requests.clear();
        let retry_message = TaskMessage {
            id: new_id("msg"),
            task_id: task_id.clone(),
            role: "system".to_string(),
            content: "Retrying this task with the current settings.".to_string(),
            created_at: now(),
            streaming: false,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        };
        task.messages.push(retry_message.clone());
        touch_task(task);
        (task.clone(), retry_message)
    };
    let task = task_snapshot(&state, &task_id)?;
    state.persist_task(&task_id)?;
    emit_task_event(
        &app,
        &state,
        &task_id,
        "task_status",
        TaskStatusEvent {
            task_id: task_id.clone(),
            status: AgentStatus::Idle,
            updated_at: task.updated_at,
            error: None,
        },
    );
    emit_task_event(&app, &state, &task_id, "task_message", retry_message);
    start_task(&app, &state, task_id);
    Ok(task)
}

#[tauri::command(rename_all = "camelCase")]
async fn archive_task(
    app: AppHandle,
    state: State<'_, AppState>,
    task_id: String,
) -> Result<TaskSummary, String> {
    if state
        .running
        .read()
        .map_err(|_| "Running task lock is poisoned".to_string())?
        .contains_key(&task_id)
    {
        return Err("Stop the running task before archiving it.".to_string());
    }

    let summary = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(&task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.archived = true;
        touch_task(task);
        task_summary(task)
    };
    state.persist_task(&task_id)?;
    emit_task_event(&app, &state, &task_id, "task_summary", summary.clone());
    Ok(summary)
}

#[tauri::command(rename_all = "camelCase")]
async fn restore_task(
    app: AppHandle,
    state: State<'_, AppState>,
    task_id: String,
) -> Result<TaskSummary, String> {
    let summary = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(&task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        task.archived = false;
        touch_task(task);
        task_summary(task)
    };
    state.persist_task(&task_id)?;
    emit_task_event(&app, &state, &task_id, "task_summary", summary.clone());
    Ok(summary)
}

#[tauri::command(rename_all = "camelCase")]
async fn delete_task(state: State<'_, AppState>, task_id: String) -> Result<TaskSummary, String> {
    if state
        .running
        .read()
        .map_err(|_| "Running task lock is poisoned".to_string())?
        .contains_key(&task_id)
    {
        return Err("Stop the running task before deleting it.".to_string());
    }

    let (task, delete_revision) = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        tasks
            .remove(&task_id)
            .map(|task| {
                let delete_revision = task.persistence_revision.saturating_add(1);
                (task, delete_revision)
            })
            .ok_or_else(|| "Task not found".to_string())?
    };
    let summary = task_summary(&task);
    state.persist_deleted_task(&task_id, delete_revision, summary.clone())?;
    if let Ok(data_dir) = attachment_data_directory(&state) {
        if let Err(error) = attachments::remove_task(&data_dir, &task_id) {
            warn!(task_id = %task_id, error = %error, "Unable to remove task attachments");
        }
    }
    Ok(summary)
}

#[tauri::command(rename_all = "camelCase")]
async fn get_settings(state: State<'_, AppState>) -> Result<SettingsView, String> {
    let settings = state
        .settings
        .read()
        .map_err(|_| "Settings lock is poisoned".to_string())?;
    Ok(settings_view(&settings, Some(&state)))
}

#[tauri::command(rename_all = "camelCase")]
async fn update_settings(
    state: State<'_, AppState>,
    input: SettingsInput,
) -> Result<SettingsView, String> {
    let mut settings = state
        .settings
        .write()
        .map_err(|_| "Settings lock is poisoned".to_string())?;
    let api_base_url = input.api_base_url.trim();
    if !api_base_url.is_empty() {
        settings.api_base_url = api_base_url.trim_end_matches('/').to_string();
    }
    let model = input.model.trim();
    if !model.is_empty() {
        settings.model = model.to_string();
    }
    if let Some(api_key) = input.api_key {
        let api_key = api_key.trim();
        settings.api_key = (!api_key.is_empty()).then(|| api_key.to_string());
    }
    if let Some(max_steps) = input.max_steps {
        settings.max_steps = normalize_max_steps(max_steps);
    }
    if let Some(timeout_secs) = input.timeout_secs {
        settings.timeout_secs = timeout_secs.clamp(5, 120);
    }
    if let Some(prompt_cache) = input.prompt_cache {
        settings.prompt_cache = prompt_cache;
    }
    if let Some(approval_mode) = input.approval_mode {
        settings.approval_mode = approval_mode;
    }
    if input.clear_approval_rules {
        settings.approval_rules.clear();
    }
    let view = settings_view(&settings, Some(&state));
    drop(settings);
    state.persist_settings()?;
    Ok(view)
}

#[tauri::command(rename_all = "camelCase")]
async fn respond_to_approval(
    app: AppHandle,
    state: State<'_, AppState>,
    task_id: String,
    approval_id: String,
    approved: bool,
    remember: Option<bool>,
) -> Result<bool, String> {
    let waiter = {
        let mut waiters = state
            .approval_waiters
            .write()
            .map_err(|_| "Approval lock is poisoned".to_string())?;
        if waiters
            .get(&approval_id)
            .is_some_and(|waiter| waiter.task_id == task_id)
        {
            waiters.remove(&approval_id)
        } else {
            None
        }
    };
    if let Some(waiter) = waiter {
        if approved && remember.unwrap_or(false) {
            if let Err(error) = remember_approval_rule(&state, &task_id, &approval_id) {
                warn!(
                    task_id = %task_id,
                    approval_id = %approval_id,
                    error = %error,
                    "Approval decision delivered but remembered rule was not persisted"
                );
            }
        }
        update_approval_status(
            &app,
            &state,
            &task_id,
            &approval_id,
            if approved { "approved" } else { "rejected" },
        );
        let _ = waiter.sender.send(approved);
        Ok(true)
    } else {
        Ok(false)
    }
}

fn remember_approval_rule(
    state: &AppState,
    task_id: &str,
    approval_id: &str,
) -> Result<(), String> {
    let request = state
        .tasks
        .read()
        .map_err(|_| "Task lock is poisoned".to_string())?
        .get(task_id)
        .and_then(|task| {
            task.approval_requests
                .iter()
                .find(|request| request.id == approval_id)
                .cloned()
        });
    let Some(request) = request else {
        return Ok(());
    };
    let (Some(action), Some(resource)) = (request.remember_action, request.remember_pattern) else {
        return Ok(());
    };
    let mut settings = state
        .settings
        .write()
        .map_err(|_| "Settings lock is poisoned".to_string())?;
    settings.approval_rules = sanitize_rules(
        settings
            .approval_rules
            .iter()
            .cloned()
            .chain(std::iter::once(ApprovalRule {
                workspace: state
                    .tasks
                    .read()
                    .ok()
                    .and_then(|tasks| tasks.get(task_id).map(|task| task.workspace.clone()))
                    .unwrap_or_else(tool_policy::workspace_key),
                action,
                resource,
            }))
            .collect(),
    );
    drop(settings);
    state.persist_settings()
}

fn spawn_a2a_server(app: &AppHandle, state: &AppState) {
    let Some(address) = first_env_value(&["RUSTPILOT_A2A_ADDR"]) else {
        return;
    };
    let mut card = protocol::default_agent_card();
    card.url = format!("http://{address}/");
    let app_handle = app.clone();
    let app_state = state.clone();
    let handler: protocol::A2AHandler = Arc::new(move |query, _session_id, cancel| {
        let app = app_handle.clone();
        let state = app_state.clone();
        Box::pin(async move {
            let task = match create_task_internal(&app, &state, query, Vec::new(), Vec::new(), None)
            {
                Ok(task) => task,
                Err(error) => return protocol::A2AResponse::error(error),
            };
            loop {
                if cancel.is_cancelled() {
                    if let Ok(running) = state.running.read() {
                        if let Some(task_cancel) = running.get(&task.id) {
                            task_cancel.cancel();
                        }
                    }
                    return protocol::A2AResponse::error("A2A task cancelled.");
                }
                match task_snapshot(&state, &task.id) {
                    Ok(snapshot) => match snapshot.status {
                        AgentStatus::Completed => {
                            return protocol::A2AResponse::completed(
                                snapshot.final_answer.unwrap_or_default(),
                            );
                        }
                        AgentStatus::Failed => {
                            return protocol::A2AResponse::error(
                                snapshot
                                    .error
                                    .unwrap_or_else(|| "A2A task failed.".to_string()),
                            );
                        }
                        AgentStatus::Cancelled => {
                            return protocol::A2AResponse::error("A2A task cancelled.");
                        }
                        AgentStatus::Idle
                        | AgentStatus::Planning
                        | AgentStatus::Executing
                        | AgentStatus::Verifying
                        | AgentStatus::WaitingApproval => {}
                    },
                    Err(error) => return protocol::A2AResponse::error(error),
                }
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                }
            }
        })
    });
    let server = protocol::A2AServer::new(card, handler);
    let bind_address = address.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(error) = server.bind(&bind_address, CancellationToken::new()).await {
            error!("A2A server stopped: {error}");
        }
    });
    info!("A2A server requested at {address}");
}

pub fn run() {
    tauri::Builder::default()
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            list_tasks,
            list_archived_tasks,
            list_projects,
            list_recent_projects,
            open_project,
            pick_project,
            close_project,
            touch_project,
            get_task,
            get_task_events,
            get_attachment_preview,
            create_task,
            continue_task,
            stop_task,
            retry_task,
            archive_task,
            restore_task,
            delete_task,
            get_settings,
            update_settings,
            respond_to_approval
        ])
        .setup(|app| {
            let state = app.state::<AppState>();
            state.initialize(app.handle()).map_err(|error| {
                error!("Unable to initialize RustPilot: {error}");
                error
            })?;
            spawn_a2a_server(app.handle(), &state);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running RustPilot");
}

#[cfg(test)]
mod lib_tests;
