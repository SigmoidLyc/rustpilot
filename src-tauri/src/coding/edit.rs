use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

use serde_json::Value;

use super::{argument, bool_argument, mutation_path, relative_path, MAX_EDIT_BYTES};

pub(super) fn replace(
    root: &Path,
    arguments: &Value,
    external_approved: bool,
) -> Result<String, String> {
    let raw_path =
        argument(arguments, "path").ok_or_else(|| "rust_code replace requires path".to_string())?;
    let old_text = arguments
        .get("old_text")
        .and_then(Value::as_str)
        .ok_or_else(|| "rust_code replace requires old_text".to_string())?;
    let new_text = arguments
        .get("new_text")
        .and_then(Value::as_str)
        .ok_or_else(|| "rust_code replace requires new_text".to_string())?;
    if old_text.is_empty() {
        return Err(
            "rust_code replace refuses an empty old_text; use write for a full-file replacement"
                .to_string(),
        );
    }
    let replace_all = bool_argument(arguments, "replace_all");
    let path = mutation_path(root, &raw_path, external_approved)?;
    let original = fs::read_to_string(&path)
        .map_err(|error| format!("Unable to read {}: {error}", relative_path(root, &path)))?;
    let occurrences = original.match_indices(old_text).count();
    if occurrences == 0 || (!replace_all && occurrences != 1) {
        return Err(format!(
            "Expected {} replacement in {raw_path}, found {occurrences}",
            if replace_all {
                "at least one"
            } else {
                "exactly one"
            }
        ));
    }
    let updated = if replace_all {
        original.replace(old_text, new_text)
    } else {
        original.replacen(old_text, new_text, 1)
    };
    let replaced = if replace_all { occurrences } else { 1 };
    write_atomic(&path, updated.as_bytes())?;
    Ok(format!(
        "Replaced {} occurrence{} in {}",
        replaced,
        if replaced == 1 { "" } else { "s" },
        relative_path(root, &path)
    ))
}

pub(super) fn write(
    root: &Path,
    arguments: &Value,
    external_approved: bool,
) -> Result<String, String> {
    let raw_path =
        argument(arguments, "path").ok_or_else(|| "rust_code write requires path".to_string())?;
    let content = arguments
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "rust_code write requires content".to_string())?;
    if content.len() > MAX_EDIT_BYTES {
        return Err(format!(
            "Content exceeds the {} MiB write limit",
            MAX_EDIT_BYTES / 1024 / 1024
        ));
    }
    let path = mutation_path(root, &raw_path, external_approved)?;
    if path.is_dir() {
        return Err(format!("Cannot write a directory: {raw_path}"));
    }
    apply_batch(
        vec![(path.clone(), content.as_bytes().to_vec())],
        Vec::new(),
    )?;
    Ok(format!(
        "Wrote {} ({} bytes)",
        relative_path(root, &path),
        content.len()
    ))
}

pub(super) fn delete(
    root: &Path,
    arguments: &Value,
    external_approved: bool,
) -> Result<String, String> {
    let raw_path =
        argument(arguments, "path").ok_or_else(|| "rust_code delete requires path".to_string())?;
    let path = mutation_path(root, &raw_path, external_approved)?;
    if !path.is_file() {
        return Err("rust_code delete only removes regular files".to_string());
    }
    fs::remove_file(&path)
        .map_err(|error| format!("Unable to delete {}: {error}", relative_path(root, &path)))?;
    Ok(format!("Deleted {}", relative_path(root, &path)))
}

pub(super) fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), String> {
    if contents.len() > MAX_EDIT_BYTES {
        return Err(format!(
            "Edit exceeds the {} MiB write limit",
            MAX_EDIT_BYTES / 1024 / 1024
        ));
    }
    apply_batch(vec![(path.to_path_buf(), contents.to_vec())], Vec::new())
}

/// Commits a set of file replacements and deletions as one filesystem batch.
/// All temporary files are prepared before any existing target is moved away.
pub(super) fn apply_batch(
    writes: Vec<(PathBuf, Vec<u8>)>,
    deletes: Vec<PathBuf>,
) -> Result<(), String> {
    validate_batch_paths(&writes, &deletes)?;
    let mut staged = Vec::with_capacity(writes.len());
    for (index, (target, contents)) in writes.iter().enumerate() {
        if contents.len() > MAX_EDIT_BYTES {
            cleanup_staged(&staged);
            return Err(format!(
                "Edit exceeds the {} MiB write limit",
                MAX_EDIT_BYTES / 1024 / 1024
            ));
        }
        if target.is_dir() {
            cleanup_staged(&staged);
            return Err(format!("Cannot replace directory: {}", target.display()));
        }
        let temporary = unique_temporary_path(target, index, "tmp")?;
        if let Err(error) = stage_file(&temporary, contents) {
            let _ = fs::remove_file(&temporary);
            cleanup_staged(&staged);
            return Err(error);
        }
        staged.push(StagedWrite {
            target: target.clone(),
            temporary,
            installed: false,
        });
    }

    let mut backups = Vec::new();
    for (index, path) in deletes.iter().enumerate() {
        if path.is_dir() {
            rollback(&mut staged, &mut backups);
            return Err(format!("Cannot delete directory: {}", path.display()));
        }
        if path.exists() {
            let backup = match unique_temporary_path(path, writes.len() + index, "backup") {
                Ok(path) => path,
                Err(error) => {
                    rollback(&mut staged, &mut backups);
                    return Err(error);
                }
            };
            if let Err(error) = fs::rename(path, &backup) {
                rollback(&mut staged, &mut backups);
                return Err(format!(
                    "Unable to stage deletion {}: {error}",
                    path.display()
                ));
            }
            backups.push(Backup {
                target: path.clone(),
                backup,
            });
        }
    }

    for index in 0..staged.len() {
        let target = staged[index].target.clone();
        if target.exists() {
            let backup = match unique_temporary_path(
                &target,
                writes.len() + deletes.len() + backups.len(),
                "backup",
            ) {
                Ok(path) => path,
                Err(error) => {
                    rollback(&mut staged, &mut backups);
                    return Err(error);
                }
            };
            if let Err(error) = fs::rename(&target, &backup) {
                rollback(&mut staged, &mut backups);
                return Err(format!(
                    "Unable to stage replacement {}: {error}",
                    target.display()
                ));
            }
            backups.push(Backup { target, backup });
        }
    }

    for index in 0..staged.len() {
        let temporary = staged[index].temporary.clone();
        let target = staged[index].target.clone();
        if let Err(error) = fs::rename(&temporary, &target) {
            rollback(&mut staged, &mut backups);
            return Err(format!(
                "Unable to install replacement {}: {error}",
                target.display()
            ));
        }
        staged[index].installed = true;
    }

    cleanup_backups(&backups);
    Ok(())
}

struct StagedWrite {
    target: PathBuf,
    temporary: PathBuf,
    installed: bool,
}

struct Backup {
    target: PathBuf,
    backup: PathBuf,
}

fn stage_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Unable to create parent directory: {error}"))?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("Unable to create temporary edit file: {error}"))?;
    file.write_all(contents)
        .map_err(|error| format!("Unable to write temporary edit file: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("Unable to flush temporary edit file: {error}"))?;
    Ok(())
}

fn temporary_path(target: &Path, index: usize, suffix: &str) -> PathBuf {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    target.with_file_name(format!(
        ".{file_name}.rustpilot-{}-{index}-{suffix}",
        std::process::id()
    ))
}

fn unique_temporary_path(target: &Path, index: usize, suffix: &str) -> Result<PathBuf, String> {
    for attempt in 0..32usize {
        let candidate = temporary_path(target, index.saturating_add(attempt), suffix);
        if !candidate.exists() && fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "Unable to allocate a temporary path beside {}",
        target.display()
    ))
}

fn validate_batch_paths(writes: &[(PathBuf, Vec<u8>)], deletes: &[PathBuf]) -> Result<(), String> {
    let mut paths = std::collections::HashSet::new();
    for (path, _) in writes {
        if !paths.insert(path_key(path)) {
            return Err(format!(
                "The same file is scheduled more than once: {}",
                path.display()
            ));
        }
    }
    for path in deletes {
        if !paths.insert(path_key(path)) {
            return Err(format!(
                "A file cannot be written and deleted in one batch: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn path_key(path: &Path) -> String {
    let mut key = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    key.make_ascii_lowercase();
    key
}

fn cleanup_staged(staged: &[StagedWrite]) {
    for file in staged {
        let _ = fs::remove_file(&file.temporary);
    }
}

fn cleanup_backups(backups: &[Backup]) {
    for backup in backups {
        let _ = fs::remove_file(&backup.backup);
    }
}

fn rollback(staged: &mut [StagedWrite], backups: &mut Vec<Backup>) {
    for file in staged.iter().rev() {
        if file.installed {
            let _ = fs::remove_file(&file.target);
        }
        let _ = fs::remove_file(&file.temporary);
    }
    for backup in backups.iter().rev() {
        if !backup.target.exists() {
            let _ = fs::rename(&backup.backup, &backup.target);
        } else {
            let _ = fs::remove_file(&backup.backup);
        }
    }
    backups.clear();
}
