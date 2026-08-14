use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{now, AppState};

const MAX_LIFECYCLE_TEXT_CHARS: usize = 16_000;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnEndReason {
    Completed,
    Cancelled,
    Failed,
    MaxSteps,
    Interrupted,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum LifecycleState {
    Idle,
    Maintenance,
    Running,
    Closed,
}

#[derive(Clone)]
pub(crate) struct AgentLifecycle {
    state: AppState,
    task_id: String,
    turn_id: String,
    step: u32,
    open_step: Option<u32>,
    lifecycle_state: LifecycleState,
}

pub(crate) fn stable_hash_hex(bytes: &[u8]) -> String {
    let mut first = 0xcbf29ce484222325u64;
    let mut second = 0x84222325decaf03du64;
    for &byte in bytes {
        first ^= u64::from(byte);
        first = first.wrapping_mul(0x100000001b3);
        second ^= u64::from(byte.wrapping_add(0x9d));
        second = second.wrapping_mul(0x100000001b3);
    }
    format!("{first:016x}{second:016x}")
}

fn bounded_text(value: &str) -> String {
    let mut chars = value.chars();
    let bounded = chars
        .by_ref()
        .take(MAX_LIFECYCLE_TEXT_CHARS)
        .collect::<String>();
    if chars.next().is_some() {
        format!("{bounded}\n[event text truncated]")
    } else {
        bounded
    }
}

impl AgentLifecycle {
    pub(crate) async fn start(state: &AppState, task_id: &str) -> Result<Self, String> {
        let mut lifecycle = Self {
            state: state.clone(),
            task_id: task_id.to_string(),
            turn_id: format!("turn-{}", Uuid::new_v4()),
            step: 0,
            open_step: None,
            lifecycle_state: LifecycleState::Idle,
        };
        lifecycle.append("turn_start", json!({}))?;
        lifecycle.transition(LifecycleState::Running, "turn_start")?;
        lifecycle.state.checkpoint_persistence().await?;
        Ok(lifecycle)
    }

    pub(crate) async fn step_start(&mut self, step: u32) -> Result<(), String> {
        self.ensure_running()?;
        if self.open_step.is_some() {
            return Err("Agent lifecycle already has an open step".to_string());
        }
        self.step = step;
        self.open_step = Some(step);
        self.append("step_start", json!({ "step": step }))?;
        Ok(())
    }

    pub(crate) async fn step_end(&mut self, reason: &str) -> Result<(), String> {
        self.ensure_open()?;
        let Some(step) = self.open_step.take() else {
            return Ok(());
        };
        self.append("step_end", json!({ "step": step, "reason": reason }))?;
        Ok(())
    }

    pub(crate) async fn request_header(
        &self,
        model: &str,
        base_url: &str,
        reasoning_effort: Option<String>,
        context_window: usize,
        input_tokens: usize,
        message_count: usize,
        tool_schema_hash: &str,
        system_hash: &str,
    ) -> Result<(), String> {
        self.ensure_running()?;
        self.append(
            "request_header",
            json!({
                "model": model,
                "base_url": base_url,
                "reasoning_effort": reasoning_effort,
                "context_window": context_window,
                "input_tokens": input_tokens,
                "message_count": message_count,
                "tool_schema_hash": tool_schema_hash,
                "system_hash": system_hash,
            }),
        )?;
        self.state.checkpoint_persistence().await
    }

    pub(crate) async fn assistant_message(
        &self,
        message_id: &str,
        content_len: usize,
        reasoning_len: usize,
        tool_call_count: usize,
    ) -> Result<(), String> {
        self.ensure_running()?;
        self.append(
            "assistant_message",
            json!({
                "message_id": message_id,
                "content_len": content_len,
                "reasoning_len": reasoning_len,
                "tool_call_count": tool_call_count,
            }),
        )?;
        Ok(())
    }

    pub(crate) fn tool_call_started(
        &self,
        call_id: &str,
        name: &str,
        arguments: &str,
    ) -> Result<(), String> {
        self.ensure_running()?;
        self.append_tool_call(call_id, name, arguments, "started", None)
    }

    pub(crate) fn tool_call_not_started(
        &self,
        call_id: &str,
        name: &str,
        arguments: &str,
    ) -> Result<(), String> {
        self.ensure_running()?;
        self.append_tool_call(
            call_id,
            name,
            arguments,
            "not_started",
            Some("skipped_due_to_cancel"),
        )
    }

    pub(crate) async fn tool_result(
        &self,
        call_id: &str,
        name: &str,
        status: &str,
        output: Option<&str>,
        error: Option<&str>,
    ) -> Result<(), String> {
        self.ensure_running()?;
        self.append(
            "tool_result",
            json!({
                "call_id": bounded_text(call_id),
                "name": bounded_text(name),
                "status": bounded_text(status),
                "output": output.map(bounded_text),
                "error": error.map(bounded_text),
            }),
        )?;
        Ok(())
    }

    pub(crate) async fn context_compaction(
        &mut self,
        before_tokens: usize,
        after_tokens: usize,
        dropped_messages: usize,
    ) -> Result<(), String> {
        self.ensure_running()?;
        self.transition(LifecycleState::Maintenance, "context_compaction")?;
        self.append(
            "context_compaction",
            json!({
                "before_tokens": before_tokens,
                "after_tokens": after_tokens,
                "dropped_messages": dropped_messages,
            }),
        )?;
        self.transition(LifecycleState::Running, "context_compaction_complete")?;
        self.state.checkpoint_persistence().await
    }

    pub(crate) fn retry_scheduled_sync(
        &self,
        attempt: usize,
        delay_ms: u64,
        error: &str,
    ) -> Result<(), String> {
        self.ensure_running()?;
        self.append(
            "retry_scheduled",
            json!({
                "attempt": attempt,
                "delay_ms": delay_ms,
                "error": bounded_text(error),
            }),
        )
    }

    pub(crate) async fn end(&mut self, reason: TurnEndReason) -> Result<(), String> {
        if self.lifecycle_state == LifecycleState::Closed {
            return Ok(());
        }
        if self.lifecycle_state == LifecycleState::Maintenance {
            self.transition(LifecycleState::Running, "turn_end_recovery")?;
        }
        if self.open_step.is_some() {
            self.step_end("turn_end").await?;
        }
        self.transition(LifecycleState::Idle, "turn_end")?;
        self.append("turn_end", json!({ "reason": reason }))?;
        self.state.checkpoint_persistence().await?;
        self.lifecycle_state = LifecycleState::Closed;
        Ok(())
    }

    fn append_tool_call(
        &self,
        call_id: &str,
        name: &str,
        arguments: &str,
        dispatch_state: &str,
        reason: Option<&str>,
    ) -> Result<(), String> {
        self.append(
            "tool_call",
            json!({
                "call_id": bounded_text(call_id),
                "name": bounded_text(name),
                "arguments": bounded_text(arguments),
                "arguments_hash": stable_hash_hex(arguments.as_bytes()),
                "dispatch_state": dispatch_state,
                "reason": reason,
            }),
        )
    }

    fn transition(&mut self, next: LifecycleState, reason: &str) -> Result<(), String> {
        if self.lifecycle_state == LifecycleState::Closed {
            return Err("Agent lifecycle is already closed".to_string());
        }
        if self.lifecycle_state == next {
            return Ok(());
        }
        let valid = matches!(
            (self.lifecycle_state, next),
            (LifecycleState::Idle, LifecycleState::Running)
                | (LifecycleState::Running, LifecycleState::Maintenance)
                | (LifecycleState::Maintenance, LifecycleState::Running)
                | (LifecycleState::Running, LifecycleState::Idle)
                | (LifecycleState::Maintenance, LifecycleState::Idle)
        );
        if !valid {
            return Err(format!(
                "Invalid agent lifecycle transition {:?} -> {:?}",
                self.lifecycle_state, next
            ));
        }
        self.append(
            "lifecycle_state",
            json!({
                "state": next,
                "reason": bounded_text(reason),
            }),
        )?;
        self.lifecycle_state = next;
        Ok(())
    }

    fn ensure_open(&self) -> Result<(), String> {
        if self.lifecycle_state == LifecycleState::Closed {
            Err("Agent lifecycle is already closed".to_string())
        } else {
            Ok(())
        }
    }

    fn ensure_running(&self) -> Result<(), String> {
        if self.lifecycle_state != LifecycleState::Running {
            Err(format!(
                "Agent lifecycle is not running ({:?})",
                self.lifecycle_state
            ))
        } else {
            Ok(())
        }
    }

    fn append(&self, kind: &str, payload: Value) -> Result<(), String> {
        self.state.persist_agent_event(
            &self.task_id,
            self.turn_id.clone(),
            self.step,
            kind.to_string(),
            now(),
            payload,
        )
    }
}
