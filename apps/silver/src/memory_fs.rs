//! Markdown memory under the data directory: `memory/global/` and `workspaces/ws_<id>/memory/`,
//! each with MEMORY.md and USER.md. Writes are serialised per scope and published atomically; a
//! strict read refuses to overwrite an unreadable file and quarantines poisoned entries.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use async_trait::async_trait;
use silver_core::memory::{
    apply_operation, canonical_content, content_hash, prompt_safe_content, MemoryChange,
    MemoryFile, MemoryOperation, MemorySnapshot, MemoryStore,
};
use silver_core::{CoreError, CoreResult};
use silver_protocol::Scope;
use tokio::sync::Mutex;

/// Cross-process lock retry policy: attempt count and exponential backoff bounds.
const LOCK_MAX_ATTEMPTS: u32 = 30;
const LOCK_INITIAL_DELAY_MS: u64 = 2;
const LOCK_MAX_DELAY_MS: u64 = 25;

/// Concurrency-safe filesystem implementation of the memory store.
#[derive(Clone)]
pub struct FsMemoryStore {
    data_dir: PathBuf,
    scope_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Content hash of the last raw view we loaded or wrote, keyed by scope + file name.
    /// A mismatch on the next read means the file changed under us (external edit or a
    /// concurrent session) and the write is refused with a '.bak' snapshot.
    baselines: Arc<StdMutex<HashMap<String, String>>>,
}

impl FsMemoryStore {
    /// Create a store rooted at the daemon data directory.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            scope_locks: Arc::new(Mutex::new(HashMap::new())),
            baselines: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// The daemon data directory this store writes under.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Directory holding the memory files for one scope.
    pub fn scope_dir(&self, scope: &Scope) -> PathBuf {
        match scope {
            Scope::Global => self.data_dir.join("memory").join("global"),
            Scope::Workspace(id) => self
                .data_dir
                .join("workspaces")
                .join(id.to_string())
                .join("memory"),
        }
    }

    fn scope_key(scope: &Scope) -> String {
        match scope {
            Scope::Global => "global".to_string(),
            Scope::Workspace(id) => id.to_string(),
        }
    }

    fn file_path(&self, scope: &Scope, file: MemoryFile) -> PathBuf {
        self.scope_dir(scope).join(file.file_name())
    }

    async fn scope_lock(&self, scope: &Scope) -> Arc<Mutex<()>> {
        let key = Self::scope_key(scope);
        let mut locks = self.scope_locks.lock().await;
        Arc::clone(locks.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))))
    }

    fn baseline_key(scope: &Scope, file: MemoryFile) -> String {
        format!("{}/{}", Self::scope_key(scope), file.file_name())
    }

    fn baseline(&self, scope: &Scope, file: MemoryFile) -> Option<String> {
        self.baselines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&Self::baseline_key(scope, file))
            .cloned()
    }

    fn set_baseline(&self, scope: &Scope, file: MemoryFile, raw: &str) {
        self.baselines
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(Self::baseline_key(scope, file), content_hash(raw));
    }

    /// Read both files. 'prompt_safe' quarantines poisoned entries for the system prompt;
    /// otherwise the canonical live entries are returned for read-modify-write editing.
    fn load_inner(&self, scope: &Scope, prompt_safe: bool) -> MemorySnapshot {
        let dir = self.scope_dir(scope);
        let mut snapshot = MemorySnapshot::default();
        for file in [MemoryFile::Memory, MemoryFile::User] {
            let raw = read_lenient(&dir.join(file.file_name()));
            self.set_baseline(scope, file, &raw);
            let content = if prompt_safe {
                prompt_safe_content(file, &raw)
            } else {
                canonical_content(&raw)
            };
            match file {
                MemoryFile::Memory => snapshot.memory = content,
                MemoryFile::User => snapshot.user = content,
            }
        }
        snapshot
    }
}

fn io_error(context: &str, path: &Path, err: &std::io::Error) -> CoreError {
    CoreError::Internal(format!("{context} {}: {err}", path.display()))
}

/// Lenient read used by the read-only load path: any failure maps to empty, matching
/// Hermes _read_file. Mutation paths use 'read_checked' instead.
fn read_lenient(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Strict read for the mutation path: None when the file does not exist, Err when it exists
/// but cannot be read or decoded (permission change, lock, invalid UTF-8, directory).
fn read_checked(path: &Path) -> CoreResult<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_error("failed to read memory file", path, &err)),
    }
}

fn file_name_of(path: &Path) -> &str {
    path.file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("memory")
}

/// '<file>.lock' next to the memory file so the memory file itself can be atomically
/// replaced while the lock is held.
fn lock_path_for(path: &Path) -> PathBuf {
    path.with_file_name(format!("{}.lock", file_name_of(path)))
}

/// '<file>.bak' snapshot written when external drift would otherwise be discarded.
fn backup_path_for(path: &Path) -> PathBuf {
    path.with_file_name(format!("{}.bak", file_name_of(path)))
}

/// Cross-process exclusive lock held by creating '<file>.lock' with create_new and released
/// (file removed) on drop. Bounded exponential retry avoids an unbounded spin.
struct FileLock {
    path: PathBuf,
}

impl FileLock {
    async fn acquire(path: PathBuf) -> CoreResult<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|err| io_error("failed to create memory dir", dir, &err))?;
        }
        let mut delay = Duration::from_millis(LOCK_INITIAL_DELAY_MS);
        for attempt in 0..LOCK_MAX_ATTEMPTS {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut handle) => {
                    use std::io::Write;
                    drop(write!(handle, "pid={}", std::process::id()));
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt + 1 >= LOCK_MAX_ATTEMPTS {
                        return Err(CoreError::Internal(format!(
                            "memory file is locked by another process ({}); retry in a moment",
                            path.display()
                        )));
                    }
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(LOCK_MAX_DELAY_MS));
                }
                Err(err) => return Err(io_error("failed to create memory lock", &path, &err)),
            }
        }
        unreachable!("the final attempt returns or errors")
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        drop(std::fs::remove_file(&self.path));
    }
}

#[async_trait]
impl MemoryStore for FsMemoryStore {
    /// Prompt-safe view: poisoned on-disk entries are quarantined with a placeholder so they
    /// never enter the system prompt. The raw file is untouched.
    async fn load(&self, scope: &Scope) -> CoreResult<MemorySnapshot> {
        Ok(self.load_inner(scope, true))
    }

    /// Live, unsanitized entries for the memory tool's read-modify-write.
    async fn load_raw(&self, scope: &Scope) -> CoreResult<MemorySnapshot> {
        Ok(self.load_inner(scope, false))
    }

    async fn apply(
        &self,
        scope: &Scope,
        file: MemoryFile,
        operation: &MemoryOperation,
    ) -> CoreResult<MemoryChange> {
        let lock = self.scope_lock(scope).await;
        let _guard = lock.lock().await;

        let path = self.file_path(scope, file);
        let _file_lock = FileLock::acquire(lock_path_for(&path)).await?;

        // Refuse to write when the file exists but cannot be read: treating an unreadable
        // file as empty and saving would wipe existing memory.
        let before = match read_checked(&path) {
            Ok(Some(text)) => text,
            Ok(None) => String::new(),
            Err(err) => {
                return Err(CoreError::Internal(format!(
                    "Refusing to write {}: the file exists on disk but could not be read right now. Nothing was changed; retry in a moment. ({err})",
                    path.display()
                )))
            }
        };

        // External drift: the bytes changed under us since the last load/write, so flushing
        // would discard content we never saw. Snapshot the previous bytes and refuse.
        if let Some(baseline) = self.baseline(scope, file) {
            if content_hash(&before) != baseline {
                let backup = backup_path_for(&path);
                std::fs::write(&backup, before.as_bytes())
                    .map_err(|err| io_error("failed to write memory backup", &backup, &err))?;
                return Err(CoreError::Internal(format!(
                    "Refusing to write {}: the file on disk changed since it was loaded (external edit, concurrent session, or shell append). A snapshot was saved to {}. Resolve the drift and retry.",
                    path.display(),
                    backup.display()
                )));
            }
        }

        let after = apply_operation(&before, operation)?;
        crate::atomic_file::write(&path, after.as_bytes())
            .map_err(|err| io_error("failed to write memory file", &path, &err))?;
        self.set_baseline(scope, file, &after);

        Ok(MemoryChange {
            file,
            operation: operation.kind(),
            before_hash: content_hash(&before),
            after_hash: content_hash(&after),
        })
    }
}
