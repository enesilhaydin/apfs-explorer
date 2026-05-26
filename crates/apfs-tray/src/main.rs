//! apfs-tray — user-session APFS auto-mount agent + tray.
//!
//! Plug an APFS USB → it auto-mounts to a free drive letter (visible to
//! this logged-in user); unplug → it unmounts. Tray shows status. The
//! actual mount is a separate `apfs-mount.exe` child (crash-isolated).

#[cfg(not(windows))]
fn main() {
    eprintln!("apfs-tray is Windows-only.");
}

#[cfg(windows)]
fn main() {
    use std::sync::mpsc::{channel, RecvTimeoutError};
    use std::time::Duration;

    use apfs_tray::child::Children;
    use apfs_tray::config::TrayConfig;
    use apfs_tray::detect::present_apfs_drives;
    use apfs_tray::supervisor::{Action, Supervisor};
    use apfs_tray::tray::{
        build_tray, install_autostart, VolumeInfo, ID_ABOUT, ID_EJECT_PREFIX, ID_MOUNT_IMAGE,
        ID_OPEN_PREFIX, ID_QUIT, ID_REMOUNT_RO_PREFIX, ID_TOGGLE_AUTO_MOUNT, ID_TOGGLE_READ_ONLY,
    };
    use apfs_tray::winwin::create_device_window;
    use tray_icon::menu::MenuEvent;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    let _ = install_autostart();

    // Load persistent settings.
    let mut cfg = TrayConfig::load();

    let (tx, rx) = channel();
    if create_device_window(tx).is_err() {
        eprintln!("apfs-tray: failed to create device window");
        std::process::exit(1);
    }

    let mut sup = Supervisor::new();
    let mut kids = Children::new();
    // Build with empty volumes on startup; first tick populates.
    let mut tray = build_tray(&[], cfg.auto_mount, cfg.default_read_only).ok();

    // Free function avoids borrow-checker issues with capturing `kids` in nested
    // closures (e.g. watchdog's alive probe borrows kids &mut, same as apply).
    fn apply(acts: Vec<Action>, kids: &mut Children, read_only: bool) {
        for a in acts {
            match a {
                Action::Mount(d) => {
                    let _ = kids.spawn_with_opts(d, !read_only);
                }
                Action::Unmount(d) => kids.kill(d),
            }
        }
    }

    // Initial scan (USB may already be plugged in at login).
    if cfg.auto_mount {
        apply(
            sup.reconcile(&present_apfs_drives()),
            &mut kids,
            cfg.default_read_only,
        );
    }

    let menu_rx = MenuEvent::receiver();
    loop {
        // Pump Win32 messages so wndproc + tray events fire.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        // Tray menu actions.
        while let Ok(ev) = menu_rx.try_recv() {
            let id = ev.id.0.as_str();

            if id == ID_QUIT {
                // Active mounts: ask for confirmation before exit is handled
                // entirely in the UI layer — for now just perform clean unmount.
                apply(sup.shutdown(), &mut kids, cfg.default_read_only);
                kids.kill_all();
                return;

            } else if id == ID_ABOUT {
                // Show version string via a message box on a best-effort basis.
                show_about_dialog();

            } else if id == ID_TOGGLE_AUTO_MOUNT {
                cfg.auto_mount = !cfg.auto_mount;
                cfg.save();
                rebuild_tray(&sup, &mut tray, &cfg);

            } else if id == ID_TOGGLE_READ_ONLY {
                cfg.default_read_only = !cfg.default_read_only;
                cfg.save();
                rebuild_tray(&sup, &mut tray, &cfg);

            } else if id == ID_MOUNT_IMAGE {
                // Future: open a file-picker dialog. No-op for now.
                eprintln!("apfs-tray: Mount Disk Image not yet implemented");

            } else if let Some(idx_str) = id.strip_prefix(ID_EJECT_PREFIX) {
                // Eject: flush + commit + unmount for this drive.
                if let Ok(drive) = idx_str.parse::<u32>() {
                    apply(vec![Action::Unmount(drive)], &mut kids, cfg.default_read_only);
                    sup.forget(drive);
                    rebuild_tray(&sup, &mut tray, &cfg);
                }

            } else if let Some(idx_str) = id.strip_prefix(ID_REMOUNT_RO_PREFIX) {
                // Re-mount read-only: kill current child, respawn with RO flag.
                if let Ok(drive) = idx_str.parse::<u32>() {
                    kids.kill(drive);
                    let _ = kids.spawn_with_opts(drive, false /* writable=false */);
                }

            } else if let Some(idx_str) = id.strip_prefix(ID_OPEN_PREFIX) {
                // Open in Explorer: shell-execute the drive letter.
                if let Ok(drive) = idx_str.parse::<u32>() {
                    open_in_explorer(drive);
                }
            }
        }

        // Device-change (debounced via the recv timeout) → re-scan.
        let rescan = match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(_) => true,
            Err(RecvTimeoutError::Timeout) => true, // also drives watchdog + dirty poll
            Err(RecvTimeoutError::Disconnected) => return,
        };

        if rescan {
            if cfg.auto_mount {
                apply(
                    sup.reconcile(&present_apfs_drives()),
                    &mut kids,
                    cfg.default_read_only,
                );
            }
            // Watchdog: check liveness of all tracked children.
            let tracked = sup.tracked();
            let alive_map: Vec<(u32, bool)> =
                tracked.iter().map(|&d| (d, kids.alive(d))).collect();
            let acts = sup.watchdog(&|d| {
                alive_map
                    .iter()
                    .find(|&&(k, _)| k == d)
                    .map(|&(_, v)| v)
                    .unwrap_or(false)
            });
            apply(acts, &mut kids, cfg.default_read_only);

            // Rebuild tray with dirty status polled from status files.
            rebuild_tray(&sup, &mut tray, &cfg);
        }
    }

    // ---------------------------------------------------------------------------
    // Helpers (defined inside the #[cfg(windows)] fn to access local types)
    // ---------------------------------------------------------------------------

    fn rebuild_tray(
        sup: &Supervisor,
        tray: &mut Option<tray_icon::TrayIcon>,
        cfg: &TrayConfig,
    ) {
        let volumes = build_volume_list(sup);
        if let Ok(t) = build_tray(&volumes, cfg.auto_mount, cfg.default_read_only) {
            *tray = Some(t);
        }
    }

    fn build_volume_list(sup: &Supervisor) -> Vec<VolumeInfo> {
        sup.tracked()
            .iter()
            .map(|&drive| {
                // Read status file written by apfs-mount for letter + dirty flag.
                let status_path = winfsp_host::mount::status_file_path(drive);
                let (letter, dirty) = read_status_file(&status_path);
                VolumeInfo {
                    drive_idx: drive,
                    letter,
                    label: String::new(), // label read from VolumeInfo future work
                    used_bytes: 0,
                    total_bytes: 0,
                    dirty,
                }
            })
            .collect()
    }

    /// Parse `{"drive":N,"mount":"Z:","dirty":bool}` from the status file.
    fn read_status_file(path: &std::path::Path) -> (String, bool) {
        let default = (String::new(), false);
        let Ok(s) = std::fs::read_to_string(path) else {
            return default;
        };
        // Minimal JSON field extraction without a full JSON crate.
        let mount = s
            .split('"')
            .skip_while(|t| *t != "mount")
            .nth(2)
            .unwrap_or("")
            .to_string();
        let dirty = s.contains("\"dirty\":true");
        (mount, dirty)
    }

    fn open_in_explorer(drive: u32) {
        // Resolve the drive letter from the status file, then ShellExecute.
        let status_path = winfsp_host::mount::status_file_path(drive);
        let (letter, _) = read_status_file(&status_path);
        if letter.is_empty() {
            return;
        }
        // ShellExecuteW "explore" on the drive letter opens an Explorer window.
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let mut path_w: Vec<u16> = letter.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            ShellExecuteW(
                None,
                windows::core::w!("explore"),
                PCWSTR(path_w.as_mut_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            );
        }
    }

    fn show_about_dialog() {
        use windows::core::PCWSTR;
        use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK, MB_ICONINFORMATION};
        let msg = format!(
            "APFS Explorer for Windows\nVersion {}\n\nhttps://github.com/USER/apfs-explorer\0",
            env!("CARGO_PKG_VERSION")
        );
        let msg_w: Vec<u16> = msg.encode_utf16().collect();
        let title_w: Vec<u16> = "About APFS Explorer\0".encode_utf16().collect();
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(msg_w.as_ptr()),
                PCWSTR(title_w.as_ptr()),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }
}
