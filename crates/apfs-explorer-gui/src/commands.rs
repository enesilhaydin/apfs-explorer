//! `#[tauri::command]` functions exposed to the JS frontend via
//! `window.__TAURI__.core.invoke('name', args)`.  The functions are thin
//! wrappers around the platform-specific `drives` / `mount` modules so the
//! cross-platform check on macOS doesn't trip over Windows-only APIs.

use serde::{Deserialize, Serialize};

/// Information about a single physical disk surfaced to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct DriveInfo {
    /// `0` for `\\.\PhysicalDrive0`, `3` for `PhysicalDrive3`, etc.
    pub index: u32,
    /// Model / friendly name reported by WMI (`Win32_DiskDrive.Model`).
    pub model: String,
    /// Size in bytes as reported by WMI.
    pub size_bytes: u64,
    /// `"Fixed"`, `"Removable"`, `"USB"`, `"Unknown"`.
    pub kind: String,
    /// True if the disk has an Apple_APFS GPT partition (detected via the
    /// Storage WMI namespace, no elevation). The UI lists only APFS disks.
    /// Fails OPEN: if detection is unavailable this is `true` for every disk
    /// so a Storage-WMI hiccup never hides all drives.
    pub is_apfs: bool,
    /// True if the GUI has actively mounted a child for this drive in this
    /// session.  The UI shows the drive letter from `letter` when set.
    pub mounted: bool,
    /// Drive letter, e.g. `"F:"`, when `mounted == true`; empty otherwise.
    pub letter: String,
}

/// One snapshot of a volume, as `apfs-mount volume list --json` gives it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub name: String,
    /// The snapshot's transaction, in hex (`0x1a2`): what a mount names.
    pub xid: String,
    /// Creation time, nanoseconds since 1970-01-01 UTC.
    pub created: u64,
}

/// One volume of a drive's container.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeEntry {
    /// Slot in the container, what `--volume-index` takes.
    pub index: u32,
    pub name: String,
    pub role: String,
    pub encrypted: bool,
    /// The volume a mount picks when none is named.
    pub default: bool,
    pub snapshots: Vec<SnapshotEntry>,
}

#[derive(Debug, Deserialize)]
struct VolumeList {
    volumes: Vec<VolumeEntry>,
}

/// The volumes of physical drive `drive` and their snapshots.
#[tauri::command]
pub async fn list_volumes(drive: u32) -> Result<Vec<VolumeEntry>, String> {
    #[cfg(windows)]
    {
        let json = crate::mount::list_volumes(drive).map_err(|e| {
            tracing::error!(drive, "volume list failed: {e}");
            e.to_string()
        })?;
        serde_json::from_str::<VolumeList>(&json)
            .map(|l| l.volumes)
            .map_err(|e| {
                tracing::error!(drive, "volume list unreadable: {e}");
                format!("volume list unreadable: {e}")
            })
    }
    #[cfg(not(windows))]
    {
        let _ = drive;
        Ok(Vec::new())
    }
}

#[tauri::command]
pub async fn list_drives() -> Result<Vec<DriveInfo>, String> {
    #[cfg(windows)]
    {
        crate::drives::list().map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        Ok(Vec::new())
    }
}

#[tauri::command]
pub async fn mount_drive(
    drive: u32,
    writable: bool,
    volume: u32,
    snapshot: Option<String>,
) -> Result<String, String> {
    // A snapshot is named by its xid in hex, as `volume list` gives it.
    let snapshot = match snapshot.as_deref() {
        Some(x) => Some(
            u64::from_str_radix(x.trim_start_matches("0x"), 16)
                .map_err(|_| format!("not a snapshot xid: {x:?}"))?,
        ),
        None => None,
    };
    // Writable is OPT-IN (read-only stays the UI's default/primary action).
    // Safe to honor now: the COW data-integrity tier shipped (post-commit
    // checkpoint verify; fsync barriers; macOS differential-validated), and the
    // launcher's writable class writes along the paths to the changed records,
    // under snapshots as well (they keep what they froze; #11).
    tracing::info!(drive, writable, volume, ?snapshot, "mount_drive requested");
    #[cfg(windows)]
    {
        crate::mount::mount(drive, writable, volume, snapshot).map_err(|e| {
            tracing::error!(drive, writable, volume, ?snapshot, "mount failed: {e}");
            e.to_string()
        })
    }
    #[cfg(not(windows))]
    {
        let _ = (drive, volume, snapshot);
        Err("mount is only supported on Windows".into())
    }
}

#[tauri::command]
pub async fn unmount_drive(drive: u32) -> Result<(), String> {
    #[cfg(windows)]
    {
        crate::mount::unmount(drive)
            .map(|()| tracing::info!(drive, "unmounted"))
            .map_err(|e| {
                tracing::error!(drive, "unmount failed: {e}");
                e.to_string()
            })
    }
    #[cfg(not(windows))]
    {
        let _ = drive;
        Err("unmount is only supported on Windows".into())
    }
}

/// `APFS Explorer <version> (<commit>, built <date>)`, shown in the window
/// so a report can say which build it came from.
#[tauri::command]
pub fn app_version(app: tauri::AppHandle) -> String {
    format!(
        "APFS Explorer {} ({})",
        app.package_info().version,
        env!("APFS_BUILD_ID")
    )
}

/// Open `<letter>:\` in Explorer.  Used by the "Open" button.
#[tauri::command]
pub async fn open_drive_letter(letter: String) -> Result<(), String> {
    #[cfg(windows)]
    {
        crate::mount::open_in_explorer(&letter).map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = letter;
        Err("open_drive_letter is only supported on Windows".into())
    }
}
