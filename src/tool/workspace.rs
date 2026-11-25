//! Workspace confinement for mutating file tools (`write`, `edit`, `delete`).
//!
//! Writes must land inside the configured roots (project root plus any
//! `permissions.extra_write_roots`). Paths are resolved through symlinks, `..`
//! cannot be used to escape, and `.git` internals and `.env*` files are always
//! protected. Reads stay unrestricted. When no roots are installed (library
//! use, unit tests) nothing is confined.

use crate::tool::tool_trait::ToolError;
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;

static ROOTS: RwLock<Option<Vec<PathBuf>>> = RwLock::new(None);

/// Install the writable roots for this process. Roots that cannot be
/// canonicalized are skipped; an empty result leaves writes unconfined.
pub fn set_roots(roots: impl IntoIterator<Item = PathBuf>) {
    let roots: Vec<PathBuf> = roots
        .into_iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();
    *ROOTS.write().unwrap_or_else(|e| e.into_inner()) = (!roots.is_empty()).then_some(roots);
}

/// Validate that `path` may be mutated under the installed roots.
pub fn guard_write(path: &Path) -> Result<(), ToolError> {
    let roots = ROOTS.read().unwrap_or_else(|e| e.into_inner());
    match roots.as_deref() {
        Some(roots) => check_write_in(roots, path),
        None => Ok(()),
    }
}

/// Core check against explicit (already canonical) roots.
pub fn check_write_in(roots: &[PathBuf], path: &Path) -> Result<(), ToolError> {
    let resolved = resolve(path)?;
    let Some((root, rel)) = roots
        .iter()
        .find_map(|r| resolved.strip_prefix(r).ok().map(|rel| (r, rel)))
    else {
        return Err(denied(path, "outside the workspace roots"));
    };
    if rel.as_os_str().is_empty() {
        return Err(denied(
            path,
            &format!("is a workspace root ({})", root.display()),
        ));
    }
    for comp in rel.components() {
        if let Component::Normal(name) = comp {
            let name = name.to_string_lossy().to_ascii_lowercase();
            if name == ".git" {
                return Err(denied(path, "is inside .git, which is protected"));
            }
        }
    }
    if let Some(name) = rel
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        && (name == ".env" || name.starts_with(".env."))
    {
        return Err(denied(
            path,
            "is an environment/secrets file, which is protected",
        ));
    }
    Ok(())
}

fn denied(path: &Path, why: &str) -> ToolError {
    ToolError::ExecutionFailed(format!("write to '{}' blocked: path {why}", path.display()))
}

/// Make `path` absolute and resolve symlinks in its existing prefix. The
/// not-yet-existing tail may not contain `..`.
fn resolve(path: &Path) -> Result<PathBuf, ToolError> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| ToolError::ExecutionFailed(format!("cannot resolve cwd: {e}")))?
            .join(path)
    };

    let mut existing = abs;
    let mut tail = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => tail.push(name.to_os_string()),
            None => return Err(denied(path, "cannot be resolved")),
        }
        if !existing.pop() {
            return Err(denied(path, "cannot be resolved"));
        }
    }
    let mut resolved = std::fs::canonicalize(&existing).map_err(|e| {
        ToolError::ExecutionFailed(format!("cannot resolve '{}': {e}", path.display()))
    })?;
    resolved.extend(tail.into_iter().rev());
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn roots(dir: &Path) -> Vec<PathBuf> {
        vec![std::fs::canonicalize(dir).unwrap()]
    }

    #[test]
    fn allows_new_and_existing_files_inside_root() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        let r = roots(dir.path());
        assert!(check_write_in(&r, &dir.path().join("a.txt")).is_ok());
        assert!(check_write_in(&r, &dir.path().join("new/deep/b.txt")).is_ok());
    }

    #[test]
    fn blocks_outside_root_and_dotdot_escape() {
        let dir = tempdir().unwrap();
        let other = tempdir().unwrap();
        let r = roots(dir.path());
        assert!(check_write_in(&r, &other.path().join("x.txt")).is_err());
        assert!(check_write_in(&r, &dir.path().join("missing/../../x.txt")).is_err());
    }

    #[test]
    fn protects_git_env_and_root() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let r = roots(dir.path());
        assert!(check_write_in(&r, &dir.path().join(".git/hooks/pre-commit")).is_err());
        assert!(check_write_in(&r, &dir.path().join(".env")).is_err());
        assert!(check_write_in(&r, &dir.path().join("sub/.env.local")).is_err());
        assert!(check_write_in(&r, dir.path()).is_err());
        assert!(check_write_in(&r, &dir.path().join(".gitignore")).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn blocks_symlink_escape() {
        let dir = tempdir().unwrap();
        let other = tempdir().unwrap();
        std::os::unix::fs::symlink(other.path(), dir.path().join("link")).unwrap();
        let r = roots(dir.path());
        assert!(check_write_in(&r, &dir.path().join("link/x.txt")).is_err());
    }
}
