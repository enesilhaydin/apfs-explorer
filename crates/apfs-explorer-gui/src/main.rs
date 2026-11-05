//! APFS Explorer - Tauri 2 single-EXE GUI.
//!
//! Replaces the previous LocalSystem service + tray architecture. The app
//! asks for elevation once when it opens (app.manifest): mounts are started
//! and stopped through the WinFsp launcher (see `mount`), whose apfs-mount
//! classes only Administrators may use. The launcher creates the drive
//! letter globally, so the normal, non-elevated Explorer sees it.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod commands;
#[cfg(windows)]
mod drives;
mod logging;
#[cfg(windows)]
mod mount;

fn main() {
    let log_dir = logging::init();

    let builder = tauri::Builder::default()
        .setup(move |app| {
            tracing::info!(
                version = %app.package_info().version,
                build = env!("APFS_BUILD_ID"),
                log_dir = %log_dir.as_ref().map_or_else(
                    |e| format!("none: {e}"),
                    |d| d.display().to_string()
                ),
                "apfs-explorer GUI starting"
            );
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::list_drives,
            commands::list_volumes,
            commands::mount_drive,
            commands::unmount_drive,
            commands::open_drive_letter,
            commands::app_version,
        ]);

    if let Err(e) = builder.run(tauri::generate_context!()) {
        tracing::error!("tauri::run failed: {e}");
    }
}
