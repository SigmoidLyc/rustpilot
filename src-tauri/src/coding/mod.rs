//! Bounded repository operations used by the software-engineering agent.

mod diagnostics;
mod edit;
mod git;
mod patch;
mod search;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use serde_json::Value;

pub(crate) const MAX_OUTPUT_CHARS: usize = 16_000;
pub(crate) const MAX_READ_LINES: usize = 240;
pub(crate) const MAX_GREP_MATCHES: usize = 200;
pub(crate) const MAX_GLOB_RESULTS: usize = 300;
pub(crate) const MAX_WALK_FILES: usize = 20_000;
pub(crate) const MAX_EDIT_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_LINE_BYTES: usize = 256 * 1024;

pub(crate) const IGNORED_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".rustpilot",
    ".runtime",
    ".cache",
    "node_modules",
    "target",
    "target-msvc",
    "target-test",
    "dist",
    "build",
];

pub fn is_mutation(arguments: &Value) -> bool {
    matches!(
        arguments.get("operation").and_then(Value::as_str),
        Some("apply_patch" | "patch" | "replace" | "write" | "delete")
    )
}

pub fn execute(
    workspace: &Path,
    arguments: &Value,
    external_path_approved: bool,
) -> Result<String, String> {
    let root = workspace
        .canonicalize()
        .map_err(|error| format!("Unable to resolve workspace: {error}"))?;
    if !root.is_dir() {
        return Err(format!("Workspace is not a directory: {}", root.display()));
    }
    let operation = argument(arguments, "operation").unwrap_or_else(|| "read".to_string());
    match operation.as_str() {
        "read" => search::read_file(&root, arguments),
        "list" => search::list_directory(&root, arguments),
        "glob" => search::glob_files(&root, arguments),
        "grep" => search::grep_files(&root, arguments),
        "check" | "diagnostics" => diagnostics::check(&root, arguments),
        "status" => git::status(&root),
        "diff" => git::diff(&root, arguments),
        "apply_patch" | "patch" => patch::apply(&root, arguments, external_path_approved),
        "replace" => edit::replace(&root, arguments, external_path_approved),
        "write" => edit::write(&root, arguments, external_path_approved),
        "delete" => edit::delete(&root, arguments, external_path_approved),
        _ => Err(format!("Unsupported rust_code operation: {operation}")),
    }
}

pub(crate) fn argument(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

pub(crate) fn integer_argument(arguments: &Value, key: &str) -> Option<usize> {
    arguments
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

pub(crate) fn bool_argument(arguments: &Value, key: &str) -> bool {
    arguments.get(key).and_then(Value::as_bool).unwrap_or(false)
}

pub(crate) fn scoped_path(root: &Path, raw: &str) -> Result<PathBuf, String> {
    crate::path_guard::resolve_scoped_path(root, raw)
}

pub(crate) fn mutation_path(
    root: &Path,
    raw: &str,
    external_approved: bool,
) -> Result<PathBuf, String> {
    Ok(crate::path_guard::resolve_mutation_path(root, raw, external_approved)?.canonical)
}

pub(crate) fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|value| {
            let text = value.to_string_lossy().replace('\\', "/");
            if text.is_empty() {
                ".".to_string()
            } else {
                text
            }
        })
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn bounded_append(output: &mut String, value: &str, chars: &mut usize) -> bool {
    let remaining = MAX_OUTPUT_CHARS.saturating_sub(*chars);
    if remaining == 0 {
        return false;
    }
    let value_chars = value.chars().count();
    if value_chars <= remaining {
        output.push_str(value);
        *chars += value_chars;
        return true;
    }
    output.extend(value.chars().take(remaining));
    *chars = MAX_OUTPUT_CHARS;
    false
}

pub(crate) fn finish_bounded(mut output: String, truncated: bool) -> String {
    if truncated || output.chars().count() >= MAX_OUTPUT_CHARS {
        output.push_str("\n[output truncated]");
    }
    output
}

pub(crate) fn truncate_text(value: &str, prefix: &str) -> String {
    let mut output: String = value.chars().take(MAX_OUTPUT_CHARS).collect();
    if value.chars().count() > MAX_OUTPUT_CHARS {
        output.push_str("\n[output truncated]");
    }
    if !prefix.is_empty() {
        output = format!("{prefix}: {output}");
    }
    output
}
