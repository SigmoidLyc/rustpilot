use serde_json::Value;

use crate::{
    coding::MAX_EDIT_BYTES, path_guard, sandbox_path_for_task, string_argument, truncate_output,
    workspace_root,
};

pub(crate) async fn run(arguments: &Value, external_path_approved: bool) -> Result<String, String> {
    let operation = string_argument(arguments, "operation").unwrap_or_else(|| "list".to_string());
    let raw_path = string_argument(arguments, "path").unwrap_or_else(|| ".".to_string());
    let path = if matches!(operation.as_str(), "write" | "delete") {
        path_guard::resolve_mutation_path(&workspace_root(), &raw_path, external_path_approved)?
            .canonical
    } else {
        path_guard::resolve_scoped_path(&workspace_root(), &raw_path)?
    };
    match operation.as_str() {
        "list" => {
            let mut entries = tokio::fs::read_dir(&path)
                .await
                .map_err(|error| format!("Unable to list {}: {error}", path.display()))?;
            let mut lines = Vec::new();
            while let Some(entry) = entries
                .next_entry()
                .await
                .map_err(|error| format!("Unable to read directory entry: {error}"))?
            {
                let kind = if entry
                    .file_type()
                    .await
                    .map_err(|error| format!("Unable to inspect entry: {error}"))?
                    .is_dir()
                {
                    "dir "
                } else {
                    "file"
                };
                lines.push(format!("{kind} {}", entry.file_name().to_string_lossy()));
                if lines.len() >= 120 {
                    lines.push("[directory listing truncated]".to_string());
                    break;
                }
            }
            Ok(lines.join("\n"))
        }
        "read" => {
            let contents = tokio::fs::read_to_string(&path)
                .await
                .map_err(|error| format!("Unable to read {}: {error}", path.display()))?;
            Ok(truncate_output(&contents))
        }
        "write" => {
            let contents = string_argument(arguments, "content")
                .ok_or_else(|| "rust_files write requires content".to_string())?;
            if contents.len() > MAX_EDIT_BYTES {
                return Err(format!(
                    "Content exceeds the {} MiB write limit",
                    MAX_EDIT_BYTES / 1024 / 1024
                ));
            }
            tokio::fs::write(&path, contents)
                .await
                .map_err(|error| format!("Unable to write {}: {error}", path.display()))?;
            Ok(format!("Wrote file: {}", path.display()))
        }
        "delete" => {
            if path.is_dir() {
                return Err("rust_files delete only removes regular files".to_string());
            }
            tokio::fs::remove_file(&path)
                .await
                .map_err(|error| format!("Unable to delete {}: {error}", path.display()))?;
            Ok(format!("Deleted {}", path.display()))
        }
        "exists" => Ok(path.exists().to_string()),
        _ => Err(format!("Unsupported rust_files operation: {operation}")),
    }
}

pub(crate) async fn run_sandbox(
    task_id: &str,
    arguments: &Value,
    external_path_approved: bool,
) -> Result<String, String> {
    let raw_path = string_argument(arguments, "path").unwrap_or_else(|| ".".to_string());
    let path = sandbox_path_for_task(task_id, &raw_path)?;
    let mut forwarded = arguments.clone();
    forwarded["path"] = Value::String(path.to_string_lossy().to_string());
    run(&forwarded, external_path_approved).await
}
