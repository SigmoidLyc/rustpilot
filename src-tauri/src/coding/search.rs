use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};

use serde_json::Value;

use super::{
    argument, bounded_append, finish_bounded, integer_argument, relative_path, scoped_path,
    IGNORED_DIRECTORIES, MAX_GLOB_RESULTS, MAX_GREP_MATCHES, MAX_LINE_BYTES, MAX_READ_LINES,
    MAX_WALK_FILES,
};

pub(super) fn read_file(root: &Path, arguments: &Value) -> Result<String, String> {
    let raw_path = argument(arguments, "path").unwrap_or_else(|| ".".to_string());
    let path = scoped_path(root, &raw_path)?;
    if !path.is_file() {
        return Err(format!("Cannot read a directory as a file: {raw_path}"));
    }
    let start = integer_argument(arguments, "line_start")
        .unwrap_or(1)
        .max(1);
    let requested_end = integer_argument(arguments, "line_end");
    let end = requested_end
        .unwrap_or_else(|| start.saturating_add(MAX_READ_LINES - 1))
        .max(start)
        .min(start.saturating_add(MAX_READ_LINES - 1));
    let include_numbers = arguments
        .get("line_numbers")
        .and_then(Value::as_bool)
        .unwrap_or(true);

    let file = File::open(&path)
        .map_err(|error| format!("Unable to open {}: {error}", relative_path(root, &path)))?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line = String::new();
    let mut line_number = 1usize;
    let mut selected = 0usize;
    let mut output = format!("{} lines {}-{}\n", relative_path(root, &path), start, end);
    let mut output_chars = output.chars().count();
    let mut truncated = false;

    while line_number <= end {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|error| format!("Unable to read {}: {error}", relative_path(root, &path)))?;
        if bytes == 0 {
            break;
        }
        if line.len() > MAX_LINE_BYTES || line.contains('\0') {
            return Err(format!(
                "{} is binary or contains an oversized line",
                raw_path
            ));
        }
        if line_number >= start {
            let value = line.trim_end_matches(['\r', '\n']);
            let formatted = if include_numbers {
                format!("{line_number:>6} | {value}\n")
            } else {
                format!("{value}\n")
            };
            if !bounded_append(&mut output, &formatted, &mut output_chars) {
                truncated = true;
                break;
            }
            selected += 1;
        }
        line_number += 1;
    }
    if selected == 0 {
        return Err(format!("Line range starts after the end of {raw_path}"));
    }
    Ok(finish_bounded(output, truncated || line_number <= end))
}

pub(super) fn list_directory(root: &Path, arguments: &Value) -> Result<String, String> {
    let raw_path = argument(arguments, "path").unwrap_or_else(|| ".".to_string());
    let path = scoped_path(root, &raw_path)?;
    if !path.is_dir() {
        return Err(format!("Cannot list a file as a directory: {raw_path}"));
    }
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(&path).map_err(|error| format!("Unable to list {raw_path}: {error}"))?
    {
        let entry = entry.map_err(|error| format!("Unable to inspect directory entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("Unable to inspect directory entry: {error}"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if file_type.is_dir() && is_ignored_directory(&name) {
            continue;
        }
        let kind = if file_type.is_dir() {
            "d"
        } else if file_type.is_file() {
            "f"
        } else {
            "l"
        };
        entries.push((kind, name));
    }
    entries.sort();
    let limit = integer_argument(arguments, "limit")
        .unwrap_or(MAX_GLOB_RESULTS)
        .clamp(1, MAX_GLOB_RESULTS);
    let mut output = format!("{}\n", relative_path(root, &path));
    let mut output_chars = output.chars().count();
    let mut truncated = false;
    for (index, (kind, name)) in entries.iter().enumerate() {
        if index >= limit
            || !bounded_append(
                &mut output,
                &format!("{kind} {}\n", relative_path(root, &path.join(name))),
                &mut output_chars,
            )
        {
            truncated = true;
            break;
        }
    }
    if entries.is_empty() {
        output.push_str("[empty]\n");
    }
    Ok(finish_bounded(output, truncated || entries.len() > limit))
}

pub(super) fn glob_files(root: &Path, arguments: &Value) -> Result<String, String> {
    let pattern = argument(arguments, "pattern")
        .or_else(|| argument(arguments, "query"))
        .ok_or_else(|| "rust_code glob requires pattern".to_string())?;
    let raw_root = argument(arguments, "path").unwrap_or_else(|| ".".to_string());
    let search_root = scoped_path(root, &raw_root)?;
    let limit = integer_argument(arguments, "limit")
        .unwrap_or(MAX_GLOB_RESULTS)
        .clamp(1, MAX_GLOB_RESULTS);
    let walk = walk_files(&search_root)?;
    let mut matches = Vec::new();
    for path in walk.files {
        let relative = relative_path(root, &path);
        if matches_glob(&pattern, &relative) {
            matches.push(relative);
            if matches.len() > limit {
                break;
            }
        }
    }
    let truncated = walk.truncated || matches.len() > limit;
    if matches.len() > limit {
        matches.truncate(limit);
    }
    matches.sort();
    let output = if matches.is_empty() {
        "No files found\n".to_string()
    } else {
        matches.join("\n") + "\n"
    };
    Ok(finish_bounded(output, truncated))
}

pub(super) fn grep_files(root: &Path, arguments: &Value) -> Result<String, String> {
    let pattern = argument(arguments, "pattern")
        .or_else(|| argument(arguments, "query"))
        .ok_or_else(|| "rust_code grep requires pattern".to_string())?;
    let raw_root = argument(arguments, "path").unwrap_or_else(|| ".".to_string());
    let search_root = scoped_path(root, &raw_root)?;
    let file_filter = argument(arguments, "glob");
    let case_sensitive = arguments
        .get("case_sensitive")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let max_matches = integer_argument(arguments, "max_results")
        .unwrap_or(MAX_GREP_MATCHES)
        .clamp(1, MAX_GREP_MATCHES);
    let candidates = if search_root.is_file() {
        vec![search_root]
    } else {
        walk_files(&search_root)?.files
    };
    let query = if case_sensitive {
        None
    } else {
        Some(pattern.to_lowercase())
    };
    let mut output = String::new();
    let mut output_chars = 0usize;
    let mut matches = 0usize;
    let mut skipped = 0usize;
    for path in candidates {
        let relative = relative_path(root, &path);
        if file_filter
            .as_deref()
            .is_some_and(|filter| !matches_glob(filter, &relative))
        {
            continue;
        }
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(_) => continue,
        };
        let mut reader = BufReader::with_capacity(64 * 1024, file);
        let mut line = String::new();
        let mut line_number = 0usize;
        loop {
            line.clear();
            let bytes = match reader.read_line(&mut line) {
                Ok(bytes) => bytes,
                Err(_) => {
                    skipped += 1;
                    break;
                }
            };
            if bytes == 0 {
                break;
            }
            line_number += 1;
            if line.len() > MAX_LINE_BYTES || line.contains('\0') {
                skipped += 1;
                break;
            }
            let value = line.trim_end_matches(['\r', '\n']);
            let found = match query.as_deref() {
                None => value.contains(pattern.as_str()),
                Some(query) if value.is_ascii() && query.is_ascii() => {
                    ascii_contains_case_insensitive(value.as_bytes(), query.as_bytes())
                }
                Some(query) => value.to_lowercase().contains(query),
            };
            if found {
                if matches >= max_matches {
                    return Ok(finish_bounded(output, true));
                }
                let snippet = value.chars().take(500).collect::<String>();
                let row = format!("{relative}:{line_number}: {snippet}\n");
                if !bounded_append(&mut output, &row, &mut output_chars) {
                    return Ok(finish_bounded(output, true));
                }
                matches += 1;
            }
        }
    }
    if matches == 0 {
        output.push_str("No matches\n");
    }
    if skipped > 0 {
        output.push_str(&format!(
            "[skipped {skipped} unreadable or binary file(s)]\n"
        ));
    }
    Ok(finish_bounded(output, false))
}

#[derive(Debug)]
struct WalkResult {
    files: Vec<PathBuf>,
    truncated: bool,
}

fn walk_files(root: &Path) -> Result<WalkResult, String> {
    if root.is_file() {
        return Ok(WalkResult {
            files: vec![root.to_path_buf()],
            truncated: false,
        });
    }
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(directory) = stack.pop() {
        let mut children = Vec::new();
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("Unable to scan {}: {error}", directory.display()))?
        {
            let entry =
                entry.map_err(|error| format!("Unable to inspect directory entry: {error}"))?;
            children.push(entry);
        }
        children.sort_by_key(|entry| entry.file_name());
        for entry in children.into_iter().rev() {
            let file_type = entry
                .file_type()
                .map_err(|error| format!("Unable to inspect directory entry: {error}"))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if file_type.is_dir() {
                if !is_ignored_directory(&name) {
                    stack.push(entry.path());
                }
            } else if file_type.is_file() {
                files.push(entry.path());
                if files.len() > MAX_WALK_FILES {
                    files.truncate(MAX_WALK_FILES);
                    return Ok(WalkResult {
                        files,
                        truncated: true,
                    });
                }
            }
        }
    }
    Ok(WalkResult {
        files,
        truncated: false,
    })
}

fn ascii_contains_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn is_ignored_directory(name: &str) -> bool {
    IGNORED_DIRECTORIES.contains(&name)
}

fn matches_glob(pattern: &str, value: &str) -> bool {
    let pattern = pattern.trim().replace('\\', "/");
    let pattern = pattern.strip_prefix("./").unwrap_or(&pattern);
    let value = value.replace('\\', "/");
    let candidate = if pattern.contains('/') {
        value.clone()
    } else {
        value.rsplit('/').next().unwrap_or(&value).to_string()
    };
    wildcard_match(pattern, &candidate)
        || (pattern.starts_with("**/") && wildcard_match(&pattern[3..], &candidate))
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut pattern_index = 0usize;
    let mut value_index = 0usize;
    let mut star_index: Option<usize> = None;
    let mut star_value_index = 0usize;
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == b'?'
                || (pattern[pattern_index] == value[value_index] && pattern[pattern_index] != b'/'))
        {
            pattern_index += 1;
            value_index += 1;
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
