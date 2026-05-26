//! System-tray icon + menu and per-user autostart. Windows-only.
//!
//! Menu layout:
//!
//! ```text
//! APFS Explorer
//! ──────────────────────────
//! [status header]            (disabled label)
//!   Z: VOLUMENAME (12 / 64 MiB)
//!        Open in Explorer
//!        Eject
//!        Re-mount Read-Only
//! ──────────────────────────
//! [x] Auto-mount on plug-in  (toggle)
//! [ ] Default Read-Only mount
//! ──────────────────────────
//! Mount Disk Image...
//! ──────────────────────────
//! About
//! Quit
//! ```

use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

// ---------------------------------------------------------------------------
// Stable menu-item IDs
// ---------------------------------------------------------------------------
pub const ID_QUIT: &str = "quit";
pub const ID_ABOUT: &str = "about";
pub const ID_TOGGLE_AUTO_MOUNT: &str = "toggle_auto_mount";
pub const ID_TOGGLE_READ_ONLY: &str = "toggle_read_only";
pub const ID_MOUNT_IMAGE: &str = "mount_image";
/// Prefix for per-volume Open-in-Explorer actions: `open_<drive_idx>`.
pub const ID_OPEN_PREFIX: &str = "open_";
/// Prefix for per-volume Eject actions: `eject_<drive_idx>`.
pub const ID_EJECT_PREFIX: &str = "eject_";
/// Prefix for per-volume Re-mount-RO actions: `remount_ro_<drive_idx>`.
pub const ID_REMOUNT_RO_PREFIX: &str = "remount_ro_";

// ---------------------------------------------------------------------------
// Volume info for menu rendering
// ---------------------------------------------------------------------------

/// Runtime info about one mounted volume, passed to `build_tray`.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// Physical drive index (matches supervisor's drive number).
    pub drive_idx: u32,
    /// Assigned drive letter, e.g. `"Z:"`.
    pub letter: String,
    /// Volume label, e.g. `"APFSTEST"`. Empty string if unknown.
    pub label: String,
    /// Used bytes (0 if unknown).
    pub used_bytes: u64,
    /// Total bytes (0 if unknown).
    pub total_bytes: u64,
    /// True when there are uncommitted batched writes.
    pub dirty: bool,
}

impl VolumeInfo {
    /// Returns a `"Z: LABEL (used / total MiB)"` display string.
    pub fn summary(&self) -> String {
        let label = if self.label.is_empty() {
            "APFS".to_string()
        } else {
            self.label.clone()
        };
        if self.total_bytes > 0 {
            let used_mib = self.used_bytes / (1024 * 1024);
            let total_mib = self.total_bytes / (1024 * 1024);
            format!("{} {} ({} / {} MiB)", self.letter, label, used_mib, total_mib)
        } else {
            format!("{} {}", self.letter, label)
        }
    }
}

// ---------------------------------------------------------------------------
// Menu builder
// ---------------------------------------------------------------------------

/// Build (or rebuild) the tray icon with the full menu.
///
/// `volumes` — live list of mounted volumes from the supervisor.
/// `auto_mount` / `default_ro` — current toggle states from config.
pub fn build_tray(
    volumes: &[VolumeInfo],
    auto_mount: bool,
    default_ro: bool,
) -> tray_icon::Result<TrayIcon> {
    let menu = Menu::new();

    // App title (disabled label).
    let _ = menu.append(&MenuItem::new("APFS Explorer", false, None));
    let _ = menu.append(&PredefinedMenuItem::separator());

    // Status header.
    let any_dirty = volumes.iter().any(|v| v.dirty);
    let status_header = match volumes.len() {
        0 => "No volumes mounted".to_string(),
        1 if any_dirty => "1 volume mounted  (unflushed writes)".to_string(),
        1 => "1 volume mounted".to_string(),
        n if any_dirty => format!("{n} volumes mounted  (unflushed writes)"),
        n => format!("{n} volumes mounted"),
    };
    let _ = menu.append(&MenuItem::new(&status_header, false, None));

    // Per-volume rows.
    for v in volumes {
        let dirty_tag = if v.dirty { "  (writing...)" } else { "" };
        let row = format!("  {}{}", v.summary(), dirty_tag);
        let _ = menu.append(&MenuItem::new(&row, false, None));
        let _ = menu.append(&MenuItem::with_id(
            format!("{}{}", ID_OPEN_PREFIX, v.drive_idx),
            "      Open in Explorer",
            true,
            None,
        ));
        let _ = menu.append(&MenuItem::with_id(
            format!("{}{}", ID_EJECT_PREFIX, v.drive_idx),
            "      Eject",
            true,
            None,
        ));
        let _ = menu.append(&MenuItem::with_id(
            format!("{}{}", ID_REMOUNT_RO_PREFIX, v.drive_idx),
            "      Re-mount Read-Only",
            true,
            None,
        ));
    }

    let _ = menu.append(&PredefinedMenuItem::separator());

    // Toggles.
    let auto_label = if auto_mount {
        "[x] Auto-mount on plug-in"
    } else {
        "[ ] Auto-mount on plug-in"
    };
    let _ = menu.append(&MenuItem::with_id(ID_TOGGLE_AUTO_MOUNT, auto_label, true, None));

    let ro_label = if default_ro {
        "[x] Default Read-Only mount"
    } else {
        "[ ] Default Read-Only mount"
    };
    let _ = menu.append(&MenuItem::with_id(ID_TOGGLE_READ_ONLY, ro_label, true, None));

    let _ = menu.append(&PredefinedMenuItem::separator());

    // Mount image.
    let _ = menu.append(&MenuItem::with_id(ID_MOUNT_IMAGE, "Mount Disk Image...", true, None));

    let _ = menu.append(&PredefinedMenuItem::separator());

    // About / Quit.
    let _ = menu.append(&MenuItem::with_id(ID_ABOUT, "About", true, None));
    let _ = menu.append(&MenuItem::with_id(ID_QUIT, "Quit", true, None));

    TrayIconBuilder::new()
        .with_tooltip("APFS Explorer")
        .with_menu(Box::new(menu))
        .build()
}

// ---------------------------------------------------------------------------
// Autostart
// ---------------------------------------------------------------------------

/// Register this exe for the current user's logon autostart (no admin).
/// Idempotent. `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
pub fn install_autostart() -> windows::core::Result<()> {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    let exe = std::env::current_exe().map_err(|_| windows::core::Error::from_win32())?;
    let mut wide: Vec<u16> = exe.to_string_lossy().encode_utf16().collect();
    wide.push(0);
    unsafe {
        let mut hkey = HKEY::default();
        // windows 0.58: RegCreateKeyExW reserved: u32 (not Option<u32>).
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            0u32,
            PCWSTR_NULL,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
        .ok()?;
        let bytes = std::slice::from_raw_parts(wide.as_ptr() as *const u8, wide.len() * 2);
        // windows 0.58: RegSetValueExW reserved: u32 (not Option<u32>).
        let _ = RegSetValueExW(hkey, w!("apfs-tray"), 0u32, REG_SZ, Some(bytes));
        let _ = RegCloseKey(hkey);
    }
    Ok(())
}

use windows::core::PCWSTR as PCWSTR_NULL_TY;
#[allow(non_upper_case_globals)]
const PCWSTR_NULL: PCWSTR_NULL_TY = windows::core::PCWSTR(std::ptr::null());
