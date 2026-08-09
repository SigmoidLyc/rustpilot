//! Small, dependency-free attachment storage for prompt files.
//!
//! The task database stores only this module's metadata. The bytes live in an
//! application-owned directory and are read only when a request or preview
//! needs them. This keeps task snapshots and stream events small.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MAX_ATTACHMENTS: usize = 8;
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_TOTAL_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
pub const MAX_TEXT_CONTEXT_BYTES: usize = 240 * 1024;

const ATTACHMENTS_DIRECTORY: &str = "attachments";
const MAX_NAME_CHARS: usize = 160;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentInput {
    pub name: String,
    #[serde(default)]
    pub mime: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentPathInput {
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mime: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentRef {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub storage_key: String,
}

pub fn store_inputs(
    data_dir: &Path,
    task_id: &str,
    inputs: &[AttachmentInput],
) -> Result<Vec<AttachmentRef>, String> {
    store_inputs_and_paths(data_dir, task_id, inputs, &[])
}

pub fn store_paths(
    data_dir: &Path,
    task_id: &str,
    inputs: &[AttachmentPathInput],
) -> Result<Vec<AttachmentRef>, String> {
    store_inputs_and_paths(data_dir, task_id, &[], inputs)
}

pub fn store_inputs_and_paths(
    data_dir: &Path,
    task_id: &str,
    encoded_inputs: &[AttachmentInput],
    path_inputs: &[AttachmentPathInput],
) -> Result<Vec<AttachmentRef>, String> {
    let count = encoded_inputs
        .len()
        .checked_add(path_inputs.len())
        .ok_or_else(|| "Too many attachments.".to_string())?;
    validate_count(task_id, count)?;
    let mut decoded = Vec::with_capacity(count);
    let mut total_size = 0usize;
    for input in encoded_inputs {
        let bytes = match decode_base64(&input.data) {
            Ok(bytes) => bytes,
            Err(error) => return Err(format!("Unable to decode {}: {error}", input.name)),
        };
        push_decoded(
            &mut decoded,
            &mut total_size,
            input.name.clone(),
            input.mime.clone(),
            bytes,
        )?;
    }
    for input in path_inputs {
        let path = PathBuf::from(&input.path);
        if !path.is_absolute() {
            return Err(format!(
                "Dropped file path must be absolute: {}",
                input.path
            ));
        }
        let metadata = fs::metadata(&path)
            .map_err(|error| format!("Unable to inspect dropped file {}: {error}", input.path))?;
        if !metadata.is_file() {
            return Err(format!("{} is not a file.", input.path));
        }
        if metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
            return Err(format!(
                "{} is too large. The per-file limit is {} MB.",
                display_path_name(input, &path),
                MAX_ATTACHMENT_BYTES / (1024 * 1024)
            ));
        }
        let path_size = usize::try_from(metadata.len())
            .map_err(|_| format!("{} is too large.", display_path_name(input, &path)))?;
        if total_size.saturating_add(path_size) > MAX_TOTAL_ATTACHMENT_BYTES {
            return Err(format!(
                "The combined attachment limit is {} MB.",
                MAX_TOTAL_ATTACHMENT_BYTES / (1024 * 1024)
            ));
        }
        let bytes = fs::read(&path)
            .map_err(|error| format!("Unable to read dropped file {}: {error}", input.path))?;
        let name = if input.name.trim().is_empty() {
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("attachment")
                .to_string()
        } else {
            input.name.clone()
        };
        push_decoded(
            &mut decoded,
            &mut total_size,
            name,
            input.mime.clone(),
            bytes,
        )?;
    }
    store_decoded(data_dir, task_id, &decoded)
}

fn display_path_name(input: &AttachmentPathInput, path: &Path) -> String {
    if !input.name.trim().is_empty() {
        input.name.clone()
    } else {
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("attachment")
            .to_string()
    }
}

fn push_decoded(
    decoded: &mut Vec<(String, String, Vec<u8>)>,
    total_size: &mut usize,
    name: String,
    mime: String,
    bytes: Vec<u8>,
) -> Result<(), String> {
    if bytes.is_empty() {
        return Err(format!("{name} is empty."));
    }
    if bytes.len() > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "{name} is too large. The per-file limit is {} MB.",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        ));
    }
    *total_size = total_size.saturating_add(bytes.len());
    if *total_size > MAX_TOTAL_ATTACHMENT_BYTES {
        return Err(format!(
            "The combined attachment limit is {} MB.",
            MAX_TOTAL_ATTACHMENT_BYTES / (1024 * 1024)
        ));
    }
    decoded.push((name, mime, bytes));
    Ok(())
}

fn store_decoded(
    data_dir: &Path,
    task_id: &str,
    inputs: &[(String, String, Vec<u8>)],
) -> Result<Vec<AttachmentRef>, String> {
    let task_directory = data_dir.join(ATTACHMENTS_DIRECTORY).join(task_id);
    let mut references = Vec::with_capacity(inputs.len());
    let mut written_paths = Vec::with_capacity(inputs.len());
    let mut total_size = 0usize;

    for (input_name, input_mime, bytes) in inputs {
        if bytes.is_empty() {
            remove_paths(&written_paths);
            return Err(format!("{input_name} is empty."));
        }
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            remove_paths(&written_paths);
            return Err(format!(
                "{input_name} is too large. The per-file limit is {} MB.",
                MAX_ATTACHMENT_BYTES / (1024 * 1024)
            ));
        }
        total_size = total_size.saturating_add(bytes.len());
        if total_size > MAX_TOTAL_ATTACHMENT_BYTES {
            remove_paths(&written_paths);
            return Err(format!(
                "The combined attachment limit is {} MB.",
                MAX_TOTAL_ATTACHMENT_BYTES / (1024 * 1024)
            ));
        }

        let mime = match detect_mime(input_mime, input_name, bytes) {
            Ok(mime) => mime,
            Err(error) => {
                remove_paths(&written_paths);
                return Err(error);
            }
        };
        let name = sanitize_name(input_name);
        let id = format!("att_{}", Uuid::new_v4().simple());
        let file_path = task_directory.join(format!("{id}.bin"));
        let temp_path = task_directory.join(format!("{id}.part"));

        if let Err(error) = fs::create_dir_all(&task_directory)
            .and_then(|_| fs::write(&temp_path, bytes))
            .and_then(|_| fs::rename(&temp_path, &file_path))
        {
            let _ = fs::remove_file(&temp_path);
            remove_paths(&written_paths);
            return Err(format!("Unable to store {name}: {error}"));
        }

        written_paths.push(file_path);
        references.push(AttachmentRef {
            id: id.clone(),
            name,
            mime,
            size: bytes.len() as u64,
            storage_key: format!("{ATTACHMENTS_DIRECTORY}/{task_id}/{id}.bin"),
        });
    }

    Ok(references)
}

fn validate_count(task_id: &str, count: usize) -> Result<(), String> {
    validate_task_id(task_id)?;
    if count > MAX_ATTACHMENTS {
        return Err(format!(
            "You can attach at most {MAX_ATTACHMENTS} files to one message."
        ));
    }
    Ok(())
}

pub fn read(data_dir: &Path, attachment: &AttachmentRef) -> Result<Vec<u8>, String> {
    let path = storage_path(data_dir, &attachment.storage_key)?;
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("Unable to inspect attachment {}: {error}", attachment.name))?;
    if metadata.len() != attachment.size {
        return Err(format!(
            "Attachment {} changed on disk and cannot be used safely.",
            attachment.name
        ));
    }
    if metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
        return Err(format!(
            "Attachment {} exceeds the file size limit.",
            attachment.name
        ));
    }
    fs::read(&path)
        .map_err(|error| format!("Unable to read attachment {}: {error}", attachment.name))
}

pub fn remove_task(data_dir: &Path, task_id: &str) -> Result<(), String> {
    validate_task_id(task_id)?;
    let directory = data_dir.join(ATTACHMENTS_DIRECTORY).join(task_id);
    match fs::remove_dir_all(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Unable to remove task attachments: {error}")),
    }
}

pub fn remove_refs(data_dir: &Path, references: &[AttachmentRef]) {
    for reference in references {
        if let Ok(path) = storage_path(data_dir, &reference.storage_key) {
            let _ = fs::remove_file(path);
        }
    }
}

pub fn is_image(mime: &str) -> bool {
    matches!(
        mime,
        "image/jpeg" | "image/png" | "image/gif" | "image/webp"
    )
}

pub fn is_text(mime: &str, name: &str) -> bool {
    if mime.starts_with("text/")
        || matches!(
            mime,
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/x-sh"
        )
    {
        return true;
    }

    let extension = Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "c" | "cc"
            | "cpp"
            | "css"
            | "csv"
            | "go"
            | "h"
            | "hpp"
            | "html"
            | "ini"
            | "java"
            | "js"
            | "jsx"
            | "json"
            | "md"
            | "py"
            | "rs"
            | "sql"
            | "toml"
            | "ts"
            | "tsx"
            | "txt"
            | "vue"
            | "xml"
            | "yaml"
            | "yml"
    )
}

fn storage_path(data_dir: &Path, storage_key: &str) -> Result<PathBuf, String> {
    let path = Path::new(storage_key);
    if path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
    {
        return Err("Attachment storage reference is invalid.".to_string());
    }
    let mut resolved = data_dir.to_path_buf();
    for component in path.components() {
        if let Component::Normal(value) = component {
            resolved.push(value);
        }
    }
    if !storage_key.starts_with(&format!("{ATTACHMENTS_DIRECTORY}/")) {
        return Err(
            "Attachment storage reference is outside the attachment directory.".to_string(),
        );
    }
    Ok(resolved)
}

fn validate_task_id(task_id: &str) -> Result<(), String> {
    if task_id.is_empty()
        || task_id.len() > 100
        || task_id
            .chars()
            .any(|character| !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_')))
    {
        return Err("Invalid task attachment scope.".to_string());
    }
    Ok(())
}

fn remove_paths(paths: &[PathBuf]) {
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

fn sanitize_name(input: &str) -> String {
    let leaf = input.split(['/', '\\']).next_back().unwrap_or(input).trim();
    let mut name = leaf
        .chars()
        .map(|character| {
            if character.is_control()
                || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    if name.chars().count() > MAX_NAME_CHARS {
        name = name.chars().take(MAX_NAME_CHARS).collect();
    }
    if name.trim_matches(['.', ' ']).is_empty() {
        "attachment".to_string()
    } else {
        name
    }
}

fn detect_mime(declared: &str, name: &str, bytes: &[u8]) -> Result<String, String> {
    let declared = declared
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let sniffed = sniff_mime(bytes);
    let extension = Path::new(name)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let inferred = sniffed.clone().or_else(|| mime_from_extension(&extension));
    let mime = if declared.is_empty() || declared == "application/octet-stream" {
        inferred.unwrap_or_else(|| "application/octet-stream".to_string())
    } else {
        declared
    };

    if mime == "image/svg+xml" {
        return Err(format!(
            "{} is an SVG image, which is not accepted for safety.",
            name
        ));
    }
    if mime.starts_with("image/") && !is_image(&mime) {
        return Err(format!(
            "Image type {mime} is not supported. Use PNG, JPEG, GIF, or WebP."
        ));
    }
    if mime.starts_with("image/") && sniffed.as_deref() != Some(mime.as_str()) {
        return Err(format!("{} does not match its declared image type.", name));
    }
    if let Some(sniffed) = sniffed {
        if is_image(&sniffed) && mime != sniffed {
            return Err(format!("{} does not match its detected image type.", name));
        }
        if mime == "application/pdf" && sniffed != "application/pdf" {
            return Err(format!("{} does not match its declared PDF type.", name));
        }
        if sniffed == "application/pdf" && mime != "application/pdf" {
            return Err(format!("{} does not match its detected PDF type.", name));
        }
    }
    Ok(mime)
}

fn sniff_mime(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png".to_string())
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg".to_string())
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif".to_string())
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp".to_string())
    } else if bytes.starts_with(b"%PDF-") {
        Some("application/pdf".to_string())
    } else if bytes.starts_with(b"PK\x03\x04") {
        Some("application/zip".to_string())
    } else {
        None
    }
}

fn mime_from_extension(extension: &str) -> Option<String> {
    let mime = match extension {
        "bmp" => "image/bmp",
        "csv" => "text/csv",
        "gif" => "image/gif",
        "htm" | "html" => "text/html",
        "jpeg" | "jpg" => "image/jpeg",
        "json" => "application/json",
        "md" => "text/markdown",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "toml" => "application/toml",
        "txt" => "text/plain",
        "webp" => "image/webp",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        _ => return None,
    };
    Some(mime.to_string())
}

fn decode_base64(value: &str) -> Result<Vec<u8>, String> {
    let encoded = value
        .strip_prefix("data:")
        .and_then(|value| value.split_once(',').map(|(_, data)| data))
        .unwrap_or(value)
        .trim();
    if encoded.is_empty() || encoded.len() % 4 != 0 {
        return Err("invalid Base64 length".to_string());
    }
    let padding = encoded
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    if padding > 2 {
        return Err("invalid Base64 padding".to_string());
    }
    let estimated = encoded
        .len()
        .saturating_div(4)
        .saturating_mul(3)
        .saturating_sub(padding);
    if estimated > MAX_ATTACHMENT_BYTES {
        return Err("decoded file exceeds the size limit".to_string());
    }

    let mut output = Vec::with_capacity(estimated);
    let bytes = encoded.as_bytes();
    for (chunk_index, chunk) in bytes.chunks_exact(4).enumerate() {
        let last = chunk_index + 1 == bytes.len() / 4;
        let a = decode_base64_byte(chunk[0])?;
        let b = decode_base64_byte(chunk[1])?;
        let c = if chunk[2] == b'=' {
            0
        } else {
            decode_base64_byte(chunk[2])?
        };
        let d = if chunk[3] == b'=' {
            0
        } else {
            decode_base64_byte(chunk[3])?
        };
        if (!last && (chunk[2] == b'=' || chunk[3] == b'='))
            || (chunk[2] == b'=' && chunk[3] != b'=')
        {
            return Err("invalid Base64 padding placement".to_string());
        }
        let value =
            (u32::from(a) << 18) | (u32::from(b) << 12) | (u32::from(c) << 6) | u32::from(d);
        output.push((value >> 16) as u8);
        if chunk[2] != b'=' {
            output.push((value >> 8) as u8);
        }
        if chunk[3] != b'=' {
            output.push(value as u8);
        }
    }
    Ok(output)
}

fn decode_base64_byte(byte: u8) -> Result<u8, String> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err("invalid Base64 character".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_only_metadata_in_the_reference() {
        let root = std::env::temp_dir().join(format!("rustpilot-attachments-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("test root should be created");
        let references = store_inputs(
            &root,
            "task-1",
            &[AttachmentInput {
                name: "../hello.txt".to_string(),
                mime: "text/plain".to_string(),
                data: "aGVsbG8=".to_string(),
            }],
        )
        .expect("attachment should store");
        assert_eq!(references[0].name, "hello.txt");
        assert_eq!(
            read(&root, &references[0]).expect("attachment should read"),
            b"hello"
        );
        assert!(references[0].storage_key.starts_with("attachments/task-1/"));
        remove_task(&root, "task-1").expect("task attachments should remove");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_mismatched_image_bytes() {
        let root = std::env::temp_dir().join(format!("rustpilot-attachments-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("test root should be created");
        let result = store_inputs(
            &root,
            "task-1",
            &[AttachmentInput {
                name: "image.png".to_string(),
                mime: "image/png".to_string(),
                data: "aGVsbG8=".to_string(),
            }],
        );
        assert!(result.is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn stores_native_path_inputs_without_persisting_the_source_path() {
        let root = std::env::temp_dir().join(format!("rustpilot-path-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).expect("test root should be created");
        let source = root.join("source.txt");
        fs::write(&source, "native file").expect("source should be written");
        let references = store_paths(
            &root,
            "task-1",
            &[AttachmentPathInput {
                path: source.to_string_lossy().to_string(),
                name: String::new(),
                mime: "text/plain".to_string(),
            }],
        )
        .expect("native path should store");
        assert_eq!(references[0].name, "source.txt");
        assert_eq!(
            read(&root, &references[0]).expect("attachment should read"),
            b"native file"
        );
        assert!(!references[0]
            .storage_key
            .contains(source.to_string_lossy().as_ref()));
        remove_task(&root, "task-1").expect("task attachments should remove");
        let _ = fs::remove_dir_all(root);
    }
}
