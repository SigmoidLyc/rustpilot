//! Durable task events and cursor-based replay.
//!
//! The event format is deliberately small: stream deltas are persisted as
//! compact tagged records, while task events retain their event name and JSON
//! payload. The UI can recover from a missed live event by asking for the page
//! after its last cursor.

use std::path::Path;

use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{apply_stream_event, llm, task_storage, Task};
use task_storage::{open_task_database, task_database_path};

pub(crate) const PAGE_SIZE: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PersistedStreamEvent {
    ReasoningDelta {
        delta: String,
    },
    ReasoningOpaque {
        value: String,
    },
    TextDelta {
        delta: String,
    },
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LegacyPersistedStreamEvent {
    TextDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedTaskEvent {
    pub(crate) event: String,
    pub(crate) payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub task_id: String,
    pub seq: i64,
    pub kind: String,
    pub event: Option<String>,
    pub message_id: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEventPage {
    pub task_id: String,
    pub snapshot: Option<Task>,
    pub events: Vec<TaskEvent>,
    pub cursor: i64,
    pub has_more: bool,
    pub reset: bool,
}

pub(crate) fn decode_persisted_stream_event(payload: &str) -> Result<PersistedStreamEvent, String> {
    serde_json::from_str(payload)
        .or_else(|_| {
            serde_json::from_str::<LegacyPersistedStreamEvent>(payload).map(|event| match event {
                LegacyPersistedStreamEvent::TextDelta(delta) => {
                    PersistedStreamEvent::TextDelta { delta }
                }
                LegacyPersistedStreamEvent::ToolCallDelta { index, id, name } => {
                    PersistedStreamEvent::ToolCallDelta { index, id, name }
                }
            })
        })
        .map_err(|error| format!("Unable to decode persisted stream event: {error}"))
}

pub(crate) fn persisted_stream_event(event: &llm::StreamEvent) -> Option<PersistedStreamEvent> {
    match event {
        llm::StreamEvent::ReasoningDelta(delta) if !delta.is_empty() => {
            Some(PersistedStreamEvent::ReasoningDelta {
                delta: delta.clone(),
            })
        }
        llm::StreamEvent::ReasoningOpaque(value) if !value.is_empty() => {
            Some(PersistedStreamEvent::ReasoningOpaque {
                value: value.clone(),
            })
        }
        llm::StreamEvent::TextDelta(delta) if !delta.is_empty() => {
            Some(PersistedStreamEvent::TextDelta {
                delta: delta.clone(),
            })
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

pub(crate) fn apply_persisted_stream_event(
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
        PersistedStreamEvent::ReasoningDelta { delta } => {
            apply_stream_event(message, &llm::StreamEvent::ReasoningDelta(delta.clone()));
        }
        PersistedStreamEvent::ReasoningOpaque { value } => {
            apply_stream_event(message, &llm::StreamEvent::ReasoningOpaque(value.clone()));
        }
        PersistedStreamEvent::TextDelta { delta } => {
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

pub(crate) fn task_event_from_row(
    task_id: String,
    seq: i64,
    message_id: String,
    kind: String,
    payload: String,
) -> Result<TaskEvent, String> {
    match kind.as_str() {
        "stream" => {
            let event = decode_persisted_stream_event(&payload)?;
            let payload = serde_json::to_value(event)
                .map_err(|error| format!("Unable to encode stream event: {error}"))?;
            Ok(TaskEvent {
                task_id,
                seq,
                kind,
                event: Some("task_message_delta".to_string()),
                message_id: Some(message_id),
                payload,
            })
        }
        "task" => {
            let persisted: PersistedTaskEvent = serde_json::from_str(&payload)
                .map_err(|error| format!("Unable to decode task event: {error}"))?;
            Ok(TaskEvent {
                task_id,
                seq,
                kind,
                event: Some(persisted.event),
                message_id: None,
                payload: persisted.payload,
            })
        }
        _ => Err(format!("Unsupported task event kind: {kind}")),
    }
}

pub(crate) fn read_page(
    data_dir: &Path,
    task_id: &str,
    after: Option<i64>,
) -> Result<TaskEventPage, String> {
    let connection = open_task_database(&task_database_path(data_dir))?;
    let (snapshot_data, snapshot_seq, floor_seq): (String, i64, i64) = connection
        .query_row(
            "SELECT data, event_seq, event_floor_seq FROM task_state WHERE id = ?1",
            params![task_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|error| format!("Unable to read task event snapshot: {error}"))?;
    let mut snapshot: Task = serde_json::from_str(&snapshot_data)
        .map_err(|error| format!("Unable to decode task event snapshot: {error}"))?;
    snapshot.event_seq = snapshot_seq;

    let reset = after.is_some_and(|cursor| cursor < floor_seq);
    let include_snapshot =
        after.is_none() || reset || after.is_some_and(|cursor| cursor < snapshot_seq);
    let query_after = if include_snapshot {
        snapshot_seq
    } else {
        after.unwrap_or(snapshot_seq)
    };
    let limit = PAGE_SIZE as i64;
    let mut statement = connection
        .prepare(
            "SELECT seq, message_id, kind, payload
             FROM task_event
             WHERE task_id = ?1 AND seq > ?2
             ORDER BY seq
             LIMIT ?3",
        )
        .map_err(|error| format!("Unable to prepare task event page: {error}"))?;
    let rows = statement
        .query_map(params![task_id, query_after, limit + 1], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| format!("Unable to query task event page: {error}"))?;
    let mut events = Vec::with_capacity(PAGE_SIZE);
    for row in rows {
        let (seq, message_id, kind, payload) =
            row.map_err(|error| format!("Unable to read task event page row: {error}"))?;
        events.push(task_event_from_row(
            task_id.to_string(),
            seq,
            message_id,
            kind,
            payload,
        )?);
    }
    let has_more = events.len() > PAGE_SIZE;
    if has_more {
        events.truncate(PAGE_SIZE);
    }
    let cursor = events
        .last()
        .map(|event| event.seq)
        .unwrap_or(query_after.max(snapshot_seq));
    Ok(TaskEventPage {
        task_id: task_id.to_string(),
        snapshot: include_snapshot.then_some(snapshot),
        events,
        cursor,
        has_more,
        reset,
    })
}
