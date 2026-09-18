//! Logging for a background application.
//!
//! A tray application has no console, so anything written to stdout or stderr is
//! silently lost. Records therefore go to a file next to the configuration, which
//! is the only place to look when something misbehaves after the window has been
//! closed. Debug builds also echo to stderr so `cargo run` stays useful.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt::MakeWriter;

/// Log files are truncated once they pass this size, which keeps a long running
/// installation from filling a disk with its own history.
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct LogFile {
    inner: Arc<Mutex<BufWriter<File>>>,
}

impl LogFile {
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let too_big = std::fs::metadata(path)
            .map(|meta| meta.len() > MAX_LOG_BYTES)
            .unwrap_or(false);
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(!too_big)
            .truncate(too_big)
            .open(path)?;
        Ok(LogFile {
            inner: Arc::new(Mutex::new(BufWriter::new(file))),
        })
    }
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = LogGuard;

    fn make_writer(&'a self) -> Self::Writer {
        LogGuard(self.inner.clone())
    }
}

pub struct LogGuard(Arc<Mutex<BufWriter<File>>>);

impl Write for LogGuard {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self.0.lock() {
            Ok(mut file) => {
                let written = file.write(buffer)?;
                // Flush eagerly: a crash is exactly when the last lines matter.
                file.flush()?;
                Ok(written)
            }
            Err(_) => Err(io::Error::other("log file is poisoned")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.0.lock() {
            Ok(mut file) => file.flush(),
            Err(_) => Ok(()),
        }
    }
}

/// Crates whose records always reach the log file. A desktop application has no
/// console, so the file is the only place a problem can be looked up after the
/// fact; leaving it empty because of an unrelated `RUST_LOG` in the environment
/// would make the application undiagnosable.
const APP_TARGETS: [&str; 5] = ["uniondesk_lib", "ud_engine", "ud_net", "ud_input", "ud_clipboard"];

/// Builds the filter from `RUST_LOG` when it says something about UnionDesk, and
/// otherwise keeps the application's own records at info while the rest of the
/// world stays quiet.
fn build_filter() -> tracing_subscriber::EnvFilter {
    let from_env = std::env::var("RUST_LOG").ok();
    let mentions_app = from_env
        .as_deref()
        .map(|value| APP_TARGETS.iter().any(|target| value.contains(target)))
        .unwrap_or(false);

    let base = match &from_env {
        Some(value) if mentions_app => value.clone(),
        // `RUST_LOG` is often set globally for Rust development; do not let an
        // unrelated `warn` silence the application entirely.
        Some(value) => format!("{value},{}", app_directives()),
        None => format!("warn,{},ud_engine=debug,ud_net=debug", app_directives()),
    };
    tracing_subscriber::EnvFilter::new(base)
}

fn app_directives() -> String {
    APP_TARGETS
        .iter()
        .map(|target| format!("{target}=info"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Installs the global subscriber. Returns the log file path when logging to a
/// file could be set up, so the caller can mention it in the UI if it wants to.
pub fn init(config_dir: &Path) -> Option<PathBuf> {
    let path = config_dir.join("uniondesk.log");
    let file = match LogFile::open(&path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("UnionDesk could not open {}: {err}", path.display());
            return None;
        }
    };

    let builder = tracing_subscriber::fmt()
        .with_env_filter(build_filter())
        .with_ansi(false);

    #[cfg(debug_assertions)]
    let installed = builder.with_writer(file.and(std::io::stderr)).try_init();
    #[cfg(not(debug_assertions))]
    let installed = builder.with_writer(file).try_init();

    if installed.is_err() {
        // Another subscriber is already installed; keep going rather than fail.
        return None;
    }
    Some(path)
}

#[cfg(debug_assertions)]
use tracing_subscriber::fmt::writer::MakeWriterExt;
