//! File-based daily-rolling logger for the GUI.  Logs land under
//! `%PROGRAMDATA%\APFS Explorer\logs\` (Windows) or `$XDG_DATA_HOME` (other),
//! matching the existing convention used by `apfs-mount`.

use std::path::PathBuf;

/// Daily GUI log files kept; older ones are deleted when a new one opens.
const GUI_LOG_DAYS: usize = 7;

/// Start logging to the first directory that takes a log file, and return
/// it. The shared directory is writable by Administrators only (the
/// installer's ACL), so a run without elevation logs to the user's own
/// directory instead of logging nothing, as it used to.
pub fn init() -> Result<PathBuf, String> {
    let mut failures = Vec::new();
    for dir in log_dirs() {
        let opened = std::fs::create_dir_all(&dir)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                tracing_appender::rolling::Builder::new()
                    .rotation(tracing_appender::rolling::Rotation::DAILY)
                    .filename_prefix("gui")
                    .filename_suffix("log")
                    // A file per day; without a cap they were never removed.
                    .max_log_files(GUI_LOG_DAYS)
                    .build(&dir)
                    .map_err(|e| e.to_string())
            });
        match opened {
            Ok(writer) => {
                let _ = tracing_subscriber::fmt()
                    .with_writer(writer)
                    .with_env_filter(
                        tracing_subscriber::EnvFilter::try_from_default_env()
                            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                    )
                    .with_ansi(false)
                    .try_init();
                for f in &failures {
                    tracing::warn!("log directory skipped: {f}");
                }
                return Ok(dir);
            }
            Err(e) => failures.push(format!("{}: {e}", dir.display())),
        }
    }
    Err(failures.join("; "))
}

#[cfg(windows)]
fn log_dirs() -> Vec<PathBuf> {
    let shared = std::env::var("PROGRAMDATA").unwrap_or_else(|_| String::from(r"C:\ProgramData"));
    let mut dirs = vec![PathBuf::from(shared).join("APFS Explorer").join("logs")];
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        dirs.push(PathBuf::from(local).join("APFS Explorer").join("logs"));
    }
    dirs
}

#[cfg(not(windows))]
fn log_dirs() -> Vec<PathBuf> {
    let base = std::env::var("XDG_DATA_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.local/share")))
        .unwrap_or_else(|_| String::from("/tmp"));
    vec![PathBuf::from(base).join("apfs-explorer").join("logs")]
}
