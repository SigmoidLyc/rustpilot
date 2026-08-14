use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc, RwLock},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    agents::{AgentSpec, MCP_DYNAMIC_TOOL_PREFIX},
    tool_catalog, AgentToolDefinition, AppState,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub(crate) struct McpToolDefinition {
    pub(crate) exposed_name: String,
    pub(crate) server_id: String,
    pub(crate) remote_name: String,
    pub(crate) description: String,
    pub(crate) input_schema: Value,
}

#[derive(Clone)]
pub(crate) struct ToolDefinitionSnapshot {
    pub(crate) definitions: Arc<Vec<Value>>,
    pub(crate) schema_hash: Arc<str>,
}

pub(crate) struct ToolDefinitionCacheEntry {
    revision: u64,
    snapshot: Arc<ToolDefinitionSnapshot>,
    agent_snapshots: HashMap<String, Arc<ToolDefinitionSnapshot>>,
}

pub(crate) type ToolDefinitionCache = Arc<RwLock<Option<ToolDefinitionCacheEntry>>>;

pub(crate) fn canonical_json(value: &Value) -> Value {
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

pub(crate) fn tool_schema_hash(definitions: &[Value]) -> String {
    let normalized = canonical_json(&Value::Array(definitions.to_vec()));
    let bytes = serde_json::to_vec(&normalized).unwrap_or_default();
    stable_hash_hex(&bytes)
}

pub(crate) fn tool_definitions_for_state(state: &AppState) -> Arc<ToolDefinitionSnapshot> {
    let revision = state.mcp_tools_revision.load(Ordering::Acquire);
    if let Ok(cache) = state.tool_definition_cache.read() {
        if let Some(entry) = cache.as_ref() {
            if entry.revision == revision {
                return entry.snapshot.clone();
            }
        }
    }

    let mut definitions = tool_catalog::tool_definitions();
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
        *cache = Some(ToolDefinitionCacheEntry {
            revision,
            snapshot: snapshot.clone(),
            agent_snapshots: HashMap::new(),
        });
    }
    snapshot
}

pub(crate) fn tool_definitions_for_agent(
    state: &AppState,
    spec: &AgentSpec,
) -> Arc<ToolDefinitionSnapshot> {
    let revision = state.mcp_tools_revision.load(Ordering::Acquire);
    let cache_key = capability_cache_key(spec);
    if let Ok(cache) = state.tool_definition_cache.read() {
        if let Some(entry) = cache.as_ref() {
            if entry.revision == revision {
                if let Some(snapshot) = entry.agent_snapshots.get(&cache_key) {
                    return snapshot.clone();
                }
            }
        }
    }

    let global = tool_definitions_for_state(state);
    let definitions = global
        .definitions
        .iter()
        .filter(|definition| {
            definition
                .pointer("/function/name")
                .and_then(Value::as_str)
                .is_some_and(|name| spec.allows_tool(name))
        })
        .cloned()
        .collect::<Vec<_>>();
    let filtered = Arc::new(ToolDefinitionSnapshot {
        schema_hash: Arc::<str>::from(tool_schema_hash(&definitions)),
        definitions: Arc::new(definitions),
    });

    if let Ok(mut cache) = state.tool_definition_cache.write() {
        if let Some(entry) = cache.as_mut() {
            if entry.revision == revision && Arc::ptr_eq(&entry.snapshot, &global) {
                entry.agent_snapshots.insert(cache_key, filtered.clone());
            }
        }
    }
    filtered
}

pub(crate) fn agent_has_tool(state: &AppState, spec: &AgentSpec, name: &str) -> bool {
    if !spec.allows_tool(name) {
        return false;
    }
    if !name.starts_with(MCP_DYNAMIC_TOOL_PREFIX) {
        return true;
    }
    state
        .mcp_tools
        .read()
        .ok()
        .is_some_and(|tools| tools.contains_key(name))
}

fn capability_cache_key(spec: &AgentSpec) -> String {
    let mut key = String::with_capacity(
        spec.key.len()
            + spec.tool_names.iter().map(String::len).sum::<usize>()
            + spec
                .special_tool_names
                .iter()
                .map(String::len)
                .sum::<usize>()
            + 32,
    );
    key.push_str(&spec.key.len().to_string());
    key.push(':');
    key.push_str(&spec.key);
    key.push('|');
    key.push(if spec.uses_mcp { '1' } else { '0' });
    key.push(if spec.uses_sandbox { '1' } else { '0' });
    key.push('|');
    append_tool_names(&mut key, &spec.tool_names);
    key.push('|');
    append_tool_names(&mut key, &spec.special_tool_names);
    key
}

fn append_tool_names(target: &mut String, names: &[String]) {
    for name in names {
        target.push_str(&name.len().to_string());
        target.push(':');
        target.push_str(name);
        target.push(';');
    }
}

pub(crate) fn available_tool_views(state: Option<&AppState>) -> Vec<AgentToolDefinition> {
    let definitions = state
        .map(tool_definitions_for_state)
        .map(|snapshot| snapshot.definitions.clone())
        .unwrap_or_else(|| Arc::new(tool_catalog::tool_definitions()));
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
