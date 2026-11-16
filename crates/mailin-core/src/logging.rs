//! Rolling log file and panic recording, shared by both front-ends (D3).
//!
//! Tracing goes to stderr *and* to `data_dir/mailin.log`. The file is capped
//! at 5 MB; when it fills, it is renamed to `mailin.log.1` and a new file is
//! opened. Only the two most recent files are kept. No message bodies or
//! credentials are ever logged.
//!
//! A panic is additionally recorded to `data_dir/mailin.panic`. The front-end
//! reads and clears that file on the next start so a crash is never silent.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::config::Paths;
use crate::error::Result;

/// Maximum size of one log file before it is rotated.
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
/// Name of the file recording a panic from the previous run.
const PANIC_FILE: &str = "mailin.panic";

/// Install the global tracing subscriber: stderr plus a size-rolling file.
///
/// `default_filter` is used when `RUST_LOG` is not set. Calling this twice in
/// one process is an error (the subscriber is global), which is exactly what
/// we want from a `main`.
pub fn init_logging(paths: &Paths, default_filter: &str) -> Result<()> {
    let rolling = RollingLog::open(paths)?;
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_writer(io::stderr),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_ansi(false)
                .with_writer(rolling),
        )
        .init();
    Ok(())
}

/// Record panics to disk and still call the previous hook (so the usual
/// backtrace behaviour is preserved).
pub fn install_panic_hook(paths: &Paths) {
    let path = paths.data_dir.join(PANIC_FILE);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let payload = if let Some(text) = info.payload().downcast_ref::<&str>() {
            (*text).to_owned()
        } else if let Some(text) = info.payload().downcast_ref::<String>() {
            text.clone()
        } else {
            "unknown panic".to_owned()
        };
        let location = info
            .location()
            .map(|location| location.to_string())
            .unwrap_or_default();
        let record = format!("{}\n{}\n{}\n", crate::now_unix(), payload, location);
        // Best effort: a panic while the disk is full must not panic again.
        let _ = fs::write(&path, record);
        previous(info);
    }));
}

/// Return and remove a panic recorded by a previous run, if any.
pub fn take_panic_notice(paths: &Paths) -> Option<String> {
    let path = paths.data_dir.join(PANIC_FILE);
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    let mut lines = text.lines();
    let _timestamp = lines.next();
    let message = lines.next().unwrap_or("The application crashed.");
    Some(message.to_owned())
}

// ---------------------------------------------------------------------------
// Size-rolling writer
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct LogState {
    dir: PathBuf,
    file: Option<File>,
    bytes: u64,
}

impl LogState {
    fn rotate(&mut self) -> io::Result<()> {
        // Drop the open handle before renaming (needed on Windows).
        self.file.take();
        let current = self.dir.join("mailin.log");
        let previous = self.dir.join("mailin.log.1");
        let _ = fs::remove_file(&previous);
        let _ = fs::rename(&current, &previous);
        self.file = Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&current)?,
        );
        self.bytes = 0;
        Ok(())
    }
}

impl Write for LogState {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.bytes.saturating_add(buf.len() as u64) > MAX_LOG_BYTES {
            self.rotate()?;
        }
        let Some(file) = self.file.as_mut() else {
            return Err(io::Error::other("log file is closed"));
        };
        let written = file.write(buf)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

/// A `MakeWriter` for tracing that keeps only two log files (D3).
#[derive(Debug)]
pub struct RollingLog {
    state: Mutex<LogState>,
}

impl RollingLog {
    fn open(paths: &Paths) -> Result<Self> {
        paths.ensure()?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.log_file())?;
        let bytes = file.metadata()?.len();
        Ok(Self {
            state: Mutex::new(LogState {
                dir: paths.data_dir.clone(),
                file: Some(file),
                bytes,
            }),
        })
    }
}

/// Guard returned by `MakeWriter` so each log event locks the file only for
/// the duration of a single formatted line.
#[derive(Debug)]
pub struct LogWriter<'a>(MutexGuard<'a, LogState>);

impl Write for LogWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for RollingLog {
    type Writer = LogWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriter(
            self.state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    #[test]
    fn panic_notice_is_taken_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        paths.ensure().expect("ensure");
        assert!(take_panic_notice(&paths).is_none());

        let path = paths.data_dir.join(PANIC_FILE);
        fs::write(&path, "1234\n panicked at main.rs\nlocation\n").expect("write");
        let notice = take_panic_notice(&paths).expect("notice");
        assert_eq!(notice, " panicked at main.rs");
        assert!(!path.exists());
        assert!(take_panic_notice(&paths).is_none());
    }

    #[test]
    fn log_writer_creates_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = Paths::with_root(dir.path());
        let log = RollingLog::open(&paths).expect("open");
        let mut writer = log.make_writer();
        writer.write_all(b"hello\n").expect("write");
        drop(writer);
        let text = fs::read_to_string(paths.log_file()).expect("read");
        assert!(text.contains("hello"));
    }
}
