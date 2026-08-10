use serde_json::{json, Value};
use tokio::process::Command;

use crate::{base64_encode, sandbox_path_for_task, string_argument, truncate_output};

pub(crate) async fn run_python(arguments: &Value) -> Result<String, String> {
    let code = string_argument(arguments, "code")
        .ok_or_else(|| "rust_python_execute requires code".to_string())?;
    #[cfg(target_os = "windows")]
    let mut process = Command::new("python");
    #[cfg(not(target_os = "windows"))]
    let mut process = Command::new("python3");
    let output = process
        .args(["-c", &code])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("Unable to start Python: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let result = json!({
        "success": output.status.success(),
        "observation": stdout.to_string(),
        "stderr": stderr.to_string(),
        "exit_code": output.status.code()
    });
    Ok(truncate_output(
        &serde_json::to_string_pretty(&result).unwrap_or_default(),
    ))
}

pub(crate) async fn run_sandbox_vision(task_id: &str, arguments: &Value) -> Result<String, String> {
    let raw_path = string_argument(arguments, "path")
        .ok_or_else(|| "rust_sandbox_vision requires path".to_string())?;
    let path = sandbox_path_for_task(task_id, &raw_path)?;
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| format!("Unable to inspect {}: {error}", path.display()))?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|error| format!("Unable to read {}: {error}", path.display()))?;
    let mime_type = match extension.to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    };
    let include_base64 = arguments
        .get("include_base64")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(serde_json::to_string_pretty(&json!({
        "path": path,
        "exists": true,
        "bytes": metadata.len(),
        "extension": extension,
        "visual_available": true,
        "mime_type": mime_type,
        "image_base64": include_base64.then(|| base64_encode(&bytes))
    }))
    .unwrap_or_default())
}
