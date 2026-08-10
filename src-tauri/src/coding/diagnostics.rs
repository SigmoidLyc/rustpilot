use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::Value;

use super::{argument, relative_path, scoped_path, truncate_text, MAX_OUTPUT_CHARS};

const MAX_CHECK_SECONDS: u64 = 90;
const MAX_CAPTURE_BYTES: usize = MAX_OUTPUT_CHARS * 4;

pub(super) fn check(root: &Path, arguments: &Value) -> Result<String, String> {
    let requested = argument(arguments, "path").unwrap_or_else(|| ".".to_string());
    let start = scoped_path(root, &requested)?;
    let backend = argument(arguments, "backend").unwrap_or_else(|| "auto".to_string());
    let project = find_project(&start, root, &backend)?;
    let offline = arguments
        .get("offline")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let (program, args, label) = command_for(&project, &backend, offline)?;
    let timeout = Duration::from_secs(
        arguments
            .get("timeout")
            .and_then(Value::as_u64)
            .unwrap_or(MAX_CHECK_SECONDS)
            .clamp(5, MAX_CHECK_SECONDS),
    );
    let result = run_bounded(&program, &args, &project, timeout)?;
    let status = if result.timed_out {
        "timed_out"
    } else if result.status.success() {
        "passed"
    } else {
        "failed"
    };
    let output = format!(
        "backend: {label}\nproject: {}\nstatus: {status}\nexit_code: {}\nstdout:\n{}\nstderr:\n{}",
        relative_path(root, &project),
        result.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr),
    );
    Ok(truncate_text(&output, "diagnostics"))
}

fn find_project(start: &Path, root: &Path, backend: &str) -> Result<PathBuf, String> {
    let mut cursor = if start.is_file() {
        start.parent().unwrap_or(root).to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        let has_cargo = cursor.join("Cargo.toml").is_file();
        let has_npm = cursor.join("package.json").is_file();
        let found = match backend {
            "auto" if has_cargo || has_npm => true,
            "cargo" if has_cargo => true,
            "npm" if has_npm => true,
            "auto" | "cargo" | "npm" => false,
            other => {
                return Err(format!(
                    "Unsupported diagnostics backend '{other}'; use auto, cargo, or npm"
                ))
            }
        };
        if found {
            return Ok(cursor);
        }
        if cursor == root {
            break;
        }
        let Some(parent) = cursor.parent() else {
            break;
        };
        if !parent.starts_with(root) {
            break;
        }
        cursor = parent.to_path_buf();
    }
    Err(format!(
        "No supported project manifest found from {}",
        start.display()
    ))
}

fn command_for(
    project: &Path,
    backend: &str,
    offline: bool,
) -> Result<(String, Vec<String>, &'static str), String> {
    let backend = if backend == "auto" {
        if project.join("Cargo.toml").is_file() {
            "cargo"
        } else {
            "npm"
        }
    } else {
        backend
    };
    match backend {
        "cargo" => {
            let mut args = vec!["check".to_string(), "--message-format=short".to_string()];
            if offline {
                args.push("--offline".to_string());
            }
            Ok(("cargo".to_string(), args, "cargo"))
        }
        "npm" => {
            #[cfg(target_os = "windows")]
            let program = "npm.cmd";
            #[cfg(not(target_os = "windows"))]
            let program = "npm";
            Ok((
                program.to_string(),
                vec!["run".to_string(), "check".to_string()],
                "npm",
            ))
        }
        other => Err(format!(
            "Unsupported diagnostics backend '{other}'; use auto, cargo, or npm"
        )),
    }
}

struct CheckOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

fn run_bounded(
    program: &str,
    args: &[String],
    cwd: &Path,
    timeout: Duration,
) -> Result<CheckOutput, String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Unable to start diagnostics command {program}: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Diagnostics stdout is unavailable".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Diagnostics stderr is unavailable".to_string())?;
    let stdout_reader = thread::spawn(|| capture_limited(stdout));
    let stderr_reader = thread::spawn(|| capture_limited(stderr));
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("Unable to inspect diagnostics process: {error}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            let _ = child.kill();
            break child
                .wait()
                .map_err(|error| format!("Unable to stop diagnostics process: {error}"))?;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "Diagnostics stdout reader failed".to_string())??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "Diagnostics stderr reader failed".to_string())??;
    Ok(CheckOutput {
        status,
        stdout,
        stderr,
        timed_out,
    })
}

fn capture_limited<R: Read>(mut reader: R) -> Result<Vec<u8>, String> {
    let mut output = Vec::with_capacity(MAX_CAPTURE_BYTES.min(8192));
    let mut buffer = [0u8; 8192];
    loop {
        let count = reader
            .read(&mut buffer)
            .map_err(|error| format!("Unable to read diagnostics output: {error}"))?;
        if count == 0 {
            break;
        }
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(output.len());
        if remaining > 0 {
            output.extend_from_slice(&buffer[..count.min(remaining)]);
        }
    }
    Ok(output)
}
