//! Filesystem checkpoints: a file's bytes before write_file / patch change it, kept under
//! `<data_dir>/checkpoints/<id>/<relative path>` and indexed in the checkpoints table. The row id
//! is generated on insert, so the bytes are staged in a temp directory and renamed to it after.

use crate::config::CheckpointsConfig;
use crate::db::{Checkpoint, Db};
use async_trait::async_trait;
use silver_core::error::{CoreError, CoreResult};
use silver_core::services::{CheckpointKind, CheckpointSink, CheckpointTarget};
use silver_protocol::SessionId;
use std::path::{Path, PathBuf};

/// Store directory name under the daemon data directory.
pub const CHECKPOINT_DIR: &str = "checkpoints";

/// Prefix for in-flight snapshot directories a crash may have left behind.
const TEMP_PREFIX: &str = ".tmp-";

/// Filesystem checkpoint store; run prune_on_startup to bound it.
pub struct Checkpoints {
    db: Db,
    base: PathBuf,
    enabled: bool,
    max_snapshots: u32,
    max_bytes: u64,
}

impl Checkpoints {
    /// Build the store rooted at <data_dir>/checkpoints.
    pub fn new(db: Db, data_dir: &Path, config: &CheckpointsConfig) -> Self {
        let base = data_dir.join(CHECKPOINT_DIR);
        let store = Self {
            db,
            base,
            enabled: config.enabled,
            max_snapshots: config.max_snapshots,
            max_bytes: config.max_bytes,
        };
        if store.enabled {
            if let Err(error) = create_private_dir(&store.base) {
                tracing::warn!(
                    path = %store.base.display(),
                    %error,
                    "could not create checkpoint store"
                );
            }
        }
        store
    }

    /// The store root (<data_dir>/checkpoints).
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// Whether snapshots are captured.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Capture the pre-mutation state of one path. Best-effort: see CheckpointSink.
    async fn capture(&self, target: &CheckpointTarget) -> CoreResult<()> {
        let relative = sanitize_relative(&target.path);
        let exists = target.absolute_path.is_file();
        // Delete is preserved only when the bytes are still there; a vanished Replace is a
        // Create so restore removes whatever the write produces instead of failing.
        let kind = match target.kind {
            CheckpointKind::Delete if exists => CheckpointKind::Delete,
            _ if exists => CheckpointKind::Replace,
            _ => CheckpointKind::Create,
        };
        let snapshot = kind != CheckpointKind::Create;
        let bytes = if snapshot {
            std::fs::metadata(&target.absolute_path)
                .map(|meta| meta.len())
                .unwrap_or(0) as i64
        } else {
            0
        };

        // Stage under a temp directory first so an interrupted capture never leaves a row
        // pointing at a half-written snapshot.
        let staged = if snapshot {
            Some(self.stage(&target.absolute_path, &relative)?)
        } else {
            None
        };

        let snapshot_dir = self.base.to_string_lossy().to_string();
        let recorded = match self
            .db
            .record_checkpoint(
                target.session_id,
                target.run_id,
                &target.path,
                kind.as_str(),
                bytes,
                Some(&snapshot_dir),
            )
            .await
        {
            Ok(row) => row,
            Err(error) => {
                if let Some(temp) = &staged {
                    drop(std::fs::remove_dir_all(temp));
                }
                return Err(CoreError::Internal(format!("record checkpoint: {error}")));
            }
        };

        let final_dir = self.base.join(&recorded.id);
        drop(std::fs::remove_dir_all(&final_dir));
        match &staged {
            Some(temp) => std::fs::rename(temp, &final_dir).map_err(|error| {
                CoreError::Internal(format!("store checkpoint {}: {error}", final_dir.display()))
            })?,
            None => create_private_dir(&final_dir).map_err(|error| {
                CoreError::Internal(format!("store checkpoint {}: {error}", final_dir.display()))
            })?,
        }
        Ok(())
    }

    /// Copy the live bytes of one path into a fresh temp directory.
    fn stage(&self, source: &Path, relative: &Path) -> CoreResult<PathBuf> {
        let temp = self
            .base
            .join(format!("{TEMP_PREFIX}{}", uuid::Uuid::now_v7()));
        create_private_dir(&temp)
            .map_err(|error| CoreError::Internal(format!("create {}: {error}", temp.display())))?;
        let dest = temp.join(relative);
        if let Some(parent) = dest.parent() {
            if let Err(error) = create_private_dir(parent) {
                drop(std::fs::remove_dir_all(&temp));
                return Err(CoreError::Internal(format!(
                    "create {}: {error}",
                    parent.display()
                )));
            }
        }
        if let Err(error) = std::fs::copy(source, &dest) {
            drop(std::fs::remove_dir_all(&temp));
            return Err(CoreError::Internal(format!(
                "copy {}: {error}",
                source.display()
            )));
        }
        Ok(temp)
    }

    /// Restore one checkpoint into workspace_root.
    pub fn restore(&self, checkpoint: &Checkpoint, workspace_root: &Path) -> CoreResult<()> {
        let relative = sanitize_relative(&checkpoint.path);
        // The file may since have become a symlink out of the workspace; confine the target
        // exactly as the write tools do, so a restore never writes outside it.
        let target = silver_core::workspace::confine_path(workspace_root, &relative)?;
        match checkpoint.kind.as_str() {
            "create" => match std::fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(CoreError::Internal(format!(
                        "restore {}: {error}",
                        target.display()
                    )))
                }
            },
            "replace" | "delete" => {
                let source = self.base.join(&checkpoint.id).join(&relative);
                let bytes = std::fs::read(&source).map_err(|error| {
                    CoreError::Internal(format!("read snapshot {}: {error}", source.display()))
                })?;
                write_bytes(&target, &bytes)?;
            }
            other => {
                return Err(CoreError::Internal(format!(
                    "cannot restore checkpoint {} with unknown kind '{other}'",
                    checkpoint.id
                )))
            }
        }
        Ok(())
    }

    /// Restore every captured checkpoint for a session, newest first so the oldest snapshot
    /// wins (the session's starting state). Returns how many rows were applied.
    pub async fn restore_session(
        &self,
        session_id: SessionId,
        workspace_root: &Path,
        limit: usize,
    ) -> CoreResult<usize> {
        let rows = self
            .db
            .list_checkpoints(session_id, limit)
            .await
            .map_err(|error| CoreError::Internal(format!("list checkpoints: {error}")))?;
        let mut restored = 0;
        for row in &rows {
            self.restore(row, workspace_root)?;
            restored += 1;
        }
        Ok(restored)
    }

    /// Bound the index to max_snapshots newest rows and the byte store to
    /// checkpoints.max_bytes, deleting the snapshot directories of dropped rows. Returns the
    /// number of index rows removed. Run at daemon start.
    pub async fn prune_checkpoints(&self, max_snapshots: usize) -> CoreResult<usize> {
        let mut removed = self
            .db
            .prune_checkpoints(max_snapshots)
            .await
            .map_err(|error| CoreError::Internal(format!("prune checkpoints: {error}")))?;
        self.sweep_unreferenced().await?;

        if self.max_bytes > 0 {
            loop {
                if self.store_bytes() <= self.max_bytes {
                    break;
                }
                let count = self.referenced_dirs().await?.len();
                if count <= 1 {
                    break;
                }
                let dropped = self
                    .db
                    .prune_checkpoints(count - 1)
                    .await
                    .map_err(|error| CoreError::Internal(format!("prune checkpoints: {error}")))?;
                if dropped == 0 {
                    break;
                }
                removed += dropped;
                self.sweep_unreferenced().await?;
            }
        }
        Ok(removed)
    }

    /// Apply the configured snapshot cap at daemon start.
    pub async fn prune_on_startup(&self) -> CoreResult<usize> {
        self.prune_checkpoints(self.max_snapshots as usize).await
    }

    /// Directories whose name is a live checkpoint id, oldest first (UUIDv7 is time-ordered).
    async fn referenced_dirs(&self) -> CoreResult<Vec<PathBuf>> {
        let mut dirs = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.base) else {
            return Ok(dirs);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(TEMP_PREFIX) {
                continue;
            }
            if self
                .db
                .get_checkpoint(name)
                .await
                .map_err(|error| CoreError::Internal(format!("get checkpoint {name}: {error}")))?
                .is_some()
            {
                dirs.push(path);
            }
        }
        dirs.sort();
        Ok(dirs)
    }

    /// Remove temp directories and snapshot directories whose row no longer exists.
    async fn sweep_unreferenced(&self) -> CoreResult<()> {
        let Ok(entries) = std::fs::read_dir(&self.base) else {
            return Ok(());
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if name.starts_with(TEMP_PREFIX) {
                drop(std::fs::remove_dir_all(&path));
                continue;
            }
            match self.db.get_checkpoint(name).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    drop(std::fs::remove_dir_all(&path));
                }
                Err(error) => {
                    return Err(CoreError::Internal(format!(
                        "get checkpoint {name}: {error}"
                    )))
                }
            }
        }
        Ok(())
    }

    /// Total bytes of the snapshot directories (temp directories excluded).
    fn store_bytes(&self) -> u64 {
        let Ok(entries) = std::fs::read_dir(&self.base) else {
            return 0;
        };
        let mut total = 0u64;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let is_temp = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(TEMP_PREFIX));
            if !is_temp {
                total += dir_size(&path);
            }
        }
        total
    }
}

#[async_trait]
impl CheckpointSink for Checkpoints {
    async fn before_write(&self, target: CheckpointTarget) -> CoreResult<()> {
        if !self.enabled {
            return Ok(());
        }
        self.capture(&target).await
    }
}

/// Map an arbitrary model-supplied path onto a safe path under a checkpoint directory.
/// Drops traversal, absolute roots and empty components; rewrites characters that are
/// illegal or dangerous on common filesystems.
fn sanitize_relative(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.split(['/', '\\']) {
        let component = component.trim();
        if component.is_empty() || component == "." || component == ".." {
            continue;
        }
        let cleaned: String = component
            .chars()
            .map(|ch| match ch {
                '\0' => '_',
                ch if ch.is_control() => '_',
                '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
                ch => ch,
            })
            .collect();
        if cleaned.is_empty() {
            continue;
        }
        out.push(cleaned);
    }
    if out.as_os_str().is_empty() {
        out.push("file");
    }
    out
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

fn write_bytes(path: &Path, bytes: &[u8]) -> CoreResult<()> {
    if let Some(parent) = path.parent() {
        create_private_dir(parent).map_err(|error| {
            CoreError::Internal(format!("create {}: {error}", parent.display()))
        })?;
    }
    std::fs::write(path, bytes)
        .map_err(|error| CoreError::Internal(format!("write {}: {error}", path.display())))
}

fn dir_size(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        let child = entry.path();
        if child.is_dir() {
            total += dir_size(&child);
        } else if let Ok(metadata) = child.metadata() {
            total += metadata.len();
        }
    }
    total
}
