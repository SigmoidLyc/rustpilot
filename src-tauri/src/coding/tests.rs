use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use super::{execute, is_mutation};

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "rustpilot-coding-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("temporary root should be created");
    root
}

#[test]
fn read_and_grep_are_bounded_and_numbered() {
    let root = temp_root("read");
    fs::create_dir_all(root.join("src")).expect("source directory should be created");
    fs::write(root.join("src/main.rs"), "fn main() {}\nneedle here\n")
        .expect("source should be written");

    let read = execute(
        &root,
        &serde_json::json!({
            "operation": "read",
            "path": "src/main.rs",
            "line_start": 2,
            "line_end": 2
        }),
        false,
    )
    .expect("read should succeed");
    assert!(read.contains("2 | needle here"));

    let grep = execute(
        &root,
        &serde_json::json!({"operation": "grep", "pattern": "needle", "path": "src"}),
        false,
    )
    .expect("grep should succeed");
    assert!(grep.contains("src/main.rs:2: needle here"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn replace_requires_unique_match_unless_replace_all_is_requested() {
    let root = temp_root("replace");
    let path = root.join("main.rs");
    fs::write(&path, "value\nvalue\n").expect("source should be written");

    assert!(execute(
        &root,
        &serde_json::json!({
            "operation": "replace",
            "path": "main.rs",
            "old_text": "value",
            "new_text": "changed"
        }),
        false,
    )
    .is_err());

    let result = execute(
        &root,
        &serde_json::json!({
            "operation": "replace",
            "path": "main.rs",
            "old_text": "value",
            "new_text": "changed",
            "replace_all": true
        }),
        false,
    )
    .expect("replace_all should succeed");
    assert!(result.contains("Replaced 2 occurrences"));
    assert_eq!(
        fs::read_to_string(path).expect("source should be readable"),
        "changed\nchanged\n"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn diagnostics_rejects_unknown_backends_without_spawning_processes() {
    let root = temp_root("diagnostics");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest should be written");
    let result = execute(
        &root,
        &serde_json::json!({"operation": "check", "backend": "shell"}),
        false,
    );
    assert!(result.is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn diagnostics_runs_a_minimal_cargo_check_with_bounded_output() {
    let root = temp_root("diagnostics-success");
    fs::create_dir_all(root.join("src")).expect("source directory should be created");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest should be written");
    fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("source should be written");

    let result = execute(
        &root,
        &serde_json::json!({
            "operation": "check",
            "backend": "cargo",
            "offline": true,
            "timeout": 30
        }),
        false,
    )
    .expect("minimal Cargo project should check");
    assert!(result.contains("backend: cargo"));
    assert!(result.contains("status: passed"));
    assert!(result.contains("exit_code: 0"));
    assert!(result.chars().count() <= super::MAX_OUTPUT_CHARS + 64);

    let _ = fs::remove_dir_all(root);
}

#[test]
fn read_rejects_paths_outside_the_workspace() {
    let root = temp_root("read-guard");
    let outside = temp_root("read-outside");
    fs::write(outside.join("secret.txt"), "outside\n").expect("outside file should be written");

    let result = execute(
        &root,
        &serde_json::json!({
            "operation": "read",
            "path": "../read-outside/secret.txt"
        }),
        false,
    );
    assert!(result.is_err());

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(outside);
}

#[test]
fn glob_skips_generated_directories() {
    let root = temp_root("glob");
    fs::create_dir_all(root.join("target")).expect("target directory should be created");
    fs::write(root.join("main.rs"), "fn main() {}\n").expect("source should be written");
    fs::write(root.join("target/generated.rs"), "generated\n")
        .expect("generated file should be written");

    let output = execute(
        &root,
        &serde_json::json!({"operation": "glob", "pattern": "**/*.rs"}),
        false,
    )
    .expect("glob should succeed");
    assert!(output.contains("main.rs"));
    assert!(!output.contains("generated.rs"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn search_limits_do_not_report_exact_matches_as_truncated() {
    let root = temp_root("search-limits");
    fs::write(root.join("a.rs"), "Needle one\nNeedle two\n").expect("source should be written");
    fs::write(root.join("b.rs"), "other\n").expect("source should be written");

    let grep = execute(
        &root,
        &serde_json::json!({
            "operation": "grep",
            "pattern": "needle",
            "case_sensitive": false,
            "max_results": 2
        }),
        false,
    )
    .expect("grep should succeed");
    assert_eq!(grep.matches("a.rs:").count(), 2);
    assert!(!grep.contains("[output truncated]"));

    let glob = execute(
        &root,
        &serde_json::json!({"operation": "glob", "pattern": "*.rs", "limit": 2}),
        false,
    )
    .expect("glob should succeed");
    assert_eq!(glob.lines().count(), 2);
    assert!(!glob.contains("[output truncated]"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn apply_patch_supports_openai_and_unified_forms() {
    let root = temp_root("patch");
    fs::write(root.join("main.rs"), "one\ntwo\nthree\n").expect("source should be written");

    execute(
        &root,
        &serde_json::json!({
            "operation": "apply_patch",
            "patch": "*** Begin Patch\n*** Update File: main.rs\n@@\n two\n-three\n+four\n*** End Patch"
        }),
        false,
    )
    .expect("apply_patch form should succeed");
    assert_eq!(
        fs::read_to_string(root.join("main.rs")).expect("source should be readable"),
        "one\ntwo\nfour\n"
    );

    execute(
        &root,
        &serde_json::json!({
            "operation": "apply_patch",
            "patch": "--- a/main.rs\n+++ b/main.rs\n@@ -1,2 +1,2 @@\n-one\n-two\n+alpha\n+beta"
        }),
        false,
    )
    .expect("unified patch should succeed");
    assert_eq!(
        fs::read_to_string(root.join("main.rs")).expect("source should be readable"),
        "alpha\nbeta\nfour\n"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn multi_file_patch_validates_every_file_before_writing() {
    let root = temp_root("patch-preflight");
    fs::write(root.join("first.txt"), "old\n").expect("first file should be written");

    let result = execute(
        &root,
        &serde_json::json!({
            "operation": "apply_patch",
            "patch": "*** Begin Patch\n*** Update File: first.txt\n@@\n-old\n+new\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch"
        }),
        false,
    );
    assert!(result.is_err());
    assert_eq!(
        fs::read_to_string(root.join("first.txt")).expect("first file should remain readable"),
        "old\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn unified_delete_checks_patch_context() {
    let root = temp_root("patch-delete");
    fs::write(root.join("delete.txt"), "keep\n").expect("file should be written");

    let result = execute(
        &root,
        &serde_json::json!({
            "operation": "apply_patch",
            "patch": "--- a/delete.txt\n+++ /dev/null\n@@ -1 +1 @@\n-remove\n"
        }),
        false,
    );
    assert!(result.is_err());
    assert!(root.join("delete.txt").exists());

    let _ = fs::remove_dir_all(root);
}

#[test]
fn patch_rejects_duplicate_paths() {
    let root = temp_root("patch-duplicate");
    fs::write(root.join("same.txt"), "old\n").expect("file should be written");

    let result = execute(
        &root,
        &serde_json::json!({
            "operation": "apply_patch",
            "patch": "*** Begin Patch\n*** Update File: same.txt\n@@\n-old\n+one\n*** Update File: same.txt\n@@\n-old\n+two\n*** End Patch"
        }),
        false,
    );
    assert!(result.is_err());
    assert_eq!(
        fs::read_to_string(root.join("same.txt")).expect("file should remain readable"),
        "old\n"
    );

    let _ = fs::remove_dir_all(root);
}

#[test]
fn mutation_and_path_guards_are_explicit() {
    let root = temp_root("guard");
    assert!(is_mutation(
        &serde_json::json!({"operation": "apply_patch"})
    ));
    assert!(!is_mutation(&serde_json::json!({"operation": "grep"})));
    assert!(execute(
        &root,
        &serde_json::json!({"operation": "write", "path": "../outside", "content": "x"}),
        true,
    )
    .is_err());
    let _ = fs::remove_dir_all(root);
}
