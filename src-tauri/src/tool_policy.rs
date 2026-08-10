use serde_json::{json, Value};

use crate::{coding, path_guard};

pub(crate) fn is_high_risk(tool_name: &str, arguments: &Value) -> bool {
    match tool_name {
        "rust_shell"
        | "rust_bash"
        | "rust_sandbox_shell"
        | "rust_python_execute"
        | "rust_ask_human" => true,
        "rust_computer_use" => {
            arguments
                .get("action")
                .and_then(Value::as_str)
                .is_some_and(|action| {
                    matches!(action, "move_to" | "click" | "scroll" | "type" | "press")
                        || (action == "screenshot"
                            && arguments
                                .get("path")
                                .and_then(Value::as_str)
                                .is_some_and(|path| !path.trim().is_empty()))
                })
        }
        "rust_mcp" => arguments
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|action| {
                matches!(action, "connect" | "call_tool")
                    || (action == "list_tools"
                        && arguments
                            .get("transport")
                            .and_then(Value::as_str)
                            .is_some_and(|transport| transport.eq_ignore_ascii_case("stdio")))
            }),
        "rust_files" | "rust_sandbox_files" => arguments
            .get("operation")
            .and_then(Value::as_str)
            .is_some_and(|operation| matches!(operation, "write" | "delete")),
        "rust_code" => {
            coding::is_mutation(arguments)
                || arguments.get("operation").and_then(Value::as_str) == Some("check")
                || arguments.get("operation").and_then(Value::as_str) == Some("diagnostics")
        }
        "rust_str_replace_editor" => arguments
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| {
                matches!(command, "create" | "str_replace" | "insert" | "undo_edit")
            }),
        "rust_http" => arguments
            .get("method")
            .and_then(Value::as_str)
            .map(|method| !method.eq_ignore_ascii_case("GET"))
            .unwrap_or(false),
        "rust_visualization_preparation" => arguments
            .get("output_path")
            .and_then(Value::as_str)
            .is_some_and(|path| !path.trim().is_empty()),
        "rust_browser_use" | "rust_sandbox_browser" => arguments
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|action| {
                matches!(
                    action,
                    "click"
                        | "type"
                        | "click_element"
                        | "input_text"
                        | "select_dropdown_option"
                        | "send_keys"
                )
            }),
        name if name.starts_with("rust_mcp_") => true,
        _ => false,
    }
}

pub(crate) fn approval_reason(tool_name: &str, arguments: &Value) -> String {
    match tool_name {
        "rust_shell" | "rust_bash" | "rust_sandbox_shell" => {
            "Shell commands can change system state or run external programs.".to_string()
        }
        "rust_python_execute" => {
            "Python can execute arbitrary local code and access the filesystem.".to_string()
        }
        "rust_code"
            if matches!(
                arguments.get("operation").and_then(Value::as_str),
                Some("check" | "diagnostics")
            ) =>
        {
            "Project diagnostics may execute build scripts or package checks.".to_string()
        }
        "rust_code" | "rust_str_replace_editor" | "rust_files" | "rust_sandbox_files" => {
            "This operation changes files on the computer.".to_string()
        }
        "rust_visualization_preparation" => {
            "This operation writes a visualization specification to a local path.".to_string()
        }
        "rust_computer_use" => {
            "Desktop input can click, type, or control another application.".to_string()
        }
        "rust_http" => "This HTTP method may modify a remote service.".to_string(),
        "rust_browser_use" | "rust_sandbox_browser" => {
            "This browser action may submit or modify a web page.".to_string()
        }
        "rust_mcp" => "The connected MCP server may perform an external operation.".to_string(),
        _ => "This tool operation requires explicit user approval.".to_string(),
    }
}

pub(crate) fn approval_details(tool_name: &str, arguments: &Value) -> String {
    let mut details = arguments.clone();
    if let Some(path) = mutation_path_argument(tool_name, arguments) {
        let resolution = match path_guard::resolve_mutation_path(
            &crate::workspace_root(),
            &path,
            true,
        ) {
            Ok(resolved) => json!({
                "requested": path,
                "resolved": resolved.canonical.display().to_string(),
                "scope": resolved.scope.as_str(),
                "exists": resolved.existed,
                "approval": "This approval applies to this exact resolved path."
            }),
            Err(error) => json!({
                "requested": path,
                "error": error,
                "approval": "The operation will be rejected if the path cannot be resolved safely."
            }),
        };
        if let Some(object) = details.as_object_mut() {
            object.insert("_rustpilot_path_authorization".to_string(), resolution);
        }
    }
    serde_json::to_string_pretty(&details).unwrap_or_else(|_| "{}".to_string())
}

fn mutation_path_argument(tool_name: &str, arguments: &Value) -> Option<String> {
    let operation = arguments.get("operation").and_then(Value::as_str);
    let command = arguments.get("command").and_then(Value::as_str);
    let action = arguments.get("action").and_then(Value::as_str);
    match tool_name {
        "rust_files" if matches!(operation, Some("write" | "delete")) => {
            Some(crate::string_argument(arguments, "path").unwrap_or_else(|| ".".to_string()))
        }
        "rust_code" if matches!(operation, Some("replace" | "write" | "delete")) => {
            crate::string_argument(arguments, "path")
        }
        "rust_str_replace_editor"
            if matches!(
                command,
                Some("create" | "str_replace" | "insert" | "undo_edit")
            ) =>
        {
            crate::string_argument(arguments, "path")
        }
        "rust_visualization_preparation" => crate::string_argument(arguments, "output_path"),
        "rust_computer_use" if action == Some("screenshot") => {
            crate::string_argument(arguments, "path")
        }
        _ => None,
    }
}
