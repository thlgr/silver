//! The per-user directories silver keeps its state in and its file guards protect.

use directories::{BaseDirs, ProjectDirs};
use std::path::PathBuf;

/// silver's platform config and data directories.
pub fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("dev", "silver", "silver")
}

/// The user's home directory: `$HOME` on Unix, the profile folder on Windows.
pub fn home_dir() -> Option<PathBuf> {
    BaseDirs::new().map(|base| base.home_dir().to_path_buf())
}
