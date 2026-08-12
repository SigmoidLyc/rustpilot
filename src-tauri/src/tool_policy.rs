use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

use crate::{coding, path_guard, ApprovalMode, ApprovalRule};

pub(crate) const MAX_APPROVAL_RULES: usize = 256;
const MAX_RULE_WORKSPACE_CHARS: usize = 512;
const MAX_RULE_ACTION_CHARS: usize = 96;
const MAX_RULE_RESOURCE_CHARS: usize = 512;

pub(crate) fn sanitize_rules(rules: Vec<ApprovalRule>) -> Vec<ApprovalRule> {
    let current_workspace = workspace_key_for(&crate::workspace_root());
    let mut sanitized = Vec::with_capacity(rules.len().min(MAX_APPROVAL_RULES));
    let first_rule = rules.len().saturating_sub(MAX_APPROVAL_RULES);
    for mut rule in rules.into_iter().skip(first_rule) {
        if rule.workspace.trim().is_empty() {
            rule.workspace = current_workspace.clone();
        } else {
            rule.workspace = normalize_resource(&rule.workspace);
        }
        rule.action = rule.action.trim().to_string();
        rule.resource = rule.resource.trim().to_string();
        if rule.workspace.is_empty()
            || rule.workspace.chars().count() > MAX_RULE_WORKSPACE_CHARS
            || rule.action.is_empty()
            || rule.resource.is_empty()
            || rule.action.chars().count() > MAX_RULE_ACTION_CHARS
            || rule.resource.chars().count() > MAX_RULE_RESOURCE_CHARS
        {
            continue;
        }
        if sanitized
            .iter()
            .any(|existing: &ApprovalRule| existing == &rule)
        {
            continue;
        }
        if sanitized.len() == MAX_APPROVAL_RULES {
            sanitized.remove(0);
        }
        sanitized.push(rule);
    }
    sanitized
}

pub(crate) fn needs_approval(
    mode: ApprovalMode,
    rules: &[ApprovalRule],
    tool_name: &str,
    arguments: &Value,
) -> bool {
    let risky = is_high_risk(tool_name, arguments);
    let network = mode == ApprovalMode::Confirm && uses_external_service(tool_name, arguments);
    if !risky && !network {
        return false;
    }

    if rule_for(tool_name, arguments).is_some_and(|rule| {
        rules.iter().rev().any(|saved| {
            rule_matches(saved, &rule)
                && (!is_shell_action(tool_name) || shell_rule_is_usable(saved, arguments))
        })
    }) {
        return false;
    }

    mode != ApprovalMode::Guarded || !guarded_auto_allow(tool_name, arguments)
}

pub(crate) fn external_path_requested(tool_name: &str, arguments: &Value) -> bool {
    let workspace = crate::workspace_root_for_arguments(arguments);
    mutation_paths(tool_name, arguments).iter().any(|path| {
        path_guard::resolve_mutation_path(&workspace, path, true)
            .map(|resolved| resolved.scope == path_guard::PathScope::External)
            .unwrap_or(false)
    })
}

pub(crate) fn rule_for(tool_name: &str, arguments: &Value) -> Option<ApprovalRule> {
    let workspace = crate::workspace_root_for_arguments(arguments);
    let action = approval_action(tool_name, arguments)?;
    let resource = match action {
        "shell" => shell_rule_resource(tool_name, arguments)?,
        "edit" => {
            let paths = mutation_paths(tool_name, arguments);
            if paths.len() != 1 {
                return None;
            }
            canonical_resource(&workspace, &paths[0])?
        }
        "diagnostics" => {
            let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
            path_guard::resolve_scoped_path(&workspace, path)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/")
        }
        _ => return None,
    };
    Some(ApprovalRule {
        workspace: workspace_key_for(&workspace),
        action: action.to_string(),
        resource: normalize_resource(&resource),
    })
}

pub(crate) fn workspace_key() -> String {
    workspace_key_for(&crate::workspace_root())
}

fn workspace_key_for(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    normalize_resource(&canonical.to_string_lossy())
}

fn rule_matches(saved: &ApprovalRule, requested: &ApprovalRule) -> bool {
    saved.workspace == requested.workspace
        && wildcard_match(&requested.action, &saved.action)
        && wildcard_match(&requested.resource, &saved.resource)
}

pub(crate) fn wildcard_match(value: &str, pattern: &str) -> bool {
    let value = normalize_resource(value);
    let pattern = normalize_resource(pattern);
    let value = value.as_bytes();
    let pattern = pattern.as_bytes();
    let mut value_index = 0usize;
    let mut pattern_index = 0usize;
    let mut star_index = None;
    let mut star_value_index = 0usize;

    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?' || pattern[pattern_index] == value[value_index])
        {
            value_index += 1;
            pattern_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star_index = Some(pattern_index);
            star_value_index = value_index;
            pattern_index += 1;
        } else if let Some(star) = star_index {
            pattern_index = star + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

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

fn approval_action(tool_name: &str, arguments: &Value) -> Option<&'static str> {
    match tool_name {
        "rust_shell" | "rust_bash" | "rust_sandbox_shell" => Some("shell"),
        "rust_python_execute" => Some("python"),
        "rust_code" => match arguments.get("operation").and_then(Value::as_str) {
            Some("check" | "diagnostics") => Some("diagnostics"),
            Some("apply_patch" | "patch" | "replace" | "write" | "delete") => Some("edit"),
            _ => None,
        },
        "rust_files" | "rust_str_replace_editor" => Some("edit"),
        "rust_visualization_preparation" => Some("edit"),
        "rust_http" => Some("http"),
        "rust_web_search" | "rust_crawl4ai" => Some("network"),
        "rust_browser_use" | "rust_sandbox_browser" => Some("browser"),
        "rust_computer_use" => Some("desktop"),
        "rust_mcp" => Some("mcp"),
        name if name.starts_with("rust_mcp_") => Some("mcp"),
        _ => None,
    }
}

fn uses_external_service(tool_name: &str, arguments: &Value) -> bool {
    match tool_name {
        "rust_http" | "rust_web_search" | "rust_crawl4ai" => true,
        "rust_browser_use" | "rust_sandbox_browser" => arguments
            .get("action")
            .and_then(Value::as_str)
            .is_none_or(|action| action != "wait"),
        "rust_mcp" => arguments
            .get("action")
            .and_then(Value::as_str)
            .is_none_or(|action| action != "disconnect"),
        name if name.starts_with("rust_mcp_") => true,
        _ => false,
    }
}

fn guarded_auto_allow(tool_name: &str, arguments: &Value) -> bool {
    match tool_name {
        "rust_shell" | "rust_bash" | "rust_sandbox_shell" => {
            shell_cwd_is_local(arguments) && safe_shell_command(arguments)
        }
        "rust_code" => {
            workspace_code_mutation_is_safe(arguments) || workspace_diagnostics_is_safe(arguments)
        }
        "rust_files" => workspace_file_mutation_is_safe(arguments),
        "rust_str_replace_editor" => workspace_editor_mutation_is_safe(arguments),
        "rust_visualization_preparation" => arguments
            .get("output_path")
            .and_then(Value::as_str)
            .is_some_and(|path| workspace_path_is_safe_with(arguments, path)),
        "rust_sandbox_files" => arguments
            .get("operation")
            .and_then(Value::as_str)
            .is_some_and(|operation| matches!(operation, "write" | "delete")),
        "rust_web_search" | "rust_crawl4ai" => true,
        "rust_browser_use" | "rust_sandbox_browser" => arguments
            .get("action")
            .and_then(Value::as_str)
            .is_some_and(|action| {
                matches!(
                    action,
                    "go_to_url"
                        | "go_back"
                        | "back"
                        | "forward"
                        | "refresh"
                        | "web_search"
                        | "extract_content"
                        | "extract"
                        | "get_dropdown_options"
                        | "scroll_down"
                        | "scroll_up"
                        | "scroll_to_text"
                        | "wait"
                        | "switch_tab"
                        | "open_tab"
                        | "close_tab"
                        | "open"
                )
            }),
        _ => false,
    }
}

fn workspace_code_mutation_is_safe(arguments: &Value) -> bool {
    let Some(operation) = arguments.get("operation").and_then(Value::as_str) else {
        return false;
    };
    if !matches!(
        operation,
        "apply_patch" | "patch" | "replace" | "write" | "delete"
    ) {
        return false;
    }
    if matches!(operation, "apply_patch" | "patch") {
        let Some(patch) = arguments.get("patch").and_then(Value::as_str) else {
            return false;
        };
        let paths = patch_paths(patch);
        return !paths.is_empty()
            && paths
                .iter()
                .all(|path| workspace_path_is_safe_with(arguments, path));
    }
    arguments
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|path| workspace_path_is_safe_with(arguments, path))
}

fn workspace_file_mutation_is_safe(arguments: &Value) -> bool {
    let operation = arguments.get("operation").and_then(Value::as_str);
    matches!(operation, Some("write" | "delete"))
        && arguments
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| workspace_path_is_safe_with(arguments, path))
}

fn workspace_editor_mutation_is_safe(arguments: &Value) -> bool {
    let command = arguments.get("command").and_then(Value::as_str);
    matches!(
        command,
        Some("create" | "str_replace" | "insert" | "undo_edit")
    ) && arguments
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|path| workspace_path_is_safe_with(arguments, path))
}

fn workspace_path_is_safe_with(arguments: &Value, raw: &str) -> bool {
    let workspace = crate::workspace_root_for_arguments(arguments);
    let Ok(resolved) = path_guard::resolve_mutation_path(&workspace, raw, false) else {
        return false;
    };
    resolved.scope == path_guard::PathScope::Workspace && !sensitive_path(&resolved.canonical)
}

fn workspace_diagnostics_is_safe(arguments: &Value) -> bool {
    if !matches!(
        arguments.get("operation").and_then(Value::as_str),
        Some("check" | "diagnostics")
    ) {
        return false;
    }
    let path = arguments.get("path").and_then(Value::as_str).unwrap_or(".");
    path_guard::resolve_scoped_path(&crate::workspace_root_for_arguments(arguments), path).is_ok()
}

fn sensitive_path(path: &Path) -> bool {
    let mut inside_git = false;
    for component in path.components() {
        if component == Component::Normal(".git".as_ref()) {
            inside_git = true;
            break;
        }
    }
    if inside_git {
        return true;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    if lower == ".env.example" {
        return false;
    }
    lower == ".env"
        || lower.starts_with(".env.")
        || lower == "id_rsa"
        || lower == "credentials"
        || lower == "credentials.json"
        || lower == "secrets.json"
        || lower.ends_with(".pem")
        || lower.ends_with(".p12")
        || lower.ends_with(".pfx")
        || lower.ends_with(".key")
        || lower.contains("password")
        || lower.contains("secret")
        || lower.contains("credential")
}

fn shell_cwd_is_local(arguments: &Value) -> bool {
    let workspace = crate::workspace_root_for_arguments(arguments);
    arguments
        .get("cwd")
        .and_then(Value::as_str)
        .map(|cwd| path_guard::resolve_scoped_path(&workspace, cwd).is_ok())
        .unwrap_or(true)
}

fn is_shell_action(tool_name: &str) -> bool {
    matches!(tool_name, "rust_shell" | "rust_bash" | "rust_sandbox_shell")
}

fn shell_rule_is_usable(saved: &ApprovalRule, arguments: &Value) -> bool {
    let saved_resource = normalize_resource(&saved.resource);
    safe_shell_command(arguments)
        || (!saved_resource.contains('*') && !saved_resource.contains('?'))
}

fn safe_shell_command(arguments: &Value) -> bool {
    let Some(command) = arguments.get("command").and_then(Value::as_str) else {
        return false;
    };
    let Some(tokens) = tokenize_shell_command(command) else {
        return false;
    };
    let Some(first) = tokens.first() else {
        return false;
    };
    if tokens
        .iter()
        .any(|token| unsafe_shell_token(token, arguments))
    {
        return false;
    }
    let executable = Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first)
        .to_ascii_lowercase()
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd")
        .to_string();
    match executable.as_str() {
        "pwd" | "dir" | "ls" | "tree" | "type" | "cat" | "head" | "tail" | "more" | "rg"
        | "grep" | "where" | "which" | "echo" | "printf" | "whoami" | "hostname" | "uname"
        | "ver" | "date" => true,
        "find" => !tokens.iter().any(|token| {
            matches!(
                token.as_str(),
                "-exec"
                    | "-execdir"
                    | "-delete"
                    | "-ok"
                    | "-okdir"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
                    | "-fls"
            )
        }),
        "cd" => tokens
            .get(1)
            .map(|path| {
                path_guard::resolve_scoped_path(
                    &crate::workspace_root_for_arguments(arguments),
                    path,
                )
                .is_ok()
            })
            .unwrap_or(true),
        "git" => safe_git_command(&tokens),
        "cargo" => safe_cargo_command(&tokens),
        "npm" | "pnpm" | "yarn" => safe_package_manager_command(&tokens),
        "node" => safe_node_command(&tokens),
        "python" | "python3" | "py" => safe_python_command(&tokens),
        "rustc" => {
            tokens.len() == 1
                || tokens[1..]
                    .iter()
                    .all(|token| matches!(token.as_str(), "--version" | "-V" | "-v" | "version"))
        }
        _ => false,
    }
}

fn safe_git_command(tokens: &[String]) -> bool {
    let Some(subcommand) = tokens.get(1).map(|value| value.as_str()) else {
        return true;
    };
    let read_only = matches!(
        subcommand,
        "status"
            | "diff"
            | "log"
            | "show"
            | "ls-files"
            | "ls-tree"
            | "rev-parse"
            | "describe"
            | "grep"
    );
    if read_only {
        return !tokens.iter().any(|token| {
            matches!(
                token.as_str(),
                "-D" | "-d" | "--delete" | "--output" | "--exec" | "--upload-pack" | "--ext-diff"
            ) || token.starts_with("--output=")
                || token.starts_with("--exec=")
                || token.starts_with("--upload-pack=")
                || token == "-c"
                || token.starts_with("-c")
        });
    }
    if subcommand != "branch" {
        return false;
    }
    tokens[2..].iter().all(|token| {
        matches!(
            token.as_str(),
            "--list"
                | "-l"
                | "--show-current"
                | "--all"
                | "-a"
                | "--remotes"
                | "-r"
                | "--verbose"
                | "-v"
                | "--no-color"
                | "--color=never"
        ) || token.starts_with("--format=")
            || token.starts_with("--sort=")
            || token.starts_with("--contains=")
            || token.starts_with("--no-contains=")
            || token.starts_with("--merged=")
            || token.starts_with("--no-merged=")
    })
}

fn safe_cargo_command(tokens: &[String]) -> bool {
    let Some(subcommand) = tokens.get(1).map(|value| value.as_str()) else {
        return true;
    };
    let arguments = &tokens[2..];
    match subcommand {
        "locate-project" => arguments.iter().all(|argument| {
            matches!(
                argument.as_str(),
                "--workspace" | "--message-format=plain" | "--message-format=json"
            )
        }),
        "version" | "--version" => arguments.is_empty(),
        "metadata" => {
            arguments.iter().any(|argument| argument == "--offline")
                && safe_cargo_arguments(arguments, CargoCommand::Metadata)
        }
        "check" | "test" | "clippy" | "build" | "doc" => {
            arguments.iter().any(|argument| argument == "--offline")
                && safe_cargo_arguments(arguments, CargoCommand::Diagnostic)
        }
        "fmt" => {
            arguments.iter().any(|argument| argument == "--check")
                && safe_cargo_arguments(arguments, CargoCommand::Format)
        }
        "tree" => {
            arguments.iter().any(|argument| argument == "--offline")
                && safe_cargo_arguments(arguments, CargoCommand::Tree)
        }
        _ => false,
    }
}

#[derive(Clone, Copy)]
enum CargoCommand {
    Metadata,
    Diagnostic,
    Format,
    Tree,
}

fn safe_cargo_arguments(arguments: &[String], command: CargoCommand) -> bool {
    let mut index = 0usize;
    while let Some(argument) = arguments.get(index) {
        let value = argument.as_str();
        let flag_is_allowed = match command {
            CargoCommand::Metadata => {
                matches!(
                    value,
                    "--offline" | "--no-deps" | "--quiet" | "-q" | "--locked" | "--frozen"
                ) || value.starts_with("--format-version=")
            }
            CargoCommand::Diagnostic => {
                matches!(
                    value,
                    "--offline"
                        | "--quiet"
                        | "-q"
                        | "--locked"
                        | "--frozen"
                        | "--workspace"
                        | "--all-targets"
                        | "--all-features"
                        | "--no-default-features"
                        | "--lib"
                        | "--bins"
                        | "--tests"
                        | "--benches"
                        | "--examples"
                        | "--doc"
                        | "--release"
                ) || value.starts_with("--message-format=")
            }
            CargoCommand::Format => matches!(value, "--check" | "--all" | "--quiet" | "-q"),
            CargoCommand::Tree => {
                matches!(
                    value,
                    "--offline"
                        | "--quiet"
                        | "-q"
                        | "--locked"
                        | "--frozen"
                        | "--workspace"
                        | "--all-features"
                        | "--no-default-features"
                ) || value.starts_with("--depth=")
            }
        };
        if flag_is_allowed {
            index += 1;
            continue;
        }
        if value == "--manifest-path" {
            let Some(path) = arguments.get(index + 1) else {
                return false;
            };
            if !safe_workspace_path_token(path) {
                return false;
            }
            index += 2;
            continue;
        }
        if let Some(path) = value.strip_prefix("--manifest-path=") {
            if !safe_workspace_path_token(path) {
                return false;
            }
            index += 1;
            continue;
        }
        return false;
    }
    true
}

fn safe_workspace_path_token(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).is_absolute()
        && !Path::new(path)
            .components()
            .any(|component| component == Component::ParentDir)
}

fn safe_package_manager_command(tokens: &[String]) -> bool {
    let Some(executable) = tokens.first() else {
        return false;
    };
    let arguments = &tokens[1..];
    if arguments
        .iter()
        .all(|argument| matches!(argument.as_str(), "--version" | "-v" | "version"))
    {
        return true;
    }
    let script = match executable
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd")
        .to_ascii_lowercase()
        .as_str()
    {
        "npm" | "pnpm" | "yarn" => match arguments {
            [command] if command == "test" => Some("test"),
            [command, script] if command == "run" || command == "run-script" => {
                Some(script.as_str())
            }
            _ => None,
        },
        _ => None,
    };
    script.is_some_and(safe_package_script)
}

fn safe_package_script(script: &str) -> bool {
    matches!(
        script,
        "check" | "test" | "lint" | "typecheck" | "build" | "dev"
    )
}

fn safe_node_command(tokens: &[String]) -> bool {
    matches!(tokens, [_])
        || tokens[1..]
            .iter()
            .all(|token| matches!(token.as_str(), "--version" | "-v" | "version"))
        || matches!(tokens, [_, flag, path] if flag == "--check" && safe_workspace_path_token(path))
}

fn safe_python_command(tokens: &[String]) -> bool {
    if tokens.len() == 1
        || tokens[1..]
            .iter()
            .all(|token| matches!(token.as_str(), "--version" | "-V" | "-v" | "version"))
    {
        return true;
    }
    matches!(
        tokens,
        [_, flag, module]
            if flag == "-m"
                && matches!(module.as_str(), "pytest" | "unittest" | "compileall" | "py_compile" | "ruff" | "mypy" | "pylint")
    )
}

fn unsafe_shell_token(token: &str, arguments: &Value) -> bool {
    let lower = token.to_ascii_lowercase();
    lower.contains("http://")
        || lower.contains("https://")
        || token.contains('$')
        || token.contains('%')
        || token.contains('!')
        || lower.starts_with('~')
        || lower.starts_with("$home")
        || PathBuf::from(token)
            .components()
            .any(|component| component == Component::ParentDir)
        || sensitive_name(token)
        || (PathBuf::from(token).is_absolute()
            && path_guard::resolve_scoped_path(
                &crate::workspace_root_for_arguments(arguments),
                token,
            )
            .is_err())
}

fn sensitive_name(token: &str) -> bool {
    let name = token
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(token)
        .to_ascii_lowercase();
    name == ".env"
        || name.starts_with(".env.") && name != ".env.example"
        || name == "credentials"
        || name == "credentials.json"
        || name == "secrets.json"
        || name == "id_rsa"
        || name.contains("password")
        || name.contains("secret")
        || name.contains("credential")
        || name.ends_with(".pem")
        || name.ends_with(".key")
}

fn tokenize_shell_command(command: &str) -> Option<Vec<String>> {
    if command.contains("$(`") || command.contains("$(") || command.contains('`') {
        return None;
    }
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for character in command.chars() {
        if let Some(active) = quote {
            if character == active {
                quote = None;
            } else {
                current.push(character);
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            ' ' | '\t' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '&' | '|' | ';' | '<' | '>' | '\r' | '\n' => return None,
            _ => current.push(character),
        }
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    Some(tokens)
}

fn mutation_paths(tool_name: &str, arguments: &Value) -> Vec<String> {
    match tool_name {
        "rust_files"
            if matches!(
                arguments.get("operation").and_then(Value::as_str),
                Some("write" | "delete")
            ) =>
        {
            arguments
                .get("path")
                .and_then(Value::as_str)
                .map(|path| vec![path.to_string()])
                .unwrap_or_default()
        }
        "rust_code"
            if matches!(
                arguments.get("operation").and_then(Value::as_str),
                Some("replace" | "write" | "delete")
            ) =>
        {
            arguments
                .get("path")
                .and_then(Value::as_str)
                .map(|path| vec![path.to_string()])
                .unwrap_or_default()
        }
        "rust_code"
            if matches!(
                arguments.get("operation").and_then(Value::as_str),
                Some("apply_patch" | "patch")
            ) =>
        {
            arguments
                .get("patch")
                .and_then(Value::as_str)
                .map(patch_paths)
                .unwrap_or_default()
        }
        "rust_str_replace_editor"
            if matches!(
                arguments.get("command").and_then(Value::as_str),
                Some("create" | "str_replace" | "insert" | "undo_edit")
            ) =>
        {
            arguments
                .get("path")
                .and_then(Value::as_str)
                .map(|path| vec![path.to_string()])
                .unwrap_or_default()
        }
        "rust_visualization_preparation" => arguments
            .get("output_path")
            .and_then(Value::as_str)
            .map(|path| vec![path.to_string()])
            .unwrap_or_default(),
        "rust_computer_use"
            if arguments.get("action").and_then(Value::as_str) == Some("screenshot") =>
        {
            arguments
                .get("path")
                .and_then(Value::as_str)
                .map(|path| vec![path.to_string()])
                .unwrap_or_default()
        }
        _ => Vec::new(),
    }
}

fn patch_paths(patch: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in patch.lines() {
        let raw = line
            .strip_prefix("*** Add File: ")
            .or_else(|| line.strip_prefix("*** Delete File: "))
            .or_else(|| line.strip_prefix("*** Update File: "))
            .or_else(|| line.strip_prefix("*** Move to: "))
            .or_else(|| line.strip_prefix("--- "))
            .or_else(|| line.strip_prefix("+++ "));
        let Some(raw) = raw else { continue };
        let path = raw
            .split('\t')
            .next()
            .unwrap_or(raw)
            .trim()
            .trim_matches('"');
        if path.is_empty() || path == "/dev/null" {
            continue;
        }
        let path = path
            .strip_prefix("a/")
            .or_else(|| path.strip_prefix("b/"))
            .unwrap_or(path);
        if !paths.iter().any(|existing| existing == path) {
            paths.push(path.to_string());
        }
    }
    paths
}

fn canonical_resource(workspace: &Path, raw: &str) -> Option<String> {
    path_guard::resolve_mutation_path(workspace, raw, true)
        .ok()
        .map(|resolved| resolved.canonical.to_string_lossy().replace('\\', "/"))
}

fn shell_rule_resource(tool_name: &str, arguments: &Value) -> Option<String> {
    let command = arguments.get("command").and_then(Value::as_str)?;
    let pattern = if safe_shell_command(arguments) {
        shell_rule_pattern(command)?
    } else {
        let exact = normalize_resource(command);
        if exact.contains('*') || exact.contains('?') {
            return None;
        }
        exact
    };
    if tool_name == "rust_sandbox_shell" {
        return Some(pattern);
    }
    let cwd = canonical_shell_cwd(arguments)?;
    Some(format!("cwd:{cwd}|command:{pattern}"))
}

fn shell_rule_pattern(command: &str) -> Option<String> {
    let tokens = tokenize_shell_command(command)?;
    let first = tokens.first()?;
    let executable = Path::new(first)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(first)
        .trim_end_matches(".exe")
        .trim_end_matches(".cmd")
        .to_string();
    let arity = match executable.to_ascii_lowercase().as_str() {
        "git" | "cargo" | "npm" | "pnpm" | "yarn" | "bun" | "go" | "docker" => 2,
        _ => 1,
    };
    let prefix = tokens
        .iter()
        .take(arity.min(tokens.len()))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!("{} *", normalize_resource(&prefix)))
}

fn canonical_shell_cwd(arguments: &Value) -> Option<String> {
    let root = crate::workspace_root_for_arguments(arguments);
    let raw = arguments.get("cwd").and_then(Value::as_str).unwrap_or(".");
    let requested = PathBuf::from(raw);
    let candidate = if requested.is_absolute() {
        requested
    } else {
        root.join(requested)
    };
    let canonical = candidate.canonicalize().ok()?;
    canonical
        .is_dir()
        .then(|| normalize_resource(&canonical.to_string_lossy()))
}

fn normalize_resource(resource: &str) -> String {
    let mut normalized = resource.trim().replace('\\', "/");
    #[cfg(windows)]
    {
        normalized.make_ascii_lowercase();
        if let Some(without_extended_prefix) = normalized.strip_prefix("//?/") {
            normalized = without_extended_prefix.to_string();
        }
    }
    normalized
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
    let workspace = crate::workspace_root_for_arguments(arguments);
    if let Some(path) = mutation_path_argument(tool_name, arguments) {
        let resolution = match path_guard::resolve_mutation_path(&workspace, &path, true) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(action: &str, resource: &str) -> ApprovalRule {
        ApprovalRule {
            workspace: workspace_key(),
            action: action.to_string(),
            resource: resource.to_string(),
        }
    }

    #[test]
    fn wildcard_matching_supports_question_and_star() {
        assert!(wildcard_match("git status --short", "git status *"));
        assert!(wildcard_match("src/main.rs", "src/????.rs"));
        assert!(!wildcard_match("src/main.rs", "src/*.ts"));
    }

    #[test]
    fn guarded_mode_allows_safe_workspace_edits_but_keeps_risky_commands_prompted() {
        assert!(!needs_approval(
            ApprovalMode::Guarded,
            &[],
            "rust_files",
            &json!({"operation": "write", "path": "target/rustpilot-approval-test.txt"})
        ));
        assert!(needs_approval(
            ApprovalMode::Guarded,
            &[],
            "rust_shell",
            &json!({"command": "Remove-Item important.txt"})
        ));
        assert!(needs_approval(
            ApprovalMode::Guarded,
            &[],
            "rust_python_execute",
            &json!({"code": "print(1)"})
        ));
    }

    #[test]
    fn guarded_mode_allows_bounded_local_checks_but_not_mutating_shell_commands() {
        assert!(!needs_approval(
            ApprovalMode::Guarded,
            &[],
            "rust_shell",
            &json!({"command": "cargo check --offline --tests"})
        ));
        assert!(!needs_approval(
            ApprovalMode::Guarded,
            &[],
            "rust_shell",
            &json!({"command": "npm.cmd run check"})
        ));
        assert!(!needs_approval(
            ApprovalMode::Guarded,
            &[],
            "rust_shell",
            &json!({"command": "git branch --show-current"})
        ));
        assert!(needs_approval(
            ApprovalMode::Guarded,
            &[],
            "rust_shell",
            &json!({"command": "git branch feature/new-work"})
        ));
        assert!(needs_approval(
            ApprovalMode::Guarded,
            &[],
            "rust_shell",
            &json!({"command": "npm run prepare"})
        ));
    }

    #[test]
    fn confirm_mode_prompts_for_a_safe_shell_action() {
        assert!(needs_approval(
            ApprovalMode::Confirm,
            &[],
            "rust_shell",
            &json!({"command": "git status"})
        ));
    }

    #[test]
    fn remembered_rule_is_workspace_scoped_and_matches_saved_patterns() {
        let arguments = json!({
            "operation": "write",
            "path": "target/rustpilot-approval-test.txt"
        });
        let saved = rule_for("rust_files", &arguments).expect("write should be rememberable");
        assert!(!needs_approval(
            ApprovalMode::Confirm,
            std::slice::from_ref(&saved),
            "rust_files",
            &arguments
        ));

        let other_workspace = ApprovalRule {
            workspace: "D:/another-project".to_string(),
            action: saved.action.clone(),
            resource: saved.resource.clone(),
        };
        assert!(needs_approval(
            ApprovalMode::Confirm,
            &[other_workspace],
            "rust_files",
            &arguments
        ));
    }

    #[test]
    fn saved_shell_patterns_never_broaden_an_unsafe_command() {
        let safe_rule = rule(
            "shell",
            &format!("cwd:{}|command:git status *", workspace_key()),
        );
        assert!(needs_approval(
            ApprovalMode::Guarded,
            &[safe_rule],
            "rust_shell",
            &json!({"command": "git status --output=outside.txt"})
        ));

        let exact = rule(
            "shell",
            &format!("cwd:{}|command:Remove-Item important.txt", workspace_key()),
        );
        assert!(!needs_approval(
            ApprovalMode::Guarded,
            &[exact],
            "rust_shell",
            &json!({"command": "Remove-Item important.txt"})
        ));
    }

    #[test]
    fn last_matching_rule_wins() {
        let requested = rule("edit", "D:/RustPilot/src/main.rs");
        let broad = rule("edit", "*");
        let exact = rule("edit", "D:/RustPilot/src/main.rs");
        let candidates = [broad, exact.clone()];
        let selected = candidates
            .iter()
            .rev()
            .find(|saved| rule_matches(saved, &requested));
        assert_eq!(selected, Some(&exact));
    }

    #[test]
    fn legacy_rules_without_workspace_are_migrated_to_the_current_workspace() {
        let rules = sanitize_rules(vec![ApprovalRule {
            workspace: String::new(),
            action: "edit".to_string(),
            resource: "*".to_string(),
        }]);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].workspace, workspace_key());
    }

    #[test]
    fn approval_rule_capacity_keeps_the_newest_rule() {
        let rules = (0..=MAX_APPROVAL_RULES)
            .map(|index| rule("edit", &format!("resource-{index}")))
            .collect();
        let sanitized = sanitize_rules(rules);

        assert_eq!(sanitized.len(), MAX_APPROVAL_RULES);
        assert_eq!(sanitized[0].resource, "resource-1");
        assert_eq!(
            sanitized.last().map(|rule| rule.resource.as_str()),
            Some("resource-256")
        );
    }

    #[test]
    fn persistent_shell_policy_uses_an_external_effective_cwd() {
        let external_cwd = std::env::temp_dir();
        assert!(needs_approval(
            ApprovalMode::Guarded,
            &[],
            "rust_bash",
            &json!({
                "command": "dir",
                "cwd": external_cwd.to_string_lossy()
            })
        ));
    }
}
