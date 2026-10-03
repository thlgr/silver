//! Logging to stderr and rotating files, every line redacted first so a secret a call site logs
//! never reaches either. agent.log keeps INFO and up, errors.log WARN and up.

use silver_core::redact::redact;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use tracing_subscriber::filter::{EnvFilter, FilterExt, LevelFilter};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Rotate a sink once it grows past this many bytes.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
/// Number of rotated files kept per sink (agent.log.1 .. agent.log.N).
const MAX_ROTATED_FILES: u32 = 3;

/// Install the global subscriber: stderr plus <data_dir>/logs/{agent,errors}.log. Writes are
/// synchronous, so there is nothing to flush on shutdown.
pub fn init(data_dir: &Path) {
    let log_dir = data_dir.join("logs");
    drop(create_log_dir(&log_dir));
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(StderrMakeWriter)
        .with_filter(EnvFilter::clone(&env_filter));
    let agent_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(RollingMakeWriter::new(log_dir.join("agent.log")))
        .with_filter(EnvFilter::clone(&env_filter).and(LevelFilter::INFO));
    let errors_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(RollingMakeWriter::new(log_dir.join("errors.log")))
        .with_filter(env_filter.and(LevelFilter::WARN));

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(agent_layer)
        .with(errors_layer)
        .init();
}

/// Create the log directory owner-only (0700 on Unix), tightening an existing directory too.
fn create_log_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// MakeWriter for stderr whose lines are redacted before they are written.
struct StderrMakeWriter;

impl<'writer> MakeWriter<'writer> for StderrMakeWriter {
    type Writer = LineRedactingWriter<io::Stderr>;

    fn make_writer(&'writer self) -> Self::Writer {
        LineRedactingWriter::new(io::stderr())
    }
}

/// MakeWriter for a size-rotating file sink.
struct RollingMakeWriter {
    sink: Mutex<RollingFile>,
}

impl RollingMakeWriter {
    fn new(path: PathBuf) -> Self {
        Self {
            sink: Mutex::new(RollingFile::new(path)),
        }
    }
}

impl<'writer> MakeWriter<'writer> for RollingMakeWriter {
    type Writer = LineRedactingWriter<SharedSink<'writer>>;

    fn make_writer(&'writer self) -> Self::Writer {
        LineRedactingWriter::new(SharedSink { sink: &self.sink })
    }
}

/// A writer that buffers bytes, redacts each complete line, then forwards it.
struct LineRedactingWriter<W: Write> {
    inner: W,
    buffer: Vec<u8>,
}

impl<W: Write> LineRedactingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
        }
    }

    /// Redact and forward every complete line currently buffered.
    fn drain_lines(&mut self) -> io::Result<()> {
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=position).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]);
            let redacted = redact(&text);
            self.inner.write_all(redacted.as_bytes())?;
            self.inner.write_all(b"\n")?;
        }
        self.inner.flush()
    }
}

impl<W: Write> Write for LineRedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        self.drain_lines()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            let text = String::from_utf8_lossy(&self.buffer).into_owned();
            let redacted = redact(&text);
            self.inner.write_all(redacted.as_bytes())?;
            self.buffer.clear();
        }
        self.inner.flush()
    }
}

impl<W: Write> Drop for LineRedactingWriter<W> {
    fn drop(&mut self) {
        drop(self.flush());
    }
}

/// A Write adapter over the shared rolling sink; one call writes one complete line.
struct SharedSink<'writer> {
    sink: &'writer Mutex<RollingFile>,
}

impl Write for SharedSink<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut file = self.sink.lock().unwrap_or_else(PoisonError::into_inner);
        file.write_line(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Append-only file that rotates once it exceeds max_bytes.
struct RollingFile {
    path: PathBuf,
    file: Option<File>,
    bytes: u64,
    max_bytes: u64,
}

impl RollingFile {
    fn new(path: PathBuf) -> Self {
        Self::with_capacity(path, MAX_LOG_BYTES)
    }

    fn with_capacity(path: PathBuf, max_bytes: u64) -> Self {
        let mut sink = Self {
            path,
            file: None,
            bytes: 0,
            max_bytes,
        };
        sink.open();
        sink
    }

    fn open(&mut self) {
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        self.file = options.open(&self.path).ok();
        #[cfg(unix)]
        if let Some(file) = self.file.as_ref() {
            use std::os::unix::fs::PermissionsExt;
            drop(file.set_permissions(fs::Permissions::from_mode(0o600)));
        }
        self.bytes = self
            .file
            .as_ref()
            .and_then(|file| file.metadata().ok())
            .map(|metadata| metadata.len())
            .unwrap_or(0);
    }

    /// True when the path no longer refers to the open file because an external logrotate
    /// moved or replaced it. Writing on would keep appending to the old, now-unlinked inode.
    fn should_reopen(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let Some(file) = self.file.as_ref() else {
                return true;
            };
            let (Ok(path_meta), Ok(file_meta)) = (fs::metadata(&self.path), file.metadata()) else {
                return true;
            };
            path_meta.ino() != file_meta.ino()
        }
        #[cfg(not(unix))]
        {
            self.file.is_none()
        }
    }

    /// Write one already-redacted line (without its trailing newline).
    fn write_line(&mut self, line: &[u8]) -> io::Result<()> {
        if self.should_reopen() {
            self.open();
        }
        if self.bytes > 0 && self.bytes + line.len() as u64 + 1 > self.max_bytes {
            self.rotate();
        }
        let Some(file) = self.file.as_mut() else {
            return Ok(());
        };
        file.write_all(line)?;
        file.write_all(b"\n")?;
        file.flush()?;
        self.bytes += line.len() as u64 + 1;
        Ok(())
    }

    fn rotate(&mut self) {
        self.file = None;
        for index in (1..MAX_ROTATED_FILES).rev() {
            drop(fs::rename(
                rotated_path(&self.path, index),
                rotated_path(&self.path, index + 1),
            ));
        }
        drop(fs::rename(&self.path, rotated_path(&self.path, 1)));
        self.open();
    }
}

/// agent.log -> agent.log.<index>
fn rotated_path(path: &Path, index: u32) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{index}"));
    path.with_file_name(name)
}
