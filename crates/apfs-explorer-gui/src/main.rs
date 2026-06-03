//! APFS Explorer - Tauri 2 single-EXE GUI.
//!
//! Replaces the previous LocalSystem service + tray architecture.  The GUI
//! runs as the interactive user (no UAC at launch); on Mount click it
//! spawns `apfs-mount.exe` elevated via `ShellExecuteEx("runas")` so the
//! drive letter is created in the user's own session and Explorer sees it.
//! Unmount signals the elevated child via a named Win32 event.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
#[cfg(windows)]
mod drives;
mod logging;
#[cfg(windows)]
mod mount;

fn main() {
    logging::init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "apfs-explorer GUI starting"
    );

    let builder = tauri::Builder::default().invoke_handler(tauri::generate_handler![
        commands::list_drives,
        commands::mount_drive,
        commands::unmount_drive,
        commands::open_drive_letter,
    ]);

    if let Err(e) = builder.run(tauri::generate_context!()) {
        tracing::error!("tauri::run failed: {e}");
    }
}
