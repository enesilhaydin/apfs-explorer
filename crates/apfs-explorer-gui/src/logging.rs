//! File-based daily-rolling logger for the GUI.  Logs land under
//! `%PROGRAMDATA%\APFS Explorer\logs\` (Windows) or `$XDG_DATA_HOME` (other),
//! matching the existing convention used by `apfs-mount`.

use std::path::PathBuf;

pub fn init() {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("gui")
        .filename_suffix("log")
        .build(&dir)
        .ok();
    if let Some(writer) = appender {
        let _ = tracing_subscriber::fmt()
            .with_writer(writer)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_ansi(false)
            .try_init();
    }
}

#[cfg(windows)]
fn log_dir() -> PathBuf {
    let base = std::env::var("PROGRAMDATA").unwrap_or_else(|_| String::from(r"C:\ProgramData"));
    PathBuf::from(base).join("APFS Explorer").join("logs")
}

#[cfg(not(windows))]
fn log_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}/.local/share")))
        .unwrap_or_else(|_| String::from("/tmp"));
    PathBuf::from(base).join("apfs-explorer").join("logs")
}
