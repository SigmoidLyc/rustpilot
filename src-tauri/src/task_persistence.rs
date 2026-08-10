//! Asynchronous task snapshot and event persistence.
//!
//! Mutations are coalesced by task id and committed in one SQLite transaction.
//! The agent remains responsive while a stream is active, and a failed batch is
//! requeued instead of being silently dropped.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    time::Duration,
};

use rusqlite::{params, Connection, Transaction};
use serde_json::Value;
use tauri::{AppHandle, Emitter};
use tokio::sync::Notify;
use tracing::{error, warn};

use super::{
    llm,
    task_events::{
        apply_persisted_stream_event, persisted_stream_event, PersistedStreamEvent,
        PersistedTaskEvent, TaskEvent,
    },
    task_storage::{insert_task_state, set_task_event_floor},
    Task,
};

const EVENT_COMPACTION_BYTES: u64 = 96 * 1024;
const WRITE_BATCH_DELAY_MS: u64 = 50;

#[derive(Debug, Clone)]
pub(crate) enum PendingTaskEvent {
    Stream {
        revision: u64,
        message_id: String,
        event: PersistedStreamEvent,
    },
    Task {
        revision: u64,
        event: String,
        payload: Value,
    },
}

impl PendingTaskEvent {
    fn revision(&self) -> u64 {
        match self {
            Self::Stream { revision, .. } | Self::Task { revision, .. } => *revision,
        }
    }
}

pub(crate) enum PendingTaskWrite {
    Upsert {
        task: Box<Task>,
        events: Vec<PendingTaskEvent>,
    },
    Events {
        events: Vec<PendingTaskEvent>,
    },
    Delete {
        revision: u64,
        events: Vec<PendingTaskEvent>,
    },
}

#[derive(Default)]
pub(crate) struct PendingTaskWrites {
    pub(crate) by_task: HashMap<String, PendingTaskWrite>,
}

fn append_pending_events(
    mut older: Vec<PendingTaskEvent>,
    newer: Vec<PendingTaskEvent>,
) -> Vec<PendingTaskEvent> {
    older.extend(newer);
    older
}

fn pending_events_after_revision(events: &[PendingTaskEvent], revision: u64) -> bool {
    events.iter().any(|event| event.revision() > revision)
}

fn pending_events_latest_revision(events: &[PendingTaskEvent]) -> u64 {
    events
        .iter()
        .map(PendingTaskEvent::revision)
        .max()
        .unwrap_or_default()
}

pub(crate) fn merge_pending_task_writes(
    older: PendingTaskWrite,
    newer: PendingTaskWrite,
) -> PendingTaskWrite {
    match (older, newer) {
        (
            PendingTaskWrite::Upsert {
                task: older_task,
                events: older_events,
            },
            PendingTaskWrite::Events {
                events: newer_events,
            },
        ) => PendingTaskWrite::Upsert {
            task: older_task,
            events: append_pending_events(older_events, newer_events),
        },
        (
            PendingTaskWrite::Events {
                events: older_events,
            },
            PendingTaskWrite::Upsert {
                task: newer_task,
                events: newer_events,
            },
        ) => PendingTaskWrite::Upsert {
            task: newer_task,
            events: append_pending_events(older_events, newer_events),
        },
        (
            PendingTaskWrite::Upsert {
                task: older_task,
                events: older_events,
            },
            PendingTaskWrite::Upsert {
                task: newer_task,
                events: newer_events,
            },
        ) => PendingTaskWrite::Upsert {
            task: if newer_task.persistence_revision >= older_task.persistence_revision {
                newer_task
            } else {
                older_task
            },
            events: append_pending_events(older_events, newer_events),
        },
        (
            PendingTaskWrite::Events {
                events: older_events,
            },
            PendingTaskWrite::Events {
                events: newer_events,
            },
        ) => PendingTaskWrite::Events {
            events: append_pending_events(older_events, newer_events),
        },
        (
            PendingTaskWrite::Delete {
                revision: older_revision,
                events: older_events,
            },
            PendingTaskWrite::Upsert { task, events },
        ) => {
            if task.persistence_revision > older_revision {
                PendingTaskWrite::Upsert { task, events }
            } else {
                PendingTaskWrite::Delete {
                    revision: older_revision,
                    events: older_events,
                }
            }
        }
        (
            PendingTaskWrite::Upsert { task, events },
            PendingTaskWrite::Delete {
                revision,
                events: delete_events,
            },
        ) => {
            if revision >= task.persistence_revision {
                PendingTaskWrite::Delete {
                    revision,
                    events: delete_events,
                }
            } else {
                PendingTaskWrite::Upsert { task, events }
            }
        }
        (
            PendingTaskWrite::Events { events },
            PendingTaskWrite::Delete {
                revision,
                events: delete_events,
            },
        ) => {
            if pending_events_latest_revision(&events) <= revision {
                PendingTaskWrite::Delete {
                    revision,
                    events: delete_events,
                }
            } else {
                PendingTaskWrite::Events { events }
            }
        }
        (
            PendingTaskWrite::Delete {
                revision: older_revision,
                events: older_events,
            },
            PendingTaskWrite::Events { events },
        ) => {
            if pending_events_after_revision(&events, older_revision) {
                PendingTaskWrite::Events { events }
            } else {
                PendingTaskWrite::Delete {
                    revision: older_revision,
                    events: older_events,
                }
            }
        }
        (
            PendingTaskWrite::Delete {
                revision: older_revision,
                events: older_events,
            },
            PendingTaskWrite::Delete { revision, events },
        ) => PendingTaskWrite::Delete {
            revision: older_revision.max(revision),
            events: if revision >= older_revision {
                events
            } else {
                older_events
            },
        },
    }
}

#[derive(Clone)]
pub(crate) struct TaskPersistence {
    pending: Arc<Mutex<PendingTaskWrites>>,
    notify: Arc<Notify>,
    started: Arc<AtomicBool>,
}

#[derive(Clone)]
struct TaskEventPublisher {
    app: AppHandle,
    event_cursors: Arc<RwLock<HashMap<String, i64>>>,
    event_floors: Arc<RwLock<HashMap<String, i64>>>,
}

impl TaskPersistence {
    pub(crate) fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(PendingTaskWrites::default())),
            notify: Arc::new(Notify::new()),
            started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn start(
        &self,
        connection: Connection,
        durable_tasks: HashMap<String, Task>,
        event_bytes: HashMap<String, u64>,
        app: AppHandle,
        event_cursors: Arc<RwLock<HashMap<String, i64>>>,
        event_floors: Arc<RwLock<HashMap<String, i64>>>,
    ) -> Result<(), String> {
        if self.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let pending = Arc::clone(&self.pending);
        let notify = Arc::clone(&self.notify);
        let publisher = TaskEventPublisher {
            app,
            event_cursors,
            event_floors,
        };
        tauri::async_runtime::spawn(async move {
            task_writer_loop(
                connection,
                pending,
                notify,
                durable_tasks,
                event_bytes,
                publisher,
            )
            .await;
        });
        Ok(())
    }

    pub(crate) fn enqueue_upsert(&self, task: Task) -> Result<(), String> {
        if !self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        let task_id = task.id.clone();
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| "Task persistence queue is poisoned".to_string())?;
        let write = PendingTaskWrite::Upsert {
            task: Box::new(task),
            events: Vec::new(),
        };
        merge_into_pending(&mut pending, task_id, write);
        drop(pending);
        self.notify.notify_one();
        Ok(())
    }

    pub(crate) fn enqueue_stream(
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
        merge_into_pending(
            &mut pending,
            task_id.to_string(),
            PendingTaskWrite::Events {
                events: vec![PendingTaskEvent::Stream {
                    revision,
                    message_id: message_id.to_string(),
                    event,
                }],
            },
        );
        drop(pending);
        self.notify.notify_one();
        Ok(())
    }

    pub(crate) fn enqueue_task_event(
        &self,
        task_id: &str,
        revision: u64,
        event: String,
        payload: Value,
    ) -> Result<(), String> {
        if !self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| "Task persistence queue is poisoned".to_string())?;
        merge_into_pending(
            &mut pending,
            task_id.to_string(),
            PendingTaskWrite::Events {
                events: vec![PendingTaskEvent::Task {
                    revision,
                    event,
                    payload,
                }],
            },
        );
        drop(pending);
        self.notify.notify_one();
        Ok(())
    }

    pub(crate) fn enqueue_delete(
        &self,
        task_id: &str,
        revision: u64,
        event: PendingTaskEvent,
    ) -> Result<(), String> {
        if !self.started.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| "Task persistence queue is poisoned".to_string())?;
        merge_into_pending(
            &mut pending,
            task_id.to_string(),
            PendingTaskWrite::Delete {
                revision,
                events: vec![event],
            },
        );
        drop(pending);
        self.notify.notify_one();
        Ok(())
    }
}

fn merge_into_pending(pending: &mut PendingTaskWrites, task_id: String, write: PendingTaskWrite) {
    if let Some(existing) = pending.by_task.remove(&task_id) {
        pending
            .by_task
            .insert(task_id, merge_pending_task_writes(existing, write));
    } else {
        pending.by_task.insert(task_id, write);
    }
}

pub(crate) fn take_pending_task_writes(
    pending: &Arc<Mutex<PendingTaskWrites>>,
) -> Result<PendingTaskWrites, String> {
    let mut pending = pending
        .lock()
        .map_err(|_| "Task persistence queue is poisoned".to_string())?;
    Ok(std::mem::take(&mut *pending))
}

pub(crate) fn requeue_task_writes(
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

pub(crate) struct ProjectedTaskChanges {
    pub(crate) tasks: HashMap<String, Option<Task>>,
    pub(crate) event_bytes: HashMap<String, u64>,
    pub(crate) compacted: HashSet<String>,
}

fn pending_event_bytes(event: &PendingTaskEvent) -> Result<u64, String> {
    let payload = match event {
        PendingTaskEvent::Stream { event, .. } => serde_json::to_vec(event),
        PendingTaskEvent::Task { event, payload, .. } => serde_json::to_vec(&PersistedTaskEvent {
            event: event.clone(),
            payload: payload.clone(),
        }),
    }
    .map_err(|error| format!("Unable to encode task event: {error}"))?;
    Ok(payload.len() as u64 + 64)
}

pub(crate) fn project_task_writes(
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
            PendingTaskWrite::Upsert { task, events } => {
                let mut task = (**task).clone();
                if let Some(durable_task) = durable_tasks.get(task_id) {
                    task.event_seq = task.event_seq.max(durable_task.event_seq);
                }
                let covered_revision = task.persistence_revision;
                let mut event_bytes = durable_event_bytes.get(task_id).copied().unwrap_or(0);
                for pending_event in events {
                    if let PendingTaskEvent::Stream {
                        revision,
                        message_id,
                        event,
                    } = pending_event
                    {
                        if *revision > covered_revision {
                            apply_persisted_stream_event(&mut task, task_id, message_id, event)?;
                            task.persistence_revision = task.persistence_revision.max(*revision);
                        }
                    }
                    event_bytes = event_bytes.saturating_add(pending_event_bytes(pending_event)?);
                }
                if event_bytes >= EVENT_COMPACTION_BYTES {
                    projected.compacted.insert(task_id.clone());
                    event_bytes = 0;
                }
                projected.tasks.insert(task_id.clone(), Some(task));
                projected.event_bytes.insert(task_id.clone(), event_bytes);
            }
            PendingTaskWrite::Events { events } => {
                let mut task = durable_tasks
                    .get(task_id)
                    .cloned()
                    .ok_or_else(|| format!("Stream event references missing task {task_id}"))?;
                let mut event_bytes = durable_event_bytes.get(task_id).copied().unwrap_or(0);
                for pending_event in events {
                    if let PendingTaskEvent::Stream {
                        revision,
                        message_id,
                        event,
                    } = pending_event
                    {
                        apply_persisted_stream_event(&mut task, task_id, message_id, event)?;
                        task.persistence_revision = task.persistence_revision.max(*revision);
                    }
                    event_bytes = event_bytes.saturating_add(pending_event_bytes(pending_event)?);
                }
                if event_bytes >= EVENT_COMPACTION_BYTES {
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

pub(crate) fn insert_task_event(
    transaction: &Transaction<'_>,
    task_id: &str,
    event: &PendingTaskEvent,
) -> Result<TaskEvent, String> {
    let (message_id, kind, event_name, payload) = match event {
        PendingTaskEvent::Stream {
            message_id, event, ..
        } => (
            message_id.as_str(),
            "stream",
            Some("task_message_delta".to_string()),
            serde_json::to_value(event),
        ),
        PendingTaskEvent::Task { event, payload, .. } => (
            "",
            "task",
            Some(event.clone()),
            serde_json::to_value(&PersistedTaskEvent {
                event: event.clone(),
                payload: payload.clone(),
            }),
        ),
    };
    let payload = payload.map_err(|error| format!("Unable to encode task event: {error}"))?;
    let stored_payload = serde_json::to_string(&payload)
        .map_err(|error| format!("Unable to encode task event payload: {error}"))?;
    transaction
        .execute(
            "INSERT INTO task_event(task_id, message_id, kind, payload) VALUES (?1, ?2, ?3, ?4)",
            params![task_id, message_id, kind, stored_payload],
        )
        .map_err(|error| format!("Unable to persist task event: {error}"))?;
    Ok(TaskEvent {
        task_id: task_id.to_string(),
        seq: transaction.last_insert_rowid(),
        kind: kind.to_string(),
        event: event_name,
        message_id: if message_id.is_empty() {
            None
        } else {
            Some(message_id.to_string())
        },
        payload: if kind == "task" {
            payload.get("payload").cloned().unwrap_or(Value::Null)
        } else {
            payload
        },
    })
}

#[cfg(test)]
pub(crate) fn insert_stream_event(
    transaction: &Transaction<'_>,
    task_id: &str,
    message_id: &str,
    event: &PersistedStreamEvent,
) -> Result<(), String> {
    insert_task_event(
        transaction,
        task_id,
        &PendingTaskEvent::Stream {
            revision: 0,
            message_id: message_id.to_string(),
            event: event.clone(),
        },
    )
    .map(|_| ())
}

pub(crate) fn commit_task_writes(
    mut connection: Connection,
    writes: PendingTaskWrites,
    mut projected: ProjectedTaskChanges,
) -> (
    Connection,
    PendingTaskWrites,
    ProjectedTaskChanges,
    Vec<TaskEvent>,
    Result<(), String>,
) {
    let mut committed_events = Vec::new();
    let result = (|| {
        let transaction = connection
            .transaction()
            .map_err(|error| format!("Unable to begin task persistence batch: {error}"))?;
        let mut snapshot_tasks = HashSet::new();
        let mut entries = writes.by_task.iter().collect::<Vec<_>>();
        entries.sort_by(|left, right| left.0.cmp(right.0));
        for (task_id, write) in entries {
            match write {
                PendingTaskWrite::Upsert { task: _, events } => {
                    snapshot_tasks.insert(task_id.clone());
                    let task = projected
                        .tasks
                        .get(task_id)
                        .and_then(Option::as_ref)
                        .ok_or_else(|| {
                            format!("Projected task snapshot is missing for {task_id}")
                        })?;
                    insert_task_state(&transaction, task)?;
                    for pending_event in events {
                        committed_events.push(insert_task_event(
                            &transaction,
                            task_id,
                            pending_event,
                        )?);
                    }
                }
                PendingTaskWrite::Events { events } => {
                    let snapshot_exists = transaction
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM task_state WHERE id = ?1)",
                            params![task_id],
                            |row| row.get::<_, bool>(0),
                        )
                        .map_err(|error| {
                            format!("Unable to inspect task snapshot for {task_id}: {error}")
                        })?;
                    if !snapshot_exists || projected.compacted.contains(task_id) {
                        let task = projected
                            .tasks
                            .get(task_id)
                            .and_then(Option::as_ref)
                            .ok_or_else(|| {
                                format!("Projected task snapshot is missing for {task_id}")
                            })?;
                        insert_task_state(&transaction, task)?;
                        snapshot_tasks.insert(task_id.clone());
                    }
                    for pending_event in events {
                        committed_events.push(insert_task_event(
                            &transaction,
                            task_id,
                            pending_event,
                        )?);
                    }
                }
                PendingTaskWrite::Delete { events, .. } => {
                    for pending_event in events {
                        committed_events.push(insert_task_event(
                            &transaction,
                            task_id,
                            pending_event,
                        )?);
                    }
                    transaction
                        .execute("DELETE FROM task_state WHERE id = ?1", params![task_id])
                        .map_err(|error| format!("Unable to delete task {task_id}: {error}"))?;
                }
            }
        }
        let mut last_event_by_task: HashMap<String, i64> = HashMap::new();
        for event in &committed_events {
            last_event_by_task
                .entry(event.task_id.clone())
                .and_modify(|seq| *seq = (*seq).max(event.seq))
                .or_insert(event.seq);
        }
        for (task_id, last_event_seq) in last_event_by_task {
            if snapshot_tasks.contains(&task_id) || projected.compacted.contains(&task_id) {
                if let Some(Some(task)) = projected.tasks.get_mut(&task_id) {
                    task.event_seq = task.event_seq.max(last_event_seq);
                    insert_task_state(&transaction, task)?;
                }
            }
        }
        for task_id in &projected.compacted {
            if let Some(Some(task)) = projected.tasks.get(task_id) {
                set_task_event_floor(&transaction, task_id, task.event_seq)?;
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
    (connection, writes, projected, committed_events, result)
}

async fn task_writer_loop(
    mut connection: Connection,
    pending: Arc<Mutex<PendingTaskWrites>>,
    notify: Arc<Notify>,
    mut durable_tasks: HashMap<String, Task>,
    mut event_bytes: HashMap<String, u64>,
    publisher: TaskEventPublisher,
) {
    loop {
        notify.notified().await;
        tokio::time::sleep(Duration::from_millis(WRITE_BATCH_DELAY_MS)).await;
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
        let (next_connection, writes, projected, committed_events, result) = match commit {
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
        let compacted_ids = compacted.clone();
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
                    if let Ok(mut cursors) = publisher.event_cursors.write() {
                        cursors.remove(&task_id);
                    }
                    if let Ok(mut floors) = publisher.event_floors.write() {
                        floors.remove(&task_id);
                    }
                }
            }
        }
        for event in committed_events {
            let deleted = event.event.as_deref() == Some("task_deleted");
            if let Some(task) = durable_tasks.get_mut(&event.task_id) {
                task.event_seq = task.event_seq.max(event.seq);
            }
            if deleted {
                if let Ok(mut cursors) = publisher.event_cursors.write() {
                    cursors.remove(&event.task_id);
                }
                if let Ok(mut floors) = publisher.event_floors.write() {
                    floors.remove(&event.task_id);
                }
            } else {
                if let Ok(mut cursors) = publisher.event_cursors.write() {
                    cursors
                        .entry(event.task_id.clone())
                        .and_modify(|cursor| *cursor = (*cursor).max(event.seq))
                        .or_insert(event.seq);
                }
                if compacted_ids.contains(&event.task_id) {
                    if let Ok(mut floors) = publisher.event_floors.write() {
                        floors.insert(event.task_id.clone(), event.seq);
                    }
                }
            }
            if let Err(error) = publisher.app.emit("task_event", event) {
                warn!("Unable to emit task_event: {error}");
            }
        }
    }
}
