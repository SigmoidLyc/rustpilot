use serde_json::{json, Value};
use uuid::Uuid;

use crate::AgentMemoryEntry;

use super::{context::ContextBudget, events::stable_hash_hex};

pub(crate) const CHECKPOINT_PREFIX: &str = "[RustPilot context checkpoint]";
const TOOL_HEAD_CHARS: usize = 1_600;
const TOOL_TAIL_CHARS: usize = 1_600;
const TOOL_MIN_PRUNE_CHARS: usize = 4_096;
const TOOL_MIN_KEEP_CHARS: usize = 64;
const SUMMARY_HEAD_CHARS: usize = 3_200;
const SUMMARY_TAIL_CHARS: usize = 3_200;

#[derive(Debug, Clone)]
pub(crate) struct CompactionPlan {
    pub(crate) compaction_id: String,
    pub(crate) source_entries: Vec<AgentMemoryEntry>,
    pub(crate) summary_entries: Vec<AgentMemoryEntry>,
    pub(crate) tail_entries: Vec<AgentMemoryEntry>,
    pub(crate) source_hash: String,
    pub(crate) expected_surface_hash: String,
    pub(crate) source_start: Option<String>,
    pub(crate) source_end: Option<String>,
    pub(crate) before_tokens: usize,
    pub(crate) shadowed_tokens: usize,
    pub(crate) tail_tokens: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolPruneResult {
    pub(crate) entries: Vec<AgentMemoryEntry>,
    pub(crate) changed_ids: Vec<String>,
    pub(crate) after_tokens: usize,
}

pub(crate) fn memory_entry_to_value(entry: &AgentMemoryEntry) -> Value {
    let mut value = json!({"role": entry.role});
    if entry.role == "tool" || !entry.content.is_empty() {
        value["content"] = Value::String(entry.content.clone());
    }
    if entry.role == "assistant" && !entry.reasoning.is_empty() {
        value["reasoning_content"] = Value::String(entry.reasoning.clone());
    }
    if let Some(reasoning_opaque) = entry.reasoning_opaque.as_deref() {
        if !reasoning_opaque.is_empty() {
            value["reasoning_opaque"] = Value::String(reasoning_opaque.to_string());
        }
    }
    if !entry.tool_calls.is_empty() {
        value["tool_calls"] = serde_json::to_value(
            entry
                .tool_calls
                .iter()
                .map(|call| {
                    json!({
                        "id": call.id,
                        "type": call.call_type,
                        "function": {
                            "name": call.function.name,
                            "arguments": call.function.arguments,
                        }
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| json!([]));
    }
    if let Some(tool_call_id) = entry.tool_call_id.as_deref() {
        value["tool_call_id"] = Value::String(tool_call_id.to_string());
    }
    if let Some(name) = entry.name.as_deref() {
        value["name"] = Value::String(name.to_string());
    }
    if !entry.attachments.is_empty() {
        let attachments = entry
            .attachments
            .iter()
            .map(|attachment| {
                json!({
                    "name": attachment.name,
                    "mime": attachment.mime,
                    "size": attachment.size,
                })
            })
            .collect::<Vec<_>>();
        value["attachments"] = Value::Array(attachments);
    }
    value
}

pub(crate) fn estimate_entries(entries: &[AgentMemoryEntry]) -> usize {
    crate::llm::TokenCounter::FORMAT_TOKENS + entries.iter().map(estimate_entry).sum::<usize>()
}

pub(crate) fn estimate_entry(entry: &AgentMemoryEntry) -> usize {
    let mut tokens = crate::llm::TokenCounter::BASE_MESSAGE_TOKENS
        + crate::llm::TokenCounter::count_text(&entry.role);
    if entry.role == "tool" || !entry.content.is_empty() {
        tokens += crate::llm::TokenCounter::count_text(&entry.content);
    }
    tokens += crate::llm::TokenCounter::count_text(&entry.reasoning);
    for call in &entry.tool_calls {
        tokens += crate::llm::TokenCounter::count_text(&call.function.name);
        tokens += crate::llm::TokenCounter::count_text(&call.function.arguments);
    }
    tokens += entry
        .name
        .as_deref()
        .map(crate::llm::TokenCounter::count_text)
        .unwrap_or_default();
    tokens += entry
        .tool_call_id
        .as_deref()
        .map(crate::llm::TokenCounter::count_text)
        .unwrap_or_default();
    tokens
}

pub(crate) fn surface_hash(entries: &[AgentMemoryEntry]) -> String {
    let values = entries
        .iter()
        .map(memory_entry_to_value)
        .collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&values).unwrap_or_default();
    stable_hash_hex(&bytes)
}

pub(crate) fn source_span(entries: &[AgentMemoryEntry]) -> (Option<String>, Option<String>) {
    (
        entries.first().map(|entry| entry.id.clone()),
        entries.last().map(|entry| entry.id.clone()),
    )
}

pub(crate) fn summary_message(compaction_id: &str, summary: &str) -> AgentMemoryEntry {
    AgentMemoryEntry {
        id: format!("checkpoint-{compaction_id}"),
        role: "user".to_string(),
        content: format!(
            "{CHECKPOINT_PREFIX}\nThis checkpoint replaces older execution history. Treat it as authoritative context and continue from the newest preserved messages.\n\n{summary}"
        ),
        reasoning: String::new(),
        reasoning_opaque: None,
        created_at: crate::now(),
        tool_call_id: None,
        tool_names: Vec::new(),
        tool_calls: Vec::new(),
        name: None,
        base64_image: None,
        attachments: Vec::new(),
    }
}

pub(crate) fn is_checkpoint(entry: &AgentMemoryEntry) -> bool {
    entry.role == "user" && entry.content.starts_with(CHECKPOINT_PREFIX)
}

pub(crate) fn latest_checkpoint(entries: &[AgentMemoryEntry]) -> Option<String> {
    entries
        .iter()
        .rev()
        .find(|entry| is_checkpoint(entry))
        .map(|entry| entry.content.clone())
}

pub(crate) fn prune_tool_results(
    entries: &[AgentMemoryEntry],
    target_tokens: usize,
) -> ToolPruneResult {
    let before_tokens = estimate_entries(entries);
    if before_tokens <= target_tokens {
        return ToolPruneResult {
            entries: entries.to_vec(),
            changed_ids: Vec::new(),
            after_tokens: before_tokens,
        };
    }

    let mut result = entries.to_vec();
    let mut changed_ids = Vec::new();
    let mut changed_flags = vec![false; result.len()];
    let prunable = entries
        .iter()
        .map(|entry| entry.role == "tool" && entry.content.chars().count() >= TOOL_MIN_PRUNE_CHARS)
        .collect::<Vec<_>>();
    let mut keep_chars = TOOL_HEAD_CHARS + TOOL_TAIL_CHARS;
    let mut current_tokens = before_tokens;
    loop {
        let mut changed_this_pass = false;
        for index in 0..result.len() {
            if !prunable[index] {
                continue;
            }
            let changed = {
                let entry = &mut result[index];
                let previous_tokens = estimate_entry(entry);
                let compacted = compact_text(&entry.content, keep_chars / 2, keep_chars / 2);
                if compacted == entry.content {
                    false
                } else {
                    entry.content = compacted;
                    current_tokens = current_tokens
                        .saturating_sub(previous_tokens)
                        .saturating_add(estimate_entry(entry));
                    if !changed_flags[index] {
                        changed_flags[index] = true;
                        changed_ids.push(entry.id.clone());
                    }
                    true
                }
            };
            changed_this_pass |= changed;
            if current_tokens <= target_tokens {
                break;
            }
        }
        if current_tokens <= target_tokens
            || !changed_this_pass
            || keep_chars <= TOOL_MIN_KEEP_CHARS
        {
            break;
        }
        keep_chars = (keep_chars * 3 / 4).max(TOOL_MIN_KEEP_CHARS);
    }

    ToolPruneResult {
        after_tokens: current_tokens,
        entries: result,
        changed_ids,
    }
}

pub(crate) fn build_compaction_plan(
    entries: &[AgentMemoryEntry],
    budget: ContextBudget,
) -> Option<CompactionPlan> {
    if entries.len() < 3 {
        return None;
    }
    let before_tokens = estimate_entries(entries);
    let blocks = history_blocks(entries);
    if blocks.len() < 2 {
        return None;
    }

    let mut entry_prefix = Vec::with_capacity(entries.len() + 1);
    entry_prefix.push(0usize);
    for entry in entries {
        let next = entry_prefix
            .last()
            .copied()
            .unwrap_or_default()
            .saturating_add(estimate_entry(entry));
        entry_prefix.push(next);
    }

    let tail_limit = budget.tail_limit();
    let mut tail_start = blocks.len() - 1;
    let mut tail_entry_tokens = 0usize;
    for (block_index, (start, end)) in blocks.iter().enumerate().rev() {
        let block_tokens = entry_prefix[*end].saturating_sub(entry_prefix[*start]);
        let would_fit = crate::llm::TokenCounter::FORMAT_TOKENS
            .saturating_add(tail_entry_tokens)
            .saturating_add(block_tokens)
            <= tail_limit;
        if block_index == blocks.len() - 1 || would_fit {
            tail_start = *start;
            tail_entry_tokens = tail_entry_tokens.saturating_add(block_tokens);
        } else {
            break;
        }
    }
    let first_block_end = blocks[0].1;
    if tail_start <= first_block_end {
        return None;
    }

    let source_entries = entries[..tail_start].to_vec();
    let tail_entries = entries[tail_start..].to_vec();
    let summary_entries = prepare_summary_entries(&source_entries, budget.input_limit());
    let shadowed_tokens = estimate_entries(&source_entries);
    let tail_tokens = estimate_entries(&tail_entries);
    if shadowed_tokens < 64 {
        return None;
    }
    let (source_start, source_end) = source_span(&source_entries);
    let source_hash = surface_hash(&source_entries);
    let expected_surface_hash = surface_hash(entries);
    Some(CompactionPlan {
        compaction_id: Uuid::new_v4().to_string(),
        source_entries,
        summary_entries,
        tail_entries,
        source_hash,
        expected_surface_hash,
        source_start,
        source_end,
        before_tokens,
        shadowed_tokens,
        tail_tokens,
    })
}

pub(crate) fn finalize_compaction(
    plan: &CompactionPlan,
    summary: &str,
    budget: ContextBudget,
) -> (Vec<AgentMemoryEntry>, Vec<String>) {
    let mut checkpoint = summary_message(&plan.compaction_id, summary);
    enforce_checkpoint_size(&mut checkpoint, plan.shadowed_tokens);
    let mut entries = Vec::with_capacity(plan.tail_entries.len() + 1);
    entries.push(checkpoint);
    entries.extend(plan.tail_entries.iter().cloned());

    let mut pruned_ids = Vec::new();
    let pruned = prune_tool_results(&entries, budget.input_limit());
    if pruned.entries != entries {
        pruned_ids = pruned.changed_ids;
        entries = pruned.entries;
    }
    if estimate_entries(&entries) > budget.input_limit() {
        let mut reduced = entries.clone();
        for entry in &mut reduced {
            if entry.role == "assistant" && !entry.reasoning.is_empty() {
                entry.reasoning.clear();
            }
            if entry.role == "tool" {
                entry.content = compact_text(&entry.content, 512, 512);
            }
        }
        entries = reduced;
    }
    (entries, pruned_ids)
}

pub(crate) fn validate_compaction_result(
    plan: &CompactionPlan,
    entries: &[AgentMemoryEntry],
    budget: ContextBudget,
) -> Result<(), String> {
    let Some(checkpoint) = entries.first() else {
        return Err("Compaction produced an empty model surface.".to_string());
    };
    if !is_checkpoint(checkpoint) {
        return Err("Compaction surface does not start with a checkpoint.".to_string());
    }
    if estimate_entries(std::slice::from_ref(checkpoint)) >= plan.shadowed_tokens {
        return Err("Compaction checkpoint is not smaller than its replaced history.".to_string());
    }
    if estimate_entries(entries) > budget.input_limit() {
        return Err("Compaction surface still exceeds the input budget.".to_string());
    }
    validate_tool_pairs(entries)
}

fn history_blocks(entries: &[AgentMemoryEntry]) -> Vec<(usize, usize)> {
    let mut blocks = Vec::new();
    let mut start = 0;
    for index in 1..entries.len() {
        if matches!(entries[index].role.as_str(), "user" | "assistant") {
            blocks.push((start, index));
            start = index;
        }
    }
    if start < entries.len() {
        blocks.push((start, entries.len()));
    }
    blocks
}

fn prepare_summary_entries(
    entries: &[AgentMemoryEntry],
    target_tokens: usize,
) -> Vec<AgentMemoryEntry> {
    let mut result = entries.to_vec();
    let mut target = target_tokens;
    let mut pruned = prune_tool_results(&result, target);
    result = pruned.entries;
    while pruned.after_tokens > target && target > 512 {
        target = target.saturating_mul(3) / 4;
        for entry in &mut result {
            if entry.role == "tool" {
                entry.content = compact_text(
                    &entry.content,
                    SUMMARY_HEAD_CHARS / 2,
                    SUMMARY_TAIL_CHARS / 2,
                );
            } else if entry.role == "assistant" && entry.reasoning.len() > 1_024 {
                entry.reasoning = compact_text(&entry.reasoning, 512, 512);
            }
        }
        pruned = prune_tool_results(&result, target);
        result = pruned.entries;
    }
    result
}

fn enforce_checkpoint_size(checkpoint: &mut AgentMemoryEntry, shadowed_tokens: usize) {
    let target_tokens = shadowed_tokens.saturating_sub(1).max(1);
    let mut target_chars = target_tokens.saturating_mul(3).max(32);
    while estimate_entries(std::slice::from_ref(checkpoint)) >= shadowed_tokens
        && checkpoint.content.chars().count() > 32
    {
        checkpoint.content = compact_text(
            &checkpoint.content,
            target_chars / 2,
            target_chars.saturating_sub(target_chars / 2),
        );
        if estimate_entries(std::slice::from_ref(checkpoint)) >= shadowed_tokens {
            target_chars = target_chars.saturating_mul(3) / 4;
            checkpoint.content = checkpoint
                .content
                .chars()
                .take(target_chars.max(32))
                .collect();
        }
    }
}

fn validate_tool_pairs(entries: &[AgentMemoryEntry]) -> Result<(), String> {
    let mut pending = std::collections::HashSet::new();
    for entry in entries {
        if !pending.is_empty() && entry.role != "tool" {
            return Err("Compaction split an assistant/tool pair.".to_string());
        }
        match entry.role.as_str() {
            "assistant" => {
                for call in &entry.tool_calls {
                    if call.id.trim().is_empty() || !pending.insert(call.id.clone()) {
                        return Err("Compaction produced invalid tool-call ids.".to_string());
                    }
                }
            }
            "tool" => {
                let Some(id) = entry.tool_call_id.as_deref() else {
                    return Err("Compaction produced a tool result without an id.".to_string());
                };
                if !pending.remove(id) {
                    return Err("Compaction produced an unmatched tool result.".to_string());
                }
            }
            "user" => {}
            _ => return Err("Compaction produced an unsupported message role.".to_string()),
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err("Compaction left an unfinished tool call.".to_string())
    }
}

pub(crate) fn compact_text(value: &str, head_chars: usize, tail_chars: usize) -> String {
    let length = value.chars().count();
    if length <= head_chars.saturating_add(tail_chars).saturating_add(64) {
        return value.to_string();
    }
    let head = value.chars().take(head_chars).collect::<String>();
    let tail = value
        .chars()
        .skip(length.saturating_sub(tail_chars))
        .collect::<String>();
    format!("{head}\n[tool output pruned: {length} characters]\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(role: &str, content: &str) -> AgentMemoryEntry {
        AgentMemoryEntry {
            id: Uuid::new_v4().to_string(),
            role: role.to_string(),
            content: content.to_string(),
            reasoning: String::new(),
            reasoning_opaque: None,
            created_at: 1,
            tool_call_id: None,
            tool_names: Vec::new(),
            tool_calls: Vec::new(),
            name: None,
            base64_image: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn compaction_keeps_complete_recent_assistant_tool_block() {
        let mut entries = vec![entry("user", "objective")];
        for index in 0..20 {
            entries.push(entry("assistant", &format!("step {index}")));
            entries.push(entry("tool", &"x".repeat(300)));
        }
        let plan = build_compaction_plan(
            &entries,
            ContextBudget {
                window: 512,
                output_reserve: 0,
                safety_margin: 0,
            },
        )
        .expect("history should have a compactable prefix");
        assert_eq!(plan.tail_entries.last().unwrap().role, "tool");
        assert_eq!(
            plan.tail_entries[plan.tail_entries.len() - 2].role,
            "assistant"
        );
        assert!(plan.source_entries.len() < entries.len());
        assert_eq!(plan.expected_surface_hash, surface_hash(&entries));
        assert_ne!(plan.expected_surface_hash, plan.source_hash);
    }

    #[test]
    fn tool_pruning_preserves_head_and_tail() {
        let content = format!("HEAD{}TAIL", "x".repeat(8_000));
        let result = prune_tool_results(&[entry("tool", &content)], 8);
        assert!(result.changed_ids.is_empty() || result.entries[0].content.contains("pruned"));
        assert!(result.entries[0].content.starts_with("HEAD"));
        assert!(result.entries[0].content.ends_with("TAIL"));
    }

    #[test]
    fn direct_surface_estimate_matches_wire_message_estimate() {
        let mut assistant = entry("assistant", "answer");
        assistant.reasoning = "private reasoning".to_string();
        assistant.reasoning_opaque = Some("opaque".to_string());
        assistant.name = Some("assistant-name".to_string());
        assistant.tool_call_id = Some("call-parent".to_string());
        assistant.tool_calls = vec![crate::agent::MessageToolCall {
            id: "call-1".to_string(),
            call_type: "function".to_string(),
            function: crate::agent::FunctionCall {
                name: "rust_clock".to_string(),
                arguments: "{\"timezone\":\"Asia/Shanghai\"}".to_string(),
            },
        }];
        let tool = {
            let mut value = entry("tool", "result");
            value.tool_call_id = Some("call-1".to_string());
            value.name = Some("rust_clock".to_string());
            value
        };
        let entries = vec![entry("user", "目标：保留 Unicode"), assistant, tool];
        let values = entries
            .iter()
            .map(memory_entry_to_value)
            .collect::<Vec<_>>();
        assert_eq!(
            estimate_entries(&entries),
            crate::llm::TokenCounter::count_messages(&values)
        );
    }

    #[test]
    fn large_cjk_tool_output_is_pruned_without_breaking_unicode() {
        let content = format!("开头{}结尾", "中间内容".repeat(20_000));
        let result = prune_tool_results(&[entry("tool", &content)], 128);
        assert_eq!(result.changed_ids.len(), 1);
        assert!(result.entries[0].content.starts_with("开头"));
        assert!(result.entries[0].content.ends_with("结尾"));
        assert!(result.entries[0].content.contains("tool output pruned"));
        assert!(
            result.after_tokens <= 128,
            "pruned CJK output still uses {} tokens",
            result.after_tokens
        );
    }

    #[test]
    fn repeated_compaction_keeps_checkpoint_and_tool_pairs_valid() {
        let mut entries = vec![entry("user", "长期任务目标：完成并验证工作。")];
        for index in 0..24 {
            let mut assistant = entry("assistant", &format!("完成第 {index} 步"));
            assistant.tool_calls = vec![crate::agent::MessageToolCall {
                id: format!("call-{index}"),
                call_type: "function".to_string(),
                function: crate::agent::FunctionCall {
                    name: "rust_read".to_string(),
                    arguments: format!("{{\"step\":{index}}}"),
                },
            }];
            let mut tool = entry("tool", &format!("证据 {index} {}", "x".repeat(700)));
            tool.tool_call_id = Some(format!("call-{index}"));
            tool.name = Some("rust_read".to_string());
            entries.push(assistant);
            entries.push(tool);
        }
        let budget = ContextBudget {
            window: 2_048,
            output_reserve: 0,
            safety_margin: 0,
        };
        let first_plan = build_compaction_plan(&entries, budget).expect("first plan should exist");
        let first_summary = crate::agent_loop::summary::normalize(
            &crate::agent_loop::summary::fallback_summary(
                &first_plan.source_entries,
                first_plan.shadowed_tokens,
            ),
            first_plan.shadowed_tokens,
        );
        let (first_surface, _) = finalize_compaction(&first_plan, &first_summary, budget);
        validate_compaction_result(&first_plan, &first_surface, budget)
            .expect("first compaction should preserve protocol pairs");

        let mut next_entries = first_surface;
        next_entries.push(entry("user", "继续验证后续步骤"));
        for index in 24..36 {
            let mut assistant = entry("assistant", &format!("完成第 {index} 步"));
            assistant.tool_calls = vec![crate::agent::MessageToolCall {
                id: format!("call-{index}"),
                call_type: "function".to_string(),
                function: crate::agent::FunctionCall {
                    name: "rust_read".to_string(),
                    arguments: format!("{{\"step\":{index}}}"),
                },
            }];
            let mut tool = entry("tool", &format!("证据 {index} {}", "y".repeat(700)));
            tool.tool_call_id = Some(format!("call-{index}"));
            tool.name = Some("rust_read".to_string());
            next_entries.push(assistant);
            next_entries.push(tool);
        }
        let second_plan =
            build_compaction_plan(&next_entries, budget).expect("second plan should exist");
        let second_summary = crate::agent_loop::summary::normalize(
            &crate::agent_loop::summary::fallback_summary(
                &second_plan.source_entries,
                second_plan.shadowed_tokens,
            ),
            second_plan.shadowed_tokens,
        );
        let (second_surface, _) = finalize_compaction(&second_plan, &second_summary, budget);
        validate_compaction_result(&second_plan, &second_surface, budget)
            .expect("repeated compaction should preserve protocol pairs");
        assert!(is_checkpoint(&second_surface[0]));
    }
}
