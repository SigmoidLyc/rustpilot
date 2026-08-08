//! Dependency-free filesystem boundary checks for tool mutations.
//!
//! The guard follows the active workspace, resolves existing ancestors, and
//! rejects relative paths that escape through `..` or symbolic links. An
//! absolute path outside the workspace is allowed only after the caller has
//! completed the explicit approval flow.

use std::{
    fs,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathScope {
    Workspace,
    External,
}

impl PathScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::External => "external",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResolvedPath {
    pub canonical: PathBuf,
    pub scope: PathScope,
    pub existed: bool,
}

pub fn resolve_scoped_path(scope_root: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.trim().is_empty() {
        return Err("A non-empty scoped path is required.".to_string());
    }

    let root = canonical_directory(scope_root, "scoped root")?;
    let requested = PathBuf::from(raw);
    let candidate = if requested.is_absolute() {
        requested
    } else {
        root.join(requested)
    };
    let lexical = normalize_lexical(&candidate);
    if !is_within(&root, &lexical) {
        return Err(format!("Scoped path escapes its root: {raw}"));
    }
    let (canonical, _) = resolve_existing_or_missing(&lexical)?;
    if !is_within(&root, &canonical) {
        return Err(format!("Scoped path escapes through a link: {raw}"));
    }
    Ok(canonical)
}

pub fn resolve_mutation_path(
    workspace_root: &Path,
    raw: &str,
    external_approved: bool,
) -> Result<ResolvedPath, String> {
    if raw.trim().is_empty() {
        return Err("A non-empty mutation path is required.".to_string());
    }

    let root = canonical_directory(workspace_root, "workspace root")?;
    let requested = PathBuf::from(raw);
    let requested_is_absolute = requested.is_absolute();
    let candidate = if requested_is_absolute {
        requested.clone()
    } else {
        root.join(&requested)
    };
    let lexical = normalize_lexical(&candidate);
    let lexical_internal = is_within(&root, &lexical);
    let (canonical, existed) = resolve_existing_or_missing(&lexical)?;
    let canonical_internal = is_within(&root, &canonical);

    // A relative path or a path lexically inside the workspace must not jump
    // outside through a symlink or junction.
    if lexical_internal && !canonical_internal {
        return Err(format!(
            "Path escapes the active workspace through a link: {}",
            raw
        ));
    }
    if !requested_is_absolute && !canonical_internal {
        return Err(format!(
            "Relative path escapes the active workspace: {}",
            raw
        ));
    }

    let scope = if canonical_internal {
        PathScope::Workspace
    } else {
        PathScope::External
    };
    if scope == PathScope::External && !external_approved {
        return Err(format!(
            "External path requires explicit approval: {}",
            canonical.display()
        ));
    }
    if canonical == root {
        return Err("The workspace root itself cannot be mutated.".to_string());
    }

    Ok(ResolvedPath {
        canonical,
        scope,
        existed,
    })
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("Unable to resolve {label}: {error}"))?;
    if !canonical.is_dir() {
        return Err(format!(
            "The {label} is not a directory: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn resolve_existing_or_missing(path: &Path) -> Result<(PathBuf, bool), String> {
    let mut cursor = path.to_path_buf();
    let mut missing = Vec::new();

    loop {
        match fs::symlink_metadata(&cursor) {
            Ok(_) => {
                let canonical = cursor.canonicalize().map_err(|error| {
                    format!(
                        "Unable to resolve mutation path {}: {error}",
                        path.display()
                    )
                })?;
                if !missing.is_empty()
                    && !fs::metadata(&canonical)
                        .map(|metadata| metadata.is_dir())
                        .unwrap_or(false)
                {
                    return Err(format!(
                        "A parent of the mutation path is not a directory: {}",
                        path.display()
                    ));
                }
                let existed = missing.is_empty();
                let mut resolved = canonical;
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok((resolved, existed));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let file_name = cursor
                    .file_name()
                    .ok_or_else(|| format!("Unable to resolve mutation path {}", path.display()))?;
                missing.push(file_name.to_os_string());
                if !cursor.pop() {
                    return Err(format!(
                        "Unable to find an existing parent for mutation path {}",
                        path.display()
                    ));
                }
            }
            Err(error) => {
                return Err(format!(
                    "Unable to inspect mutation path {}: {error}",
                    path.display()
                ));
            }
        }
    }
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() && !normalized.has_root() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn is_within(root: &Path, candidate: &Path) -> bool {
    let mut root_text = root.to_string_lossy().replace('\\', "/");
    let mut candidate_text = candidate.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        root_text = root_text.to_ascii_lowercase();
        candidate_text = candidate_text.to_ascii_lowercase();
    }

    if root_text == "/" {
        return candidate_text.starts_with('/');
    }
    let root_text = root_text.trim_end_matches('/');
    candidate_text == root_text || candidate_text.starts_with(&format!("{root_text}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("rustpilot-path-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("test root should be created");
        root
    }

    #[test]
    fn allows_workspace_path_and_missing_file() {
        let root = temp_root("inside");
        let result = resolve_mutation_path(&root, "nested/output.json", false)
            .expect("workspace path should be allowed");
        assert_eq!(result.scope, PathScope::Workspace);
        assert!(!result.existed);
        assert!(result.canonical.ends_with(Path::new("nested/output.json")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_relative_escape_even_after_approval() {
        let root = temp_root("relative-escape");
        let result = resolve_mutation_path(&root, "../outside.txt", true);
        assert!(result.is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn scoped_paths_allow_root_and_reject_escape() {
        let root = temp_root("scoped");
        assert_eq!(
            resolve_scoped_path(&root, ".").expect("scoped root should be allowed"),
            root.canonicalize().expect("root should canonicalize")
        );
        let escape = Path::new("..").join("outside.txt");
        assert!(resolve_scoped_path(&root, &escape.to_string_lossy()).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn requires_approval_for_explicit_external_path() {
        let root = temp_root("external");
        let outside =
            std::env::temp_dir().join(format!("rustpilot-outside-{}", std::process::id()));
        let without_approval = resolve_mutation_path(&root, &outside.to_string_lossy(), false);
        assert!(without_approval.is_err());
        let with_approval = resolve_mutation_path(&root, &outside.to_string_lossy(), true)
            .expect("explicitly approved external path should be allowed");
        assert_eq!(with_approval.scope, PathScope::External);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_relative_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = temp_root("unix-link");
        let outside = temp_root("unix-outside");
        symlink(&outside, root.join("link")).expect("test symlink should be created");
        assert!(resolve_mutation_path(&root, "link/output.txt", true).is_err());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[cfg(windows)]
    #[test]
    fn rejects_relative_junction_or_symlink_escape() {
        use std::os::windows::fs::symlink_dir;

        let root = temp_root("windows-link");
        let outside = temp_root("windows-outside");
        if symlink_dir(&outside, root.join("link")).is_err() {
            let _ = fs::remove_dir_all(root);
            let _ = fs::remove_dir_all(outside);
            return;
        }
        assert!(resolve_mutation_path(&root, "link\\output.txt", true).is_err());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }
}
