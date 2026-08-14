//! Task database schema, migration, and snapshot loading.

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{params, Connection, Transaction};
use tracing::{info, warn};

use crate::agent_loop::provenance::ContextEventRecord;

use super::task_events::{
    apply_persisted_stream_event, decode_persisted_stream_event, PersistedTaskEvent,
};
use super::{now, Task};

pub(crate) const LEGACY_TASK_FILE: &str = "tasks.json";
const TASK_DATABASE_FILE: &str = "tasks.db";
const TASK_SCHEMA_VERSION: i64 = 4;

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

pub(crate) fn legacy_task_temp_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

pub(crate) fn legacy_task_backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.bak")
}

pub(crate) fn load_legacy_task_records(path: &Path) -> Option<Vec<Task>> {
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

#[derive(Debug)]
pub(crate) struct LoadedTaskStore {
    pub(crate) tasks: HashMap<String, Task>,
    pub(crate) event_bytes: HashMap<String, u64>,
    pub(crate) event_cursors: HashMap<String, i64>,
    pub(crate) event_floors: HashMap<String, i64>,
    pub(crate) connection: Connection,
}

pub(crate) fn task_database_path(data_dir: &Path) -> PathBuf {
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

pub(crate) fn open_task_database(path: &Path) -> Result<Connection, String> {
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
                 event_seq INTEGER NOT NULL DEFAULT 0,
                 event_floor_seq INTEGER NOT NULL DEFAULT 0,
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
                 ON task_event(task_id, seq);
             CREATE TABLE IF NOT EXISTS agent_event (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 task_id TEXT NOT NULL REFERENCES task_state(id) ON DELETE CASCADE,
                 revision INTEGER NOT NULL,
                 turn_id TEXT NOT NULL,
                 step INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 occurred_at INTEGER NOT NULL,
                 payload TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS agent_event_task_seq_idx
                 ON agent_event(task_id, seq);
             CREATE TABLE IF NOT EXISTS context_event (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 task_id TEXT NOT NULL REFERENCES task_state(id) ON DELETE CASCADE,
                 compaction_id TEXT NOT NULL,
                 generation INTEGER NOT NULL,
                 kind TEXT NOT NULL,
                 source_start TEXT,
                 source_end TEXT,
                 source_hash TEXT NOT NULL,
                 shadowed_tokens INTEGER NOT NULL DEFAULT 0,
                 surface_tokens INTEGER NOT NULL DEFAULT 0,
                 occurred_at INTEGER NOT NULL,
                 payload TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS context_event_task_seq_idx
                 ON context_event(task_id, seq);
             CREATE INDEX IF NOT EXISTS context_event_task_compaction_idx
                 ON context_event(task_id, compaction_id, seq);",
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
    if !task_state_columns.contains("event_seq") {
        connection
            .execute(
                "ALTER TABLE task_state ADD COLUMN event_seq INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| format!("Unable to migrate task state event cursors: {error}"))?;
    }
    if !task_state_columns.contains("event_floor_seq") {
        connection
            .execute(
                "ALTER TABLE task_state ADD COLUMN event_floor_seq INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| format!("Unable to migrate task event floors: {error}"))?;
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

pub(crate) fn insert_task_state(transaction: &Transaction<'_>, task: &Task) -> Result<(), String> {
    let data = task_state_json(task)?;
    transaction
        .execute(
            "INSERT INTO task_state(id, updated_at, event_seq, data) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 updated_at = excluded.updated_at,
                 event_seq = excluded.event_seq,
                 data = excluded.data",
            params![task.id, task.updated_at, task.event_seq, data],
        )
        .map_err(|error| format!("Unable to persist task {}: {error}", task.id))?;
    Ok(())
}

pub(crate) fn set_task_event_floor(
    transaction: &Transaction<'_>,
    task_id: &str,
    seq: i64,
) -> Result<(), String> {
    transaction
        .execute(
            "UPDATE task_state SET event_floor_seq = ?2 WHERE id = ?1",
            params![task_id, seq],
        )
        .map_err(|error| format!("Unable to persist task event floor: {error}"))?;
    Ok(())
}

pub(crate) fn insert_agent_event(
    transaction: &Transaction<'_>,
    task_id: &str,
    revision: u64,
    turn_id: &str,
    step: u32,
    kind: &str,
    occurred_at: i64,
    payload: &str,
) -> Result<(), String> {
    transaction
        .execute(
            "INSERT INTO agent_event(task_id, revision, turn_id, step, kind, occurred_at, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                task_id,
                revision as i64,
                turn_id,
                step as i64,
                kind,
                occurred_at,
                payload
            ],
        )
        .map_err(|error| format!("Unable to persist agent event for {task_id}: {error}"))?;
    Ok(())
}

pub(crate) fn insert_context_event(
    transaction: &Transaction<'_>,
    task_id: &str,
    event: &ContextEventRecord,
) -> Result<(), String> {
    let payload = serde_json::to_string(&event.payload)
        .map_err(|error| format!("Unable to encode context event payload: {error}"))?;
    transaction
        .execute(
            "INSERT INTO context_event(
                 task_id, compaction_id, generation, kind, source_start, source_end,
                 source_hash, shadowed_tokens, surface_tokens, occurred_at, payload
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                task_id,
                event.compaction_id,
                event.generation as i64,
                event.kind,
                event.source_start,
                event.source_end,
                event.source_hash,
                event.shadowed_tokens as i64,
                event.surface_tokens as i64,
                event.occurred_at,
                payload,
            ],
        )
        .map_err(|error| format!("Unable to persist context event for {task_id}: {error}"))?;
    Ok(())
}

pub(crate) fn repair_interrupted_context_events(
    connection: &mut Connection,
) -> Result<usize, String> {
    let mut statement = connection
        .prepare(
            "SELECT task_id, compaction_id, generation, kind, source_start, source_end,
                    source_hash, shadowed_tokens, surface_tokens
             FROM context_event
             ORDER BY seq",
        )
        .map_err(|error| format!("Unable to inspect context events: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)? as usize,
                row.get::<_, i64>(8)? as usize,
            ))
        })
        .map_err(|error| format!("Unable to read context events: {error}"))?;
    let mut open = HashMap::<
        (String, String),
        (u64, Option<String>, Option<String>, String, usize, usize),
    >::new();
    for row in rows {
        let (
            task_id,
            compaction_id,
            generation,
            kind,
            source_start,
            source_end,
            source_hash,
            shadowed_tokens,
            surface_tokens,
        ) = row.map_err(|error| format!("Unable to decode context event: {error}"))?;
        let key = (task_id, compaction_id);
        if kind == "compaction_start" {
            open.insert(
                key,
                (
                    generation,
                    source_start,
                    source_end,
                    source_hash,
                    shadowed_tokens,
                    surface_tokens,
                ),
            );
        } else if kind == "compaction_end" {
            open.remove(&key);
        }
    }
    drop(statement);
    if open.is_empty() {
        return Ok(0);
    }

    let transaction = connection
        .transaction()
        .map_err(|error| format!("Unable to begin context repair: {error}"))?;
    let occurred_at = super::now();
    let count = open.len();
    for (
        (task_id, compaction_id),
        (generation, source_start, source_end, source_hash, shadowed_tokens, surface_tokens),
    ) in open
    {
        let event = ContextEventRecord::new(
            "compaction_end",
            compaction_id,
            generation,
            source_start,
            source_end,
            source_hash,
            shadowed_tokens,
            surface_tokens,
            occurred_at,
            serde_json::json!({
                "status": "interrupted_recovery",
                "reason": "The process stopped before compaction completed.",
            }),
        );
        insert_context_event(&transaction, &task_id, &event)?;
    }
    transaction
        .commit()
        .map_err(|error| format!("Unable to commit context repair: {error}"))?;
    Ok(count)
}

pub(crate) fn repair_interrupted_agent_events(
    connection: &mut Connection,
) -> Result<usize, String> {
    let mut statement = connection
        .prepare(
            "SELECT task_id, turn_id, step, kind
             FROM agent_event
             ORDER BY seq",
        )
        .map_err(|error| format!("Unable to inspect agent lifecycle events: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u32,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| format!("Unable to read agent lifecycle events: {error}"))?;
    let mut open = HashMap::<(String, String), Option<u32>>::new();
    for row in rows {
        let (task_id, turn_id, step, kind) =
            row.map_err(|error| format!("Unable to decode agent lifecycle event: {error}"))?;
        match kind.as_str() {
            "turn_start" => {
                open.insert((task_id, turn_id), None);
            }
            "turn_end" => {
                open.retain(|(open_task, open_turn), _| {
                    open_task != &task_id || open_turn != &turn_id
                });
            }
            "step_start" => {
                if let Some(current) = open.get_mut(&(task_id, turn_id)) {
                    *current = Some(step);
                }
            }
            "step_end" => {
                if let Some(current) = open.get_mut(&(task_id, turn_id)) {
                    *current = None;
                }
            }
            _ => {}
        }
    }
    drop(statement);
    if open.is_empty() {
        return Ok(0);
    }

    let mut pending = open.into_iter().collect::<Vec<_>>();
    pending.sort_by(|left, right| left.0.cmp(&right.0));
    let transaction = connection
        .transaction()
        .map_err(|error| format!("Unable to begin agent lifecycle repair: {error}"))?;
    let interrupted_reason =
        serde_json::to_value(crate::agent_loop::events::TurnEndReason::Interrupted)
            .unwrap_or_else(|_| serde_json::Value::String("interrupted".to_string()));
    for ((task_id, turn_id), open_step) in &pending {
        if let Some(step) = open_step {
            insert_agent_event(
                &transaction,
                task_id,
                0,
                turn_id,
                *step,
                "step_end",
                super::now(),
                &serde_json::json!({"reason":interrupted_reason.clone()}).to_string(),
            )?;
        }
        insert_agent_event(
            &transaction,
            task_id,
            0,
            turn_id,
            open_step.unwrap_or_default(),
            "lifecycle_state",
            super::now(),
            &serde_json::json!({
                "state": "idle",
                "reason": "interrupted_recovery"
            })
            .to_string(),
        )?;
        insert_agent_event(
            &transaction,
            task_id,
            0,
            turn_id,
            open_step.unwrap_or_default(),
            "turn_end",
            super::now(),
            &serde_json::json!({"reason":interrupted_reason}).to_string(),
        )?;
    }
    transaction
        .commit()
        .map_err(|error| format!("Unable to commit agent lifecycle repair: {error}"))?;
    Ok(pending.len())
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

pub(crate) fn load_task_store(data_dir: &Path) -> Result<LoadedTaskStore, String> {
    let database_path = task_database_path(data_dir);
    let database_existed = database_path.exists();
    let mut connection = open_task_database(&database_path)?;
    let mut tasks = HashMap::new();
    let mut event_floors = HashMap::new();

    {
        let mut statement = connection
            .prepare("SELECT id, event_seq, event_floor_seq, data FROM task_state ORDER BY id")
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
            let event_seq: i64 = row
                .get(1)
                .map_err(|error| format!("Unable to read task event cursor: {error}"))?;
            let event_floor_seq: i64 = row
                .get(2)
                .map_err(|error| format!("Unable to read task event floor: {error}"))?;
            let data: String = row
                .get(3)
                .map_err(|error| format!("Unable to read task data: {error}"))?;
            let mut task: Task = serde_json::from_str(&data)
                .map_err(|error| format!("Unable to decode task {id}: {error}"))?;
            if task.id != id {
                return Err(format!("Task database id mismatch for {id}"));
            }
            task.event_seq = event_seq;
            tasks.insert(id.clone(), task);
            event_floors.insert(id.clone(), event_floor_seq);
        }
    }

    let mut event_bytes = HashMap::new();
    let mut event_cursors = HashMap::new();
    {
        let mut statement = connection
            .prepare("SELECT task_id, seq, message_id, kind, payload FROM task_event ORDER BY seq")
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
            let seq: i64 = row
                .get(1)
                .map_err(|error| format!("Unable to read task event sequence: {error}"))?;
            let message_id: String = row
                .get(2)
                .map_err(|error| format!("Unable to read task event message id: {error}"))?;
            let kind: String = row
                .get(3)
                .map_err(|error| format!("Unable to read task event kind: {error}"))?;
            let payload: String = row
                .get(4)
                .map_err(|error| format!("Unable to read task event payload: {error}"))?;
            let snapshot_seq = tasks
                .get(&task_id)
                .ok_or_else(|| format!("Task event references missing task {task_id}"))?
                .event_seq;
            match kind.as_str() {
                "stream" => {
                    let event = decode_persisted_stream_event(&payload)?;
                    if seq > snapshot_seq {
                        let task = tasks.get_mut(&task_id).ok_or_else(|| {
                            format!("Task event references missing task {task_id}")
                        })?;
                        apply_persisted_stream_event(task, &task_id, &message_id, &event)?;
                    }
                }
                "task" => {
                    serde_json::from_str::<PersistedTaskEvent>(&payload).map_err(|error| {
                        format!("Unable to decode task event for {task_id}: {error}")
                    })?;
                }
                _ => return Err(format!("Unsupported task event kind: {kind}")),
            }
            *event_bytes.entry(task_id.clone()).or_insert(0) += payload.len() as u64 + 32;
            let cursor = event_cursors.entry(task_id).or_insert(seq);
            *cursor = (*cursor).max(seq);
        }
    }

    for (task_id, task) in &mut tasks {
        let cursor = task
            .event_seq
            .max(event_cursors.get(task_id).copied().unwrap_or_default());
        task.event_seq = cursor;
        event_cursors.insert(task_id.clone(), cursor);
        event_bytes.entry(task_id.clone()).or_insert(0);
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
            event_cursors.clear();
            event_floors.clear();
            remove_legacy_task_files(&legacy_path);
            info!("Migrated legacy tasks.json into SQLite task storage");
        }
    } else {
        remove_legacy_task_files(&legacy_path);
    }

    Ok(LoadedTaskStore {
        tasks,
        event_bytes,
        event_cursors,
        event_floors,
        connection,
    })
}
