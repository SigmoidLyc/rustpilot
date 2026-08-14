use serde_json::Value;

use crate::llm::TokenCounter;

pub(crate) const DEFAULT_OUTPUT_RESERVE: usize = 4096;
pub(crate) const CONTEXT_SAFETY_MARGIN: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextBudget {
    pub(crate) window: usize,
    pub(crate) output_reserve: usize,
    pub(crate) safety_margin: usize,
}

impl ContextBudget {
    pub(crate) fn for_model(model: &str, base_url: &str) -> Self {
        let window = crate::model_catalog::context_window_for_model(model, base_url, "");
        let output_reserve = if crate::model_catalog::is_deepseek_v4(model, base_url, "") {
            32 * 1024
        } else if crate::model_catalog::uses_max_completion_tokens(model, base_url, "") {
            8 * 1024
        } else {
            DEFAULT_OUTPUT_RESERVE
        };
        Self {
            window,
            output_reserve,
            safety_margin: CONTEXT_SAFETY_MARGIN,
        }
    }

    pub(crate) fn input_limit(self) -> usize {
        self.window
            .saturating_sub(self.output_reserve)
            .saturating_sub(self.safety_margin)
    }

    pub(crate) fn pressure_limit(self) -> usize {
        self.input_limit().saturating_mul(80) / 100
    }

    pub(crate) fn tail_limit(self) -> usize {
        self.input_limit().saturating_mul(16) / 100
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextReport {
    pub(crate) before_tokens: usize,
    pub(crate) after_tokens: usize,
    pub(crate) dropped_messages: usize,
    pub(crate) compacted: bool,
}

pub(crate) fn fit_openai_messages(
    messages: &mut Vec<Value>,
    tools: &[Value],
    budget: ContextBudget,
) -> Result<ContextReport, String> {
    let before_tokens = total_tokens(messages, tools);
    let limit = budget.input_limit();
    if before_tokens <= limit {
        return Ok(ContextReport {
            before_tokens,
            after_tokens: before_tokens,
            dropped_messages: 0,
            compacted: false,
        });
    }

    let prefix_len = stable_prefix_len(messages);
    let mut prefix = messages[..prefix_len].to_vec();
    let mut compacted = false;
    if total_tokens(&prefix, tools) > limit {
        let prefix_tokens = total_tokens(&prefix, tools);
        let system_len = leading_system_len(&prefix);
        let system_tokens = total_tokens(&prefix[..system_len], tools);
        let mut anchor = prefix[system_len..].to_vec();
        compact_block(&mut anchor, limit.saturating_sub(system_tokens));
        prefix.truncate(system_len);
        prefix.extend(anchor);
        compacted = total_tokens(&prefix, tools) < prefix_tokens;
    }
    if total_tokens(&prefix, tools) > limit {
        return Err(format!(
            "Conversation context exceeds the configured input budget ({} tokens).",
            limit
        ));
    }

    if prefix_len >= messages.len() {
        *messages = prefix;
        return Ok(ContextReport {
            before_tokens,
            after_tokens: total_tokens(messages, tools),
            dropped_messages: messages.len().saturating_sub(prefix_len),
            compacted,
        });
    }

    let blocks = history_blocks(messages, prefix_len);
    let mut selected = Vec::new();
    for block in blocks.iter().rev() {
        let block_values = messages[block.0..block.1].to_vec();
        let mut with_block = prefix.clone();
        with_block.extend(block_values.iter().cloned());
        with_block.extend(selected.iter().cloned());
        if total_tokens(&with_block, tools) <= limit {
            selected.splice(0..0, block_values);
            continue;
        }
        if selected.is_empty() {
            let mut compacted_block = block_values;
            let block_tokens = TokenCounter::count_messages(&compacted_block);
            compact_block(
                &mut compacted_block,
                limit.saturating_sub(total_tokens(&prefix, tools)),
            );
            if TokenCounter::count_messages(&compacted_block) < block_tokens {
                compacted = true;
            }
            if !compacted_block.is_empty() {
                selected = compacted_block;
            }
        }
        break;
    }

    let mut bounded = prefix;
    bounded.extend(selected);
    let after_tokens = total_tokens(&bounded, tools);
    if after_tokens > limit {
        return Err(format!(
            "Conversation context still exceeds the configured input budget ({} tokens).",
            limit
        ));
    }
    let dropped_messages = messages.len().saturating_sub(bounded.len());
    *messages = bounded;
    Ok(ContextReport {
        before_tokens,
        after_tokens,
        dropped_messages,
        compacted,
    })
}

pub(crate) fn total_tokens(messages: &[Value], tools: &[Value]) -> usize {
    TokenCounter::count_messages(messages)
        + tools
            .iter()
            .map(|tool| TokenCounter::count_text(&tool.to_string()))
            .sum::<usize>()
}

fn leading_system_len(messages: &[Value]) -> usize {
    messages
        .iter()
        .take_while(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .count()
}

fn stable_prefix_len(messages: &[Value]) -> usize {
    let mut index = leading_system_len(messages);
    if index < messages.len() {
        let first_role = messages[index].get("role").and_then(Value::as_str);
        let has_tool_calls = messages[index]
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| !calls.is_empty());
        index += 1;
        if first_role == Some("assistant") && has_tool_calls {
            while index < messages.len()
                && messages[index].get("role").and_then(Value::as_str) == Some("tool")
            {
                index += 1;
            }
        }
    }
    index
}

fn history_blocks(messages: &[Value], start: usize) -> Vec<(usize, usize)> {
    let mut blocks = Vec::new();
    let mut index = start;
    while index < messages.len() {
        let block_start = index;
        let role = messages[index].get("role").and_then(Value::as_str);
        index += 1;
        if role == Some("assistant")
            && messages[block_start]
                .get("tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|calls| !calls.is_empty())
        {
            while index < messages.len()
                && messages[index].get("role").and_then(Value::as_str) == Some("tool")
            {
                index += 1;
            }
        }
        blocks.push((block_start, index));
    }
    blocks
}

fn compact_block(block: &mut [Value], target_tokens: usize) {
    if block.is_empty() || target_tokens == 0 {
        return;
    }
    let mut current = TokenCounter::count_messages(block);
    if current <= target_tokens {
        return;
    }
    for index in (0..block.len()).rev() {
        if current <= target_tokens {
            break;
        }
        {
            let content = block[index]
                .get("content")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(content) = content {
                let original_len = content.chars().count();
                let mut keep_chars = original_len.min(target_tokens.saturating_mul(4));
                loop {
                    let truncated = if keep_chars < original_len {
                        let mut value = content.chars().take(keep_chars).collect::<String>();
                        value.push_str("\n[context truncated]");
                        value
                    } else {
                        content.clone()
                    };
                    if let Some(object) = block[index].as_object_mut() {
                        object.insert("content".to_string(), Value::String(truncated));
                    }
                    current = TokenCounter::count_messages(block);
                    if current <= target_tokens || keep_chars == 0 {
                        break;
                    }
                    let next = keep_chars.saturating_mul(3) / 4;
                    keep_chars = next.min(keep_chars.saturating_sub(1));
                }
            }
            if let Some(reasoning) = block[index].get_mut("reasoning_content") {
                *reasoning = Value::Null;
            }
        }
        current = TokenCounter::count_messages(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn budget_keeps_tool_pairs_intact() {
        let mut messages = vec![
            json!({"role":"system","content":"system"}),
            json!({"role":"user","content":"request"}),
            json!({"role":"assistant","content":"older answer"}),
            json!({"role":"assistant","tool_calls":[{"id":"call-1","function":{"name":"rust_clock","arguments":"{}"}}]}),
            json!({"role":"tool","tool_call_id":"call-1","content":"result"}),
        ];
        let report = fit_openai_messages(
            &mut messages,
            &[],
            ContextBudget {
                window: 42,
                output_reserve: 0,
                safety_margin: 0,
            },
        )
        .expect("history should fit");
        assert!(report.dropped_messages > 0);
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["tool_calls"][0]["id"], "call-1");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-1");
    }

    #[test]
    fn oversized_first_user_message_is_compacted_without_dropping_the_turn() {
        let mut messages = vec![
            json!({"role":"system","content":"system"}),
            json!({"role":"user","content":"x".repeat(400)}),
        ];
        let report = fit_openai_messages(
            &mut messages,
            &[],
            ContextBudget {
                window: 48,
                output_reserve: 0,
                safety_margin: 0,
            },
        )
        .expect("the oversized user turn should be compacted");
        assert_eq!(messages[1]["role"], "user");
        assert!(messages[1]["content"]
            .as_str()
            .expect("user content should remain text")
            .ends_with("[context truncated]"));
        assert!(report.after_tokens <= 48);
    }
}
