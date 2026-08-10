use std::path::Path;

use serde_json::Value;

use super::{argument, bool_argument, relative_path, scoped_path, truncate_text};

pub(super) fn status(root: &Path) -> Result<String, String> {
    run(
        root,
        &["status", "--short", "--branch", "--untracked-files=normal"],
        None,
    )
}

pub(super) fn diff(root: &Path, arguments: &Value) -> Result<String, String> {
    let mut args = vec!["diff", "--no-ext-diff", "--unified=3"];
    if bool_argument(arguments, "staged") {
        args.push("--cached");
    }
    let relative = argument(arguments, "path")
        .map(|path| scoped_path(root, &path).map(|resolved| relative_path(root, &resolved)))
        .transpose()?
        .unwrap_or_default();
    args.push("--");
    if !relative.is_empty() && relative != "." {
        args.push(&relative);
    }
    run(
        root,
        &args,
        Some("Working tree is clean for the selected path."),
    )
}

fn run(root: &Path, args: &[&str], empty_message: Option<&str>) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .map_err(|error| format!("Unable to run git: {error}"))?;
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(stderr.trim());
    }
    if !output.status.success() {
        return Err(truncate_text(
            &text,
            &format!("git {} failed", args.join(" ")),
        ));
    }
    if text.trim().is_empty() {
        Ok(empty_message.unwrap_or("No git output.").to_string())
    } else {
        Ok(truncate_text(&text, ""))
    }
}
