use serde_json::{json, Value};

use crate::tool;

pub(crate) fn tool_definitions() -> Vec<Value> {
    fn function(name: &str, description: &str, parameters: Value) -> Value {
        tool::ToolDefinition::new(name, description, parameters).to_param()
    }

    vec![
        function(
            "rust_clock",
            "Read the local machine time.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        function(
            "rust_code",
            "Bounded repository coding operations: read files with line numbers, list directories, glob and literal-grep source files, inspect git status or diff, run bounded offline Cargo/npm diagnostics, and apply precise edits. Read operations stay inside the workspace; sensitive mutations and project checks may require approval and use the existing path guard.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "operation": {"type": "string", "enum": ["read", "list", "glob", "grep", "status", "diff", "check", "diagnostics", "apply_patch", "patch", "replace", "write", "delete"]},
                    "path": {"type": "string", "description": "Relative path inside the active workspace."},
                    "pattern": {"type": "string", "description": "Glob pattern or literal grep query."},
                    "glob": {"type": "string", "description": "Optional file glob filter for grep."},
                    "line_start": {"type": "integer", "minimum": 1},
                    "line_end": {"type": "integer", "minimum": 1},
                    "line_numbers": {"type": "boolean"},
                    "case_sensitive": {"type": "boolean"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 300},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 200},
                    "staged": {"type": "boolean"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring exactly one match."},
                    "backend": {"type": "string", "enum": ["auto", "cargo", "npm"], "description": "Diagnostics backend for check; auto selects the nearest supported project manifest."},
                    "offline": {"type": "boolean", "description": "Keep Cargo diagnostics offline (defaults to true)."},
                    "timeout": {"type": "integer", "minimum": 5, "maximum": 90},
                    "content": {"type": "string"},
                    "patch": {"type": "string", "description": "OpenAI apply_patch or unified diff text."}
                },
                "required": ["operation"]
            }),
        ),
        function(
            "rust_files",
            "List, read, inspect, write, or delete local files. Mutating operations require approval.",
            json!({
                "type": "object",
                "properties": {
                    "operation": {"type": "string", "enum": ["list", "read", "write", "delete", "exists"]},
                    "path": {"type": "string", "description": "A relative or absolute path inside the active workspace. Paths cannot escape through .. or links."},
                    "content": {"type": "string"}
                },
                "required": ["operation"]
            }),
        ),
        function(
            "rust_http",
            "Make a bounded HTTP request. GET is read-only; other methods require approval.",
            json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"]},
                    "headers": {"type": "object"},
                    "body": {"type": "string"}
                },
                "required": ["url"]
            }),
        ),
        function(
            "rust_shell",
            "Run a local shell command. Safe inspection commands may run automatically in guarded mode; unsafe commands require approval.",
            json!({
                "type": "object",
                "properties": {"command": {"type": "string"}, "cwd": {"type": "string"}},
                "required": ["command"]
            }),
        ),
        function(
            "rust_bash",
            "Run a persistent named shell session with a remembered working directory. Unsafe commands require approval.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "session_id": {"type": "string"},
                    "cwd": {"type": "string"},
                    "restart": {"type": "boolean"}
                },
                "required": ["command"]
            }),
        ),
        function(
            "rust_str_replace_editor",
            "View, create, replace, insert, and undo edits in files. Sensitive mutations require approval; guarded mode can allow safe workspace edits.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["view", "create", "str_replace", "insert", "undo_edit"]},
                    "path": {"type": "string", "description": "A relative or absolute path inside the active workspace."},
                    "file_text": {"type": "string"},
                    "old_str": {"type": "string"},
                    "new_str": {"type": "string"},
                    "insert_line": {"type": "integer"},
                    "view_range": {"type": "array", "items": {"type": "integer"}}
                },
                "required": ["command", "path"]
            }),
        ),
        function(
            "rust_planning",
            "Create and manage a durable plan with step statuses and notes.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["create", "update", "list", "get", "set_active", "mark_step", "delete"]},
                    "plan_id": {"type": "string"},
                    "title": {"type": "string"},
                    "steps": {"type": "array", "items": {"type": "string"}},
                    "step_index": {"type": "integer"},
                    "step_status": {"type": "string", "enum": ["not_started", "in_progress", "completed", "blocked"]},
                    "step_notes": {"type": "string"}
                },
                "required": ["command"]
            }),
        ),
        function(
            "rust_terminate",
            "End the current agent run with a success or failure message.",
            json!({
                "type": "object",
                "properties": {
                    "status": {"type": "string", "enum": ["success", "failure"]},
                    "message": {"type": "string"}
                },
                "required": ["status", "message"]
            }),
        ),
        function(
            "rust_ask_human",
            "Ask the desktop user for a blocking approval or decision.",
            json!({
                "type": "object",
                "properties": {"question": {"type": "string"}, "options": {"type": "array", "items": {"type": "string"}}},
                "required": ["question"]
            }),
        ),
        function(
            "rust_python_execute",
            "Execute a bounded Python snippet and return stdout/stderr. Approval required.",
            json!({
                "type": "object",
                "properties": {"code": {"type": "string"}, "timeout": {"type": "integer"}},
                "required": ["code"]
            }),
        ),
        function(
            "rust_web_search",
            "Search the public web and return titles, URLs, snippets, and optional page text.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "num_results": {"type": "integer"},
                    "fetch_content": {"type": "boolean"},
                    "lang": {"type": "string"}
                },
                "required": ["query"]
            }),
        ),
        function(
            "rust_crawl4ai",
            "Fetch one or more pages and extract clean, bounded text and link metadata.",
            json!({
                "type": "object",
                "properties": {
                    "urls": {"type": "array", "items": {"type": "string"}},
                    "timeout": {"type": "integer"},
                    "word_count_threshold": {"type": "integer"},
                    "bypass_cache": {"type": "boolean"}
                },
                "required": ["urls"]
            }),
        ),
        function(
            "rust_browser_use",
            "Use a persistent browser session with indexed DOM interaction, extraction, scrolling, tabs, search, and real Chromium screenshots. HTTP DOM mode remains available when Chromium is not present.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["go_to_url", "click_element", "input_text", "scroll_down", "scroll_up", "scroll_to_text", "send_keys", "get_dropdown_options", "select_dropdown_option", "go_back", "web_search", "wait", "extract_content", "switch_tab", "open_tab", "close_tab", "open", "back", "forward", "refresh", "extract", "click", "type", "scroll", "screenshot"]},
                    "url": {"type": "string"},
                    "text": {"type": "string"},
                    "selector": {"type": "string"},
                    "session_id": {"type": "string"},
                    "amount": {"type": "integer"},
                    "scroll_amount": {"type": "integer"},
                    "field": {"type": "string"},
                    "index": {"type": "integer"},
                    "tab_id": {"type": "integer"},
                    "query": {"type": "string"},
                    "goal": {"type": "string"},
                    "keys": {"type": "string"},
                    "seconds": {"type": "integer"}
                },
                "required": ["action"]
            }),
        ),
        function(
            "rust_computer_use",
            "Use the local desktop input surface for cursor, click, scroll, typing, keys, wait, and actual screen capture. Mutating actions require approval.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["move_to", "click", "scroll", "type", "press", "wait", "screenshot"]},
                    "x": {"type": "integer"},
                    "y": {"type": "integer"},
                    "amount": {"type": "integer"},
                    "text": {"type": "string"},
                    "key": {"type": "string"},
                    "duration": {"type": "number"},
                    "path": {"type": "string", "description": "Optional screenshot path. Relative paths stay inside the active workspace; an explicit external path requires approval."},
                    "include_base64": {"type": "boolean"}
                },
                "required": ["action"]
            }),
        ),
        function(
            "rust_sandbox_files",
            "Operate on files inside the RustPilot workspace sandbox. Safe sandbox mutations may run automatically in guarded mode.",
            json!({
                "type": "object",
                "properties": {"operation": {"type": "string", "enum": ["list", "read", "write", "delete", "exists"]}, "path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["operation"]
            }),
        ),
        function(
            "rust_sandbox_shell",
            "Run a command in a persistent RustPilot workspace sandbox shell. Unsafe commands require approval.",
            json!({
                "type": "object",
                "properties": {"command": {"type": "string"}, "session_id": {"type": "string"}, "cwd": {"type": "string"}},
                "required": ["command"]
            }),
        ),
        function(
            "rust_sandbox_browser",
            "Use the browser session scoped to the local sandbox workspace.",
            json!({
                "type": "object",
                "properties": {"action": {"type": "string"}, "url": {"type": "string"}, "text": {"type": "string"}, "selector": {"type": "string"}, "session_id": {"type": "string"}, "amount": {"type": "integer"}},
                "required": ["action"]
            }),
        ),
        function(
            "rust_sandbox_vision",
            "Inspect a local sandbox image and optionally return its image payload for multimodal workflows.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "include_base64": {"type": "boolean"}}, "required": ["path"]}),
        ),
        function(
            "rust_mcp",
            "Connect to MCP servers over HTTP/SSE or persistent stdio, initialize them, discover live tool schemas, refresh them, call tools, and disconnect. Discovered tools are exposed as rust_mcp_<server>_<tool>.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["connect", "list_tools", "call_tool", "disconnect"]},
                    "transport": {"type": "string", "enum": ["http", "sse", "stdio"]},
                    "server_id": {"type": "string"},
                    "url": {"type": "string"},
                    "command": {"type": "string"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "tool_name": {"type": "string"},
                    "arguments": {"type": "object"}
                },
                "required": ["action"]
            }),
        ),
        function(
            "rust_create_chat_completion",
            "Request a non-streaming structured completion from the configured OpenAI-compatible endpoint.",
            json!({
                "type": "object",
                "properties": {"messages": {"type": "array"}, "response_format": {"type": "object"}},
                "required": ["messages"]
            }),
        ),
        function(
            "rust_visualization_preparation",
            "Prepare a compact chart specification from tabular data. An output_path writes a local file and requires explicit approval; relative paths stay inside the active workspace.",
            json!({"type": "object", "properties": {"title": {"type": "string"}, "kind": {"type": "string"}, "labels": {"type": "array"}, "values": {"type": "array"}, "output_path": {"type": "string", "description": "Optional file path. Relative paths must stay inside the active workspace; external absolute paths require explicit approval."}}, "required": ["kind"]}),
        ),
        function(
            "rust_data_visualization",
            "Generate real HTML or PNG charts and optional Markdown insights from CSV/JSON data or a json_path descriptor.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "json_path": {"type": "string"}, "kind": {"type": "string"}, "title": {"type": "string"}, "output_type": {"type": "string", "enum": ["html", "png"]}, "tool_type": {"type": "string", "enum": ["visualization", "insight"]}, "language": {"type": "string", "enum": ["en", "zh"]}}, "required": []}),
        ),
        function(
            "rust_data_analysis",
            "Profile a CSV or JSON file with row counts, columns, numeric summaries, and missing values.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "sample_rows": {"type": "integer"}}, "required": ["path"]}),
        ),
    ]
}
