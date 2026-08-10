use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};
use uuid::Uuid;

use crate::{
    sandbox_root_for_task, string_argument, truncate_output, workspace_root, AppState,
    PersistentShell, MAX_OUTPUT_CHARS,
};

pub(crate) async fn run(arguments: &Value, forced_cwd: Option<&Path>) -> Result<String, String> {
    let command = string_argument(arguments, "command")
        .ok_or_else(|| "rust_shell requires a command string".to_string())?;
    let explicit_cwd = string_argument(arguments, "cwd").map(PathBuf::from);
    let cwd = forced_cwd.or(explicit_cwd.as_deref());
    run_process(&command, cwd).await
}

pub(crate) async fn run_persistent(
    state: &AppState,
    task_id: &str,
    arguments: &Value,
    sandbox_prefix: Option<&str>,
) -> Result<String, String> {
    let command = string_argument(arguments, "command")
        .ok_or_else(|| "rust_bash requires a command string".to_string())?;
    let session_id =
        string_argument(arguments, "session_id").unwrap_or_else(|| "default".to_string());
    let key = match sandbox_prefix {
        Some(prefix) => format!("{prefix}:{task_id}:{session_id}"),
        None => session_id.clone(),
    };
    let restart = arguments
        .get("restart")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let sandbox_root = sandbox_prefix
        .map(|_| sandbox_root_for_task(task_id))
        .transpose()?;
    let initial_cwd = if let Some(raw_cwd) = string_argument(arguments, "cwd") {
        let path = PathBuf::from(raw_cwd);
        if let Some(root) = &sandbox_root {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        } else {
            path
        }
    } else {
        sandbox_root.clone().unwrap_or_else(workspace_root)
    };
    if let Some(root) = &sandbox_root {
        let normalized = if initial_cwd.exists() {
            initial_cwd
                .canonicalize()
                .map_err(|error| format!("Unable to resolve sandbox cwd: {error}"))?
        } else {
            initial_cwd.clone()
        };
        if !normalized.starts_with(root) {
            return Err("Sandbox shell cwd must stay inside the task sandbox.".to_string());
        }
    }
    if !initial_cwd.is_dir() {
        return Err(format!(
            "Shell working directory does not exist: {}",
            initial_cwd.display()
        ));
    }

    let mut sessions = state.shell_sessions.lock().await;
    if restart {
        sessions.remove(&key);
    }
    let should_spawn = match sessions.get_mut(&key) {
        Some(shell) => shell
            .child
            .try_wait()
            .map_err(|error| format!("Unable to inspect shell session: {error}"))?
            .is_some(),
        None => true,
    };
    if should_spawn {
        sessions.insert(key.clone(), spawn(&initial_cwd).await?);
    }
    let shell = sessions
        .get_mut(&key)
        .ok_or_else(|| "Persistent shell session was not created.".to_string())?;
    let sentinel = format!("__RUSTPILOT_DONE_{}__", Uuid::new_v4().simple());
    #[cfg(target_os = "windows")]
    let payload = format!("{command} 2>&1\r\necho {sentinel}:%errorlevel%:%CD%\r\n");
    #[cfg(not(target_os = "windows"))]
    let payload = format!("{{ {command}; }} 2>&1\nprintf '{sentinel}:%s:%s\\n' \"$?\" \"$PWD\"\n");
    shell
        .stdin
        .write_all(payload.as_bytes())
        .await
        .map_err(|error| format!("Unable to write to shell session: {error}"))?;
    shell
        .stdin
        .flush()
        .await
        .map_err(|error| format!("Unable to flush shell session: {error}"))?;
    let mut output = String::new();
    let exit_code = loop {
        let mut line = String::new();
        let bytes = shell
            .stdout
            .read_line(&mut line)
            .await
            .map_err(|error| format!("Unable to read shell session: {error}"))?;
        if bytes == 0 {
            return Err("Persistent shell exited before returning a result.".to_string());
        }
        if let Some(marker) = line.find(&sentinel) {
            let metadata = line[marker + sentinel.len()..]
                .trim()
                .trim_start_matches(':');
            let mut parts = metadata.splitn(2, ':');
            let exit_code = parts.next().unwrap_or("-1").to_string();
            if let Some(next_cwd) = parts.next().filter(|value| !value.is_empty()) {
                shell.cwd = PathBuf::from(next_cwd);
            }
            break exit_code;
        }
        output.push_str(&line);
        if output.len() > MAX_OUTPUT_CHARS * 2 {
            output.truncate(MAX_OUTPUT_CHARS * 2);
        }
    };
    let cwd = shell.cwd.clone();
    Ok(format!(
        "session: {session_id}\ncwd: {}\nexit_code: {exit_code}\n{}",
        cwd.display(),
        truncate_output(&output)
    ))
}

async fn run_process(command: &str, cwd: Option<&Path>) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    let mut process = {
        let mut command_builder = Command::new("cmd.exe");
        command_builder.args(["/C", command]);
        command_builder
    };
    #[cfg(not(target_os = "windows"))]
    let mut process = {
        let mut command_builder = Command::new("sh");
        command_builder.args(["-c", command]);
        command_builder
    };
    if let Some(cwd) = cwd {
        process.current_dir(cwd);
    }
    let output = process
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("Unable to run shell command: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut result = format!(
        "exit_code: {}\nstdout:\n{stdout}",
        output.status.code().unwrap_or(-1)
    );
    if !stderr.trim().is_empty() {
        result.push_str(&format!("\nstderr:\n{stderr}"));
    }
    if !output.status.success() {
        return Err(truncate_output(&result));
    }
    Ok(truncate_output(&result))
}

async fn spawn(cwd: &Path) -> Result<PersistentShell, String> {
    #[cfg(target_os = "windows")]
    let mut process = {
        let mut command_builder = Command::new("cmd.exe");
        command_builder.args(["/Q", "/D", "/K"]);
        command_builder
    };
    #[cfg(not(target_os = "windows"))]
    let mut process = Command::new("sh");
    let mut child = process
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Unable to start persistent shell: {error}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Persistent shell stdin is unavailable.".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Persistent shell stdout is unavailable.".to_string())?;
    Ok(PersistentShell {
        child,
        stdin,
        stdout: BufReader::new(stdout),
        cwd: cwd.to_path_buf(),
    })
}
