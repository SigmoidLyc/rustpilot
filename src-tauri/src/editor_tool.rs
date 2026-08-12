use std::{collections::VecDeque, fs, path::Path};

use serde_json::Value;

use crate::{path_guard, string_argument, truncate_output, AppState};

const CREATED_FILE_MARKER: &str = "__RUSTPILOT_CREATED__";
const MAX_HISTORY_ENTRIES: usize = 20;

pub(crate) async fn run(
    state: &AppState,
    arguments: &Value,
    external_path_approved: bool,
    workspace: &Path,
) -> Result<String, String> {
    let command = string_argument(arguments, "command")
        .ok_or_else(|| "rust_str_replace_editor requires command".to_string())?;
    let raw_path = string_argument(arguments, "path")
        .ok_or_else(|| "rust_str_replace_editor requires path".to_string())?;
    let path = if matches!(
        command.as_str(),
        "create" | "str_replace" | "insert" | "undo_edit"
    ) {
        path_guard::resolve_mutation_path(workspace, &raw_path, external_path_approved)?.canonical
    } else {
        path_guard::resolve_scoped_path(workspace, &raw_path)?
    };
    match command.as_str() {
        "view" => view(&path, arguments).await,
        "create" => create(state, &path, arguments).await,
        "str_replace" => replace(state, &path, arguments).await,
        "insert" => insert(state, &path, arguments).await,
        "undo_edit" => undo(state, &path).await,
        _ => Err(format!("Unsupported editor command: {command}")),
    }
}

async fn view(path: &Path, arguments: &Value) -> Result<String, String> {
    if path.is_dir() {
        let mut lines = Vec::new();
        for entry in
            fs::read_dir(path).map_err(|error| format!("Unable to view directory: {error}"))?
        {
            let entry =
                entry.map_err(|error| format!("Unable to read directory entry: {error}"))?;
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
    let contents = tokio::fs::read_to_string(path)
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

async fn create(state: &AppState, path: &Path, arguments: &Value) -> Result<String, String> {
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
    tokio::fs::write(path, content)
        .await
        .map_err(|error| format!("Unable to create {}: {error}", path.display()))?;
    record_file_snapshot(state, path, CREATED_FILE_MARKER.to_string())?;
    Ok(format!("File created successfully at: {}", path.display()))
}

async fn replace(state: &AppState, path: &Path, arguments: &Value) -> Result<String, String> {
    let old = string_argument(arguments, "old_str")
        .ok_or_else(|| "Parameter old_str is required for str_replace".to_string())?;
    let new = string_argument(arguments, "new_str").unwrap_or_default();
    let original = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| format!("Unable to read {}: {error}", path.display()))?;
    let count = original.match_indices(&old).count();
    if count != 1 {
        return Err(format!(
            "old_str must match exactly one location; found {count}."
        ));
    }
    record_file_snapshot(state, path, original.clone())?;
    let updated = original.replacen(&old, &new, 1);
    tokio::fs::write(path, updated)
        .await
        .map_err(|error| format!("Unable to write {}: {error}", path.display()))?;
    Ok(format!("Replacement applied to {}", path.display()))
}

async fn insert(state: &AppState, path: &Path, arguments: &Value) -> Result<String, String> {
    let line = arguments
        .get("insert_line")
        .and_then(Value::as_i64)
        .ok_or_else(|| "Parameter insert_line is required for insert".to_string())?;
    let new = string_argument(arguments, "new_str")
        .ok_or_else(|| "Parameter new_str is required for insert".to_string())?;
    let original = tokio::fs::read_to_string(path)
        .await
        .map_err(|error| format!("Unable to read {}: {error}", path.display()))?;
    let mut lines: Vec<String> = original.lines().map(ToString::to_string).collect();
    let index = line.max(0) as usize;
    if index > lines.len() {
        return Err(format!("insert_line {line} is outside the file."));
    }
    record_file_snapshot(state, path, original)?;
    lines.insert(index, new);
    tokio::fs::write(path, lines.join("\n"))
        .await
        .map_err(|error| format!("Unable to write {}: {error}", path.display()))?;
    Ok(format!("Text inserted into {}", path.display()))
}

async fn undo(state: &AppState, path: &Path) -> Result<String, String> {
    let previous = state
        .edit_history
        .lock()
        .map_err(|_| "Edit history lock is poisoned".to_string())?
        .get_mut(&path.to_string_lossy().to_string())
        .and_then(VecDeque::pop_back)
        .ok_or_else(|| format!("No edit history is available for {}", path.display()))?;
    if previous == CREATED_FILE_MARKER {
        tokio::fs::remove_file(path)
            .await
            .map_err(|error| format!("Unable to undo created file: {error}"))?;
    } else {
        tokio::fs::write(path, previous)
            .await
            .map_err(|error| format!("Unable to restore {}: {error}", path.display()))?;
    }
    Ok(format!("Last edit undone for {}", path.display()))
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
    while entries.len() > MAX_HISTORY_ENTRIES {
        entries.pop_front();
    }
    Ok(())
}
