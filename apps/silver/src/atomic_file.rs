//! Replace a file in one step: a same-directory temp file renamed over the target, so a reader
//! sees the old contents or the new, never a partial write.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace(path, bytes, 0o666)
}

/// [write] for a file that holds a secret: the temp file is created 0600, so the secret is
/// never readable under the umask, even briefly.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace(path, bytes, 0o600)
}

fn replace(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let temp = path.with_file_name(format!(".{name}.{}.tmp", uuid::Uuid::now_v7().simple()));
    let result = create(&temp, mode)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&temp, path));
    if result.is_err() {
        drop(fs::remove_file(&temp));
    }
    result
}

#[cfg(unix)]
fn create(path: &Path, mode: u32) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
}

#[cfg(not(unix))]
fn create(path: &Path, _mode: u32) -> io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}
