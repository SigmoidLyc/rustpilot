use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    coding, computer_tool, data_tool, editor_tool, file_tool, http_tool, mcp_tool, planning_tool,
    runtime_tool, shell_tool, string_argument, workspace_root, AgentSettings, AppState,
};

pub(crate) async fn run(
    state: &AppState,
    task_id: &str,
    name: &str,
    arguments: &Value,
    settings: &AgentSettings,
    cancel: &CancellationToken,
    external_path_approved: bool,
) -> Result<String, String> {
    match name {
        "rust_clock" => Ok(format!("Local time (unix_millis): {}", crate::now())),
        "rust_shell" => shell_tool::run(arguments, None).await,
        "rust_bash" => shell_tool::run_persistent(state, task_id, arguments, None).await,
        "rust_sandbox_shell" => {
            shell_tool::run_persistent(state, task_id, arguments, Some("sandbox")).await
        }
        "rust_code" => {
            let workspace = workspace_root();
            let arguments = arguments.clone();
            tokio::task::spawn_blocking(move || {
                coding::execute(&workspace, &arguments, external_path_approved)
            })
            .await
            .map_err(|error| format!("Coding tool worker failed: {error}"))?
        }
        "rust_files" => file_tool::run(arguments, external_path_approved).await,
        "rust_sandbox_files" => {
            file_tool::run_sandbox(task_id, arguments, external_path_approved).await
        }
        "rust_str_replace_editor" => {
            editor_tool::run(state, arguments, external_path_approved).await
        }
        "rust_http" => http_tool::run(arguments).await,
        "rust_web_search" => crate::run_web_search_tool(arguments).await,
        "rust_crawl4ai" => crate::run_crawl_tool(arguments).await,
        "rust_browser_use" => crate::run_browser_tool(state, arguments, "browser").await,
        "rust_sandbox_browser" => {
            crate::run_browser_tool(state, arguments, "sandbox_browser").await
        }
        "rust_computer_use" => computer_tool::run(arguments, external_path_approved).await,
        "rust_python_execute" => runtime_tool::run_python(arguments).await,
        "rust_planning" => planning_tool::run(state, task_id, arguments).await,
        "rust_mcp" => mcp_tool::run(state, arguments).await,
        "rust_create_chat_completion" => {
            crate::run_chat_completion_tool(state, task_id, arguments, settings, cancel).await
        }
        "rust_visualization_preparation" => {
            data_tool::run_visualization_preparation(arguments, external_path_approved)
        }
        "rust_data_analysis" => data_tool::run_data_analysis_tool(arguments).await,
        "rust_data_visualization" => data_tool::run_data_visualization_tool(arguments).await,
        "rust_sandbox_vision" => runtime_tool::run_sandbox_vision(task_id, arguments).await,
        "rust_terminate" => {
            let status =
                string_argument(arguments, "status").unwrap_or_else(|| "success".to_string());
            let message = string_argument(arguments, "message")
                .unwrap_or_else(|| "Agent terminated.".to_string());
            Ok(format!("terminated: {status}\n{message}"))
        }
        "rust_ask_human" => Ok("The user approval dialog was completed.".to_string()),
        _ if name.starts_with("rust_mcp_") => mcp_tool::run_dynamic(state, name, arguments).await,
        _ => Err(format!("Unknown tool: {name}")),
    }
}
