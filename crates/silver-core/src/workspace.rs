//! Workspace registration and the path-confinement boundary (INV-6, INV-7).

use crate::error::{CoreError, CoreResult};
use chrono::{DateTime, Utc};
use silver_protocol::WorkspaceId;
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    /// Path as supplied by the user.
    pub root: PathBuf,
    /// Canonicalised, persisted root used for every validation.
    pub canonical_root: PathBuf,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Workspace {
    /// Validate and canonicalise a candidate root. The directory must exist.
    pub fn canonicalize_root(path: &Path) -> CoreResult<PathBuf> {
        let canonical = std::fs::canonicalize(path).map_err(|e| {
            CoreError::WorkspaceUnavailable(match e.kind() {
                std::io::ErrorKind::NotFound => format!(
                    "{} doesn't exist; check the path or create the folder first",
                    path.display()
                ),
                kind => format!("{} can't be used as a folder: {kind}", path.display()),
            })
        })?;
        if !canonical.is_dir() {
            return Err(CoreError::WorkspaceUnavailable(format!(
                "{} is not a directory",
                canonical.display()
            )));
        }
        Ok(canonical)
    }

    /// Resolve a model- or client-supplied path inside this workspace (see [confine_path]).
    pub fn resolve_path(&self, requested: &Path) -> CoreResult<PathBuf> {
        confine_path(&self.canonical_root, requested)
    }
}

/// Confine a path to a canonical root: no implicit `~`, relative paths resolve
/// against the root, `..` segments are removed, and the nearest existing ancestor is canonicalised
/// with the rest re-checked, so a symlink out is rejected.
pub fn confine_path(canonical_root: &Path, requested: &Path) -> CoreResult<PathBuf> {
    let raw = requested.to_string_lossy();
    if raw.is_empty() {
        return Err(CoreError::PathOutsideWorkspace("empty path".into()));
    }
    if raw == "~" || raw.starts_with("~/") || raw.starts_with("~\\") {
        return Err(CoreError::PathOutsideWorkspace(
            "tilde paths are not implicitly authorised".into(),
        ));
    }

    let rooted = Path::new("/").join(requested);
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else if rooted.starts_with(canonical_root) && !canonical_root.join(requested).exists() {
        // Small models copy the absolute root from the prompt and drop its leading slash
        // ("tmp/ws/src/main.rs"); a relative path that spells out the root means the root.
        rooted
    } else {
        canonical_root.join(requested)
    };
    // Canonicalise the longest existing prefix, then re-append the rest. Without one, the
    // re-appended path is the normalized request itself.
    let mut existing = lexical_normalize(&joined);
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let real = loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            break Some(real);
        }
        let Some(name) = existing.file_name().map(std::ffi::OsStr::to_os_string) else {
            break None;
        };
        if !existing.pop() {
            break None;
        }
        tail.push(name);
    };
    let found = real.is_some();
    let mut resolved = real.unwrap_or(existing);
    for part in tail.iter().rev() {
        resolved.push(part);
    }
    if found && resolved.starts_with(canonical_root) {
        return Ok(resolved);
    }
    Err(CoreError::PathOutsideWorkspace(
        resolved.display().to_string(),
    ))
}

/// Remove dot and dot-dot components lexically (no filesystem access).
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
