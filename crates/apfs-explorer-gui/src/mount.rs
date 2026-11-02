//! Mount/unmount an APFS physical drive through the **WinFsp Launcher**.
//!
//! Why the launcher (vs. the old `ShellExecuteEx("runas")` + log-scraping):
//! * The launcher service (`WinFsp.Launcher`, LocalSystem) starts `apfs-mount`
//!   for us, so the WinFsp drive is created in session 0 and is **globally
//!   visible** - it shows up in the normal, non-elevated Explorer. A drive
//!   mounted by an elevated child in the user's session is NOT visible to the
//!   non-elevated Explorer (separate logon session), which is why the old
//!   approach "mounted" but nothing appeared in This PC.
//! * No per-mount **UAC** prompt: the launcher is already privileged, and its
//!   service classes let Administrators and SYSTEM start and stop them
//!   (`Security=D:P(A;;RPWPLC;;;BA)(A;;RPWPLC;;;SY)`); the app itself is
//!   elevated once when it opens (app.manifest), so it can ask.
//! * Clean, structured **start/stop** via `launchctl` - the GUI picks the drive
//!   letter itself and passes it in, so there is nothing to scrape back out of a
//!   log file (the old `find("APFS mounted at ")` returned a stale letter).
//!
//! The launcher class `apfs-mount` is registered at install time (see the MSI
//! WiX fragment) with a READ-ONLY command line:
//!   Executable  = <install>\apfs-mount.exe
//!   CommandLine = --drive %1 --mount %2 --wait-for-kill   (NO --writable)
//! so %1 = physical drive index and %2 = the drive letter the GUI chose.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Storage::FileSystem::GetLogicalDrives;
use windows::Win32::UI::Shell::{
    SHChangeNotify, ShellExecuteExW, SHCNE_DRIVEADD, SHCNE_DRIVEREMOVED, SHCNF_PATHW,
    SHELLEXECUTEINFOW,
};

/// WinFsp launcher service class names (must match the MSI registration).
/// `apfs-mount` is read-only; `apfs-mount-rw` adds `--writable`. The writable
/// class is still SAFE: apfs-mount's snapshot-guard downgrades to read-only on
/// any volume that has snapshots (#149), and only clean-volume writes (which are
/// fsck-validated on real hardware) actually proceed.
const LAUNCH_CLASS: &str = "apfs-mount";
const LAUNCH_CLASS_RW: &str = "apfs-mount-rw";

/// `CREATE_NO_WINDOW` - suppress the transient console window Windows would
/// otherwise pop when the GUI spawns the console-subsystem `launchctl-x64.exe`.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("WinFsp launchctl not found (is WinFsp installed?)")]
    LaunchctlMissing,
    #[error("launchctl {0} failed: {1}")]
    Launchctl(&'static str, String),
    #[error("no free drive letter available")]
    NoFreeLetter,
    #[error("the drive did not appear within {0} seconds")]
    MountTimeout(u32),
    #[error("open in Explorer failed: {0}")]
    OpenFailed(String),
    #[error("no mount tracked for drive {0}")]
    NotTracked(u32),
}

/// Per-drive launcher instance name (unique per physical drive index).
fn instance_name(drive: u32) -> String {
    format!("apfs{drive}")
}

/// Mount `\\.\PhysicalDrive<drive>` read-only as a globally-visible Windows
/// drive letter via the WinFsp launcher. Returns the assigned letter (`"M:"`).
///
/// `writable` selects the launcher class: `apfs-mount-rw` (which passes
/// `--writable`) vs the read-only `apfs-mount`. Writable is still safe - the
/// helper's snapshot-guard refuses writes on snapshotted volumes and only
/// clean-volume writes (fsck-validated) proceed.
pub fn mount(drive: u32, writable: bool) -> Result<String, MountError> {
    let launchctl = locate_launchctl()?;
    let letter = first_free_drive_letter().ok_or(MountError::NoFreeLetter)?;
    let instance = instance_name(drive);
    let class = if writable {
        LAUNCH_CLASS_RW
    } else {
        LAUNCH_CLASS
    };

    // launchctl start <class> <instance> <args...>  → args map to %1, %2 …
    // i.e. apfs-mount --drive <drive> --mount <letter> [--writable] --wait-for-kill.
    let out = Command::new(&launchctl)
        .args(["start", class, &instance, &drive.to_string(), &letter])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| MountError::Launchctl("start", e.to_string()))?;
    if !out.status.success() {
        return Err(MountError::Launchctl("start", combine_output(&out)));
    }

    // The launcher starts apfs-mount asynchronously; the WinFsp volume appears a
    // moment later. Poll for the drive root (visible because the mount is
    // global). If it never appears, stop the instance so we don't leak it.
    if !wait_for_drive(&letter, 15) {
        let _ = Command::new(&launchctl)
            .args(["stop", class, &instance])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
        return Err(MountError::MountTimeout(15));
    }

    tracing::info!(drive, letter = %letter, "launcher mounted APFS drive");
    // Persist so a GUI restart re-discovers this still-mounted drive (the
    // launcher instance outlives the GUI), then mirror into the in-process
    // registry for immediate UI feedback.
    persist_add(drive, &letter);
    if let Ok(mut g) = crate::drives::MOUNTED.lock() {
        g.retain(|(d, _)| *d != drive);
        g.push((drive, letter.clone()));
    }
    // Nudge Explorer to refresh "This PC" without a manual F5.
    notify_shell_drive(&letter, true);
    Ok(letter)
}

/// Stop the launcher instance for `drive` (unmounts the WinFsp drive).
pub fn unmount(drive: u32) -> Result<(), MountError> {
    let launchctl = locate_launchctl()?;
    let instance = instance_name(drive);

    // The drive may have been mounted via either class (read-only or writable),
    // so stop both - the non-matching one is a harmless no-op.
    let mut any_ok = false;
    for class in [LAUNCH_CLASS, LAUNCH_CLASS_RW] {
        match Command::new(&launchctl)
            .args(["stop", class, &instance])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        {
            Ok(o) if o.status.success() => any_ok = true,
            Ok(_) => {}
            Err(e) => return Err(MountError::Launchctl("stop", e.to_string())),
        }
    }

    // Drop our tracking + refresh Explorer regardless of the exact exit status
    // (a missing instance is effectively "already unmounted").
    persist_remove(drive);
    let removed_letter = match crate::drives::MOUNTED.lock() {
        Ok(mut g) => {
            let l = g.iter().find(|(d, _)| *d == drive).map(|(_, l)| l.clone());
            g.retain(|(d, _)| *d != drive);
            l
        }
        Err(_) => None,
    };
    if let Some(l) = removed_letter {
        notify_shell_drive(&l, false);
    } else if !any_ok {
        return Err(MountError::NotTracked(drive));
    }
    Ok(())
}

/// Poll for `<letter>\` to exist, up to `timeout_seconds`.
fn wait_for_drive(letter: &str, timeout_seconds: u32) -> bool {
    let root = format!("{}\\", letter.trim_end_matches('\\'));
    for _ in 0..(timeout_seconds * 10) {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if Path::new(&root).exists() {
            return true;
        }
    }
    false
}

/// First free drive letter as `"X:"`, scanning D..=Z (A/B/C are reserved).
fn first_free_drive_letter() -> Option<String> {
    // SAFETY: GetLogicalDrives takes no args and returns a bit-mask (0 on error).
    let mask = unsafe { GetLogicalDrives() };
    first_free_from_mask(mask)
}

/// Pure helper: first free letter in D..=Z given a `GetLogicalDrives` mask
/// (bit i set ⇒ letter `A`+i is in use). Returns `"X:"`.
fn first_free_from_mask(mask: u32) -> Option<String> {
    for i in 3u32..26 {
        // skip A,B,C (0,1,2)
        if mask & (1 << i) == 0 {
            let ch = (b'A' + i as u8) as char;
            return Some(format!("{ch}:"));
        }
    }
    None
}

/// Combine a process's stdout+stderr into one trimmed string for error context.
fn combine_output(out: &std::process::Output) -> String {
    let mut s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    if !err.is_empty() {
        if !s.is_empty() {
            s.push_str(" | ");
        }
        s.push_str(err);
    }
    s
}

/// Resolve the path to `launchctl-x64.exe` shipped with WinFsp.
fn locate_launchctl() -> Result<String, MountError> {
    const CANDIDATES: [&str; 2] = [
        r"C:\Program Files (x86)\WinFsp\bin\launchctl-x64.exe",
        r"C:\Program Files\WinFsp\bin\launchctl-x64.exe",
    ];
    for c in CANDIDATES {
        if Path::new(c).exists() {
            return Ok(c.to_string());
        }
    }
    Err(MountError::LaunchctlMissing)
}

/// Broadcast a drive-arrival / drive-removal so Explorer refreshes "This PC".
fn notify_shell_drive(letter: &str, arrived: bool) {
    let l = letter.trim().trim_end_matches(':');
    if l.len() != 1 {
        return;
    }
    let root = format!("{l}:\\");
    let wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
    let event = if arrived {
        SHCNE_DRIVEADD
    } else {
        SHCNE_DRIVEREMOVED
    };
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 buffer that outlives the
    //         call; the second item ptr is null (unused for drive events).
    unsafe {
        SHChangeNotify(
            event,
            SHCNF_PATHW,
            Some(wide.as_ptr() as *const core::ffi::c_void),
            None,
        );
    }
}

/// Open `<letter>:\` in Explorer.  No elevation - just the shell.
pub fn open_in_explorer(letter: &str) -> Result<(), MountError> {
    let l = letter.trim().trim_end_matches(':');
    if l.len() != 1 {
        return Err(MountError::OpenFailed(format!(
            "invalid drive letter: {letter:?}"
        )));
    }
    let path = format!("{l}:\\");
    let path_w = HSTRING::from(path.as_str());
    let verb_w = HSTRING::from("open");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        lpVerb: PCWSTR(verb_w.as_ptr()),
        lpFile: PCWSTR(path_w.as_ptr()),
        nShow: 1, // SW_SHOWNORMAL
        ..Default::default()
    };
    // SAFETY: info is correctly sized and the HSTRING values outlive the call.
    let ok = unsafe { ShellExecuteExW(&mut info) };
    if ok.is_err() {
        return Err(MountError::OpenFailed(format!("{ok:?}")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Persisted mount state (survives GUI restarts; the launcher keeps the mount
// alive independently of this process).
// ---------------------------------------------------------------------------

/// Per-user state file: `%LOCALAPPDATA%\APFS Explorer\mounts.txt`, one
/// `drive=letter` line per active mount. Per-user so the non-elevated GUI can
/// always write it.
fn state_file() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let mut p = PathBuf::from(base);
    p.push("APFS Explorer");
    let _ = std::fs::create_dir_all(&p);
    p.push("mounts.txt");
    Some(p)
}

/// Parse one `drive=letter` line (e.g. `3=F:`) into `(drive, "F:")`.
fn parse_state_line(line: &str) -> Option<(u32, String)> {
    let (d, l) = line.split_once('=')?;
    let drive = d.trim().parse::<u32>().ok()?;
    let letter = l.trim().to_string();
    let ok =
        letter.len() == 2 && letter.ends_with(':') && letter.as_bytes()[0].is_ascii_alphabetic();
    ok.then_some((drive, letter))
}

fn persist_load() -> Vec<(u32, String)> {
    let Some(p) = state_file() else {
        return Vec::new();
    };
    let Ok(s) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    s.lines().filter_map(parse_state_line).collect()
}

fn persist_save(entries: &[(u32, String)]) {
    if let Some(p) = state_file() {
        let body = entries
            .iter()
            .map(|(d, l)| format!("{d}={l}"))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(&p, body);
    }
}

fn persist_add(drive: u32, letter: &str) {
    let mut e = persist_load();
    e.retain(|(d, _)| *d != drive);
    e.push((drive, letter.to_string()));
    persist_save(&e);
}

fn persist_remove(drive: u32) {
    let mut e = persist_load();
    e.retain(|(d, _)| *d != drive);
    persist_save(&e);
}

/// Rebuild the in-process `MOUNTED` registry from the persisted state, dropping
/// any entry whose drive letter no longer exists. Called at the top of
/// `drives::list()` so a reopened GUI re-discovers drives still mounted by the
/// launcher, and stale entries (unmounted out-of-band) self-heal.
pub fn reconcile() {
    let survivors: Vec<(u32, String)> = persist_load()
        .into_iter()
        .filter(|(_, letter)| Path::new(&format!("{}\\", letter.trim_end_matches('\\'))).exists())
        .collect();
    persist_save(&survivors);
    if let Ok(mut g) = crate::drives::MOUNTED.lock() {
        *g = survivors;
    }
}

#[cfg(test)]
mod tests {
    use super::first_free_from_mask;
    use super::parse_state_line;

    #[test]
    fn skips_abc_and_picks_first_free() {
        // A,B,C,D used (bits 0..=3) → first free is E.
        let mask = 0b1111;
        assert_eq!(first_free_from_mask(mask).as_deref(), Some("E:"));
    }

    #[test]
    fn picks_d_when_only_abc_used() {
        let mask = 0b0111; // A,B,C used
        assert_eq!(first_free_from_mask(mask).as_deref(), Some("D:"));
    }

    #[test]
    fn none_when_all_used() {
        assert_eq!(first_free_from_mask(u32::MAX), None);
    }

    #[test]
    fn instance_name_is_per_drive() {
        assert_eq!(super::instance_name(3), "apfs3");
    }

    #[test]
    fn parses_valid_state_line() {
        assert_eq!(parse_state_line("3=F:"), Some((3, "F:".to_string())));
        assert_eq!(parse_state_line(" 12 = Z: "), Some((12, "Z:".to_string())));
    }

    #[test]
    fn rejects_bad_state_lines() {
        assert_eq!(parse_state_line("3=F:\\dir"), None); // not a bare letter
        assert_eq!(parse_state_line("x=F:"), None); // non-numeric drive
        assert_eq!(parse_state_line("3="), None); // no letter
        assert_eq!(parse_state_line("garbage"), None);
    }
}
