use std::{
    cmp::Reverse,
    collections::{HashMap, HashSet, VecDeque},
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    sync::{Arc, Mutex, RwLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::Client;
use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::{oneshot, Mutex as AsyncMutex, Notify},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use uuid::Uuid;

pub mod agent;
pub mod agents;
pub mod attachments;
pub mod bedrock;
pub mod config;
pub mod flow;
pub mod llm;
pub mod mcp_server;
pub mod path_guard;
pub mod protocol;
pub mod react;
pub mod schema;
pub mod tool;

const LEGACY_TASK_FILE: &str = "tasks.json";
const TASK_DATABASE_FILE: &str = "tasks.db";
const TASK_SCHEMA_VERSION: i64 = 1;
const TASK_EVENT_COMPACTION_BYTES: u64 = 128 * 1024;
const TASK_WRITE_BATCH_DELAY_MS: u64 = 50;
const SETTINGS_FILE: &str = "settings.json";
const MAX_OUTPUT_CHARS: usize = 16_000;
const MAX_MEMORY_ENTRIES: usize = 100;
const APPROVAL_TIMEOUT_SECS: u64 = 300;
const API_KEY_REQUIRED_MESSAGE: &str = "Configure an API key in Settings before sending a task.";

fn ensure_writable_directory(preferred: PathBuf, fallback_name: &str) -> Result<PathBuf, String> {
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

fn preserve_invalid_task_file(path: &Path) {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(LEGACY_TASK_FILE);
    let backup = path.with_file_name(format!("{file_name}.corrupt-{}", now()));
    match fs::copy(path, &backup) {
        Ok(_) => warn!(
            path = %path.display(),
            backup = %backup.display(),
            "Preserved an unreadable task file"
        ),
        Err(error) => warn!(
            path = %path.display(),
            error = %error,
            "Unable to preserve an unreadable task file"
        ),
    }
}

fn legacy_task_temp_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

fn legacy_task_backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

fn load_legacy_task_records(path: &Path) -> Option<Vec<Task>> {
    let candidates = [
        (path.to_path_buf(), false),
        (legacy_task_temp_path(path), true),
        (legacy_task_backup_path(path), true),
    ];
    for (candidate, is_recovery_file) in candidates {
        let contents = match fs::read_to_string(&candidate) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                warn!(path = %candidate.display(), error = %error, "Unable to read task file");
                continue;
            }
        };
        match serde_json::from_str::<Vec<Task>>(&contents) {
            Ok(tasks) => {
                if is_recovery_file {
                    warn!(path = %candidate.display(), "Recovered tasks from a temporary file");
                }
                return Some(tasks);
            }
            Err(error) => {
                if candidate == *path {
                    preserve_invalid_task_file(&candidate);
                } else {
                    warn!(
                        path = %candidate.display(),
                        error = %error,
                        "Unable to parse task recovery file"
                    );
                }
            }
        }
    }
    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistedStreamEvent {
    TextDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
    },
}

#[derive(Debug)]
struct LoadedTaskStore {
    tasks: HashMap<String, Task>,
    event_bytes: HashMap<String, u64>,
    connection: Connection,
}

fn task_database_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TASK_DATABASE_FILE)
}

fn sqlite_table_columns(connection: &Connection, table: &str) -> Result<HashSet<String>, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| format!("Unable to inspect task database schema: {error}"))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("Unable to read task database schema: {error}"))?;
    rows.collect::<Result<HashSet<_>, _>>()
        .map_err(|error| format!("Unable to collect task database schema: {error}"))
}

fn open_task_database(path: &Path) -> Result<Connection, String> {
    let connection = Connection::open(path)
        .map_err(|error| format!("Unable to open task database {}: {error}", path.display()))?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| format!("Unable to configure task database timeout: {error}"))?;
    connection
        .execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA wal_autocheckpoint = 128;
             PRAGMA journal_size_limit = 262144;
             CREATE TABLE IF NOT EXISTS task_state (
                 id TEXT PRIMARY KEY NOT NULL,
                 updated_at INTEGER NOT NULL,
                 data TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS task_event (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 task_id TEXT NOT NULL REFERENCES task_state(id) ON DELETE CASCADE,
                 message_id TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 payload TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS task_event_task_seq_idx
                 ON task_event(task_id, seq);",
        )
        .map_err(|error| format!("Unable to initialize task database schema: {error}"))?;

    let task_state_columns = sqlite_table_columns(&connection, "task_state")?;
    if !task_state_columns.contains("updated_at") {
        connection
            .execute(
                "ALTER TABLE task_state ADD COLUMN updated_at INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| format!("Unable to migrate task state schema: {error}"))?;
    }
    if !task_state_columns.contains("data") {
        return Err("Task database is missing the task state data column".to_string());
    }

    let event_columns = sqlite_table_columns(&connection, "task_event")?;
    if !event_columns.contains("message_id") {
        connection
            .execute(
                "ALTER TABLE task_event ADD COLUMN message_id TEXT NOT NULL DEFAULT ''",
                [],
            )
            .map_err(|error| format!("Unable to migrate task event message ids: {error}"))?;
    }
    if !event_columns.contains("kind") || !event_columns.contains("payload") {
        return Err("Task database is missing the task event payload columns".to_string());
    }
    let user_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|error| format!("Unable to read task database version: {error}"))?;
    if user_version > TASK_SCHEMA_VERSION {
        return Err(format!(
            "Task database version {user_version} is newer than supported version {TASK_SCHEMA_VERSION}"
        ));
    }
    connection
        .execute_batch(&format!("PRAGMA user_version = {TASK_SCHEMA_VERSION};"))
        .map_err(|error| format!("Unable to record task database version: {error}"))?;
    Ok(connection)
}

fn task_state_json(task: &Task) -> Result<String, String> {
    serde_json::to_string(task)
        .map_err(|error| format!("Unable to encode task {}: {error}", task.id))
}

fn insert_task_state(transaction: &Transaction<'_>, task: &Task) -> Result<(), String> {
    let data = task_state_json(task)?;
    transaction
        .execute(
            "INSERT INTO task_state(id, updated_at, data) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET updated_at = excluded.updated_at, data = excluded.data",
            params![task.id, task.updated_at, data],
        )
        .map_err(|error| format!("Unable to persist task {}: {error}", task.id))?;
    Ok(())
}

fn remove_legacy_task_files(path: &Path) {
    for candidate in [
        path.to_path_buf(),
        legacy_task_temp_path(path),
        legacy_task_backup_path(path),
    ] {
        if let Err(error) = fs::remove_file(&candidate) {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!(path = %candidate.display(), error = %error, "Unable to remove migrated task file");
            }
        }
    }
}

fn load_task_store(data_dir: &Path) -> Result<LoadedTaskStore, String> {
    let database_path = task_database_path(data_dir);
    let database_existed = database_path.exists();
    let mut connection = open_task_database(&database_path)?;
    let mut tasks = HashMap::new();

    {
        let mut statement = connection
            .prepare("SELECT id, data FROM task_state ORDER BY id")
            .map_err(|error| format!("Unable to read task state: {error}"))?;
        let mut rows = statement
            .query([])
            .map_err(|error| format!("Unable to query task state: {error}"))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("Unable to iterate task state: {error}"))?
        {
            let id: String = row
                .get(0)
                .map_err(|error| format!("Unable to read task id: {error}"))?;
            let data: String = row
                .get(1)
                .map_err(|error| format!("Unable to read task data: {error}"))?;
            let task: Task = serde_json::from_str(&data)
                .map_err(|error| format!("Unable to decode task {id}: {error}"))?;
            if task.id != id {
                return Err(format!("Task database id mismatch for {id}"));
            }
            tasks.insert(id, task);
        }
    }

    let mut event_bytes = HashMap::new();
    {
        let mut statement = connection
            .prepare("SELECT task_id, message_id, kind, payload FROM task_event ORDER BY seq")
            .map_err(|error| format!("Unable to read task events: {error}"))?;
        let mut rows = statement
            .query([])
            .map_err(|error| format!("Unable to query task events: {error}"))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| format!("Unable to iterate task events: {error}"))?
        {
            let task_id: String = row
                .get(0)
                .map_err(|error| format!("Unable to read task event id: {error}"))?;
            let message_id: String = row
                .get(1)
                .map_err(|error| format!("Unable to read task event message id: {error}"))?;
            let kind: String = row
                .get(2)
                .map_err(|error| format!("Unable to read task event kind: {error}"))?;
            let payload: String = row
                .get(3)
                .map_err(|error| format!("Unable to read task event payload: {error}"))?;
            if kind != "stream" {
                return Err(format!("Unsupported task event kind: {kind}"));
            }
            let event: PersistedStreamEvent = serde_json::from_str(&payload)
                .map_err(|error| format!("Unable to decode task event for {task_id}: {error}"))?;
            let task = tasks
                .get_mut(&task_id)
                .ok_or_else(|| format!("Task event references missing task {task_id}"))?;
            apply_persisted_stream_event(task, &task_id, &message_id, &event)?;
            *event_bytes.entry(task_id).or_insert(0) += payload.len() as u64 + 32;
        }
    }

    let legacy_path = data_dir.join(LEGACY_TASK_FILE);
    if !database_existed {
        if let Some(legacy_tasks) = load_legacy_task_records(&legacy_path) {
            let transaction = connection
                .transaction()
                .map_err(|error| format!("Unable to begin legacy task migration: {error}"))?;
            for task in &legacy_tasks {
                insert_task_state(&transaction, task)?;
            }
            transaction
                .commit()
                .map_err(|error| format!("Unable to commit legacy task migration: {error}"))?;
            tasks = legacy_tasks
                .into_iter()
                .map(|task| (task.id.clone(), task))
                .collect();
            event_bytes.clear();
            remove_legacy_task_files(&legacy_path);
            info!("Migrated legacy tasks.json into SQLite task storage");
        }
    } else {
        remove_legacy_task_files(&legacy_path);
    }

    Ok(LoadedTaskStore {
        tasks,
        event_bytes,
        connection,
    })
}

fn persisted_stream_event(event: &llm::StreamEvent) -> Option<PersistedStreamEvent> {
    match event {
        llm::StreamEvent::TextDelta(delta) if !delta.is_empty() => {
            Some(PersistedStreamEvent::TextDelta(delta.clone()))
        }
        llm::StreamEvent::ToolCallDelta {
            index, id, name, ..
        } => Some(PersistedStreamEvent::ToolCallDelta {
            index: *index,
            id: id.clone(),
            name: name.clone(),
        }),
        _ => None,
    }
}

fn apply_persisted_stream_event(
    task: &mut Task,
    task_id: &str,
    message_id: &str,
    event: &PersistedStreamEvent,
) -> Result<(), String> {
    let fallback_message_id = task.messages.last().map(|message| message.id.clone());
    let message_id = if message_id.is_empty() {
        fallback_message_id.as_deref().unwrap_or_default()
    } else {
        message_id
    };
    let message = task
        .messages
        .iter_mut()
        .find(|message| message.id == message_id)
        .ok_or_else(|| {
            format!("Task event references missing message {message_id} in {task_id}")
        })?;
    message.streaming = true;
    match event {
        PersistedStreamEvent::TextDelta(delta) => {
            apply_stream_event(message, &llm::StreamEvent::TextDelta(delta.clone()));
        }
        PersistedStreamEvent::ToolCallDelta { index, id, name } => {
            apply_stream_event(
                message,
                &llm::StreamEvent::ToolCallDelta {
                    index: *index,
                    id: id.clone(),
                    name: name.clone(),
                    arguments: None,
                },
            );
        }
    }
    Ok(())
}

enum PendingTaskWrite {
    Upsert {
        task: Task,
        stream_events: Vec<PendingStreamEvent>,
    },
    Stream {
        events: Vec<PendingStreamEvent>,
    },
    Delete {
        revision: u64,
    },
}

#[derive(Debug, Clone)]
struct PendingStreamEvent {
    revision: u64,
    message_id: String,
    event: PersistedStreamEvent,
}

#[derive(Default)]
struct PendingTaskWrites {
    by_task: HashMap<String, PendingTaskWrite>,
}

fn merge_pending_task_writes(older: PendingTaskWrite, newer: PendingTaskWrite) -> PendingTaskWrite {
    match (older, newer) {
        (
            PendingTaskWrite::Upsert {
                task: older_task,
                stream_events: older_events,
            },
            PendingTaskWrite::Stream {
                events: newer_events,
            },
        ) => {
            let covered_revision = older_task.persistence_revision;
            PendingTaskWrite::Upsert {
                task: older_task,
                stream_events: filter_stream_events(
                    append_stream_events(older_events, newer_events),
                    covered_revision,
                ),
            }
        }
        (
            PendingTaskWrite::Stream {
                events: older_events,
            },
            PendingTaskWrite::Upsert {
                task: newer_task,
                stream_events: newer_events,
            },
        ) => PendingTaskWrite::Upsert {
            stream_events: filter_stream_events(
                append_stream_events(older_events, newer_events),
                newer_task.persistence_revision,
            ),
            task: newer_task,
        },
        (
            PendingTaskWrite::Upsert {
                task: older_task,
                stream_events: older_events,
            },
            PendingTaskWrite::Upsert {
                task: newer_task,
                stream_events: newer_events,
            },
        ) => {
            if newer_task.persistence_revision >= older_task.persistence_revision {
                PendingTaskWrite::Upsert {
                    stream_events: filter_stream_events(
                        append_stream_events(older_events, newer_events),
                        newer_task.persistence_revision,
                    ),
                    task: newer_task,
                }
            } else {
                PendingTaskWrite::Upsert {
                    stream_events: filter_stream_events(
                        append_stream_events(older_events, newer_events),
                        older_task.persistence_revision,
                    ),
                    task: older_task,
                }
            }
        }
        (
            PendingTaskWrite::Stream {
                events: mut older_events,
            },
            PendingTaskWrite::Stream {
                events: newer_events,
            },
        ) => {
            older_events.extend(newer_events);
            PendingTaskWrite::Stream {
                events: older_events,
            }
        }
        (
            PendingTaskWrite::Delete {
                revision: older_revision,
            },
            PendingTaskWrite::Upsert {
                task,
                stream_events,
            },
        ) => {
            if task.persistence_revision > older_revision {
                PendingTaskWrite::Upsert {
                    task,
                    stream_events,
                }
            } else {
                PendingTaskWrite::Delete {
                    revision: older_revision,
                }
            }
        }
        (
            PendingTaskWrite::Upsert {
                task,
                stream_events,
            },
            PendingTaskWrite::Delete { revision },
        ) => {
            if revision >= task.persistence_revision {
                PendingTaskWrite::Delete { revision }
            } else {
                PendingTaskWrite::Upsert {
                    task,
                    stream_events,
                }
            }
        }
        (PendingTaskWrite::Stream { events }, PendingTaskWrite::Delete { revision }) => {
            if events
                .iter()
                .map(|event| event.revision)
                .max()
                .unwrap_or_default()
                <= revision
            {
                PendingTaskWrite::Delete { revision }
            } else {
                PendingTaskWrite::Stream { events }
            }
        }
        (
            PendingTaskWrite::Delete {
                revision: older_revision,
            },
            PendingTaskWrite::Stream { events },
        ) => {
            if events.iter().any(|event| event.revision > older_revision) {
                PendingTaskWrite::Stream { events }
            } else {
                PendingTaskWrite::Delete {
                    revision: older_revision,
                }
            }
        }
        (
            PendingTaskWrite::Delete {
                revision: older_revision,
            },
            PendingTaskWrite::Delete { revision },
        ) => PendingTaskWrite::Delete {
            revision: older_revision.max(revision),
        },
    }
}

fn append_stream_events(
    mut older: Vec<PendingStreamEvent>,
    newer: Vec<PendingStreamEvent>,
) -> Vec<PendingStreamEvent> {
    older.extend(newer);
    older
}

fn filter_stream_events(
    events: Vec<PendingStreamEvent>,
    covered_revision: u64,
) -> Vec<PendingStreamEvent> {
    events
        .into_iter()
        .filter(|event| event.revision > covered_revision)
        .collect()
}

#[derive(Clone)]
struct TaskPersistence {
    pending: Arc<Mutex<PendingTaskWrites>>,
    notify: Arc<Notify>,
    started: Arc<AtomicBool>,
}

impl TaskPersistence {
    fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(PendingTaskWrites::default())),
            notify: Arc::new(Notify::new()),
            started: Arc::new(AtomicBool::new(false)),
        }
    }

    fn start(
        &self,
        connection: Connection,
        durable_tasks: HashMap<String, Task>,
        event_bytes: HashMap<String, u64>,
    ) -> Result<(), String> {
        if self.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let pending = Arc::clone(&self.pending);
        let notify = Arc::clone(&self.notify);
        tauri::async_runtime::spawn(async move {
            task_writer_loop(connection, pending, notify, durable_tasks, event_bytes).await;
        });
        Ok(())
    }

    fn enqueue_upsert(&self, task: Task) -> Result<(), String> {
        if !self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        let task_id = task.id.clone();
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| "Task persistence queue is poisoned".to_string())?;
        let write = PendingTaskWrite::Upsert {
            task,
            stream_events: Vec::new(),
        };
        if let Some(existing) = pending.by_task.remove(&task_id) {
            pending
                .by_task
                .insert(task_id, merge_pending_task_writes(existing, write));
        } else {
            pending.by_task.insert(task_id, write);
        }
        drop(pending);
        self.notify.notify_one();
        Ok(())
    }

    fn enqueue_stream(
        &self,
        task_id: &str,
        message_id: &str,
        revision: u64,
        event: &llm::StreamEvent,
    ) -> Result<(), String> {
        let Some(event) = persisted_stream_event(event) else {
            return Ok(());
        };
        if !self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| "Task persistence queue is poisoned".to_string())?;
        let write = PendingTaskWrite::Stream {
            events: vec![PendingStreamEvent {
                revision,
                message_id: message_id.to_string(),
                event,
            }],
        };
        if let Some(existing) = pending.by_task.remove(task_id) {
            pending.by_task.insert(
                task_id.to_string(),
                merge_pending_task_writes(existing, write),
            );
        } else {
            pending.by_task.insert(task_id.to_string(), write);
        }
        drop(pending);
        self.notify.notify_one();
        Ok(())
    }

    fn enqueue_delete(&self, task_id: &str, revision: u64) -> Result<(), String> {
        if !self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| "Task persistence queue is poisoned".to_string())?;
        let write = PendingTaskWrite::Delete { revision };
        if let Some(existing) = pending.by_task.remove(task_id) {
            pending.by_task.insert(
                task_id.to_string(),
                merge_pending_task_writes(existing, write),
            );
        } else {
            pending.by_task.insert(task_id.to_string(), write);
        }
        drop(pending);
        self.notify.notify_one();
        Ok(())
    }
}

fn take_pending_task_writes(
    pending: &Arc<Mutex<PendingTaskWrites>>,
) -> Result<PendingTaskWrites, String> {
    let mut pending = pending
        .lock()
        .map_err(|_| "Task persistence queue is poisoned".to_string())?;
    Ok(std::mem::take(&mut *pending))
}

fn requeue_task_writes(
    pending: &Arc<Mutex<PendingTaskWrites>>,
    writes: PendingTaskWrites,
) -> Result<(), String> {
    let mut pending = pending
        .lock()
        .map_err(|_| "Task persistence queue is poisoned".to_string())?;
    for (task_id, write) in writes.by_task {
        if let Some(current) = pending.by_task.remove(&task_id) {
            pending
                .by_task
                .insert(task_id, merge_pending_task_writes(write, current));
        } else {
            pending.by_task.insert(task_id, write);
        }
    }
    Ok(())
}

struct ProjectedTaskChanges {
    tasks: HashMap<String, Option<Task>>,
    event_bytes: HashMap<String, u64>,
    compacted: HashSet<String>,
}

fn stream_event_bytes(event: &PersistedStreamEvent) -> Result<u64, String> {
    serde_json::to_vec(event)
        .map(|payload| payload.len() as u64 + 32)
        .map_err(|error| format!("Unable to encode streamed task event: {error}"))
}

fn project_task_writes(
    durable_tasks: &HashMap<String, Task>,
    durable_event_bytes: &HashMap<String, u64>,
    writes: &PendingTaskWrites,
) -> Result<ProjectedTaskChanges, String> {
    let mut entries = writes.by_task.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(right.0));
    let mut projected = ProjectedTaskChanges {
        tasks: HashMap::new(),
        event_bytes: HashMap::new(),
        compacted: HashSet::new(),
    };
    for (task_id, write) in entries {
        match write {
            PendingTaskWrite::Upsert {
                task,
                stream_events,
            } => {
                let mut task = task.clone();
                for pending_event in stream_events {
                    apply_persisted_stream_event(
                        &mut task,
                        task_id,
                        &pending_event.message_id,
                        &pending_event.event,
                    )?;
                    task.persistence_revision =
                        task.persistence_revision.max(pending_event.revision);
                }
                projected.tasks.insert(task_id.clone(), Some(task));
                projected.event_bytes.insert(task_id.clone(), 0);
            }
            PendingTaskWrite::Stream {
                events: stream_events,
            } => {
                let mut task = durable_tasks
                    .get(task_id)
                    .cloned()
                    .ok_or_else(|| format!("Stream event references missing task {task_id}"))?;
                let mut event_bytes = durable_event_bytes.get(task_id).copied().unwrap_or(0);
                for pending_event in stream_events {
                    apply_persisted_stream_event(
                        &mut task,
                        task_id,
                        &pending_event.message_id,
                        &pending_event.event,
                    )?;
                    task.persistence_revision =
                        task.persistence_revision.max(pending_event.revision);
                    event_bytes =
                        event_bytes.saturating_add(stream_event_bytes(&pending_event.event)?);
                }
                if event_bytes >= TASK_EVENT_COMPACTION_BYTES {
                    projected.compacted.insert(task_id.clone());
                    event_bytes = 0;
                }
                projected.tasks.insert(task_id.clone(), Some(task));
                projected.event_bytes.insert(task_id.clone(), event_bytes);
            }
            PendingTaskWrite::Delete { .. } => {
                projected.tasks.insert(task_id.clone(), None);
                projected.event_bytes.insert(task_id.clone(), 0);
            }
        }
    }
    Ok(projected)
}

fn insert_stream_event(
    transaction: &Transaction<'_>,
    task_id: &str,
    message_id: &str,
    event: &PersistedStreamEvent,
) -> Result<(), String> {
    let payload = serde_json::to_string(event)
        .map_err(|error| format!("Unable to encode streamed task event: {error}"))?;
    transaction
        .execute(
            "INSERT INTO task_event(task_id, message_id, kind, payload) VALUES (?1, ?2, 'stream', ?3)",
            params![task_id, message_id, payload],
        )
        .map_err(|error| format!("Unable to persist streamed task event: {error}"))?;
    Ok(())
}

fn commit_task_writes(
    mut connection: Connection,
    writes: PendingTaskWrites,
    projected: ProjectedTaskChanges,
) -> (
    Connection,
    PendingTaskWrites,
    ProjectedTaskChanges,
    Result<(), String>,
) {
    let result = (|| {
        let transaction = connection
            .transaction()
            .map_err(|error| format!("Unable to begin task persistence batch: {error}"))?;
        let mut entries = writes.by_task.iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(right.0));
        for (task_id, write) in entries {
            match write {
                PendingTaskWrite::Upsert {
                    task: _,
                    stream_events: _,
                } => {
                    let task = projected
                        .tasks
                        .get(task_id)
                        .and_then(Option::as_ref)
                        .ok_or_else(|| {
                            format!("Projected task snapshot is missing for {task_id}")
                        })?;
                    insert_task_state(&transaction, task)?;
                    transaction
                        .execute(
                            "DELETE FROM task_event WHERE task_id = ?1",
                            params![task_id],
                        )
                        .map_err(|error| {
                            format!("Unable to reset task events for {task_id}: {error}")
                        })?;
                }
                PendingTaskWrite::Stream { events } => {
                    for pending_event in events {
                        insert_stream_event(
                            &transaction,
                            task_id,
                            &pending_event.message_id,
                            &pending_event.event,
                        )?;
                    }
                }
                PendingTaskWrite::Delete { .. } => {
                    transaction
                        .execute("DELETE FROM task_state WHERE id = ?1", params![task_id])
                        .map_err(|error| format!("Unable to delete task {task_id}: {error}"))?;
                }
            }
        }
        for task_id in &projected.compacted {
            if let Some(Some(task)) = projected.tasks.get(task_id) {
                insert_task_state(&transaction, task)?;
                transaction
                    .execute(
                        "DELETE FROM task_event WHERE task_id = ?1",
                        params![task_id],
                    )
                    .map_err(|error| {
                        format!("Unable to compact task events for {task_id}: {error}")
                    })?;
            }
        }
        transaction
            .commit()
            .map_err(|error| format!("Unable to commit task persistence batch: {error}"))?;
        let _ = connection.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        Ok(())
    })();
    (connection, writes, projected, result)
}

async fn task_writer_loop(
    mut connection: Connection,
    pending: Arc<Mutex<PendingTaskWrites>>,
    notify: Arc<Notify>,
    mut durable_tasks: HashMap<String, Task>,
    mut event_bytes: HashMap<String, u64>,
) {
    loop {
        notify.notified().await;
        tokio::time::sleep(Duration::from_millis(TASK_WRITE_BATCH_DELAY_MS)).await;
        let writes = match take_pending_task_writes(&pending) {
            Ok(writes) if writes.by_task.is_empty() => continue,
            Ok(writes) => writes,
            Err(error) => {
                error!("Task persistence stopped: {error}");
                return;
            }
        };
        let projected = match project_task_writes(&durable_tasks, &event_bytes, &writes) {
            Ok(projected) => projected,
            Err(error) => {
                error!("Task persistence batch rejected: {error}");
                let _ = requeue_task_writes(&pending, writes);
                tokio::time::sleep(Duration::from_millis(500)).await;
                notify.notify_one();
                continue;
            }
        };
        let commit = tauri::async_runtime::spawn_blocking(move || {
            commit_task_writes(connection, writes, projected)
        })
        .await;
        let (next_connection, writes, projected, result) = match commit {
            Ok(result) => result,
            Err(error) => {
                error!("Task persistence worker failed: {error}");
                return;
            }
        };
        connection = next_connection;
        if let Err(error) = result {
            error!("Task persistence batch failed: {error}");
            let _ = requeue_task_writes(&pending, writes);
            tokio::time::sleep(Duration::from_millis(500)).await;
            notify.notify_one();
            continue;
        }
        let ProjectedTaskChanges {
            tasks,
            event_bytes: projected_event_bytes,
            compacted,
        } = projected;
        for (task_id, task) in tasks {
            match task {
                Some(task) => {
                    durable_tasks.insert(task_id.clone(), task);
                    event_bytes.insert(
                        task_id.clone(),
                        if compacted.contains(&task_id) {
                            0
                        } else {
                            projected_event_bytes.get(&task_id).copied().unwrap_or(0)
                        },
                    );
                }
                None => {
                    durable_tasks.remove(&task_id);
                    event_bytes.remove(&task_id);
                }
            }
        }
    }
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
struct BrowserSession {
    current_url: String,
    title: String,
    html: String,
    history: Vec<String>,
    history_index: usize,
    scroll_y: i64,
    typed_values: HashMap<String, String>,
    #[serde(default)]
    tabs: Vec<BrowserTab>,
    #[serde(default)]
    active_tab_id: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct BrowserTab {
    id: usize,
    url: String,
    title: String,
}

struct PersistentShell {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct McpToolDefinition {
    pub exposed_name: String,
    pub server_id: String,
    pub remote_name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Clone)]
struct ToolDefinitionSnapshot {
    definitions: Arc<Vec<Value>>,
    schema_hash: Arc<str>,
}

struct McpStdioSession {
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

struct McpSession {
    _server_id: String,
    transport: String,
    endpoint: Option<String>,
    stdio: Option<McpStdioSession>,
    next_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantPart {
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

struct ToolInvocation {
    model_tool_call_id: Option<String>,
    name: String,
    arguments: Value,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub prompt: String,
    pub status: AgentStatus,
    pub created_at: i64,
    pub updated_at: i64,
    pub demo_mode: bool,
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
    #[serde(skip)]
    persistence_revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub status: AgentStatus,
    pub updated_at: i64,
    pub demo_mode: bool,
    #[serde(default)]
    pub archived: bool,
    pub error: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingsView {
    pub api_base_url: String,
    pub model: String,
    pub api_key_configured: bool,
    pub max_steps: u32,
    pub timeout_secs: u64,
    pub prompt_cache: llm::PromptCacheMode,
    pub demo_mode: bool,
    pub available_tools: Vec<AgentToolDefinition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PersistedSettings {
    api_base_url: Option<String>,
    model: Option<String>,
    max_steps: Option<u32>,
    timeout_secs: Option<u64>,
    prompt_cache: Option<llm::PromptCacheMode>,
}

#[derive(Clone)]
pub struct AppState {
    tasks: Arc<RwLock<HashMap<String, Task>>>,
    task_persistence: TaskPersistence,
    running: Arc<RwLock<HashMap<String, CancellationToken>>>,
    approval_waiters: Arc<RwLock<HashMap<String, oneshot::Sender<bool>>>>,
    settings: Arc<RwLock<AgentSettings>>,
    storage_dir: Arc<RwLock<Option<PathBuf>>>,
    persist_lock: Arc<Mutex<()>>,
    edit_history: Arc<Mutex<HashMap<String, VecDeque<String>>>>,
    shell_sessions: Arc<AsyncMutex<HashMap<String, PersistentShell>>>,
    browser_sessions: Arc<Mutex<HashMap<String, BrowserSession>>>,
    mcp_sessions: Arc<AsyncMutex<HashMap<String, McpSession>>>,
    mcp_tools: Arc<RwLock<HashMap<String, McpToolDefinition>>>,
    mcp_tools_revision: Arc<AtomicU64>,
    tool_definition_cache: Arc<RwLock<Option<(u64, Arc<ToolDefinitionSnapshot>)>>>,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            tasks: Arc::new(RwLock::new(HashMap::new())),
            task_persistence: TaskPersistence::new(),
            running: Arc::new(RwLock::new(HashMap::new())),
            approval_waiters: Arc::new(RwLock::new(HashMap::new())),
            settings: Arc::new(RwLock::new(default_settings())),
            storage_dir: Arc::new(RwLock::new(None)),
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
            connection,
        } = load_task_store(&data_dir)?;
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

        self.task_persistence
            .start(connection, durable_tasks, event_bytes)?;
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

    fn persist_deleted_task(&self, task_id: &str, revision: u64) -> Result<(), String> {
        self.task_persistence.enqueue_delete(task_id, revision)
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
        };
        let contents = serde_json::to_string_pretty(&safe_settings)
            .map_err(|error| format!("Unable to encode settings: {error}"))?;
        fs::write(data_dir.join(SETTINGS_FILE), contents)
            .map_err(|error| format!("Unable to persist settings: {error}"))
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
    }
}

fn normalize_max_steps(max_steps: u32) -> u32 {
    max_steps.max(1)
}

fn default_agent_name() -> String {
    "RustPilot Manus".to_string()
}

fn default_agent_kind() -> String {
    "manus".to_string()
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

fn emit_event<T: Serialize + Clone>(app: &AppHandle, event: &str, payload: T) {
    if let Err(error) = app.emit(event, payload) {
        warn!("Unable to emit {event}: {error}");
    }
}

fn emit_current_plan(app: &AppHandle, state: &AppState, task_id: &str) {
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
        emit_event(
            app,
            "task_plan",
            TaskPlanEvent {
                task_id: task_id.to_string(),
                plan,
            },
        );
    }
}

fn task_snapshot(state: &AppState, task_id: &str) -> Result<Task, String> {
    state
        .tasks
        .read()
        .map_err(|_| "Task lock is poisoned".to_string())?
        .get(task_id)
        .cloned()
        .ok_or_else(|| "Task not found".to_string())
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
    state.persist_task(task_id)
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
    emit_event(app, "task_status", event);
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
    emit_event(app, "task_message", message.clone());
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
    emit_event(app, "task_message", message.clone());
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
                .unwrap_or_else(|| "")
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
    emit_event(app, "task_message", message);
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
        emit_event(app, "task_message", message);
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
    emit_event(app, "task_message", message);
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
    emit_event(app, "task_step", step.clone());
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
    emit_event(app, "task_step", step);
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
    emit_event(app, "task_tool_call", call.clone());
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
    emit_event(app, "task_tool_call", call);
    emit_event(app, "task_tool_result", result.clone());
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
    emit_event(app, "task_approval_required", request);
    Ok(())
}

fn update_approval_status(state: &AppState, task_id: &str, approval_id: &str, status: &str) {
    if let Ok(mut tasks) = state.tasks.write() {
        if let Some(task) = tasks.get_mut(task_id) {
            if let Some(request) = task
                .approval_requests
                .iter_mut()
                .find(|item| item.id == approval_id)
            {
                request.status = status.to_string();
                touch_task(task);
            }
        }
    }
    let _ = state.persist_task(task_id);
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
    let request = ApprovalRequest {
        id: approval_id.clone(),
        task_id: task_id.to_string(),
        tool_name: tool_name.to_string(),
        reason: approval_reason(tool_name),
        details: approval_details(tool_name, arguments),
        created_at: now(),
        status: "pending".to_string(),
    };
    let (sender, receiver) = oneshot::channel();
    state
        .approval_waiters
        .write()
        .map_err(|_| AgentError::Message("Approval lock is poisoned".to_string()))?
        .insert(approval_id.clone(), sender);
    set_status(app, state, task_id, AgentStatus::WaitingApproval, None)
        .map_err(AgentError::Message)?;
    add_approval_request(app, state, request).map_err(AgentError::Message)?;

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
    update_approval_status(state, task_id, &approval_id, status);
    set_status(app, state, task_id, AgentStatus::Executing, None).map_err(AgentError::Message)?;
    Ok(decision)
}

fn is_high_risk(tool_name: &str, arguments: &Value) -> bool {
    match tool_name {
        "rust_shell"
        | "rust_bash"
        | "rust_sandbox_shell"
        | "rust_python_execute"
        | "rust_ask_human" => true,
        "rust_computer_use" => {
            arguments
                .get("action")
                .and_then(Value::as_str)
                .is_some_and(|action| {
                    matches!(action, "move_to" | "click" | "scroll" | "type" | "press")
                        || (action == "screenshot"
                            && arguments
                                .get("path")
                                .and_then(Value::as_str)
                                .is_some_and(|path| !path.trim().is_empty()))
                })
        }
        "rust_mcp" => arguments
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|action| {
                matches!(action, "connect" | "call_tool")
                    || (action == "list_tools"
                        && arguments
                            .get("transport")
                            .and_then(Value::as_str)
                            .is_some_and(|transport| transport.eq_ignore_ascii_case("stdio")))
            }),
        "rust_files" | "rust_sandbox_files" => arguments
            .get("operation")
            .and_then(Value::as_str)
            .is_some_and(|operation| matches!(operation, "write" | "delete")),
        "rust_str_replace_editor" => arguments
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| {
                matches!(command, "create" | "str_replace" | "insert" | "undo_edit")
            }),
        "rust_http" => arguments
            .get("method")
            .and_then(Value::as_str)
            .map(|method| !method.eq_ignore_ascii_case("GET"))
            .unwrap_or(false),
        "rust_visualization_preparation" => arguments
            .get("output_path")
            .and_then(Value::as_str)
            .is_some_and(|path| !path.trim().is_empty()),
        "rust_browser_use" | "rust_sandbox_browser" => arguments
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|action| {
                matches!(
                    action,
                    "click"
                        | "type"
                        | "click_element"
                        | "input_text"
                        | "select_dropdown_option"
                        | "send_keys"
                )
            }),
        name if name.starts_with("rust_mcp_") => true,
        _ => false,
    }
}

fn approval_reason(tool_name: &str) -> String {
    match tool_name {
        "rust_shell" | "rust_bash" | "rust_sandbox_shell" => {
            "Shell commands can change system state or run external programs.".to_string()
        }
        "rust_python_execute" => {
            "Python can execute arbitrary local code and access the filesystem.".to_string()
        }
        "rust_str_replace_editor" | "rust_files" | "rust_sandbox_files" => {
            "This operation changes files on the computer.".to_string()
        }
        "rust_visualization_preparation" => {
            "This operation writes a visualization specification to a local path.".to_string()
        }
        "rust_computer_use" => {
            "Desktop input can click, type, or control another application.".to_string()
        }
        "rust_http" => "This HTTP method may modify a remote service.".to_string(),
        "rust_browser_use" | "rust_sandbox_browser" => {
            "This browser action may submit or modify a web page.".to_string()
        }
        "rust_mcp" => "The connected MCP server may perform an external operation.".to_string(),
        _ => "This tool operation requires explicit user approval.".to_string(),
    }
}

fn mutation_path_argument(tool_name: &str, arguments: &Value) -> Option<String> {
    let operation = arguments.get("operation").and_then(Value::as_str);
    let command = arguments.get("command").and_then(Value::as_str);
    let action = arguments.get("action").and_then(Value::as_str);
    match tool_name {
        "rust_files" if matches!(operation, Some("write" | "delete")) => {
            Some(string_argument(arguments, "path").unwrap_or_else(|| ".".to_string()))
        }
        "rust_str_replace_editor"
            if matches!(
                command,
                Some("create" | "str_replace" | "insert" | "undo_edit")
            ) =>
        {
            string_argument(arguments, "path")
        }
        "rust_visualization_preparation" => string_argument(arguments, "output_path"),
        "rust_computer_use" if action == Some("screenshot") => string_argument(arguments, "path"),
        _ => None,
    }
}

fn approval_details(tool_name: &str, arguments: &Value) -> String {
    let mut details = arguments.clone();
    if let Some(path) = mutation_path_argument(tool_name, arguments) {
        let resolution = match path_guard::resolve_mutation_path(&workspace_root(), &path, true) {
            Ok(resolved) => json!({
                "requested": path,
                "resolved": resolved.canonical.display().to_string(),
                "scope": resolved.scope.as_str(),
                "exists": resolved.existed,
                "approval": "This approval applies to this exact resolved path."
            }),
            Err(error) => json!({
                "requested": path,
                "error": error,
                "approval": "The operation will be rejected if the path cannot be resolved safely."
            }),
        };
        if let Some(object) = details.as_object_mut() {
            object.insert("_rustpilot_path_authorization".to_string(), resolution);
        }
    }
    serde_json::to_string_pretty(&details).unwrap_or_else(|_| "{}".to_string())
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

async fn run_tool_inner(
    state: &AppState,
    task_id: &str,
    name: &str,
    arguments: &Value,
    settings: &AgentSettings,
    cancel: &CancellationToken,
    external_path_approved: bool,
) -> Result<String, String> {
    match name {
        "rust_clock" => Ok(format!("Local time (unix_millis): {}", now())),
        "rust_shell" => run_shell_tool(arguments, None).await,
        "rust_bash" => run_bash_tool(state, task_id, arguments, None).await,
        "rust_sandbox_shell" => run_bash_tool(state, task_id, arguments, Some("sandbox")).await,
        "rust_files" => run_files_tool(arguments, external_path_approved).await,
        "rust_sandbox_files" => {
            run_sandbox_files_tool(task_id, arguments, external_path_approved).await
        }
        "rust_str_replace_editor" => {
            run_editor_tool(state, arguments, external_path_approved).await
        }
        "rust_http" => run_http_tool(arguments).await,
        "rust_web_search" => run_web_search_tool(arguments).await,
        "rust_crawl4ai" => run_crawl_tool(arguments).await,
        "rust_browser_use" => run_browser_tool(state, arguments, "browser").await,
        "rust_sandbox_browser" => run_browser_tool(state, arguments, "sandbox_browser").await,
        "rust_computer_use" => run_computer_tool(arguments, external_path_approved).await,
        "rust_python_execute" => run_python_tool(arguments).await,
        "rust_planning" => run_planning_tool(state, task_id, arguments).await,
        "rust_mcp" => run_mcp_tool(state, arguments).await,
        "rust_create_chat_completion" => {
            run_chat_completion_tool(state, task_id, arguments, settings, cancel).await
        }
        "rust_visualization_preparation" => {
            run_visualization_preparation(arguments, external_path_approved)
        }
        "rust_data_analysis" => run_data_analysis_tool(arguments).await,
        "rust_data_visualization" => run_data_visualization_tool(arguments).await,
        "rust_sandbox_vision" => run_sandbox_vision_tool(task_id, arguments).await,
        "rust_terminate" => {
            let status =
                string_argument(arguments, "status").unwrap_or_else(|| "success".to_string());
            let message = string_argument(arguments, "message")
                .unwrap_or_else(|| "Agent terminated.".to_string());
            Ok(format!("terminated: {status}\n{message}"))
        }
        "rust_ask_human" => Ok("The user approval dialog was completed.".to_string()),
        _ if name.starts_with("rust_mcp_") => run_dynamic_mcp_tool(state, name, arguments).await,
        _ => Err(format!("Unknown tool: {name}")),
    }
}

fn workspace_root() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn sandbox_root_for_task(task_id: &str) -> Result<PathBuf, String> {
    let workspace = workspace_root();
    let requested = format!(".rustpilot/sandboxes/{task_id}");
    let root = path_guard::resolve_scoped_path(&workspace, &requested)?;
    fs::create_dir_all(&root).map_err(|error| format!("Unable to create task sandbox: {error}"))?;
    path_guard::resolve_scoped_path(&workspace, &root.to_string_lossy())
}

fn sandbox_path_for_task(task_id: &str, raw: &str) -> Result<PathBuf, String> {
    let root = sandbox_root_for_task(task_id)?;
    path_guard::resolve_scoped_path(&root, raw)
}

async fn run_shell_process(command: &str, cwd: Option<&Path>) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    let mut process = {
        let mut command_builder = Command::new("cmd.exe");
        command_builder.args(["/C", command]);
        command_builder
    };
    #[cfg(not(target_os = "windows"))]
    let mut process = {
        let mut command_builder = Command::new("sh");
        command_builder.args(["-c", command]);
        command_builder
    };
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    let output = process
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("Unable to run shell command: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut result = format!(
        "exit_code: {}\nstdout:\n{stdout}",
        output.status.code().unwrap_or(-1)
    );
    if !stderr.trim().is_empty() {
        result.push_str(&format!("\nstderr:\n{stderr}"));
    }
    if !output.status.success() {
        return Err(truncate_output(&result));
    }
    Ok(truncate_output(&result))
}

async fn run_shell_tool(arguments: &Value, forced_cwd: Option<&Path>) -> Result<String, String> {
    let command = string_argument(arguments, "command")
        .ok_or_else(|| "rust_shell requires a command string".to_string())?;
    let explicit_cwd = string_argument(arguments, "cwd").map(PathBuf::from);
    let cwd = forced_cwd.or(explicit_cwd.as_deref());
    run_shell_process(&command, cwd).await
}

async fn spawn_persistent_shell(cwd: &Path) -> Result<PersistentShell, String> {
    #[cfg(target_os = "windows")]
    let mut process = {
        let mut command_builder = Command::new("cmd.exe");
        command_builder.args(["/Q", "/D", "/K"]);
        command_builder
    };
    #[cfg(not(target_os = "windows"))]
    let mut process = Command::new("sh");
    let mut child = process
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Unable to start persistent shell: {error}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Persistent shell stdin is unavailable.".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Persistent shell stdout is unavailable.".to_string())?;
    Ok(PersistentShell {
        child,
        stdin,
        stdout: BufReader::new(stdout),
        cwd: cwd.to_path_buf(),
    })
}

async fn run_bash_tool(
    state: &AppState,
    task_id: &str,
    arguments: &Value,
    sandbox_prefix: Option<&str>,
) -> Result<String, String> {
    let command = string_argument(arguments, "command")
        .ok_or_else(|| "rust_bash requires a command string".to_string())?;
    let session_id =
        string_argument(arguments, "session_id").unwrap_or_else(|| "default".to_string());
    let key = match sandbox_prefix {
        Some(prefix) => format!("{prefix}:{task_id}:{session_id}"),
        None => session_id.clone(),
    };
    let restart = arguments
        .get("restart")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sandbox_root = sandbox_prefix
        .map(|_| sandbox_root_for_task(task_id))
        .transpose()?;
    let initial_cwd = if let Some(raw_cwd) = string_argument(arguments, "cwd") {
        let path = PathBuf::from(raw_cwd);
        if let Some(root) = &sandbox_root {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        } else {
            path
        }
    } else {
        sandbox_root.clone().unwrap_or_else(workspace_root)
    };
    if let Some(root) = &sandbox_root {
        let normalized = if initial_cwd.exists() {
            initial_cwd
                .canonicalize()
                .map_err(|error| format!("Unable to resolve sandbox cwd: {error}"))?
        } else {
            initial_cwd.clone()
        };
        if !normalized.starts_with(root) {
            return Err("Sandbox shell cwd must stay inside the task sandbox.".to_string());
        }
    }
    if !initial_cwd.is_dir() {
        return Err(format!(
            "Shell working directory does not exist: {}",
            initial_cwd.display()
        ));
    }

    let mut sessions = state.shell_sessions.lock().await;
    if restart {
        sessions.remove(&key);
    }
    let should_spawn = match sessions.get_mut(&key) {
        Some(shell) => shell
            .child
            .try_wait()
            .map_err(|error| format!("Unable to inspect shell session: {error}"))?
            .is_some(),
        None => true,
    };
    if should_spawn {
        sessions.insert(key.clone(), spawn_persistent_shell(&initial_cwd).await?);
    }
    let shell = sessions
        .get_mut(&key)
        .ok_or_else(|| "Persistent shell session was not created.".to_string())?;
    let sentinel = format!("__RUSTPILOT_DONE_{}__", Uuid::new_v4().simple());
    #[cfg(target_os = "windows")]
    let payload = format!("{command} 2>&1\r\necho {sentinel}:%errorlevel%:%CD%\r\n");
    #[cfg(not(target_os = "windows"))]
    let payload = format!("{{ {command}; }} 2>&1\nprintf '{sentinel}:%s:%s\\n' \"$?\" \"$PWD\"\n");
    shell
        .stdin
        .write_all(payload.as_bytes())
        .await
        .map_err(|error| format!("Unable to write to shell session: {error}"))?;
    shell
        .stdin
        .flush()
        .await
        .map_err(|error| format!("Unable to flush shell session: {error}"))?;
    let mut output = String::new();
    let exit_code = loop {
        let mut line = String::new();
        let bytes = shell
            .stdout
            .read_line(&mut line)
            .await
            .map_err(|error| format!("Unable to read shell session: {error}"))?;
        if bytes == 0 {
            return Err("Persistent shell exited before returning a result.".to_string());
        }
        if let Some(marker) = line.find(&sentinel) {
            let metadata = line[marker + sentinel.len()..]
                .trim()
                .trim_start_matches(':');
            let mut parts = metadata.splitn(2, ':');
            let exit_code = parts.next().unwrap_or("-1").to_string();
            if let Some(next_cwd) = parts.next().filter(|value| !value.is_empty()) {
                shell.cwd = PathBuf::from(next_cwd);
            }
            break exit_code;
        }
        output.push_str(&line);
        if output.len() > MAX_OUTPUT_CHARS * 2 {
            output.truncate(MAX_OUTPUT_CHARS * 2);
        }
    };
    let cwd = shell.cwd.clone();
    Ok(format!(
        "session: {session_id}\ncwd: {}\nexit_code: {exit_code}\n{}",
        cwd.display(),
        truncate_output(&output)
    ))
}

async fn run_files_tool(arguments: &Value, external_path_approved: bool) -> Result<String, String> {
    let operation = string_argument(arguments, "operation").unwrap_or_else(|| "list".to_string());
    let raw_path = string_argument(arguments, "path").unwrap_or_else(|| ".".to_string());
    let path = if matches!(operation.as_str(), "write" | "delete") {
        path_guard::resolve_mutation_path(&workspace_root(), &raw_path, external_path_approved)?
            .canonical
    } else {
        PathBuf::from(raw_path)
    };
    match operation.as_str() {
        "list" => {
            let mut entries = tokio::fs::read_dir(&path)
                .await
                .map_err(|error| format!("Unable to list {}: {error}", path.display()))?;
            let mut lines = Vec::new();
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|error| format!("Unable to read directory entry: {error}"))?
            {
                let kind = if entry
                    .file_type()
                    .await
                    .map_err(|error| format!("Unable to inspect entry: {error}"))?
                    .is_dir()
                {
                    "dir "
                } else {
                    "file"
                };
                lines.push(format!("{kind} {}", entry.file_name().to_string_lossy()));
                if lines.len() >= 120 {
                    lines.push("[directory listing truncated]".to_string());
                    break;
                }
            }
            Ok(lines.join("\n"))
        }
        "read" => {
            let contents = tokio::fs::read_to_string(&path)
                .await
                .map_err(|error| format!("Unable to read {}: {error}", path.display()))?;
            Ok(truncate_output(&contents))
        }
        "write" => {
            let contents = string_argument(arguments, "content")
                .ok_or_else(|| "rust_files write requires content".to_string())?;
            tokio::fs::write(&path, contents)
                .await
                .map_err(|error| format!("Unable to write {}: {error}", path.display()))?;
            Ok(format!("Wrote file: {}", path.display()))
        }
        "delete" => {
            if path.is_dir() {
                tokio::fs::remove_dir_all(&path)
                    .await
                    .map_err(|error| format!("Unable to delete {}: {error}", path.display()))?;
            } else {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|error| format!("Unable to delete {}: {error}", path.display()))?;
            }
            Ok(format!("Deleted {}", path.display()))
        }
        "exists" => Ok(path.exists().to_string()),
        _ => Err(format!("Unsupported rust_files operation: {operation}")),
    }
}

async fn run_sandbox_files_tool(
    task_id: &str,
    arguments: &Value,
    external_path_approved: bool,
) -> Result<String, String> {
    let raw_path = string_argument(arguments, "path").unwrap_or_else(|| ".".to_string());
    let path = sandbox_path_for_task(task_id, &raw_path)?;
    let mut forwarded = arguments.clone();
    forwarded["path"] = Value::String(path.to_string_lossy().to_string());
    run_files_tool(&forwarded, external_path_approved).await
}

async fn run_http_tool(arguments: &Value) -> Result<String, String> {
    let url =
        string_argument(arguments, "url").ok_or_else(|| "rust_http requires a URL".to_string())?;
    let method_name = string_argument(arguments, "method").unwrap_or_else(|| "GET".to_string());
    let method = reqwest::Method::from_bytes(method_name.as_bytes())
        .map_err(|error| format!("Invalid HTTP method: {error}"))?;
    let client = Client::builder()
        .user_agent("RustPilot/0.1")
        .build()
        .map_err(|error| format!("Unable to create HTTP client: {error}"))?;
    let mut request = client.request(method, &url);
    if let Some(headers) = arguments.get("headers").and_then(Value::as_object) {
        for (key, value) in headers {
            let value = match value.as_str() {
                Some(value) => value.to_string(),
                None => value.to_string(),
            };
            request = request.header(key, value);
        }
    }
    if let Some(body) = string_argument(arguments, "body") {
        request = request.body(body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("HTTP request failed: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Unable to read HTTP response: {error}"))?;
    Ok(truncate_output(&format!("HTTP {status}\n\n{body}")))
}

fn record_file_snapshot(state: &AppState, path: &Path, contents: String) -> Result<(), String> {
    let mut history = state
        .edit_history
        .lock()
        .map_err(|_| "Edit history lock is poisoned".to_string())?;
    let entries = history
        .entry(path.to_string_lossy().to_string())
        .or_default();
    if entries.back() != Some(&contents) {
        entries.push_back(contents);
    }
    while entries.len() > 20 {
        entries.pop_front();
    }
    Ok(())
}

async fn run_editor_tool(
    state: &AppState,
    arguments: &Value,
    external_path_approved: bool,
) -> Result<String, String> {
    let command = string_argument(arguments, "command")
        .ok_or_else(|| "rust_str_replace_editor requires command".to_string())?;
    let raw_path = string_argument(arguments, "path")
        .ok_or_else(|| "rust_str_replace_editor requires path".to_string())?;
    let requested_path = PathBuf::from(&raw_path);
    if !requested_path.is_absolute() {
        return Err("The editor path must be absolute.".to_string());
    }
    let path = if matches!(
        command.as_str(),
        "create" | "str_replace" | "insert" | "undo_edit"
    ) {
        path_guard::resolve_mutation_path(&workspace_root(), &raw_path, external_path_approved)?
            .canonical
    } else {
        requested_path
    };
    match command.as_str() {
        "view" => {
            if path.is_dir() {
                let mut lines = Vec::new();
                for entry in fs::read_dir(&path)
                    .map_err(|error| format!("Unable to view directory: {error}"))?
                {
                    let entry = entry
                        .map_err(|error| format!("Unable to read directory entry: {error}"))?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !name.starts_with('.') {
                        let kind = if entry.path().is_dir() {
                            "dir "
                        } else {
                            "file"
                        };
                        lines.push(format!("{kind}{name}"));
                    }
                    if lines.len() >= 120 {
                        break;
                    }
                }
                return Ok(lines.join("\n"));
            }
            let contents = tokio::fs::read_to_string(&path)
                .await
                .map_err(|error| format!("Unable to view {}: {error}", path.display()))?;
            let lines: Vec<&str> = contents.lines().collect();
            let range = arguments.get("view_range").and_then(Value::as_array);
            let start = range
                .and_then(|items| items.first())
                .and_then(Value::as_i64)
                .unwrap_or(1)
                .max(1) as usize;
            let end = range
                .and_then(|items| items.get(1))
                .and_then(Value::as_i64)
                .filter(|value| *value >= 0)
                .map(|value| value as usize)
                .unwrap_or(lines.len())
                .min(lines.len());
            if start > end || lines.is_empty() {
                return Ok(String::new());
            }
            Ok(truncate_output(
                &lines[start - 1..end]
                    .iter()
                    .enumerate()
                    .map(|(offset, line)| format!("{:>6}\t{}", start + offset, line))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        }
        "create" => {
            if path.exists() {
                return Err(format!("File already exists: {}", path.display()));
            }
            let content = string_argument(arguments, "file_text")
                .ok_or_else(|| "Parameter file_text is required for create".to_string())?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|error| format!("Unable to create parent directory: {error}"))?;
            }
            tokio::fs::write(&path, content)
                .await
                .map_err(|error| format!("Unable to create {}: {error}", path.display()))?;
            record_file_snapshot(state, &path, "__RUSTPILOT_CREATED__".to_string())?;
            Ok(format!("File created successfully at: {}", path.display()))
        }
        "str_replace" => {
            let old = string_argument(arguments, "old_str")
                .ok_or_else(|| "Parameter old_str is required for str_replace".to_string())?;
            let new = string_argument(arguments, "new_str").unwrap_or_default();
            let original = tokio::fs::read_to_string(&path)
                .await
                .map_err(|error| format!("Unable to read {}: {error}", path.display()))?;
            let count = original.match_indices(&old).count();
            if count != 1 {
                return Err(format!(
                    "old_str must match exactly one location; found {count}."
                ));
            }
            record_file_snapshot(state, &path, original.clone())?;
            let updated = original.replacen(&old, &new, 1);
            tokio::fs::write(&path, updated)
                .await
                .map_err(|error| format!("Unable to write {}: {error}", path.display()))?;
            Ok(format!("Replacement applied to {}", path.display()))
        }
        "insert" => {
            let line = arguments
                .get("insert_line")
                .and_then(Value::as_i64)
                .ok_or_else(|| "Parameter insert_line is required for insert".to_string())?;
            let new = string_argument(arguments, "new_str")
                .ok_or_else(|| "Parameter new_str is required for insert".to_string())?;
            let original = tokio::fs::read_to_string(&path)
                .await
                .map_err(|error| format!("Unable to read {}: {error}", path.display()))?;
            let mut lines: Vec<String> = original.lines().map(ToString::to_string).collect();
            let index = line.max(0) as usize;
            if index > lines.len() {
                return Err(format!("insert_line {line} is outside the file."));
            }
            record_file_snapshot(state, &path, original)?;
            lines.insert(index, new);
            tokio::fs::write(&path, lines.join("\n"))
                .await
                .map_err(|error| format!("Unable to write {}: {error}", path.display()))?;
            Ok(format!("Text inserted into {}", path.display()))
        }
        "undo_edit" => {
            let previous = state
                .edit_history
                .lock()
                .map_err(|_| "Edit history lock is poisoned".to_string())?
                .get_mut(&path.to_string_lossy().to_string())
                .and_then(VecDeque::pop_back)
                .ok_or_else(|| format!("No edit history is available for {}", path.display()))?;
            if previous == "__RUSTPILOT_CREATED__" {
                tokio::fs::remove_file(&path)
                    .await
                    .map_err(|error| format!("Unable to undo created file: {error}"))?;
            } else {
                tokio::fs::write(&path, previous)
                    .await
                    .map_err(|error| format!("Unable to restore {}: {error}", path.display()))?;
            }
            Ok(format!("Last edit undone for {}", path.display()))
        }
        _ => Err(format!("Unsupported editor command: {command}")),
    }
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn remove_html_block(mut html: String, tag: &str) -> String {
    let lower_tag = tag.to_lowercase();
    loop {
        let lower = html.to_lowercase();
        let Some(start) = lower.find(&format!("<{lower_tag}")) else {
            break;
        };
        let Some(end_offset) = lower[start..].find(&format!("</{lower_tag}>")) else {
            html.replace_range(start.., " ");
            break;
        };
        let end = start + end_offset + lower_tag.len() + 3;
        html.replace_range(start..end, " ");
    }
    html
}

fn html_text(html: &str) -> String {
    let mut cleaned = remove_html_block(html.to_string(), "script");
    cleaned = remove_html_block(cleaned, "style");
    cleaned = remove_html_block(cleaned, "nav");
    cleaned = remove_html_block(cleaned, "header");
    cleaned = remove_html_block(cleaned, "footer");
    let mut text = String::with_capacity(cleaned.len());
    let mut in_tag = false;
    for character in cleaned.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(character),
            _ => {}
        }
    }
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_title(html: &str) -> String {
    let lower = html.to_lowercase();
    let Some(start) = lower.find("<title") else {
        return String::new();
    };
    let Some(open_end) = lower[start..].find('>') else {
        return String::new();
    };
    let content_start = start + open_end + 1;
    let Some(close_offset) = lower[content_start..].find("</title>") else {
        return String::new();
    };
    html_text(&html[content_start..content_start + close_offset])
}

fn html_attribute(fragment: &str, attribute: &str) -> Option<String> {
    let lower = fragment.to_lowercase();
    let marker = format!("{attribute}=");
    let start = lower.find(&marker)? + marker.len();
    let rest = fragment[start..].trim_start();
    let quote = rest.chars().next()?;
    if quote == '\'' || quote == '"' {
        let end = rest[1..].find(quote)? + 1;
        Some(rest[1..end].to_string())
    } else {
        Some(
            rest.split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_end_matches('>')
                .to_string(),
        )
    }
}

fn absolute_url(base: &str, href: &str) -> String {
    let href = href.trim();
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if href.starts_with("//") {
        if let Some(scheme_end) = base.find("://") {
            return format!("{}:{}", &base[..scheme_end], href);
        }
    }
    let host_end = base
        .find("//")
        .and_then(|offset| base[offset + 2..].find('/').map(|end| offset + 2 + end))
        .unwrap_or(base.len());
    let origin = &base[..host_end];
    if href.starts_with('/') {
        format!("{origin}{href}")
    } else {
        let directory = base
            .rsplit_once('/')
            .map(|(prefix, _)| prefix)
            .unwrap_or(base);
        format!("{directory}/{href}")
    }
}

fn html_links(html: &str, base: &str) -> Vec<(String, String)> {
    let lower = html.to_lowercase();
    let mut links = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("<a") {
        let start = cursor + offset;
        let Some(open_offset) = lower[start..].find('>') else {
            break;
        };
        let open_end = start + open_offset;
        let Some(close_offset) = lower[open_end + 1..].find("</a>") else {
            break;
        };
        let close_end = open_end + 1 + close_offset;
        let fragment = &html[start..=open_end];
        if let Some(href) = html_attribute(fragment, "href") {
            let label = html_text(&html[open_end + 1..close_end]);
            if !href.starts_with('#') && !href.starts_with("javascript:") {
                links.push((label, absolute_url(base, &href)));
            }
        }
        cursor = close_end + 4;
        if links.len() >= 80 {
            break;
        }
    }
    links
}

fn html_select_options(html: &str) -> Vec<Value> {
    let lower = html.to_lowercase();
    let mut options = Vec::new();
    let mut cursor = 0;
    while let Some(offset) = lower[cursor..].find("<option") {
        let start = cursor + offset;
        let Some(tag_end_offset) = lower[start..].find('>') else {
            break;
        };
        let tag_end = start + tag_end_offset;
        let Some(close_offset) = lower[tag_end + 1..].find("</option>") else {
            break;
        };
        let close = tag_end + 1 + close_offset;
        let fragment = &html[start..=tag_end];
        let label = html_text(&html[tag_end + 1..close]);
        options.push(json!({
            "text": label,
            "value": html_attribute(fragment, "value").unwrap_or_default(),
            "index": options.len()
        }));
        cursor = close + "</option>".len();
        if options.len() >= 100 {
            break;
        }
    }
    options
}

async fn load_browser_page(
    session: &mut BrowserSession,
    url: String,
) -> Result<reqwest::StatusCode, String> {
    let (status, html) = fetch_page(&url).await?;
    session.current_url = url;
    session.title = html_title(&html);
    session.html = html;
    if let Some(tab) = session
        .tabs
        .iter_mut()
        .find(|tab| tab.id == session.active_tab_id)
    {
        tab.url = session.current_url.clone();
        tab.title = session.title.clone();
    }
    Ok(status)
}

async fn capture_browser_screenshot(url: &str) -> Result<PathBuf, String> {
    let browser = first_env_value(&["RUSTPILOT_BROWSER_PATH"]).or_else(|| {
        [
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        ]
        .iter()
        .find(|path| Path::new(*path).exists())
        .map(|path| (*path).to_string())
    });
    let Some(browser) = browser else {
        return Err("No Chromium-compatible browser executable was found.".to_string());
    };
    let workspace = workspace_root();
    let output_dir = path_guard::resolve_scoped_path(&workspace, ".rustpilot/browser-artifacts")?;
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|error| format!("Unable to create browser artifact directory: {error}"))?;
    let output_path = output_dir.join(format!("{}.png", Uuid::new_v4()));
    let profile_dir = output_dir.join(format!("profile-{}", Uuid::new_v4()));
    let status = Command::new(browser)
        .args([
            "--headless=new",
            "--disable-gpu",
            "--hide-scrollbars",
            "--no-first-run",
            "--no-default-browser-check",
        ])
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .arg(format!("--screenshot={}", output_path.display()))
        .arg("--window-size=1440,1000")
        .arg("--virtual-time-budget=2500")
        .arg(url)
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|error| format!("Unable to start browser screenshot: {error}"))?;
    if !status.success() || !output_path.exists() {
        return Err(format!("Browser screenshot command failed with {status}."));
    }
    Ok(output_path)
}

async fn fetch_page(url: &str) -> Result<(reqwest::StatusCode, String), String> {
    let client = Client::builder()
        .user_agent("RustPilot/0.1 (lightweight agent browser)")
        .build()
        .map_err(|error| format!("Unable to create browser client: {error}"))?;
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("Unable to fetch {url}: {error}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| format!("Unable to read {url}: {error}"))?;
    Ok((status, body))
}

async fn run_web_search_tool(arguments: &Value) -> Result<String, String> {
    let query = string_argument(arguments, "query")
        .ok_or_else(|| "rust_web_search requires query".to_string())?;
    let number = arguments
        .get("num_results")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 10);
    let fetch_content = arguments
        .get("fetch_content")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let engines = [
        (
            "duckduckgo",
            format!(
                "https://html.duckduckgo.com/html/?q={}",
                percent_encode(&query)
            ),
        ),
        (
            "bing",
            format!(
                "https://www.bing.com/search?q={}&count={number}",
                percent_encode(&query)
            ),
        ),
        (
            "google",
            format!(
                "https://www.google.com/search?q={}&num={number}",
                percent_encode(&query)
            ),
        ),
        (
            "baidu",
            format!("https://www.baidu.com/s?wd={}", percent_encode(&query)),
        ),
    ];
    let mut results = Vec::new();
    let mut source = "none";
    let mut fallback_text = String::new();
    for (engine, url) in engines {
        let fetched = tokio::time::timeout(Duration::from_secs(15), fetch_page(&url)).await;
        let Ok(Ok((_, html))) = fetched else {
            continue;
        };
        fallback_text = html_text(&html);
        let mut candidates = html_links(&html, &url)
            .into_iter()
            .filter(|(title, link)| {
                !title.trim().is_empty()
                    && link.starts_with("http")
                    && !link.contains("duckduckgo.com")
                    && !link.contains("bing.com")
                    && !link.contains("google.com")
                    && !link.contains("baidu.com")
            })
            .collect::<Vec<_>>();
        candidates.dedup_by(|left, right| left.1 == right.1);
        for (title, result_url) in candidates.into_iter().take(number as usize) {
            let content = if fetch_content {
                tokio::time::timeout(Duration::from_secs(10), fetch_page(&result_url))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .map(|(_, page)| truncate_output(&html_text(&page)))
            } else {
                None
            };
            results.push(json!({
                "position": results.len() + 1,
                "title": title,
                "url": result_url,
                "description": "",
                "source": engine,
                "raw_content": content
            }));
        }
        if !results.is_empty() {
            source = engine;
            break;
        }
    }
    if results.is_empty() {
        return Ok(format!(
            "No parsed search results for '{query}'.\n{}",
            truncate_output(&fallback_text)
        ));
    }
    Ok(truncate_output(
        &serde_json::to_string_pretty(&json!({
            "query": query,
            "source": source,
            "total_results": results.len(),
            "results": results
        }))
        .unwrap_or_default(),
    ))
}

async fn run_crawl_tool(arguments: &Value) -> Result<String, String> {
    let urls = match arguments.get("urls") {
        Some(Value::String(url)) => vec![url.clone()],
        Some(Value::Array(urls)) => urls
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect(),
        _ => return Err("rust_crawl4ai requires urls as a string or array".to_string()),
    };
    if urls.is_empty() {
        return Err("rust_crawl4ai requires at least one URL".to_string());
    }
    let threshold = arguments
        .get("word_count_threshold")
        .and_then(Value::as_u64)
        .unwrap_or(10) as usize;
    let timeout_secs = arguments
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(30)
        .clamp(5, 120);
    let bypass_cache = arguments
        .get("bypass_cache")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut pages = Vec::new();
    for url in urls.into_iter().take(8) {
        match tokio::time::timeout(Duration::from_secs(timeout_secs), fetch_page(&url)).await {
            Ok(Ok((status, html))) => {
                let text = html_text(&html);
                if text.split_whitespace().count() >= threshold {
                    pages.push(json!({
                        "url": url,
                        "success": status.is_success(),
                        "status_code": status.as_u16(),
                        "title": html_title(&html),
                        "markdown": truncate_output(&text),
                        "word_count": text.split_whitespace().count(),
                        "links_count": html_links(&html, &url).len(),
                        "cache_bypassed": bypass_cache
                    }));
                } else {
                    pages.push(json!({"url": url, "success": status.is_success(), "status_code": status.as_u16(), "word_count": text.split_whitespace().count(), "markdown": text, "cache_bypassed": bypass_cache}));
                }
            }
            Ok(Err(error)) => pages.push(json!({"url": url, "success": false, "error_message": error})),
            Err(_) => pages.push(json!({"url": url, "success": false, "error_message": format!("crawl timed out after {timeout_secs}s")})),
        }
    }
    Ok(truncate_output(
        &serde_json::to_string_pretty(
            &json!({"crawler": "RustPilot lightweight Crawl4AI-compatible", "results": pages}),
        )
        .unwrap_or_default(),
    ))
}

async fn run_browser_tool(
    state: &AppState,
    arguments: &Value,
    namespace: &str,
) -> Result<String, String> {
    let action = string_argument(arguments, "action")
        .ok_or_else(|| "rust_browser_use requires action".to_string())?;
    let raw_session_id =
        string_argument(arguments, "session_id").unwrap_or_else(|| "default".to_string());
    let session_id = format!("{namespace}:{raw_session_id}");
    let mut session = state
        .browser_sessions
        .lock()
        .map_err(|_| "Browser session lock is poisoned".to_string())?
        .get(&session_id)
        .cloned()
        .unwrap_or_default();

    let output = match action.as_str() {
        "open" | "go_to_url" | "open_tab" => {
            let url = string_argument(arguments, "url")
                .ok_or_else(|| "browser open requires url".to_string())?;
            let status = load_browser_page(&mut session, url.clone()).await?;
            if session.history_index + 1 < session.history.len() {
                session.history.truncate(session.history_index + 1);
            }
            session.history.push(url);
            session.history_index = session.history.len().saturating_sub(1);
            let tab_id = session.tabs.len();
            session.tabs.push(BrowserTab {
                id: tab_id,
                url: session.current_url.clone(),
                title: session.title.clone(),
            });
            session.active_tab_id = tab_id;
            browser_state_output(&session, status.as_u16(), true)
        }
        "refresh" => {
            if session.current_url.is_empty() {
                return Err("No browser page is open.".to_string());
            }
            let url = session.current_url.clone();
            let status = load_browser_page(&mut session, url).await?;
            browser_state_output(&session, status.as_u16(), true)
        }
        "back" | "go_back" => {
            if session.history_index == 0 || session.history.is_empty() {
                return Err("Browser history has no previous page.".to_string());
            }
            session.history_index -= 1;
            let url = session.history[session.history_index].clone();
            let status = load_browser_page(&mut session, url).await?;
            browser_state_output(&session, status.as_u16(), true)
        }
        "forward" => {
            if session.history_index + 1 >= session.history.len() {
                return Err("Browser history has no next page.".to_string());
            }
            session.history_index += 1;
            let url = session.history[session.history_index].clone();
            let status = load_browser_page(&mut session, url).await?;
            browser_state_output(&session, status.as_u16(), true)
        }
        "extract" | "extract_content" => {
            let mut output = browser_state_output(&session, 200, true)?;
            if let Some(goal) = string_argument(arguments, "goal") {
                output.push_str(&format!("\nextraction_goal: {}", truncate_output(&goal)));
            }
            Ok(output)
        }
        "click" => {
            let needle = string_argument(arguments, "selector")
                .or_else(|| string_argument(arguments, "text"))
                .ok_or_else(|| "browser click requires selector or text".to_string())?;
            let link = html_links(&session.html, &session.current_url)
                .into_iter()
                .find(|(label, url)| label.contains(&needle) || url.contains(&needle))
                .ok_or_else(|| format!("No link matched '{needle}'."))?;
            let status = load_browser_page(&mut session, link.1.clone()).await?;
            if session.history_index + 1 < session.history.len() {
                session.history.truncate(session.history_index + 1);
            }
            session.history.push(link.1);
            session.history_index = session.history.len().saturating_sub(1);
            browser_state_output(&session, status.as_u16(), true)
        }
        "click_element" => {
            let index = arguments
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| "click_element requires index".to_string())? as usize;
            let link = html_links(&session.html, &session.current_url)
                .into_iter()
                .nth(index)
                .ok_or_else(|| format!("Element with index {index} not found"))?;
            let status = load_browser_page(&mut session, link.1.clone()).await?;
            if session.history_index + 1 < session.history.len() {
                session.history.truncate(session.history_index + 1);
            }
            session.history.push(link.1);
            session.history_index = session.history.len().saturating_sub(1);
            browser_state_output(&session, status.as_u16(), true)
        }
        "type" | "input_text" => {
            let field = string_argument(arguments, "field")
                .or_else(|| string_argument(arguments, "selector"))
                .or_else(|| {
                    arguments
                        .get("index")
                        .and_then(Value::as_u64)
                        .map(|index| format!("element_{index}"))
                })
                .unwrap_or_else(|| "active".to_string());
            let text = string_argument(arguments, "text").unwrap_or_default();
            if action == "input_text" && text.is_empty() {
                return Err("input_text requires text".to_string());
            }
            session.typed_values.insert(field.clone(), text.clone());
            Ok(format!("Recorded input for {field}: {}", truncate_output(&text)))
        }
        "scroll" | "scroll_down" | "scroll_up" => {
            let raw_amount = arguments
                .get("amount")
                .or_else(|| arguments.get("scroll_amount"))
                .and_then(Value::as_i64)
                .unwrap_or(1);
            let amount = if action == "scroll_up" {
                -raw_amount.abs()
            } else {
                raw_amount.abs()
            };
            session.scroll_y = (session.scroll_y + amount * 600).max(0);
            browser_state_output(&session, 200, false)
        }
        "scroll_to_text" => {
            let text = string_argument(arguments, "text")
                .ok_or_else(|| "scroll_to_text requires text".to_string())?;
            if !html_text(&session.html)
                .to_lowercase()
                .contains(&text.to_lowercase())
            {
                return Err(format!("Text not found on current page: {text}"));
            }
            session.scroll_y = session.scroll_y.max(600);
            Ok(format!("Scrolled to text: '{text}'"))
        }
        "send_keys" => {
            let keys = string_argument(arguments, "keys")
                .ok_or_else(|| "send_keys requires keys".to_string())?;
            session
                .typed_values
                .insert("keyboard".to_string(), keys.clone());
            Ok(format!("Sent keys: {keys}"))
        }
        "get_dropdown_options" => {
            let options = html_select_options(&session.html);
            serde_json::to_string_pretty(&json!({"options": options}))
                .map_err(|error| format!("Unable to encode dropdown options: {error}"))
        }
        "select_dropdown_option" => {
            let index = arguments
                .get("index")
                .and_then(Value::as_u64)
                .ok_or_else(|| "select_dropdown_option requires index".to_string())?;
            let text = string_argument(arguments, "text")
                .ok_or_else(|| "select_dropdown_option requires text".to_string())?;
            session
                .typed_values
                .insert(format!("select_{index}"), text.clone());
            Ok(format!("Selected option '{text}' from dropdown at index {index}"))
        }
        "web_search" => {
            let query = string_argument(arguments, "query")
                .ok_or_else(|| "web_search requires query".to_string())?;
            let result = run_web_search_tool(&json!({
                "query": query,
                "num_results": 5,
                "fetch_content": true
            }))
            .await?;
            if let Ok(value) = serde_json::from_str::<Value>(&result) {
                if let Some(url) = value
                    .pointer("/results/0/url")
                    .and_then(Value::as_str)
                {
                    let status = load_browser_page(&mut session, url.to_string()).await?;
                    return browser_state_output(&session, status.as_u16(), true);
                }
            }
            Ok(result)
        }
        "wait" => {
            let seconds = arguments
                .get("seconds")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .min(30);
            tokio::time::sleep(Duration::from_secs(seconds)).await;
            Ok(format!("Waited {seconds} second(s)."))
        }
        "switch_tab" => {
            let tab_id = arguments
                .get("tab_id")
                .and_then(Value::as_u64)
                .ok_or_else(|| "switch_tab requires tab_id".to_string())?
                as usize;
            let tab = session
                .tabs
                .iter()
                .find(|tab| tab.id == tab_id)
                .cloned()
                .ok_or_else(|| format!("Tab {tab_id} not found"))?;
            let status = load_browser_page(&mut session, tab.url).await?;
            session.active_tab_id = tab_id;
            browser_state_output(&session, status.as_u16(), true)
        }
        "close_tab" => {
            let tab_id = arguments
                .get("tab_id")
                .and_then(Value::as_u64)
                .unwrap_or(session.active_tab_id as u64) as usize;
            session.tabs.retain(|tab| tab.id != tab_id);
            if session.tabs.is_empty() {
                session.current_url.clear();
                session.title.clear();
                session.html.clear();
            } else if let Some(tab) = session.tabs.last().cloned() {
                session.active_tab_id = tab.id;
                let _ = load_browser_page(&mut session, tab.url).await?;
            }
            Ok(format!("Closed tab {tab_id}."))
        }
        "screenshot" => serde_json::to_string_pretty(&json!({
            "session_id": raw_session_id,
            "url": session.current_url,
            "title": session.title,
            "visual_available": true,
            "image_path": capture_browser_screenshot(&session.current_url).await?.display().to_string()
        }))
        .map_err(|error| format!("Unable to encode browser state: {error}")),
        _ => Err(format!("Unsupported browser action: {action}")),
    }?;

    state
        .browser_sessions
        .lock()
        .map_err(|_| "Browser session lock is poisoned".to_string())?
        .insert(session_id, session);
    Ok(truncate_output(&output))
}

fn browser_state_output(
    session: &BrowserSession,
    status_code: u16,
    include_text: bool,
) -> Result<String, String> {
    let links = html_links(&session.html, &session.current_url);
    let text = if include_text {
        Some(truncate_output(&html_text(&session.html)))
    } else {
        None
    };
    serde_json::to_string_pretty(&json!({
        "url": session.current_url,
        "title": session.title,
        "status_code": status_code,
        "scroll_y": session.scroll_y,
        "history_index": session.history_index,
        "active_tab_id": session.active_tab_id,
        "tabs": &session.tabs,
        "interactive_elements": links.iter().enumerate().take(50).map(|(index, (label, url))| json!({"index": index, "type": "link", "text": label, "url": url})).collect::<Vec<_>>(),
        "links": links.iter().take(30).map(|(label, url)| json!({"text": label, "url": url})).collect::<Vec<_>>(),
        "text": text
    }))
    .map_err(|error| format!("Unable to encode browser state: {error}"))
}

async fn run_python_tool(arguments: &Value) -> Result<String, String> {
    let code = string_argument(arguments, "code")
        .ok_or_else(|| "rust_python_execute requires code".to_string())?;
    #[cfg(target_os = "windows")]
    let mut process = Command::new("python");
    #[cfg(not(target_os = "windows"))]
    let mut process = Command::new("python3");
    let output = process
        .args(["-c", &code])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("Unable to start Python: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let result = json!({
        "success": output.status.success(),
        "observation": stdout.to_string(),
        "stderr": stderr.to_string(),
        "exit_code": output.status.code()
    });
    Ok(truncate_output(
        &serde_json::to_string_pretty(&result).unwrap_or_default(),
    ))
}

async fn run_sandbox_vision_tool(task_id: &str, arguments: &Value) -> Result<String, String> {
    let raw_path = string_argument(arguments, "path")
        .ok_or_else(|| "rust_sandbox_vision requires path".to_string())?;
    let path = sandbox_path_for_task(task_id, &raw_path)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| format!("Unable to inspect {}: {error}", path.display()))?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("Unable to read {}: {error}", path.display()))?;
    let mime_type = match extension.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    };
    let include_base64 = arguments
        .get("include_base64")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(serde_json::to_string_pretty(&json!({
        "path": path,
        "exists": true,
        "bytes": metadata.len(),
        "extension": extension,
        "visual_available": true,
        "mime_type": mime_type,
        "image_base64": include_base64.then(|| base64_encode(&bytes))
    }))
    .unwrap_or_default())
}

#[cfg(all(target_os = "windows", not(test)))]
#[repr(C)]
struct WinPoint {
    x: i32,
    y: i32,
}

#[cfg(all(target_os = "windows", not(test)))]
#[link(name = "user32")]
unsafe extern "system" {
    fn SetCursorPos(x: i32, y: i32) -> i32;
    fn GetCursorPos(point: *mut WinPoint) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn mouse_event(flags: u32, x: u32, y: u32, data: u32, extra_info: usize);
    fn keybd_event(virtual_key: u8, scan_code: u8, flags: u32, extra_info: usize);
    fn VkKeyScanW(character: u16) -> i16;
}

#[cfg(all(target_os = "windows", not(test)))]
#[repr(C)]
struct WinBitmapInfoHeader {
    size: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    size_image: u32,
    x_pels_per_meter: i32,
    y_pels_per_meter: i32,
    clr_used: u32,
    clr_important: u32,
}

#[cfg(all(target_os = "windows", not(test)))]
#[repr(C)]
struct WinBitmapInfo {
    header: WinBitmapInfoHeader,
    colors: [u32; 1],
}

#[cfg(all(target_os = "windows", not(test)))]
#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(device: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn CreateCompatibleBitmap(
        device: *mut std::ffi::c_void,
        width: i32,
        height: i32,
    ) -> *mut std::ffi::c_void;
    fn SelectObject(
        device: *mut std::ffi::c_void,
        object: *mut std::ffi::c_void,
    ) -> *mut std::ffi::c_void;
    fn BitBlt(
        destination: *mut std::ffi::c_void,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        source: *mut std::ffi::c_void,
        source_x: i32,
        source_y: i32,
        operation: u32,
    ) -> i32;
    fn GetDIBits(
        device: *mut std::ffi::c_void,
        bitmap: *mut std::ffi::c_void,
        start_scan: u32,
        scan_lines: u32,
        bits: *mut std::ffi::c_void,
        info: *mut WinBitmapInfo,
        usage: u32,
    ) -> i32;
    fn DeleteObject(object: *mut std::ffi::c_void) -> i32;
    fn DeleteDC(device: *mut std::ffi::c_void) -> i32;
}

#[cfg(all(target_os = "windows", not(test)))]
#[link(name = "user32")]
unsafe extern "system" {
    fn GetDC(window: *mut std::ffi::c_void) -> *mut std::ffi::c_void;
    fn ReleaseDC(window: *mut std::ffi::c_void, device: *mut std::ffi::c_void) -> i32;
}

#[cfg(all(target_os = "windows", not(test)))]
fn windows_key_code(key: &str) -> Option<u8> {
    let normalized = key.to_lowercase();
    let code = match normalized.as_str() {
        "enter" => 0x0D,
        "escape" | "esc" => 0x1B,
        "tab" => 0x09,
        "backspace" => 0x08,
        "space" => 0x20,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "home" => 0x24,
        "end" => 0x23,
        "delete" => 0x2E,
        "f1" => 0x70,
        "f2" => 0x71,
        "f3" => 0x72,
        "f4" => 0x73,
        "f5" => 0x74,
        "f6" => 0x75,
        "f7" => 0x76,
        "f8" => 0x77,
        "f9" => 0x78,
        "f10" => 0x79,
        "f11" => 0x7A,
        "f12" => 0x7B,
        _ if normalized.len() == 1 => return normalized.as_bytes().first().copied(),
        _ => return None,
    };
    Some(code)
}

#[cfg(all(target_os = "windows", not(test)))]
fn capture_screen_bmp() -> Result<(i32, i32, Vec<u8>), String> {
    const SRCCOPY: u32 = 0x00CC0020;
    const DIB_RGB_COLORS: u32 = 0;
    let width = unsafe { GetSystemMetrics(0) };
    let height = unsafe { GetSystemMetrics(1) };
    if width <= 0 || height <= 0 {
        return Err("Windows returned an invalid screen size.".to_string());
    }
    let screen = unsafe { GetDC(std::ptr::null_mut()) };
    if screen.is_null() {
        return Err("Unable to acquire the Windows screen device context.".to_string());
    }
    let memory = unsafe { CreateCompatibleDC(screen) };
    let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
    if memory.is_null() || bitmap.is_null() {
        if !bitmap.is_null() {
            unsafe { DeleteObject(bitmap) };
        }
        if !memory.is_null() {
            unsafe { DeleteDC(memory) };
        }
        unsafe { ReleaseDC(std::ptr::null_mut(), screen) };
        return Err("Unable to allocate a Windows screen bitmap.".to_string());
    }
    unsafe {
        SelectObject(memory, bitmap);
    }
    let copied = unsafe { BitBlt(memory, 0, 0, width, height, screen, 0, 0, SRCCOPY) };
    if copied == 0 {
        unsafe {
            DeleteObject(bitmap);
            DeleteDC(memory);
            ReleaseDC(std::ptr::null_mut(), screen);
        }
        return Err("Windows BitBlt failed while capturing the screen.".to_string());
    }
    let stride = (width as usize * 3).div_ceil(4) * 4;
    let mut pixels = vec![0u8; stride * height as usize];
    let mut info = WinBitmapInfo {
        header: WinBitmapInfoHeader {
            size: std::mem::size_of::<WinBitmapInfoHeader>() as u32,
            width,
            height: -height,
            planes: 1,
            bit_count: 24,
            compression: 0,
            size_image: pixels.len() as u32,
            x_pels_per_meter: 0,
            y_pels_per_meter: 0,
            clr_used: 0,
            clr_important: 0,
        },
        colors: [0],
    };
    let scan_lines = unsafe {
        GetDIBits(
            memory,
            bitmap,
            0,
            height as u32,
            pixels.as_mut_ptr().cast(),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    unsafe {
        DeleteObject(bitmap);
        DeleteDC(memory);
        ReleaseDC(std::ptr::null_mut(), screen);
    }
    if scan_lines == 0 {
        return Err("Windows GetDIBits failed while capturing the screen.".to_string());
    }
    let mut bmp = Vec::with_capacity(14 + 40 + pixels.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + 40 + pixels.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(54u32).to_le_bytes());
    bmp.extend_from_slice(&(40u32).to_le_bytes());
    bmp.extend_from_slice(&width.to_le_bytes());
    bmp.extend_from_slice(&(-height).to_le_bytes());
    bmp.extend_from_slice(&(1u16).to_le_bytes());
    bmp.extend_from_slice(&(24u16).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 16]);
    bmp.extend_from_slice(&pixels);
    Ok((width, height, bmp))
}

#[cfg(all(target_os = "windows", not(test)))]
fn computer_snapshot(arguments: &Value, external_path_approved: bool) -> Result<String, String> {
    let mut point = WinPoint { x: 0, y: 0 };
    let cursor_ok = unsafe { GetCursorPos(&mut point) != 0 };
    let (width, height, bmp) = capture_screen_bmp()?;
    let requested_path = string_argument(arguments, "path").unwrap_or_else(|| {
        workspace_root()
            .join(".rustpilot")
            .join(format!("screen-{}.bmp", Uuid::new_v4()))
            .display()
            .to_string()
    });
    let path = path_guard::resolve_mutation_path(
        &workspace_root(),
        &requested_path,
        external_path_approved,
    )?
    .canonical;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Unable to create screenshot directory: {error}"))?;
    }
    fs::write(&path, &bmp).map_err(|error| format!("Unable to write screenshot: {error}"))?;
    let include_base64 = arguments
        .get("include_base64")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    serde_json::to_string_pretty(&json!({
        "screen_width": width,
        "screen_height": height,
        "cursor": if cursor_ok { json!({"x": point.x, "y": point.y}) } else { Value::Null },
        "screenshot_available": true,
        "path": path,
        "mime_type": "image/bmp",
        "image_base64": include_base64.then(|| base64_encode(&bmp))
    }))
    .map_err(|error| format!("Unable to encode screenshot metadata: {error}"))
}

#[cfg(all(target_os = "windows", test))]
fn computer_snapshot(_arguments: &Value, _external_path_approved: bool) -> Result<String, String> {
    Ok(serde_json::to_string_pretty(&json!({
        "screenshot_available": false,
        "note": "Screen capture is disabled in the Windows GNU test binary."
    }))
    .unwrap_or_default())
}

#[cfg(not(target_os = "windows"))]
fn computer_snapshot(_arguments: &Value, _external_path_approved: bool) -> Result<String, String> {
    Ok(serde_json::to_string_pretty(&json!({
        "screenshot_available": false,
        "note": "Computer input is only available on Windows in this desktop build."
    }))
    .unwrap_or_default())
}

async fn run_computer_tool(
    arguments: &Value,
    external_path_approved: bool,
) -> Result<String, String> {
    let action = string_argument(arguments, "action")
        .ok_or_else(|| "rust_computer_use requires action".to_string())?;
    if action == "wait" {
        let duration = arguments
            .get("duration")
            .and_then(Value::as_f64)
            .unwrap_or(0.5)
            .clamp(0.0, 30.0);
        tokio::time::sleep(Duration::from_secs_f64(duration)).await;
        return Ok(format!("Waited for {duration:.2} seconds."));
    }
    if action == "screenshot" {
        return computer_snapshot(arguments, external_path_approved);
    }

    #[cfg(all(target_os = "windows", not(test)))]
    {
        match action.as_str() {
            "move_to" => {
                let x = arguments
                    .get("x")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "move_to requires x".to_string())?
                    as i32;
                let y = arguments
                    .get("y")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "move_to requires y".to_string())?
                    as i32;
                if unsafe { SetCursorPos(x, y) } == 0 {
                    return Err("Windows rejected SetCursorPos.".to_string());
                }
                Ok(format!("Moved cursor to ({x}, {y})."))
            }
            "click" => {
                if let (Some(x), Some(y)) = (
                    arguments.get("x").and_then(Value::as_i64),
                    arguments.get("y").and_then(Value::as_i64),
                ) {
                    unsafe {
                        SetCursorPos(x as i32, y as i32);
                    }
                }
                let button =
                    string_argument(arguments, "button").unwrap_or_else(|| "left".to_string());
                let (down, up) = match button.as_str() {
                    "right" => (0x0008, 0x0010),
                    "middle" => (0x0020, 0x0040),
                    _ => (0x0002, 0x0004),
                };
                let clicks = arguments
                    .get("num_clicks")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .clamp(1, 3);
                for _ in 0..clicks {
                    unsafe {
                        mouse_event(down, 0, 0, 0, 0);
                        mouse_event(up, 0, 0, 0, 0);
                    }
                }
                Ok(format!("Performed {clicks} {button} click(s)."))
            }
            "scroll" => {
                let amount = arguments
                    .get("amount")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    .clamp(-10, 10);
                unsafe {
                    mouse_event(0x0800, 0, 0, (amount * 120) as u32, 0);
                }
                Ok(format!("Scrolled by {amount}."))
            }
            "type" => {
                let text = string_argument(arguments, "text")
                    .ok_or_else(|| "type requires text".to_string())?;
                for character in text.encode_utf16() {
                    let mapped = unsafe { VkKeyScanW(character) };
                    if mapped == -1 {
                        continue;
                    }
                    let virtual_key = (mapped & 0xFF) as u8;
                    let shift_state = ((mapped >> 8) & 0xFF) as u8;
                    if shift_state & 1 != 0 {
                        unsafe {
                            keybd_event(0x10, 0, 0, 0);
                        }
                    }
                    unsafe {
                        keybd_event(virtual_key, 0, 0, 0);
                        keybd_event(virtual_key, 0, 0x0002, 0);
                    }
                    if shift_state & 1 != 0 {
                        unsafe {
                            keybd_event(0x10, 0, 0x0002, 0);
                        }
                    }
                }
                Ok(format!("Typed {} characters.", text.chars().count()))
            }
            "press" => {
                let key = string_argument(arguments, "key")
                    .ok_or_else(|| "press requires key".to_string())?;
                let virtual_key =
                    windows_key_code(&key).ok_or_else(|| format!("Unsupported key: {key}"))?;
                unsafe {
                    keybd_event(virtual_key, 0, 0, 0);
                    keybd_event(virtual_key, 0, 0x0002, 0);
                }
                Ok(format!("Pressed {key}."))
            }
            _ => Err(format!("Unsupported computer action: {action}")),
        }
    }
    #[cfg(any(not(target_os = "windows"), test))]
    {
        Err(
            "rust_computer_use input actions require Windows user32 in a production build."
                .to_string(),
        )
    }
}

fn format_plan(plan: &AgentPlan) -> String {
    let completed = plan
        .steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::Completed)
        .count();
    let in_progress = plan
        .steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::InProgress)
        .count();
    let blocked = plan
        .steps
        .iter()
        .filter(|step| step.status == PlanStepStatus::Blocked)
        .count();
    let mut output = format!(
        "Plan: {} (ID: {})\nProgress: {}/{} completed\nStatus: {} completed, {} in progress, {} blocked\nSteps:\n",
        plan.title,
        plan.id,
        completed,
        plan.steps.len(),
        completed,
        in_progress,
        blocked
    );
    for (index, step) in plan.steps.iter().enumerate() {
        let symbol = match step.status {
            PlanStepStatus::NotStarted => "[ ]",
            PlanStepStatus::InProgress => "[>]",
            PlanStepStatus::Completed => "[x]",
            PlanStepStatus::Blocked => "[!]",
        };
        output.push_str(&format!("{index}. {symbol} {}\n", step.title));
        if !step.description.is_empty() && step.description != step.title {
            output.push_str(&format!("   detail: {}\n", step.description));
        }
        if !step.notes.is_empty() {
            output.push_str(&format!("   notes: {}\n", step.notes));
        }
    }
    output
}

fn parse_plan_status(value: &str) -> Result<PlanStepStatus, String> {
    match value {
        "not_started" => Ok(PlanStepStatus::NotStarted),
        "in_progress" => Ok(PlanStepStatus::InProgress),
        "completed" => Ok(PlanStepStatus::Completed),
        "blocked" => Ok(PlanStepStatus::Blocked),
        _ => Err(format!("Invalid plan step status: {value}")),
    }
}

async fn run_planning_tool(
    state: &AppState,
    task_id: &str,
    arguments: &Value,
) -> Result<String, String> {
    let command = string_argument(arguments, "command")
        .ok_or_else(|| "rust_planning requires command".to_string())?;
    let output = {
        let mut tasks = state
            .tasks
            .write()
            .map_err(|_| "Task lock is poisoned".to_string())?;
        let task = tasks
            .get_mut(task_id)
            .ok_or_else(|| "Task not found".to_string())?;
        match command.as_str() {
            "create" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .ok_or_else(|| "plan_id is required for create".to_string())?;
                if task.plans.iter().any(|plan| plan.id == plan_id) {
                    return Err(format!("A plan with ID '{plan_id}' already exists."));
                }
                let title = string_argument(arguments, "title")
                    .ok_or_else(|| "title is required for create".to_string())?;
                let steps = arguments
                    .get("steps")
                    .and_then(Value::as_array)
                    .ok_or_else(|| "steps must be a non-empty array for create".to_string())?;
                if steps.is_empty() {
                    return Err("steps must be a non-empty array for create".to_string());
                }
                let plan = AgentPlan {
                    id: plan_id,
                    title,
                    steps: steps
                        .iter()
                        .enumerate()
                        .map(|(index, value)| {
                            let title = value.as_str().unwrap_or_default().trim().to_string();
                            AgentPlanStep {
                                id: new_id(&format!("plan_step_{index}")),
                                description: title.clone(),
                                title,
                                status: PlanStepStatus::NotStarted,
                                notes: String::new(),
                            }
                        })
                        .collect(),
                    created_at: now(),
                    updated_at: now(),
                };
                if plan.steps.iter().any(|step| step.title.is_empty()) {
                    return Err("Every plan step must be a non-empty string.".to_string());
                }
                task.active_plan_id = Some(plan.id.clone());
                let result = format_plan(&plan);
                task.plans.push(plan);
                mark_task_revision(task);
                format!("Plan created successfully.\n{result}")
            }
            "update" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .ok_or_else(|| "plan_id is required for update".to_string())?;
                let plan = task
                    .plans
                    .iter_mut()
                    .find(|plan| plan.id == plan_id)
                    .ok_or_else(|| format!("No plan found with ID: {plan_id}"))?;
                if let Some(title) = string_argument(arguments, "title") {
                    if !title.trim().is_empty() {
                        plan.title = title;
                    }
                }
                if let Some(steps) = arguments.get("steps").and_then(Value::as_array) {
                    let old_steps = plan.steps.clone();
                    let mut next_steps = Vec::new();
                    for (index, value) in steps.iter().enumerate() {
                        let title = value.as_str().unwrap_or_default().trim().to_string();
                        if title.is_empty() {
                            return Err("Every plan step must be a non-empty string.".to_string());
                        }
                        if let Some(old) = old_steps.get(index).filter(|old| old.title == title) {
                            next_steps.push(old.clone());
                        } else {
                            next_steps.push(AgentPlanStep {
                                id: new_id("plan_step"),
                                title: title.clone(),
                                description: title,
                                status: PlanStepStatus::NotStarted,
                                notes: String::new(),
                            });
                        }
                    }
                    plan.steps = next_steps;
                }
                plan.updated_at = now();
                let output = format!("Plan updated successfully.\n{}", format_plan(plan));
                mark_task_revision(task);
                output
            }
            "list" => {
                if task.plans.is_empty() {
                    "No plans available.".to_string()
                } else {
                    task.plans
                        .iter()
                        .map(|plan| {
                            let marker = if task.active_plan_id.as_deref() == Some(&plan.id) {
                                " (active)"
                            } else {
                                ""
                            };
                            let completed = plan
                                .steps
                                .iter()
                                .filter(|step| step.status == PlanStepStatus::Completed)
                                .count();
                            format!(
                                "- {}{}: {} ({}/{})",
                                plan.id,
                                marker,
                                plan.title,
                                completed,
                                plan.steps.len()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            "get" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .or_else(|| task.active_plan_id.clone())
                    .ok_or_else(|| "No active plan. Specify plan_id.".to_string())?;
                let plan = task
                    .plans
                    .iter()
                    .find(|plan| plan.id == plan_id)
                    .ok_or_else(|| format!("No plan found with ID: {plan_id}"))?;
                format_plan(plan)
            }
            "set_active" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .ok_or_else(|| "plan_id is required for set_active".to_string())?;
                if !task.plans.iter().any(|plan| plan.id == plan_id) {
                    return Err(format!("No plan found with ID: {plan_id}"));
                }
                task.active_plan_id = Some(plan_id.clone());
                mark_task_revision(task);
                format!("Plan '{plan_id}' is now active.")
            }
            "mark_step" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .or_else(|| task.active_plan_id.clone())
                    .ok_or_else(|| "No active plan. Specify plan_id.".to_string())?;
                let index = arguments
                    .get("step_index")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "step_index is required for mark_step".to_string())?;
                let status = string_argument(arguments, "step_status")
                    .map(|value| parse_plan_status(&value))
                    .transpose()?;
                let note = string_argument(arguments, "step_notes");
                let plan = task
                    .plans
                    .iter_mut()
                    .find(|plan| plan.id == plan_id)
                    .ok_or_else(|| format!("No plan found with ID: {plan_id}"))?;
                let step = plan
                    .steps
                    .get_mut(index.max(0) as usize)
                    .ok_or_else(|| format!("Invalid step_index: {index}"))?;
                if let Some(status) = status {
                    step.status = status;
                }
                if let Some(note) = note {
                    step.notes = note;
                }
                plan.updated_at = now();
                let output = format!("Step updated.\n{}", format_plan(plan));
                mark_task_revision(task);
                output
            }
            "delete" => {
                let plan_id = string_argument(arguments, "plan_id")
                    .ok_or_else(|| "plan_id is required for delete".to_string())?;
                let index = task
                    .plans
                    .iter()
                    .position(|plan| plan.id == plan_id)
                    .ok_or_else(|| format!("No plan found with ID: {plan_id}"))?;
                task.plans.remove(index);
                if task.active_plan_id.as_deref() == Some(&plan_id) {
                    task.active_plan_id = None;
                }
                mark_task_revision(task);
                format!("Plan '{plan_id}' deleted.")
            }
            _ => return Err(format!("Unsupported planning command: {command}")),
        }
    };
    state.persist_task(task_id)?;
    Ok(truncate_output(&output))
}

fn mcp_request(action: &str, request_id: u64, arguments: &Value) -> Result<Value, String> {
    match action {
        "initialize" => Ok(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "rustpilot", "version": "0.1.0"}
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

fn sanitize_mcp_name(value: &str) -> String {
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

fn parse_mcp_response(body: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
        return Ok(value);
    }
    let mut last = None;
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim();
            if !data.is_empty() && data != "[DONE]" {
                if let Ok(value) = serde_json::from_str::<Value>(data) {
                    last = Some(value);
                }
            }
        }
    }
    last.ok_or_else(|| "MCP returned neither JSON nor an SSE data payload.".to_string())
}

async fn mcp_session_request(session: &mut McpSession, request: Value) -> Result<Value, String> {
    if session.transport.eq_ignore_ascii_case("http") {
        let url = session
            .endpoint
            .clone()
            .ok_or_else(|| "MCP HTTP session has no endpoint.".to_string())?;
        let response = Client::builder()
            .user_agent("RustPilot/0.1 MCP client")
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| format!("Unable to create MCP client: {error}"))?
            .post(url)
            .json(&request)
            .send()
            .await
            .map_err(|error| format!("MCP HTTP request failed: {error}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| format!("Unable to read MCP response: {error}"))?;
        if !status.is_success() {
            return Err(format!("MCP HTTP {status}: {}", truncate_output(&body)));
        }
        return parse_mcp_response(&body);
    }

    let stdio = session
        .stdio
        .as_mut()
        .ok_or_else(|| "MCP stdio session is not connected.".to_string())?;
    stdio
        .stdin
        .write_all(format!("{}\n", request).as_bytes())
        .await
        .map_err(|error| format!("Unable to write to MCP stdio server: {error}"))?;
    stdio
        .stdin
        .flush()
        .await
        .map_err(|error| format!("Unable to flush MCP stdio server: {error}"))?;
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

async fn connect_mcp_session(
    state: &AppState,
    server_id: &str,
    transport: &str,
    url: Option<String>,
    command: Option<String>,
    args: Vec<String>,
) -> Result<String, String> {
    let transport = transport.to_lowercase();
    if !matches!(transport.as_str(), "http" | "sse" | "stdio") {
        return Err(format!("Unsupported MCP transport: {transport}"));
    }
    let mut session = if transport == "stdio" {
        let command = command.ok_or_else(|| "stdio MCP transport requires command".to_string())?;
        let mut child = Command::new(&command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
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
        McpSession {
            _server_id: server_id.to_string(),
            transport,
            endpoint: None,
            stdio: Some(McpStdioSession {
                _child: child,
                stdin,
                stdout: BufReader::new(stdout),
            }),
            next_id: 1,
        }
    } else {
        McpSession {
            _server_id: server_id.to_string(),
            transport,
            endpoint: Some(url.ok_or_else(|| "HTTP/SSE MCP transport requires url".to_string())?),
            stdio: None,
            next_id: 1,
        }
    };

    let initialize = mcp_request("initialize", session.next_id, &json!({}))?;
    session.next_id += 1;
    let initialized_response = mcp_session_request(&mut session, initialize).await?;
    if initialized_response.get("error").is_some() {
        return Err(format!("MCP initialize failed: {initialized_response}"));
    }
    let notification = mcp_request("initialized", session.next_id, &json!({}))?;
    session.next_id += 1;
    if session.transport == "stdio" {
        if let Some(stdio) = session.stdio.as_mut() {
            stdio
                .stdin
                .write_all(format!("{}\n", notification).as_bytes())
                .await
                .map_err(|error| format!("Unable to notify MCP server: {error}"))?;
            stdio
                .stdin
                .flush()
                .await
                .map_err(|error| format!("Unable to flush MCP notification: {error}"))?;
        }
    }
    let tools_request = mcp_request("list_tools", session.next_id, &json!({}))?;
    session.next_id += 1;
    let tools_response = mcp_session_request(&mut session, tools_request).await?;
    if tools_response.get("error").is_some() {
        return Err(format!("MCP tools/list failed: {tools_response}"));
    }
    register_mcp_tools(state, server_id, &tools_response)?;
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

fn register_mcp_tools(state: &AppState, server_id: &str, response: &Value) -> Result<(), String> {
    let tools = response
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .ok_or_else(|| "MCP tools/list response has no result.tools array.".to_string())?;

    let server = sanitize_mcp_name(server_id);
    let mut incoming = tools
        .iter()
        .map(|tool| {
            let remote_name = tool
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| "MCP tool is missing name.".to_string())?;
            let remote = sanitize_mcp_name(remote_name);
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
    // MCP servers are allowed to return tools in any order. Sort before merging
    // so sanitized-name collisions have deterministic winner selection too.
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

async fn run_dynamic_mcp_tool(
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
    let request = mcp_request(
        "call_tool",
        session.next_id,
        &json!({
            "tool_name": definition.remote_name,
            "arguments": arguments
        }),
    )?;
    session.next_id += 1;
    let response = mcp_session_request(session, request).await?;
    if response.get("error").is_some() {
        return Err(format!("MCP tool failed: {response}"));
    }
    Ok(truncate_output(
        &serde_json::to_string_pretty(&response).unwrap_or_else(|_| response.to_string()),
    ))
}

async fn run_mcp_tool(state: &AppState, arguments: &Value) -> Result<String, String> {
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
                connect_mcp_session(state, &server_id, &transport, url, command, args).await?;
            } else {
                let response = {
                    let mut sessions = state.mcp_sessions.lock().await;
                    let session = sessions
                        .get_mut(&server_id)
                        .ok_or_else(|| "MCP session disappeared.".to_string())?;
                    let request = mcp_request("list_tools", session.next_id, &json!({}))?;
                    session.next_id += 1;
                    mcp_session_request(session, request).await?
                };
                register_mcp_tools(state, &server_id, &response)?;
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
                return run_dynamic_mcp_tool(
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
            let request = mcp_request("call_tool", session.next_id, arguments)?;
            session.next_id += 1;
            let response = mcp_session_request(session, request).await?;
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

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '"' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => {
                values.push(current.trim().to_string());
                current.clear();
            }
            _ => current.push(character),
        }
    }
    values.push(current.trim().to_string());
    values
}

fn value_to_cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Bool(value) => value.to_string(),
        _ => value.to_string(),
    }
}

fn table_from_contents(
    path: &str,
    contents: &str,
) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    if path.to_lowercase().ends_with(".json") {
        let value: Value = serde_json::from_str(contents)
            .map_err(|error| format!("Unable to parse JSON data: {error}"))?;
        let rows = value
            .get("data")
            .and_then(Value::as_array)
            .or_else(|| value.as_array())
            .cloned()
            .unwrap_or_default();
        let mut headers = Vec::new();
        for row in &rows {
            if let Some(object) = row.as_object() {
                for key in object.keys() {
                    if !headers.contains(key) {
                        headers.push(key.clone());
                    }
                }
            }
        }
        if headers.is_empty() && !rows.is_empty() {
            headers.push("value".to_string());
        }
        let cells = rows
            .iter()
            .map(|row| {
                if let Some(object) = row.as_object() {
                    headers
                        .iter()
                        .map(|header| object.get(header).map(value_to_cell).unwrap_or_default())
                        .collect::<Vec<_>>()
                } else {
                    vec![value_to_cell(row)]
                }
            })
            .collect();
        return Ok((headers, cells));
    }
    let mut lines = contents.lines().filter(|line| !line.trim().is_empty());
    let headers = parse_csv_line(lines.next().unwrap_or_default());
    if headers.is_empty() || headers.iter().all(String::is_empty) {
        return Err("CSV data has no header row.".to_string());
    }
    Ok((headers, lines.map(parse_csv_line).collect()))
}

async fn load_table(path: &str) -> Result<(Vec<String>, Vec<Vec<String>>), String> {
    let contents = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| format!("Unable to read data file {path}: {error}"))?;
    table_from_contents(path, &contents)
}

async fn run_data_analysis_tool(arguments: &Value) -> Result<String, String> {
    let path = string_argument(arguments, "path")
        .or_else(|| string_argument(arguments, "json_path"))
        .ok_or_else(|| "rust_data_analysis requires path".to_string())?;
    let (headers, rows) = load_table(&path).await?;
    let mut missing = vec![0usize; headers.len()];
    let mut numeric_sum = vec![0.0f64; headers.len()];
    let mut numeric_count = vec![0usize; headers.len()];
    let sample_limit = arguments
        .get("sample_rows")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 20) as usize;
    let mut sample = Vec::new();
    for values in &rows {
        if sample.len() < sample_limit {
            sample.push(values.clone());
        }
        for index in 0..headers.len() {
            let value = values.get(index).map(String::as_str).unwrap_or_default();
            if value.is_empty() {
                missing[index] += 1;
            } else if let Ok(number) = value.parse::<f64>() {
                numeric_sum[index] += number;
                numeric_count[index] += 1;
            }
        }
    }
    let summaries = headers
        .iter()
        .enumerate()
        .map(|(index, header)| {
            json!({
                "name": header,
                "missing": missing[index],
                "numeric_count": numeric_count[index],
                "mean": (numeric_count[index] > 0).then_some(numeric_sum[index] / numeric_count[index] as f64)
            })
        })
        .collect::<Vec<_>>();
    Ok(truncate_output(
        &serde_json::to_string_pretty(&json!({
            "format": "csv",
            "rows": rows.len(),
            "columns": headers,
            "summaries": summaries,
            "sample": sample
        }))
        .unwrap_or_default(),
    ))
}

fn escape_html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn chart_values(headers: &[String], rows: &[Vec<String>]) -> (Vec<String>, Vec<f64>) {
    let numeric_index = headers
        .iter()
        .enumerate()
        .find(|(index, _)| {
            rows.iter().any(|row| {
                row.get(*index)
                    .and_then(|value| value.parse::<f64>().ok())
                    .is_some()
            })
        })
        .map(|(index, _)| index)
        .unwrap_or(0);
    let labels = rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            row.first()
                .filter(|value| !value.is_empty())
                .cloned()
                .unwrap_or_else(|| (index + 1).to_string())
        })
        .collect::<Vec<_>>();
    let values = rows
        .iter()
        .map(|row| {
            row.get(numeric_index)
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    (labels, values)
}

fn render_svg_chart(title: &str, labels: &[String], values: &[f64]) -> String {
    let width = 900.0;
    let height = 500.0;
    let max = values.iter().copied().fold(0.0f64, f64::max).max(1.0);
    let bar_width = if values.is_empty() {
        0.0
    } else {
        760.0 / values.len() as f64
    };
    let bars = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let bar_height = (value.max(0.0) / max) * 360.0;
            let x = 90.0 + index as f64 * bar_width + bar_width * 0.12;
            let y = 420.0 - bar_height;
            format!(
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{:.1}\" height=\"{bar_height:.1}\" fill=\"#c45132\"/><text x=\"{:.1}\" y=\"445\" font-size=\"11\" text-anchor=\"middle\">{}</text>",
                (bar_width * 0.76).max(3.0),
                x + (bar_width * 0.76).max(3.0) / 2.0,
                escape_html(labels.get(index).map(String::as_str).unwrap_or_default())
            )
        })
        .collect::<String>();
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width} {height}\"><rect width=\"100%\" height=\"100%\" fill=\"#fbfaf7\"/><text x=\"40\" y=\"42\" font-family=\"sans-serif\" font-size=\"22\" fill=\"#262522\">{}</text><line x1=\"90\" y1=\"60\" x2=\"90\" y2=\"420\" stroke=\"#77736b\"/><line x1=\"90\" y1=\"420\" x2=\"850\" y2=\"420\" stroke=\"#77736b\"/>{bars}</svg>",
        escape_html(title)
    )
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(12 + data.len());
    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
    chunk.extend_from_slice(kind);
    chunk.extend_from_slice(data);
    let mut checksum = Vec::with_capacity(4 + data.len());
    checksum.extend_from_slice(kind);
    checksum.extend_from_slice(data);
    chunk.extend_from_slice(&crc32(&checksum).to_be_bytes());
    chunk
}

fn write_png_chart(path: &Path, values: &[f64]) -> Result<(), String> {
    let width = 900usize;
    let height = 500usize;
    let stride = width * 3;
    let mut pixels = vec![255u8; stride * height];
    let set_pixel = |pixels: &mut [u8], x: usize, y: usize, color: [u8; 3]| {
        if x < width && y < height {
            let index = y * stride + x * 3;
            pixels[index..index + 3].copy_from_slice(&color);
        }
    };
    for x in 90..850 {
        set_pixel(&mut pixels, x, 420, [119, 115, 107]);
    }
    for y in 60..421 {
        set_pixel(&mut pixels, 90, y, [119, 115, 107]);
    }
    let max = values.iter().copied().fold(0.0f64, f64::max).max(1.0);
    let bar_width = if values.is_empty() {
        0
    } else {
        760 / values.len()
    };
    for (index, value) in values.iter().enumerate() {
        let bar_height = ((value.max(0.0) / max) * 360.0) as usize;
        let start_x = 90 + index * bar_width + bar_width / 8;
        let end_x = (start_x + bar_width * 3 / 4).min(width);
        let start_y = 420usize.saturating_sub(bar_height);
        for y in start_y..420 {
            for x in start_x..end_x {
                set_pixel(&mut pixels, x, y, [196, 81, 50]);
            }
        }
    }
    let mut raw = Vec::with_capacity((stride + 1) * height);
    for row in pixels.chunks(stride) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut compressed = vec![0x78, 0x01];
    let mut offset = 0;
    while offset < raw.len() {
        let length = (raw.len() - offset).min(65_535);
        let final_block = offset + length == raw.len();
        compressed.push(u8::from(final_block));
        compressed.extend_from_slice(&(length as u16).to_le_bytes());
        compressed.extend_from_slice(&(!(length as u16)).to_le_bytes());
        compressed.extend_from_slice(&raw[offset..offset + length]);
        offset += length;
    }
    let mut a = 1u32;
    let mut b = 0u32;
    for byte in &raw {
        a = (a + *byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    compressed.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&(width as u32).to_be_bytes());
    header.extend_from_slice(&(height as u32).to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    png.extend_from_slice(&png_chunk(b"IHDR", &header));
    png.extend_from_slice(&png_chunk(b"IDAT", &compressed));
    png.extend_from_slice(&png_chunk(b"IEND", &[]));
    fs::write(path, png).map_err(|error| format!("Unable to write chart PNG: {error}"))
}

fn run_visualization_preparation(
    arguments: &Value,
    external_path_approved: bool,
) -> Result<String, String> {
    let kind = string_argument(arguments, "kind").unwrap_or_else(|| "bar".to_string());
    let title =
        string_argument(arguments, "title").unwrap_or_else(|| "RustPilot chart".to_string());
    let labels = arguments
        .get("labels")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let values = arguments
        .get("values")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let specification = json!({
        "type": kind,
        "title": title,
        "labels": labels,
        "values": values
    });
    if let Some(path) = string_argument(arguments, "output_path") {
        let path =
            path_guard::resolve_mutation_path(&workspace_root(), &path, external_path_approved)?
                .canonical;
        fs::write(
            &path,
            serde_json::to_vec_pretty(&specification).unwrap_or_default(),
        )
        .map_err(|error| format!("Unable to write visualization specification: {error}"))?;
    }
    Ok(serde_json::to_string_pretty(&json!({
        "specification": specification,
        "renderer": "rustpilot_svg_png_html"
    }))
    .unwrap_or_default())
}

async fn run_data_visualization_tool(arguments: &Value) -> Result<String, String> {
    let input_path = string_argument(arguments, "path")
        .or_else(|| string_argument(arguments, "json_path"))
        .ok_or_else(|| "rust_data_visualization requires path or json_path".to_string())?;
    let output_type = string_argument(arguments, "output_type")
        .unwrap_or_else(|| "html".to_string())
        .to_lowercase();
    let tool_type = string_argument(arguments, "tool_type")
        .unwrap_or_else(|| "visualization".to_string())
        .to_lowercase();
    let mut sources = Vec::new();
    let descriptor = if input_path.to_lowercase().ends_with(".json") {
        tokio::fs::read_to_string(&input_path)
            .await
            .ok()
            .and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
    } else {
        None
    };
    if let Some(Value::Array(items)) = descriptor {
        for item in items {
            let path = item
                .get("csvFilePath")
                .or_else(|| item.get("path"))
                .or_else(|| item.get("file"))
                .and_then(Value::as_str)
                .map(ToString::to_string);
            if let Some(path) = path {
                sources.push((
                    path,
                    item.get("chartTitle")
                        .or_else(|| item.get("title"))
                        .and_then(Value::as_str)
                        .unwrap_or("RustPilot data chart")
                        .to_string(),
                ));
            }
        }
    }
    if sources.is_empty() {
        sources.push((
            input_path,
            string_argument(arguments, "title")
                .unwrap_or_else(|| "RustPilot data report".to_string()),
        ));
    }
    let workspace = workspace_root();
    let output_dir = path_guard::resolve_scoped_path(&workspace, ".rustpilot/visualization")?;
    tokio::fs::create_dir_all(&output_dir)
        .await
        .map_err(|error| format!("Unable to create visualization directory: {error}"))?;
    let mut results = Vec::new();
    for (source_path, title) in sources.into_iter().take(16) {
        let source_path = if Path::new(&source_path).is_absolute() {
            source_path
        } else {
            workspace_root().join(source_path).display().to_string()
        };
        let (headers, rows) = load_table(&source_path).await?;
        let (labels, values) = chart_values(&headers, &rows);
        let slug = title
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let stem = format!("{}_{}", slug.trim_matches('_'), Uuid::new_v4().simple());
        let chart_path = if output_type == "png" {
            output_dir.join(format!("{stem}.png"))
        } else {
            output_dir.join(format!("{stem}.html"))
        };
        if output_type == "png" {
            write_png_chart(&chart_path, &values)?;
        } else {
            let svg = render_svg_chart(&title, &labels, &values);
            let rows_html = rows
                .iter()
                .take(100)
                .map(|row| {
                    format!(
                        "<tr>{}</tr>",
                        row.iter()
                            .map(|cell| format!("<td>{}</td>", escape_html(cell)))
                            .collect::<String>()
                    )
                })
                .collect::<String>();
            let html = format!("<!doctype html><meta charset=\"utf-8\"><title>{}</title><style>body{{font-family:system-ui,sans-serif;background:#fbfaf7;color:#262522;margin:32px}}table{{border-collapse:collapse;margin-top:24px}}td,th{{border:1px solid #d5d1c8;padding:6px 9px;text-align:left}}</style><h1>{}</h1>{}<table><thead><tr>{}</tr></thead><tbody>{}</tbody></table>", escape_html(&title), escape_html(&title), svg, headers.iter().map(|header| format!("<th>{}</th>", escape_html(header))).collect::<String>(), rows_html);
            tokio::fs::write(&chart_path, html)
                .await
                .map_err(|error| format!("Unable to write chart HTML: {error}"))?;
        }
        let insight_path = if tool_type == "insight" {
            let path = output_dir.join(format!("{stem}.md"));
            let numeric = values
                .iter()
                .filter(|value| value.is_finite())
                .copied()
                .collect::<Vec<_>>();
            let average = if numeric.is_empty() {
                0.0
            } else {
                numeric.iter().sum::<f64>() / numeric.len() as f64
            };
            tokio::fs::write(
                &path,
                format!(
                    "# {}\n\n- Rows: {}\n- Numeric points: {}\n- Mean: {:.3}\n",
                    title,
                    rows.len(),
                    numeric.len(),
                    average
                ),
            )
            .await
            .map_err(|error| format!("Unable to write chart insights: {error}"))?;
            Some(path.display().to_string())
        } else {
            None
        };
        results.push(json!({"title": title, "chart_path": chart_path, "output_type": output_type.clone(), "insight_path": insight_path, "rows": rows.len()}));
    }
    Ok(truncate_output(
        &serde_json::to_string_pretty(&json!({
            "status": "success",
            "observation": "Chart Generated Successful!",
            "results": results
        }))
        .unwrap_or_default(),
    ))
}

async fn terminate_shell_session(state: &AppState, task_id: &str, name: &str, arguments: &Value) {
    if !matches!(name, "rust_bash" | "rust_sandbox_shell") {
        return;
    }
    let session_id =
        string_argument(arguments, "session_id").unwrap_or_else(|| "default".to_string());
    let key = if name == "rust_sandbox_shell" {
        format!("sandbox:{task_id}:{session_id}")
    } else {
        session_id
    };
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
            run_tool_inner(
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

    let requires_approval = is_high_risk(name, arguments);
    if requires_approval {
        let approval = match wait_for_approval(app, state, task_id, name, arguments, cancel).await {
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
            ApprovalOutcome::Approved => {}
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
        arguments,
        settings,
        cancel,
        requires_approval,
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
fn demo_is_cjk(prompt: &str) -> bool {
    prompt
        .chars()
        .any(|character| ('\u{4e00}'..='\u{9fff}').contains(&character))
}

#[cfg(test)]
fn demo_capability_request(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    demo_contains_any(
        &lower,
        &[
            "what can you do",
            "what are you able to do",
            "capabilities",
            "help",
            "你可以",
            "能做什么",
            "你会什么",
            "干什么",
            "功能",
            "介绍",
        ],
    )
}

#[cfg(test)]
fn demo_requested_time(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    demo_contains_any(&lower, &["time", "date", "clock", "时间", "几点", "日期"])
}

#[cfg(test)]
fn demo_requested_files(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
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
            "文件",
            "目录",
            "项目",
            "仓库",
            "代码",
        ],
    )
}

#[cfg(test)]
fn demo_requested_search(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    demo_contains_any(
        &lower,
        &[
            "find", "search", "research", "web", "lookup", "查找", "搜索", "资料", "学习", "研究",
        ],
    )
}

#[cfg(test)]
fn demo_requested_shell(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    demo_contains_any(
        &lower,
        &[
            "shell",
            "terminal",
            "command",
            "run command",
            "运行命令",
            "执行命令",
        ],
    )
}

#[cfg(test)]
fn demo_requested_write(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    demo_contains_any(
        &lower,
        &[
            "write",
            "create file",
            "save",
            "写入",
            "创建文件",
            "保存",
            "修改文件",
        ],
    )
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
    let lower = prompt.to_lowercase();
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
fn demo_result_summary(name: &str, output: &str, cjk: bool) -> String {
    match name {
        "rust_clock" => {
            if cjk {
                "已读取本机时间。具体原始值已收进 Rust Trace。".to_string()
            } else {
                "Read the local time. The raw value is available in Rust Trace.".to_string()
            }
        }
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
            if cjk {
                format!(
                    "已检查当前工作区，共发现 {count} 个文件和目录。完整清单已收进 Rust Trace。"
                )
            } else {
                format!("Inspected the workspace and found {count} files and directories. The full list is in Rust Trace.")
            }
        }
        "rust_shell" => {
            let exit_code = output
                .lines()
                .find_map(|line| line.strip_prefix("exit_code: "))
                .unwrap_or("unknown");
            if cjk {
                format!("命令已执行，退出码为 {exit_code}。")
            } else {
                format!("The command completed with exit code {exit_code}.")
            }
        }
        "rust_http" => {
            let status = output.lines().next().unwrap_or("HTTP response received");
            if cjk {
                format!("已完成网络请求（{status}）。响应正文已收进 Rust Trace。")
            } else {
                format!(
                    "Completed the network request ({status}). The response body is in Rust Trace."
                )
            }
        }
        "rust_web_search" => {
            if cjk {
                "已完成网页检索。候选来源已保留在 Rust Trace，便于继续核验。".to_string()
            } else {
                "Completed the web search. Candidate sources are available in Rust Trace for verification.".to_string()
            }
        }
        _ => {
            if cjk {
                format!("{name} 已返回结果，并完成了基础校验。")
            } else {
                format!("{name} returned a result and it passed the basic verification.")
            }
        }
    }
}

#[cfg(test)]
fn demo_answer(prompt: &str, results: &[(String, ToolResult)]) -> String {
    let cjk = demo_is_cjk(prompt);
    if demo_capability_request(prompt) {
        return if cjk {
            "我可以帮你完成本地工作：阅读和整理文件、运行受控命令、访问网页、分析数据、创建文件，并在高风险操作前请求确认。\n\n当前是 Demo 模式：安全的本地工具会真实执行，但不会调用在线模型。配置 OpenAI-compatible API Key 后，才会启用完整的模型驱动 Agent。".to_string()
        } else {
            "I can help with local work: inspect and organize files, run controlled commands, visit web pages, analyze data, and create files. High-risk actions always ask for approval first.\n\nDemo mode is active: safe local tools run for real, but no online model is called. Configure an OpenAI-compatible API key to enable the full model-driven agent.".to_string()
        };
    }
    if results.is_empty() {
        return if cjk {
            "我已理解这个请求。当前 Demo 模式会在确实需要时调用本地工具，并把结果整理成简洁结论。"
                .to_string()
        } else {
            "I understood the request. Demo mode only calls a local tool when it is actually needed, then returns a concise conclusion.".to_string()
        };
    }

    let summaries = results
        .iter()
        .map(|(name, result)| match (&result.output, &result.error) {
            (_, Some(error)) => {
                if cjk {
                    format!("{name} 执行失败：{error}")
                } else {
                    format!("{name} failed: {error}")
                }
            }
            (Some(output), _) => demo_result_summary(name, output, cjk),
            _ => {
                if cjk {
                    format!("{name} 没有返回可用结果。")
                } else {
                    format!("{name} returned no usable result.")
                }
            }
        })
        .collect::<Vec<_>>();
    if cjk {
        format!("本次任务已完成。\n\n{}", summaries.join("\n"))
    } else {
        format!("The task is complete.\n\n{}", summaries.join("\n"))
    }
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
    if task.prompt.to_lowercase().contains("fail") || task.prompt.contains("失败") {
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

fn tool_definitions() -> Vec<Value> {
    fn function(name: &str, description: &str, parameters: Value) -> Value {
        tool::ToolDefinition::new(name, description, parameters).to_param()
    }

    vec![
        function(
            "rust_clock",
            "Read the local machine time.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        function(
            "rust_files",
            "List, read, inspect, write, or delete local files. Mutating operations require approval.",
            json!({
                "type": "object",
                "properties": {
                    "operation": {"type": "string", "enum": ["list", "read", "write", "delete", "exists"]},
                    "path": {"type": "string", "description": "A relative path inside the active workspace or an absolute path. Relative paths cannot escape through .. or links; external absolute mutation paths require explicit approval."},
                    "content": {"type": "string"}
                },
                "required": ["operation"]
            }),
        ),
        function(
            "rust_http",
            "Make a bounded HTTP request. GET is read-only; other methods require approval.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"]},
                    "headers": {"type": "object"},
                    "body": {"type": "string"}
                },
                "required": ["url"]
            }),
        ),
        function(
            "rust_shell",
            "Run a local shell command. Every call requires explicit user approval.",
            json!({
                "type": "object",
                "properties": {"command": {"type": "string"}, "cwd": {"type": "string"}},
                "required": ["command"]
            }),
        ),
        function(
            "rust_bash",
            "Run a persistent named shell session with a remembered working directory. Approval required.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "session_id": {"type": "string"},
                    "cwd": {"type": "string"},
                    "restart": {"type": "boolean"}
                },
                "required": ["command"]
            }),
        ),
        function(
            "rust_str_replace_editor",
            "View, create, replace, insert, and undo edits in files. Mutating commands require approval.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["view", "create", "str_replace", "insert", "undo_edit"]},
                    "path": {"type": "string"},
                    "file_text": {"type": "string"},
                    "old_str": {"type": "string"},
                    "new_str": {"type": "string"},
                    "insert_line": {"type": "integer"},
                    "view_range": {"type": "array", "items": {"type": "integer"}}
                },
                "required": ["command", "path"]
            }),
        ),
        function(
            "rust_planning",
            "Create and manage a durable plan with step statuses and notes.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["create", "update", "list", "get", "set_active", "mark_step", "delete"]},
                    "plan_id": {"type": "string"},
                    "title": {"type": "string"},
                    "steps": {"type": "array", "items": {"type": "string"}},
                    "step_index": {"type": "integer"},
                    "step_status": {"type": "string", "enum": ["not_started", "in_progress", "completed", "blocked"]},
                    "step_notes": {"type": "string"}
                },
                "required": ["command"]
            }),
        ),
        function(
            "rust_terminate",
            "End the current agent run with a success or failure message.",
            json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["success", "failure"]},
                    "message": {"type": "string"}
                },
                "required": ["status", "message"]
            }),
        ),
        function(
            "rust_ask_human",
            "Ask the desktop user for a blocking approval or decision.",
            json!({
                "type": "object",
                "properties": {"question": {"type": "string"}, "options": {"type": "array", "items": {"type": "string"}}},
                "required": ["question"]
            }),
        ),
        function(
            "rust_python_execute",
            "Execute a bounded Python snippet and return stdout/stderr. Approval required.",
            json!({
                "type": "object",
                "properties": {"code": {"type": "string"}, "timeout": {"type": "integer"}},
                "required": ["code"]
            }),
        ),
        function(
            "rust_web_search",
            "Search the public web and return titles, URLs, snippets, and optional page text.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "num_results": {"type": "integer"},
                    "fetch_content": {"type": "boolean"},
                    "lang": {"type": "string"}
                },
                "required": ["query"]
            }),
        ),
        function(
            "rust_crawl4ai",
            "Fetch one or more pages and extract clean, bounded text and link metadata.",
            json!({
                "type": "object",
                "properties": {
                    "urls": {"type": "array", "items": {"type": "string"}},
                    "timeout": {"type": "integer"},
                    "word_count_threshold": {"type": "integer"},
                    "bypass_cache": {"type": "boolean"}
                },
                "required": ["urls"]
            }),
        ),
        function(
            "rust_browser_use",
            "Use a persistent browser session with indexed DOM interaction, extraction, scrolling, tabs, search, and real Chromium screenshots. HTTP DOM mode remains available when Chromium is not present.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["go_to_url", "click_element", "input_text", "scroll_down", "scroll_up", "scroll_to_text", "send_keys", "get_dropdown_options", "select_dropdown_option", "go_back", "web_search", "wait", "extract_content", "switch_tab", "open_tab", "close_tab", "open", "back", "forward", "refresh", "extract", "click", "type", "scroll", "screenshot"]},
                    "url": {"type": "string"},
                    "text": {"type": "string"},
                    "selector": {"type": "string"},
                    "session_id": {"type": "string"},
                    "amount": {"type": "integer"},
                    "scroll_amount": {"type": "integer"},
                    "field": {"type": "string"},
                    "index": {"type": "integer"},
                    "tab_id": {"type": "integer"},
                    "query": {"type": "string"},
                    "goal": {"type": "string"},
                    "keys": {"type": "string"},
                    "seconds": {"type": "integer"}
                },
                "required": ["action"]
            }),
        ),
        function(
            "rust_computer_use",
            "Use the local desktop input surface for cursor, click, scroll, typing, keys, wait, and actual screen capture. Mutating actions require approval.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["move_to", "click", "scroll", "type", "press", "wait", "screenshot"]},
                    "x": {"type": "integer"},
                    "y": {"type": "integer"},
                    "amount": {"type": "integer"},
                    "text": {"type": "string"},
                    "key": {"type": "string"},
                    "duration": {"type": "number"},
                    "path": {"type": "string", "description": "Optional screenshot path. Relative paths stay inside the active workspace; an explicit external path requires approval."},
                    "include_base64": {"type": "boolean"}
                },
                "required": ["action"]
            }),
        ),
        function(
            "rust_sandbox_files",
            "Operate on files inside the RustPilot workspace sandbox. Mutating operations require approval.",
            json!({
                "type": "object",
                "properties": {"operation": {"type": "string", "enum": ["list", "read", "write", "delete", "exists"]}, "path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["operation"]
            }),
        ),
        function(
            "rust_sandbox_shell",
            "Run a command in a persistent RustPilot workspace sandbox shell. Approval required.",
            json!({
                "type": "object",
                "properties": {"command": {"type": "string"}, "session_id": {"type": "string"}, "cwd": {"type": "string"}},
                "required": ["command"]
            }),
        ),
        function(
            "rust_sandbox_browser",
            "Use the browser session scoped to the local sandbox workspace.",
            json!({
                "type": "object",
                "properties": {"action": {"type": "string"}, "url": {"type": "string"}, "text": {"type": "string"}, "selector": {"type": "string"}, "session_id": {"type": "string"}, "amount": {"type": "integer"}},
                "required": ["action"]
            }),
        ),
        function(
            "rust_sandbox_vision",
            "Inspect a local sandbox image and optionally return its image payload for multimodal workflows.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "include_base64": {"type": "boolean"}}, "required": ["path"]}),
        ),
        function(
            "rust_mcp",
            "Connect to MCP servers over HTTP/SSE or persistent stdio, initialize them, discover live tool schemas, refresh them, call tools, and disconnect. Discovered tools are exposed as rust_mcp_<server>_<tool>.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["connect", "list_tools", "call_tool", "disconnect"]},
                    "transport": {"type": "string", "enum": ["http", "sse", "stdio"]},
                    "server_id": {"type": "string"},
                    "url": {"type": "string"},
                    "command": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "tool_name": {"type": "string"},
                    "arguments": {"type": "object"}
                },
                "required": ["action"]
            }),
        ),
        function(
            "rust_create_chat_completion",
            "Request a non-streaming structured completion from the configured OpenAI-compatible endpoint.",
            json!({
                "type": "object",
                "properties": {"messages": {"type": "array"}, "response_format": {"type": "object"}},
                "required": ["messages"]
            }),
        ),
        function(
            "rust_visualization_preparation",
            "Prepare a compact chart specification from tabular data. An output_path writes a local file and requires explicit approval; relative paths stay inside the active workspace.",
            json!({"type": "object", "properties": {"title": {"type": "string"}, "kind": {"type": "string"}, "labels": {"type": "array"}, "values": {"type": "array"}, "output_path": {"type": "string", "description": "Optional file path. Relative paths must stay inside the active workspace; external absolute paths require explicit approval."}}, "required": ["kind"]}),
        ),
        function(
            "rust_data_visualization",
            "Generate real HTML or PNG charts and optional Markdown insights from CSV/JSON data or a json_path descriptor.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "json_path": {"type": "string"}, "kind": {"type": "string"}, "title": {"type": "string"}, "output_type": {"type": "string", "enum": ["html", "png"]}, "tool_type": {"type": "string", "enum": ["visualization", "insight"]}, "language": {"type": "string", "enum": ["en", "zh"]}}, "required": []}),
        ),
        function(
            "rust_data_analysis",
            "Profile a CSV or JSON file with row counts, columns, numeric summaries, and missing values.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "sample_rows": {"type": "integer"}}, "required": ["path"]}),
        ),
    ]
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut entries = object.iter().collect::<Vec<_>>();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            let mut normalized = serde_json::Map::new();
            for (key, value) in entries {
                normalized.insert(key.clone(), canonical_json(value));
            }
            Value::Object(normalized)
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        _ => value.clone(),
    }
}

fn stable_hash_hex(bytes: &[u8]) -> String {
    // Two independent FNV-1a lanes provide a stable, dependency-free 128-bit
    // identifier. This is a cache namespace, not a security boundary.
    let mut first = 0xcbf29ce484222325u64;
    let mut second = 0x84222325cbf29ce4u64;
    for &byte in bytes {
        first ^= u64::from(byte);
        first = first.wrapping_mul(0x100000001b3);
        second ^= u64::from(byte.wrapping_add(0x9d));
        second = second.wrapping_mul(0x100000001b3);
    }
    format!("{first:016x}{second:016x}")
}

fn tool_schema_hash(definitions: &[Value]) -> String {
    let normalized = canonical_json(&Value::Array(definitions.to_vec()));
    let bytes = serde_json::to_vec(&normalized).unwrap_or_default();
    stable_hash_hex(&bytes)
}

fn tool_definitions_for_state(state: &AppState) -> Arc<ToolDefinitionSnapshot> {
    let revision = state.mcp_tools_revision.load(Ordering::Acquire);
    if let Ok(cache) = state.tool_definition_cache.read() {
        if let Some((cached_revision, snapshot)) = cache.as_ref() {
            if *cached_revision == revision {
                return snapshot.clone();
            }
        }
    }

    let mut definitions = tool_definitions();
    let mut dynamic = state
        .mcp_tools
        .read()
        .ok()
        .map(|tools| tools.values().cloned().collect::<Vec<_>>())
        .unwrap_or_default();
    dynamic.sort_by(|left, right| left.exposed_name.cmp(&right.exposed_name));
    for tool in dynamic {
        definitions.push(json!({
            "type": "function",
            "function": {
                "name": tool.exposed_name,
                "description": format!("MCP tool {} from server {}. {}", tool.remote_name, tool.server_id, tool.description),
                "parameters": tool.input_schema
            }
        }));
    }
    definitions.sort_by(|left, right| {
        left.pointer("/function/name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .cmp(
                right
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
            )
    });
    let snapshot = Arc::new(ToolDefinitionSnapshot {
        schema_hash: Arc::<str>::from(tool_schema_hash(&definitions)),
        definitions: Arc::new(definitions),
    });
    if let Ok(mut cache) = state.tool_definition_cache.write() {
        *cache = Some((revision, snapshot.clone()));
    }
    snapshot
}

fn available_tool_views(state: Option<&AppState>) -> Vec<AgentToolDefinition> {
    let definitions = state
        .map(tool_definitions_for_state)
        .map(|snapshot| snapshot.definitions.clone())
        .unwrap_or_else(|| Arc::new(tool_definitions()));
    definitions
        .iter()
        .filter_map(|definition| {
            let function = definition.get("function")?;
            Some(AgentToolDefinition {
                name: function.get("name")?.as_str()?.to_string(),
                description: function.get("description")?.as_str()?.to_string(),
            })
        })
        .collect()
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

fn system_prompt_parts(agent_kind: &str) -> (String, String) {
    let kind = agent::parse_agent_kind(agent_kind).unwrap_or(agent::AgentKind::Manus);
    let workspace = workspace_root().display().to_string();
    let spec = agents::AgentSpec::for_kind(kind, &workspace);
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
    let (system_header, system_policy) = system_prompt_parts(&task.agent_kind);
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
    let agent_spec =
        agents::AgentSpec::for_kind(agent_kind, &workspace_root().display().to_string());
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
            emit_event(
                &app,
                "task_failed",
                TaskFailedEvent {
                    task_id,
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
            emit_event(
                &app,
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
            emit_event(
                &app,
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
            emit_event(
                &app,
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
        demo_mode: settings.api_key.is_none(),
        available_tools: available_tool_views(state),
    }
}

fn task_summary(task: &Task) -> TaskSummary {
    TaskSummary {
        id: task.id.clone(),
        title: task.title.clone(),
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

#[tauri::command(rename_all = "camelCase")]
async fn list_tasks(state: State<'_, AppState>) -> Result<Vec<TaskSummary>, String> {
    task_summaries(&state, false)
}

#[tauri::command(rename_all = "camelCase")]
async fn list_archived_tasks(state: State<'_, AppState>) -> Result<Vec<TaskSummary>, String> {
    task_summaries(&state, true)
}

#[tauri::command(rename_all = "camelCase")]
async fn get_task(state: State<'_, AppState>, task_id: String) -> Result<Task, String> {
    task_snapshot(&state, &task_id)
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
) -> Result<Task, String> {
    let prompt = prompt.trim().to_string();
    if prompt.is_empty() && attachment_inputs.is_empty() && attachment_paths.is_empty() {
        return Err("Add a prompt or attach at least one file.".to_string());
    }
    if !api_key_configured(state)? {
        return Err(API_KEY_REQUIRED_MESSAGE.to_string());
    }
    let demo_mode = false;
    let task_id = new_id("task");
    let attachment_refs =
        store_task_attachments(state, &task_id, &attachment_inputs, &attachment_paths)?;
    let created_at = now();
    let task = Task {
        id: task_id.clone(),
        title: task_title(&prompt, attachment_refs.len()),
        prompt: prompt.clone(),
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
    emit_event(app, "task_created", task.clone());
    emit_event(
        app,
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
) -> Result<Task, String> {
    create_task_internal(
        &app,
        &state,
        prompt,
        attachment_inputs.unwrap_or_default(),
        attachment_paths.unwrap_or_default(),
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
    emit_event(
        app,
        "task_status",
        TaskStatusEvent {
            task_id: task_id.clone(),
            status: AgentStatus::Idle,
            updated_at: task.updated_at,
            error: None,
        },
    );
    emit_event(app, "task_message", message);
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
    let _updated_task = {
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
        task.messages.push(TaskMessage {
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
        });
        touch_task(task);
        task.clone()
    };
    let task = task_snapshot(&state, &task_id)?;
    state.persist_task(&task_id)?;
    emit_event(
        &app,
        "task_status",
        TaskStatusEvent {
            task_id: task_id.clone(),
            status: AgentStatus::Idle,
            updated_at: task.updated_at,
            error: None,
        },
    );
    start_task(&app, &state, task_id);
    Ok(task)
}

#[tauri::command(rename_all = "camelCase")]
async fn archive_task(state: State<'_, AppState>, task_id: String) -> Result<TaskSummary, String> {
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
    Ok(summary)
}

#[tauri::command(rename_all = "camelCase")]
async fn restore_task(state: State<'_, AppState>, task_id: String) -> Result<TaskSummary, String> {
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
    state.persist_deleted_task(&task_id, delete_revision)?;
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
    let view = settings_view(&settings, Some(&state));
    drop(settings);
    state.persist_settings()?;
    Ok(view)
}

#[tauri::command(rename_all = "camelCase")]
async fn respond_to_approval(
    state: State<'_, AppState>,
    task_id: String,
    approval_id: String,
    approved: bool,
) -> Result<bool, String> {
    let sender = state
        .approval_waiters
        .write()
        .map_err(|_| "Approval lock is poisoned".to_string())?
        .remove(&approval_id);
    if let Some(sender) = sender {
        update_approval_status(
            &state,
            &task_id,
            &approval_id,
            if approved { "approved" } else { "rejected" },
        );
        let _ = sender.send(approved);
        Ok(true)
    } else {
        Ok(false)
    }
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
            let task = match create_task_internal(&app, &state, query, Vec::new(), Vec::new()) {
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
            get_task,
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
mod tests {
    use super::*;

    #[test]
    fn title_is_trimmed_and_bounded() {
        let title = make_title("  first   task   with   spacing  ");
        assert_eq!(title, "first task with spacing");

        let long = make_title(&"a".repeat(100));
        assert_eq!(long.chars().count(), 59);
        assert!(long.ends_with("..."));
    }

    #[test]
    fn max_agent_steps_default_to_100_and_preserve_custom_values() {
        assert_eq!(default_settings().max_steps, 100);
        assert_eq!(normalize_max_steps(0), 1);
        assert_eq!(normalize_max_steps(250), 250);
    }

    #[test]
    fn legacy_task_summaries_default_to_unarchived() {
        let summary: TaskSummary = serde_json::from_value(json!({
            "id": "task-1",
            "title": "Legacy task",
            "status": "completed",
            "updated_at": 1,
            "demo_mode": true,
            "error": null
        }))
        .expect("legacy task summary should remain readable");

        assert!(!summary.archived);
    }

    fn test_task(task_id: &str) -> Task {
        let message_id = format!("{task_id}-message");
        Task {
            id: task_id.to_string(),
            title: "Test task".to_string(),
            prompt: "test prompt".to_string(),
            status: AgentStatus::Idle,
            created_at: 1,
            updated_at: 1,
            demo_mode: false,
            archived: false,
            agent_name: default_agent_name(),
            agent_kind: default_agent_kind(),
            messages: vec![TaskMessage {
                id: message_id,
                task_id: task_id.to_string(),
                role: "assistant".to_string(),
                content: String::new(),
                created_at: 1,
                streaming: false,
                parts: Vec::new(),
                tool_calls: Vec::new(),
                tool_call_id: None,
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            }],
            memory: Vec::new(),
            plans: Vec::new(),
            active_plan_id: None,
            steps: Vec::new(),
            tool_calls: Vec::new(),
            approval_requests: Vec::new(),
            llm_usage: llm::TokenUsage::default(),
            final_answer: None,
            error: None,
            persistence_revision: 1,
        }
    }

    #[test]
    fn invalid_task_files_are_preserved_and_recovery_files_are_read() {
        let directory =
            std::env::temp_dir().join(format!("rustpilot-task-recovery-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("test directory should be created");
        let path = directory.join(LEGACY_TASK_FILE);
        fs::write(&path, "{ invalid json").expect("invalid task file should be written");
        fs::write(legacy_task_temp_path(&path), "[]").expect("recovery file should be written");

        let tasks = load_legacy_task_records(&path).expect("recovery file should be readable");
        assert!(tasks.is_empty());
        assert!(fs::read_dir(&directory)
            .expect("test directory should be readable")
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("corrupt-")));

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn legacy_tasks_migrate_to_sqlite_and_are_removed_after_commit() {
        let directory =
            std::env::temp_dir().join(format!("rustpilot-task-migration-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("test directory should be created");
        let path = directory.join(LEGACY_TASK_FILE);
        let task = test_task("task-1");
        fs::write(
            &path,
            serde_json::to_string(&vec![task.clone()]).expect("task should be encoded"),
        )
        .expect("legacy task file should be written");

        let loaded = load_task_store(&directory).expect("legacy task should migrate");
        assert_eq!(loaded.tasks.len(), 1);
        assert_eq!(loaded.tasks["task-1"].prompt, task.prompt);
        assert!(task_database_path(&directory).exists());
        assert!(!path.exists());
        assert!(!legacy_task_temp_path(&path).exists());
        assert!(!legacy_task_backup_path(&path).exists());

        let reopened = load_task_store(&directory).expect("migrated task should reopen");
        assert_eq!(reopened.tasks["task-1"].prompt, "test prompt");

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn sqlite_replays_stream_events_without_rewriting_the_task_snapshot() {
        let directory =
            std::env::temp_dir().join(format!("rustpilot-task-events-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("test directory should be created");
        let task = test_task("task-1");
        let mut connection =
            open_task_database(&task_database_path(&directory)).expect("task database should open");
        let transaction = connection
            .transaction()
            .expect("event transaction should begin");
        insert_task_state(&transaction, &task).expect("task snapshot should insert");
        insert_stream_event(
            &transaction,
            "task-1",
            "task-1-message",
            &PersistedStreamEvent::TextDelta("hello".to_string()),
        )
        .expect("stream event should insert");
        transaction
            .commit()
            .expect("event transaction should commit");
        drop(connection);

        let loaded = load_task_store(&directory).expect("stream event should replay");
        assert_eq!(loaded.tasks["task-1"].messages[0].content, "hello");
        assert!(loaded.event_bytes["task-1"] > 0);

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn pending_snapshot_and_stream_delta_are_applied_once() {
        let base = test_task("task-1");
        let event = PendingStreamEvent {
            revision: 2,
            message_id: "task-1-message".to_string(),
            event: PersistedStreamEvent::TextDelta("hello".to_string()),
        };
        let stream_write = PendingTaskWrite::Stream {
            events: vec![event.clone()],
        };
        let merged = merge_pending_task_writes(
            stream_write,
            PendingTaskWrite::Upsert {
                task: {
                    let mut task = base.clone();
                    apply_persisted_stream_event(
                        &mut task,
                        "task-1",
                        "task-1-message",
                        &event.event,
                    )
                    .expect("stream event should apply");
                    task.persistence_revision = 2;
                    task
                },
                stream_events: Vec::new(),
            },
        );
        let writes = PendingTaskWrites {
            by_task: HashMap::from([("task-1".to_string(), merged)]),
        };
        let durable = HashMap::from([("task-1".to_string(), base)]);
        let projected = project_task_writes(&durable, &HashMap::new(), &writes)
            .expect("merged task write should project");
        assert_eq!(
            projected.tasks["task-1"]
                .as_ref()
                .expect("task should be projected")
                .messages[0]
                .content,
            "hello"
        );
    }

    #[test]
    fn stream_events_compact_back_into_the_task_snapshot() {
        let directory =
            std::env::temp_dir().join(format!("rustpilot-task-compaction-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("test directory should be created");
        let task = test_task("task-1");
        let events = (0..5000)
            .map(|revision| PendingStreamEvent {
                revision: revision + 2,
                message_id: "task-1-message".to_string(),
                event: PersistedStreamEvent::TextDelta("x".to_string()),
            })
            .collect::<Vec<_>>();
        let writes = PendingTaskWrites {
            by_task: HashMap::from([("task-1".to_string(), PendingTaskWrite::Stream { events })]),
        };
        let durable = HashMap::from([("task-1".to_string(), task.clone())]);
        let projected = project_task_writes(&durable, &HashMap::new(), &writes)
            .expect("stream batch should project");
        assert!(projected.compacted.contains("task-1"));
        let connection =
            open_task_database(&task_database_path(&directory)).expect("task database should open");
        let (_, _, _, result) = commit_task_writes(connection, writes, projected);
        result.expect("compaction transaction should commit");
        let loaded = load_task_store(&directory).expect("compacted database should reopen");
        assert_eq!(loaded.tasks["task-1"].messages[0].content.len(), 5000);
        assert_eq!(loaded.event_bytes.get("task-1").copied(), Some(0));

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn sqlite_delete_commit_does_not_resurrect_deleted_tasks() {
        let directory =
            std::env::temp_dir().join(format!("rustpilot-task-delete-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory).expect("test directory should be created");
        let task = test_task("task-1");
        let mut connection =
            open_task_database(&task_database_path(&directory)).expect("task database should open");
        let transaction = connection
            .transaction()
            .expect("task transaction should begin");
        insert_task_state(&transaction, &task).expect("task snapshot should insert");
        transaction
            .commit()
            .expect("task transaction should commit");

        let writes = PendingTaskWrites {
            by_task: HashMap::from([(
                "task-1".to_string(),
                PendingTaskWrite::Delete { revision: 2 },
            )]),
        };
        let projected = ProjectedTaskChanges {
            tasks: HashMap::from([("task-1".to_string(), None)]),
            event_bytes: HashMap::from([("task-1".to_string(), 0)]),
            compacted: HashSet::new(),
        };
        let (_, _, _, result) = commit_task_writes(connection, writes, projected);
        result.expect("delete transaction should commit");
        assert!(load_task_store(&directory)
            .expect("deleted database should reopen")
            .tasks
            .is_empty());

        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn demo_capability_questions_do_not_run_unrelated_tools() {
        assert!(demo_tool_calls("你可以干什么").is_empty());
        assert!(demo_tool_calls("What can you do?").is_empty());
    }

    #[test]
    fn demo_answers_summarize_tool_evidence() {
        let result = ToolResult {
            id: "result-1".to_string(),
            task_id: "task-1".to_string(),
            tool_call_id: "call-1".to_string(),
            status: ToolCallStatus::Completed,
            output: Some("file src/main.rs\ndir src".to_string()),
            error: None,
            duration_ms: Some(2),
        };
        let answer = demo_answer("检查项目文件", &[("rust_files".to_string(), result)]);
        assert!(answer.contains("2 个文件和目录"));
        assert!(!answer.contains("src/main.rs"));
    }

    #[test]
    fn status_uses_snake_case() {
        assert_eq!(
            serde_json::to_string(&AgentStatus::WaitingApproval).expect("status should serialize"),
            "\"waiting_approval\""
        );
    }

    #[test]
    fn shell_and_file_writes_are_high_risk() {
        assert!(is_high_risk("rust_shell", &json!({"command": "echo hi"})));
        assert!(is_high_risk(
            "rust_files",
            &json!({"operation": "write", "path": "a.txt"})
        ));
        assert!(!is_high_risk(
            "rust_files",
            &json!({"operation": "list", "path": "."})
        ));
    }

    #[test]
    fn explicit_file_outputs_are_high_risk_and_show_resolved_scope() {
        let visualization = json!({
            "kind": "bar",
            "output_path": "artifacts/chart.json"
        });
        assert!(is_high_risk(
            "rust_visualization_preparation",
            &visualization
        ));
        assert!(!is_high_risk(
            "rust_visualization_preparation",
            &json!({"kind": "bar"})
        ));

        let screenshot = json!({
            "action": "screenshot",
            "path": "artifacts/screen.bmp"
        });
        assert!(is_high_risk("rust_computer_use", &screenshot));

        let details = approval_details("rust_visualization_preparation", &visualization);
        assert!(details.contains("_rustpilot_path_authorization"));
        assert!(details.contains("resolved"));
        assert!(details.contains("workspace"));
    }

    #[test]
    fn completion_url_accepts_common_base_url_shapes() {
        assert_eq!(
            llm::OpenAiCompatibleClient::completion_url("https://example.test/v1"),
            "https://example.test/v1/chat/completions"
        );
        assert_eq!(
            llm::OpenAiCompatibleClient::completion_url("https://example.test/v1/"),
            "https://example.test/v1/chat/completions"
        );
        assert_eq!(
            llm::OpenAiCompatibleClient::completion_url("https://example.test/v1/chat/completions",),
            "https://example.test/v1/chat/completions"
        );
    }

    #[test]
    fn every_registered_tool_uses_rust_prefix() {
        let definitions = tool_definitions();
        assert!(definitions.len() >= 20);
        for definition in definitions {
            let name = definition["function"]["name"]
                .as_str()
                .expect("tool name should be a string");
            assert!(name.starts_with("rust_"), "unexpected tool name: {name}");
        }
    }

    #[test]
    fn tool_snapshot_order_and_hash_are_stable() {
        let first = vec![
            json!({"function": {"name": "rust_b", "parameters": {"b": 1, "a": 2}}}),
            json!({"function": {"name": "rust_a", "parameters": {"nested": {"z": true, "x": false}}}}),
        ];
        let second = vec![
            json!({"function": {"name": "rust_b", "parameters": {"a": 2, "b": 1}}}),
            json!({"function": {"name": "rust_a", "parameters": {"nested": {"x": false, "z": true}}}}),
        ];
        assert_eq!(tool_schema_hash(&first), tool_schema_hash(&second));

        let state = AppState::new();
        let snapshot = tool_definitions_for_state(&state);
        let names = snapshot
            .definitions
            .iter()
            .filter_map(|definition| definition.pointer("/function/name").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert!(names.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(snapshot.schema_hash.len(), 32);
    }

    #[test]
    fn identical_mcp_refresh_does_not_invalidate_tool_snapshot() {
        let state = AppState::new();
        let response = json!({
            "result": {
                "tools": [
                    {
                        "name": "z_tool",
                        "description": "Z",
                        "inputSchema": {"type": "object"}
                    },
                    {
                        "name": "a_tool",
                        "description": "A",
                        "inputSchema": {"type": "object"}
                    }
                ]
            }
        });
        register_mcp_tools(&state, "demo", &response).unwrap();
        let first_revision = state.mcp_tools_revision.load(Ordering::Acquire);
        let first_snapshot = tool_definitions_for_state(&state);
        register_mcp_tools(&state, "demo", &response).unwrap();
        let second_snapshot = tool_definitions_for_state(&state);
        assert_eq!(
            state.mcp_tools_revision.load(Ordering::Acquire),
            first_revision
        );
        assert_eq!(first_snapshot.schema_hash, second_snapshot.schema_hash);
    }

    #[test]
    fn system_prompt_is_split_into_stable_cacheable_parts() {
        let first = system_prompt_parts("manus");
        let second = system_prompt_parts("manus");
        assert_eq!(first, second);
        assert!(!first.0.is_empty());
        assert!(!first.1.is_empty());
    }

    #[test]
    fn agent_kind_tracks_agent_specializations() {
        assert_eq!(infer_agent_kind("分析这个 CSV 并画图"), "data_analysis");
        assert_eq!(infer_agent_kind("修复这个 code bug"), "swe");
        assert_eq!(infer_agent_kind("打开浏览器网页"), "browser");
        assert_eq!(infer_agent_kind("普通任务"), "manus");
    }

    #[test]
    fn html_tools_extract_text_and_resolve_links() {
        let html = "<html><script>ignore()</script><title>Page</title><body>Hello <a href='/next'>Next page</a></body></html>";
        assert_eq!(html_title(html), "Page");
        assert_eq!(html_text(html), "Page Hello Next page");
        assert_eq!(
            html_links(html, "https://example.test/start")[0].1,
            "https://example.test/next"
        );
    }

    #[test]
    fn planning_format_reports_expected_statuses() {
        let plan = AgentPlan {
            id: "p1".to_string(),
            title: "Inspect".to_string(),
            steps: vec![AgentPlanStep {
                id: "s1".to_string(),
                title: "Read".to_string(),
                description: "Read the input".to_string(),
                status: PlanStepStatus::InProgress,
                notes: "started".to_string(),
            }],
            created_at: 0,
            updated_at: 0,
        };
        let formatted = format_plan(&plan);
        assert!(formatted.contains("0/1 completed"));
        assert!(formatted.contains("[>] Read"));
        assert!(formatted.contains("notes: started"));
    }

    #[test]
    fn browser_and_mcp_mutations_require_approval() {
        assert!(is_high_risk(
            "rust_browser_use",
            &json!({"action": "click", "text": "Submit"})
        ));
        assert!(!is_high_risk(
            "rust_browser_use",
            &json!({"action": "extract"})
        ));
        assert!(is_high_risk("rust_mcp", &json!({"action": "call_tool"})));
    }

    #[test]
    fn csv_parser_handles_quoted_commas_and_mcp_names_are_safe() {
        let (headers, rows) = table_from_contents("sample.csv", "name,value\n\"A, B\",2\n")
            .expect("CSV should parse");
        assert_eq!(headers, vec!["name", "value"]);
        assert_eq!(rows[0][0], "A, B");
        assert_eq!(sanitize_mcp_name("Weather Tool/v2"), "weather_tool_v2");
    }

    #[test]
    fn screenshot_and_mcp_payload_helpers_are_real_encodings() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"M"), "TQ==");
        let response = parse_mcp_response("event: message\ndata: {\"result\":{\"tools\":[]}}\n")
            .expect("SSE MCP response should parse");
        assert_eq!(response["result"]["tools"], json!([]));
    }

    #[test]
    fn chart_png_writer_emits_a_valid_png_signature() {
        let path = std::env::temp_dir().join(format!("rustpilot-chart-{}.png", Uuid::new_v4()));
        write_png_chart(&path, &[1.0, 3.0, 2.0]).expect("chart should be written");
        let bytes = fs::read(&path).expect("chart should be readable");
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn persisted_task_messages_accept_pre_feature_records() {
        let message: TaskMessage = serde_json::from_value(json!({
            "id": "m1",
            "task_id": "t1",
            "role": "user",
            "content": "hello",
            "created_at": 0,
            "streaming": false
        }))
        .expect("old task message should deserialize");
        assert!(message.parts.is_empty());
        assert!(message.tool_calls.is_empty());
        assert!(message.tool_call_id.is_none());
    }

    #[test]
    fn assistant_parts_keep_text_and_tool_order_with_unicode_offsets() {
        let mut message = TaskMessage {
            id: "assistant-1".to_string(),
            task_id: "task-1".to_string(),
            role: "assistant".to_string(),
            content: String::new(),
            created_at: 1,
            streaming: true,
            parts: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        };

        apply_stream_event(&mut message, &llm::StreamEvent::TextDelta("先".to_string()));
        apply_stream_event(
            &mut message,
            &llm::StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".to_string()),
                name: Some("rust_clock".to_string()),
                arguments: Some("{}".to_string()),
            },
        );
        apply_stream_event(
            &mut message,
            &llm::StreamEvent::TextDelta("🙂后".to_string()),
        );

        assert_eq!(message.content, "先🙂后");
        assert!(matches!(
            &message.parts[0],
            AssistantPart::Text {
                start: 0,
                end: 1,
                ..
            }
        ));
        assert!(matches!(
            &message.parts[1],
            AssistantPart::Tool { index: 0, call_id, name, .. }
                if call_id == "call-1" && name == "rust_clock"
        ));
        assert!(matches!(
            &message.parts[2],
            AssistantPart::Text {
                start: 1,
                end: 4,
                ..
            }
        ));
    }

    #[test]
    fn legacy_assistant_messages_get_compact_ordered_parts() {
        let mut message: TaskMessage = serde_json::from_value(json!({
            "id": "assistant-legacy",
            "task_id": "task-1",
            "role": "assistant",
            "content": "先检查",
            "created_at": 1,
            "streaming": false,
            "tool_calls": [{
                "id": "call-1",
                "type": "function",
                "function": {"name": "rust_clock", "arguments": "{}"}
            }]
        }))
        .expect("legacy assistant message should deserialize");

        ensure_assistant_parts(&mut message);
        assert!(matches!(
            &message.parts[0],
            AssistantPart::Text {
                start: 0,
                end: 3,
                ..
            }
        ));
        assert!(matches!(
            &message.parts[1],
            AssistantPart::Tool { index: 0, call_id, .. } if call_id == "call-1"
        ));
    }

    #[test]
    fn interrupted_stream_placeholder_is_repaired_without_entering_context() {
        let mut task: Task = serde_json::from_value(json!({
            "id": "task-1",
            "title": "Interrupted request",
            "prompt": "question",
            "status": "failed",
            "created_at": 1,
            "updated_at": 1,
            "demo_mode": false,
            "messages": [
                {
                    "id": "user-1",
                    "task_id": "task-1",
                    "role": "user",
                    "content": "question",
                    "created_at": 1,
                    "streaming": false
                },
                {
                    "id": "assistant-placeholder",
                    "task_id": "task-1",
                    "role": "assistant",
                    "content": "",
                    "created_at": 2,
                    "streaming": true
                }
            ],
            "memory": [],
            "plans": [],
            "active_plan_id": null,
            "steps": [],
            "tool_calls": [],
            "approval_requests": [],
            "llm_usage": {},
            "final_answer": null,
            "error": "interrupted"
        }))
        .expect("interrupted task record should deserialize");

        assert!(repair_task_record(&mut task));
        assert!(!task.messages[1].streaming);
        assert_eq!(task.memory.len(), 1);
        assert_eq!(task.memory[0].id, "user-1");
    }

    #[test]
    fn context_repairs_legacy_ui_tool_ids_to_model_ids() {
        let call = agent::MessageToolCall {
            id: "call-model-1".to_string(),
            call_type: "function".to_string(),
            function: agent::FunctionCall {
                name: "rust_clock".to_string(),
                arguments: "{}".to_string(),
            },
        };
        let entries = vec![
            AgentMemoryEntry {
                id: "assistant-1".to_string(),
                role: "assistant".to_string(),
                content: String::new(),
                created_at: 1,
                tool_call_id: None,
                tool_names: vec!["rust_clock".to_string()],
                tool_calls: vec![call.clone()],
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            },
            AgentMemoryEntry {
                id: "tool-1".to_string(),
                role: "tool".to_string(),
                content: "12:00".to_string(),
                created_at: 2,
                tool_call_id: Some("tool-ui-1".to_string()),
                tool_names: vec!["rust_clock".to_string()],
                tool_calls: Vec::new(),
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            },
        ];

        let (normalized, changed) = normalize_memory_for_context(&entries);
        assert!(changed);
        assert_eq!(normalized[1].tool_call_id.as_deref(), Some("call-model-1"));
        validate_chat_message_context(&memory_to_chat_messages(&normalized))
            .expect("repaired history should be valid for the model");
        assert_eq!(normalized[1].content, "12:00");
        assert_eq!(normalized[0].tool_calls, vec![call]);
    }

    #[test]
    fn context_inserts_a_truthful_result_for_an_interrupted_tool_call() {
        let entries = vec![
            AgentMemoryEntry {
                id: "assistant-1".to_string(),
                role: "assistant".to_string(),
                content: String::new(),
                created_at: 1,
                tool_call_id: None,
                tool_names: Vec::new(),
                tool_calls: vec![agent::MessageToolCall {
                    id: "call-missing".to_string(),
                    call_type: "function".to_string(),
                    function: agent::FunctionCall {
                        name: "rust_shell".to_string(),
                        arguments: "{\"command\":\"pwd\"}".to_string(),
                    },
                }],
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            },
            AgentMemoryEntry {
                id: "user-2".to_string(),
                role: "user".to_string(),
                content: "继续".to_string(),
                created_at: 2,
                tool_call_id: None,
                tool_names: Vec::new(),
                tool_calls: Vec::new(),
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            },
        ];

        let (normalized, changed) = normalize_memory_for_context(&entries);
        assert!(changed);
        assert_eq!(normalized.len(), 3);
        assert_eq!(normalized[1].role, "tool");
        assert_eq!(normalized[1].tool_call_id.as_deref(), Some("call-missing"));
        assert!(normalized[1].content.contains("No result was recorded"));
        validate_chat_message_context(&memory_to_chat_messages(&normalized))
            .expect("interrupted history should be made replayable");
    }

    #[test]
    fn context_budget_keeps_assistant_and_tool_messages_together() {
        let mut entries = vec![
            AgentMemoryEntry {
                id: "old-user".to_string(),
                role: "user".to_string(),
                content: "old".to_string(),
                created_at: 1,
                tool_call_id: None,
                tool_names: Vec::new(),
                tool_calls: Vec::new(),
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            },
            AgentMemoryEntry {
                id: "old-assistant".to_string(),
                role: "assistant".to_string(),
                content: "done".to_string(),
                created_at: 2,
                tool_call_id: None,
                tool_names: Vec::new(),
                tool_calls: Vec::new(),
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            },
            AgentMemoryEntry {
                id: "new-user".to_string(),
                role: "user".to_string(),
                content: "new".to_string(),
                created_at: 3,
                tool_call_id: None,
                tool_names: Vec::new(),
                tool_calls: Vec::new(),
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            },
            AgentMemoryEntry {
                id: "new-assistant".to_string(),
                role: "assistant".to_string(),
                content: String::new(),
                created_at: 4,
                tool_call_id: None,
                tool_names: Vec::new(),
                tool_calls: vec![agent::MessageToolCall {
                    id: "call-new".to_string(),
                    call_type: "function".to_string(),
                    function: agent::FunctionCall {
                        name: "rust_clock".to_string(),
                        arguments: "{}".to_string(),
                    },
                }],
                name: None,
                base64_image: None,
                attachments: Vec::new(),
            },
            AgentMemoryEntry {
                id: "new-tool".to_string(),
                role: "tool".to_string(),
                content: "now".to_string(),
                created_at: 5,
                tool_call_id: Some("call-new".to_string()),
                tool_names: vec!["rust_clock".to_string()],
                tool_calls: Vec::new(),
                name: Some("rust_clock".to_string()),
                base64_image: None,
                attachments: Vec::new(),
            },
        ];

        trim_memory_to_budget(&mut entries, 3);
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.role.as_str())
                .collect::<Vec<_>>(),
            vec!["user", "assistant", "tool"]
        );
        validate_chat_message_context(&memory_to_chat_messages(&entries))
            .expect("bounded history should remain protocol-valid");
    }

    #[tokio::test]
    async fn visualization_tool_writes_a_real_html_artifact() {
        let source = std::env::temp_dir().join(format!("rustpilot-data-{}.csv", Uuid::new_v4()));
        fs::write(&source, "label,value\nA,2\nB,5\n").expect("source should be written");
        let output = run_data_visualization_tool(&json!({
            "path": source,
            "output_type": "html",
            "title": "Test chart"
        }))
        .await
        .expect("visualization should succeed");
        let value: Value =
            serde_json::from_str(&output).expect("visualization output should be JSON");
        let chart_path = value["results"][0]["chart_path"]
            .as_str()
            .expect("chart path should be returned");
        assert!(Path::new(chart_path).exists());
        let html = fs::read_to_string(chart_path).expect("chart HTML should be readable");
        assert!(html.contains("<svg"));
        let _ = fs::remove_file(chart_path);
        let _ = fs::remove_file(source);
    }
}
