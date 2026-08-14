use crate::AgentMemoryEntry;
use serde_json::{json, Value};

use super::{
    context::ContextBudget,
    surface::{compact_text, estimate_entries, latest_checkpoint, memory_entry_to_value},
};

const SUMMARY_INSTRUCTION: &str = "You are RustPilot's context checkpoint writer. Summarize the execution history into a compact, factual checkpoint for another agent instance. Preserve the user's objective, constraints, decisions, completed work, current state, blocked or unknown items, important files and artifacts, errors and fixes, the next action, and critical evidence. Do not reveal chain-of-thought. Do not invent facts. Keep exact paths, identifiers, commands, and error messages when they are operationally important. Output only the requested headings and content.";
const SUMMARY_MAX_CHARS: usize = 12_000;

pub(crate) fn request_messages(
    system_messages: &[Value],
    entries: &[AgentMemoryEntry],
    budget: ContextBudget,
) -> Vec<Value> {
    let mut messages = Vec::with_capacity(system_messages.len() + entries.len() + 1);
    messages.extend_from_slice(system_messages);
    messages.extend(entries.iter().map(memory_entry_to_value));
    messages.push(json!({
        "role": "user",
        "content": format!(
            "{SUMMARY_INSTRUCTION}\n\nThe checkpoint must be shorter than the replaced history ({} estimated tokens).",
            estimate_entries(entries)
        )
    }));
    let mut bounded = messages;
    // The compaction input is already pruned, but keep a final hard bound for
    // pathological attachment metadata or legacy records.
    while crate::llm::TokenCounter::count_messages(&bounded) > budget.input_limit()
        && bounded.len() > system_messages.len() + 1
    {
        let remove_at = system_messages.len();
        bounded.remove(remove_at);
    }
    bounded
}

pub(crate) fn normalize(summary: &str, shadowed_tokens: usize) -> String {
    let mut summary = summary.trim().to_string();
    if summary.is_empty() {
        return fallback_summary(&[], shadowed_tokens);
    }
    summary = compact_text(&summary, SUMMARY_MAX_CHARS / 2, SUMMARY_MAX_CHARS / 2);
    let max_tokens = shadowed_tokens.saturating_sub(1).max(1);
    while crate::llm::TokenCounter::count_text(&summary) >= shadowed_tokens
        && summary.chars().count() > 32
    {
        let target_chars = (max_tokens.saturating_mul(3)).max(32);
        summary = compact_text(&summary, target_chars / 2, target_chars / 2);
        if crate::llm::TokenCounter::count_text(&summary) >= shadowed_tokens
            && summary.chars().count() <= target_chars
        {
            summary = summary.chars().take(target_chars).collect();
            break;
        }
    }
    if summary.is_empty() {
        fallback_summary(&[], shadowed_tokens)
    } else {
        summary
    }
}

pub(crate) fn fallback_summary(entries: &[AgentMemoryEntry], shadowed_tokens: usize) -> String {
    let objective = entries
        .iter()
        .find(|entry| entry.role == "user" && !entry.content.starts_with("[RustPilot context"))
        .map(|entry| compact_text(&entry.content, 1_600, 800))
        .unwrap_or_else(|| "The original objective is preserved in the transcript.".to_string());
    let completed = entries
        .iter()
        .filter(|entry| entry.role == "assistant" && !entry.content.trim().is_empty())
        .rev()
        .take(4)
        .map(|entry| compact_text(&entry.content, 800, 400))
        .collect::<Vec<_>>();
    let evidence = entries
        .iter()
        .filter(|entry| entry.role == "tool")
        .rev()
        .take(6)
        .map(|entry| {
            let name = entry
                .name
                .as_deref()
                .or_else(|| entry.tool_names.first().map(String::as_str))
                .unwrap_or("tool");
            format!("- {name}: {}", compact_text(&entry.content, 500, 300))
        })
        .collect::<Vec<_>>();
    let previous = latest_checkpoint(entries)
        .map(|value| compact_text(&value, 1_000, 600))
        .unwrap_or_else(|| "None".to_string());
    let mut summary = format!(
        "Objective\n{objective}\n\nConstraints / Decisions\nPreserve the existing task objective and verified decisions from the transcript.\n\nCompleted\n{}\n\nCurrent State\nThe newest preserved assistant and tool records are authoritative.\n\nBlocked / Unknown\nNo additional blocker was inferred by the code-only fallback.\n\nFiles / Artifacts\nUse the paths and artifacts recorded in the transcript.\n\nErrors / Fixes\nSee the critical evidence below; do not assume an unverified fix.\n\nNext Action\nContinue from the newest preserved turn and verify the next required result.\n\nCritical Evidence\n{}\n\nPrevious Checkpoint\n{previous}",
        if completed.is_empty() {
            "No assistant completion was available.".to_string()
        } else {
            completed.into_iter().rev().collect::<Vec<_>>().join("\n- ")
        },
        if evidence.is_empty() {
            "No tool evidence was available.".to_string()
        } else {
            evidence.into_iter().rev().collect::<Vec<_>>().join("\n")
        },
    );
    if crate::llm::TokenCounter::count_text(&summary) >= shadowed_tokens {
        let max_chars = shadowed_tokens.saturating_sub(1).saturating_mul(3).max(32);
        summary = compact_text(&summary, max_chars / 2, max_chars / 2);
    }
    summary
}
