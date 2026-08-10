use std::{collections::HashSet, fs, path::Path};

use serde_json::Value;

use super::{mutation_path, relative_path, MAX_EDIT_BYTES, MAX_LINE_BYTES};

#[derive(Debug, Clone)]
enum FileOperation {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
        hunks: Option<Vec<Hunk>>,
    },
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug, Clone)]
struct Hunk {
    old_start: Option<usize>,
    lines: Vec<Line>,
}

#[derive(Debug, Clone)]
enum Line {
    Context(String),
    Remove(String),
    Add(String),
}

pub(super) fn apply(
    root: &Path,
    arguments: &Value,
    external_approved: bool,
) -> Result<String, String> {
    let patch = arguments
        .get("patch")
        .and_then(Value::as_str)
        .ok_or_else(|| "rust_code apply_patch requires patch".to_string())?;
    if patch.len() > MAX_EDIT_BYTES {
        return Err("Patch exceeds the 16 MiB limit".to_string());
    }
    let operations = parse(patch)?;
    let mut writes = Vec::new();
    let mut deletes = Vec::new();
    let mut labels = Vec::new();
    let mut seen_paths = HashSet::new();

    for operation in operations {
        match operation {
            FileOperation::Add { path, lines } => {
                let path = mutation_path(root, &path, external_approved)?;
                register_path(&mut seen_paths, &path)?;
                if path.exists() {
                    return Err(format!(
                        "Cannot add existing file {}",
                        relative_path(root, &path)
                    ));
                }
                writes.push((path.clone(), join_lines(&lines, true).into_bytes()));
                labels.push(format!("add {}", relative_path(root, &path)));
            }
            FileOperation::Delete { path, hunks } => {
                let path = mutation_path(root, &path, external_approved)?;
                register_path(&mut seen_paths, &path)?;
                if !path.is_file() {
                    return Err(format!(
                        "Cannot delete missing file {}",
                        relative_path(root, &path)
                    ));
                }
                if let Some(hunks) = hunks {
                    let original = fs::read_to_string(&path).map_err(|error| {
                        format!("Unable to read {}: {error}", relative_path(root, &path))
                    })?;
                    apply_hunks(&original, &hunks)?;
                }
                deletes.push(path.clone());
                labels.push(format!("delete {}", relative_path(root, &path)));
            }
            FileOperation::Update {
                path,
                move_to,
                hunks,
            } => {
                let source = mutation_path(root, &path, external_approved)?;
                register_path(&mut seen_paths, &source)?;
                let original = fs::read_to_string(&source).map_err(|error| {
                    format!("Unable to read {}: {error}", relative_path(root, &source))
                })?;
                let updated = apply_hunks(&original, &hunks)?;
                let destination = move_to
                    .as_deref()
                    .map(|value| mutation_path(root, value, external_approved))
                    .transpose()?
                    .unwrap_or_else(|| source.clone());
                if destination != source {
                    register_path(&mut seen_paths, &destination)?;
                }
                if destination != source && destination.exists() {
                    return Err(format!(
                        "Cannot move over existing file {}",
                        relative_path(root, &destination)
                    ));
                }
                writes.push((destination.clone(), updated.into_bytes()));
                if destination != source {
                    deletes.push(source);
                }
                labels.push(format!("update {}", relative_path(root, &destination)));
            }
        }
    }

    if writes.is_empty() && deletes.is_empty() {
        return Err("Patch contains no file operations".to_string());
    }
    super::edit::apply_batch(writes, deletes)?;
    Ok(format!("Applied patch: {}", labels.join(", ")))
}

fn parse(patch: &str) -> Result<Vec<FileOperation>, String> {
    let lines = patch
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(str::to_string)
        .collect::<Vec<_>>();
    if lines.iter().any(|line| line.len() > MAX_LINE_BYTES) {
        return Err("Patch contains an oversized line".to_string());
    }
    if lines
        .first()
        .is_some_and(|line| line.trim() == "*** Begin Patch")
    {
        parse_apply_patch(&lines)
    } else {
        parse_unified(&lines)
    }
}

fn is_file_header(line: &str) -> bool {
    line.starts_with("*** Add File: ")
        || line.starts_with("*** Delete File: ")
        || line.starts_with("*** Update File: ")
}

fn parse_apply_patch(lines: &[String]) -> Result<Vec<FileOperation>, String> {
    let mut operations = Vec::new();
    let mut index = 1usize;
    while index < lines.len() {
        let line = &lines[index];
        if line.trim().is_empty() {
            index += 1;
            continue;
        }
        if line.trim() == "*** End Patch" {
            return Ok(operations);
        }
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            index += 1;
            let mut content = Vec::new();
            while index < lines.len()
                && !is_file_header(&lines[index])
                && lines[index] != "*** End Patch"
            {
                let value = lines[index]
                    .strip_prefix('+')
                    .ok_or_else(|| "Added-file patch lines must start with +".to_string())?;
                content.push(value.to_string());
                index += 1;
            }
            operations.push(FileOperation::Add {
                path: path.trim().to_string(),
                lines: content,
            });
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Delete File: ") {
            operations.push(FileOperation::Delete {
                path: path.trim().to_string(),
                hunks: None,
            });
            index += 1;
            continue;
        }
        if let Some(path) = line.strip_prefix("*** Update File: ") {
            let source = path.trim().to_string();
            index += 1;
            let mut move_to = None;
            let mut body = Vec::new();
            while index < lines.len()
                && !is_file_header(&lines[index])
                && lines[index] != "*** End Patch"
            {
                if let Some(destination) = lines[index].strip_prefix("*** Move to: ") {
                    move_to = Some(destination.trim().to_string());
                } else {
                    body.push(lines[index].clone());
                }
                index += 1;
            }
            operations.push(FileOperation::Update {
                path: source,
                move_to,
                hunks: parse_hunks(&body)?,
            });
            continue;
        }
        return Err(format!("Unsupported apply_patch directive: {line}"));
    }
    Err("Patch is missing *** End Patch".to_string())
}

fn parse_unified(lines: &[String]) -> Result<Vec<FileOperation>, String> {
    let mut operations = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        if !lines[index].starts_with("--- ") {
            index += 1;
            continue;
        }
        let old_path = header_path(&lines[index], "--- ")?;
        index += 1;
        if index >= lines.len() || !lines[index].starts_with("+++ ") {
            return Err("Unified patch is missing its +++ path".to_string());
        }
        let new_path = header_path(&lines[index], "+++ ")?;
        index += 1;
        let mut body = Vec::new();
        while index < lines.len() && !lines[index].starts_with("--- ") {
            body.push(lines[index].clone());
            index += 1;
        }
        if old_path == "/dev/null" {
            let hunks = parse_hunks(&body)?;
            operations.push(FileOperation::Add {
                path: strip_prefix(&new_path),
                lines: added_lines(&hunks)?,
            });
        } else if new_path == "/dev/null" {
            operations.push(FileOperation::Delete {
                path: strip_prefix(&old_path),
                hunks: Some(parse_hunks(&body)?),
            });
        } else {
            operations.push(FileOperation::Update {
                path: strip_prefix(&old_path),
                move_to: None,
                hunks: parse_hunks(&body)?,
            });
        }
    }
    if operations.is_empty() {
        return Err("Unable to find a supported unified patch".to_string());
    }
    Ok(operations)
}

fn header_path(line: &str, prefix: &str) -> Result<String, String> {
    let value = line
        .strip_prefix(prefix)
        .ok_or_else(|| "Invalid patch path header".to_string())?;
    Ok(value
        .split('\t')
        .next()
        .unwrap_or(value)
        .trim()
        .trim_matches('"')
        .to_string())
}

fn strip_prefix(path: &str) -> String {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .to_string()
}

fn added_lines(hunks: &[Hunk]) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for hunk in hunks {
        for line in &hunk.lines {
            match line {
                Line::Add(value) => lines.push(value.clone()),
                Line::Context(_) | Line::Remove(_) => {
                    return Err("A unified add patch may contain only added lines".to_string())
                }
            }
        }
    }
    Ok(lines)
}

fn register_path(seen: &mut HashSet<String>, path: &Path) -> Result<(), String> {
    let mut key = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    key.make_ascii_lowercase();
    if !seen.insert(key) {
        return Err(format!(
            "Patch references the same path more than once: {}",
            path.display()
        ));
    }
    Ok(())
}

fn parse_hunks(lines: &[String]) -> Result<Vec<Hunk>, String> {
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    for line in lines {
        if line.starts_with("@@") {
            if let Some(hunk) = current.take() {
                hunks.push(hunk);
            }
            current = Some(Hunk {
                old_start: parse_old_start(line),
                lines: Vec::new(),
            });
            continue;
        }
        if line.starts_with("\\ No newline at end of file")
            || line.starts_with("diff ")
            || line.starts_with("index ")
        {
            continue;
        }
        let hunk = current.get_or_insert_with(|| Hunk {
            old_start: None,
            lines: Vec::new(),
        });
        if let Some(value) = line.strip_prefix(' ') {
            hunk.lines.push(Line::Context(value.to_string()));
        } else if let Some(value) = line.strip_prefix('-') {
            hunk.lines.push(Line::Remove(value.to_string()));
        } else if let Some(value) = line.strip_prefix('+') {
            hunk.lines.push(Line::Add(value.to_string()));
        } else if !line.is_empty() {
            return Err(format!("Invalid patch hunk line: {line}"));
        }
    }
    if let Some(hunk) = current {
        hunks.push(hunk);
    }
    if hunks.is_empty() {
        return Err("Patch update contains no hunks".to_string());
    }
    Ok(hunks)
}

fn parse_old_start(header: &str) -> Option<usize> {
    let marker = header.find('-')? + 1;
    let digits = header[marker..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>();
    (!digits.is_empty())
        .then(|| digits.parse::<usize>().ok())
        .flatten()
}

fn split_text(text: &str) -> (Vec<String>, bool) {
    let trailing_newline = text.ends_with('\n');
    let mut lines = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line).to_string())
        .collect::<Vec<_>>();
    if trailing_newline {
        lines.pop();
    }
    (lines, trailing_newline)
}

fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut output = lines.join("\n");
    if trailing_newline {
        output.push('\n');
    }
    output
}

fn apply_hunks(original: &str, hunks: &[Hunk]) -> Result<String, String> {
    let (mut lines, trailing_newline) = split_text(original);
    let mut cursor = 0usize;
    for hunk in hunks {
        let expected = hunk.old_start.map(|value| value.saturating_sub(1));
        let old_lines = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                Line::Context(value) | Line::Remove(value) => Some(value.as_str()),
                Line::Add(_) => None,
            })
            .collect::<Vec<_>>();
        let start = find_hunk_start(&lines, &old_lines, expected, cursor).ok_or_else(|| {
            format!(
                "Unable to find patch context near line {}",
                expected.unwrap_or(cursor + 1)
            )
        })?;
        let mut replacement = Vec::new();
        let mut source_index = start;
        for line in &hunk.lines {
            match line {
                Line::Context(value) => {
                    if lines.get(source_index).map(String::as_str) != Some(value.as_str()) {
                        return Err("Patch context changed while applying hunks".to_string());
                    }
                    replacement.push(value.clone());
                    source_index += 1;
                }
                Line::Remove(value) => {
                    if lines.get(source_index).map(String::as_str) != Some(value.as_str()) {
                        return Err("Patch removal does not match the file".to_string());
                    }
                    source_index += 1;
                }
                Line::Add(value) => replacement.push(value.clone()),
            }
        }
        lines.splice(start..source_index, replacement.iter().cloned());
        cursor = start + replacement.len();
    }
    let output = join_lines(&lines, trailing_newline);
    if output.len() > MAX_EDIT_BYTES {
        return Err("Patched file exceeds the 16 MiB limit".to_string());
    }
    Ok(output)
}

fn find_hunk_start(
    lines: &[String],
    old_lines: &[&str],
    expected: Option<usize>,
    cursor: usize,
) -> Option<usize> {
    if old_lines.is_empty() {
        return Some(expected.unwrap_or(cursor).min(lines.len()));
    }
    let first = expected.unwrap_or(cursor).min(lines.len());
    let mut candidates =
        std::iter::once(first).chain((cursor..lines.len()).filter(|value| *value != first));
    candidates.find(|start| {
        *start + old_lines.len() <= lines.len()
            && lines[*start..*start + old_lines.len()]
                .iter()
                .zip(old_lines.iter())
                .all(|(actual, expected)| actual == expected)
    })
}
