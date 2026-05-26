# M5 — User-Session Auto-Mount Agent + Tray Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When an APFS USB/SSD is plugged in, it automatically appears as a Windows drive letter for the logged-in user (NTFS-like), with a tray icon to see status and unmount; pure-Rust, crash-isolated, zero-config, UAC-free.

**Architecture:** A single user-session process (the `apfs-tray` binary) that owns a hidden Win32 window. The window receives `WM_DEVICECHANGE` (via `RegisterDeviceNotificationW`) and drives a pure device→mount **supervisor** state machine. For each detected APFS physical drive the supervisor spawns the existing `apfs-mount.exe` (winfsp-host) **out-of-process** (fault isolation: a mount crash never kills the agent/tray; a watchdog respawns). Removal terminates the child (WinFsp unmounts on process exit). A `tray-icon` menu shows mounted volumes + Unmount/Quit. Per-user autostart via `HKCU\...\Run` (no admin). Running in the user session is what makes the drive letter visible (the empirical M4b finding).

**Tech Stack:** Rust 2021, `windows` crate (device notification, hidden window, message loop, registry), `tray-icon` (`/tauri-apps/tray-icon`), existing `winfsp-host` (APFS detection + `apfs-mount` binary), `apfs-core`/`apfs`. All Windows code `#[cfg(windows)]` / `[target.'cfg(windows)'.dependencies]`; macOS workspace keeps building/testing the pure logic.

**Crate decision (CLAUDE.md "no artificial split"):** the auto-mount agent and the tray are one tightly-coupled user-session process → ONE binary in the `apfs-tray` crate. The `apfs-service` crate stays a documented skeleton (a hardened boot-time Windows Service is out of scope for M5; revisit in M8 if multi-user/boot mounting is ever needed). The pure supervisor logic lives in `apfs-tray`'s lib part and is unit-tested on macOS.

**Empirically locked (from M4b, durable):** APFS USB = GPT partition type GUID `7C3457EF-0000-11AA-AA11-00306543ECAC` + `NXSB` magic; `winfsp_host::winraw::PhysicalDrive::autodetect_apfs()` already finds it; `apfs-mount.exe` already mounts it with auto free drive-letter (`64e0a78`), self-provisions the WinFsp DLL, unmounts cleanly on process exit. The agent must NOT reimplement any of that — it orchestrates `apfs-mount.exe`.

**Source-of-truth discipline:** exact `tray-icon` / `windows`-crate device-notification API shapes are HYPOTHESIS until the pinned crates build on the Windows box (`apfswin`); the compiler + a real plug/unplug are the arbiter. Pin versions; resolve signature mismatches on the box; record resolved shapes in `// <crate> <ver>:` comments. No APFS on-disk spec is touched (no md.6).

---

## File Structure

- `crates/apfs-tray/Cargo.toml` — bin + Windows-only target deps (`windows`, `tray-icon`); depends on `winfsp-host` for APFS detection.
- `crates/apfs-tray/src/lib.rs` — module wiring + pure re-exports.
- `crates/apfs-tray/src/supervisor.rs` — **pure** device→mount state machine + diff (unit-tested on macOS).
- `crates/apfs-tray/src/detect.rs` — `#[cfg(windows)]` enumerate physical drives → APFS set (thin wrapper over `winfsp_host::winraw`/`gpt`).
- `crates/apfs-tray/src/child.rs` — `#[cfg(windows)]` spawn/track/kill an `apfs-mount.exe` child per device + watchdog.
- `crates/apfs-tray/src/winwin.rs` — `#[cfg(windows)]` hidden window + `RegisterDeviceNotificationW` + message loop.
- `crates/apfs-tray/src/tray.rs` — `#[cfg(windows)]` tray icon + menu (status / Unmount / Quit) + autostart install.
- `crates/apfs-tray/src/main.rs` — `#[cfg(windows)]` wire-up; `#[cfg(not(windows))]` no-op.
- `crates/apfs-tray/tests/supervisor_it.rs` — cross-platform supervisor integration test.
- `docs/ROADMAP.md` — M5 status update (final task).

Each pure module is independently unit-tested on macOS in the normal `cargo test`. Windows modules compile only under `#[cfg(windows)]`; their proof is the SSH real-USB run (final task).

---

### Task 1: Crate skeleton — Windows-target deps, cfg gating, macOS build stays green

**Files:**
- Modify: `crates/apfs-tray/Cargo.toml`
- Modify: `crates/apfs-tray/src/lib.rs`
- Create: `crates/apfs-tray/src/main.rs`, `supervisor.rs`, `detect.rs`, `child.rs`, `winwin.rs`, `tray.rs`

- [ ] **Step 1: Write the failing test**

Replace `crates/apfs-tray/src/lib.rs` with:

```rust
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! apfs-tray — user-session auto-mount agent + tray for APFS volumes.
//!
//! Cross-platform (unit-tested on macOS): [`supervisor`].
//! Windows-only (`#[cfg(windows)]`): device watch, child mgmt, tray.

pub mod supervisor;

#[cfg(windows)]
pub mod detect;
#[cfg(windows)]
pub mod child;
#[cfg(windows)]
pub mod winwin;
#[cfg(windows)]
pub mod tray;

#[cfg(test)]
mod skeleton_tests {
    #[test]
    fn crate_compiles_cross_platform() {
        assert_eq!(2 + 2, 4);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p apfs-tray`
Expected: FAIL — `supervisor` module file does not exist yet (unresolved module).

- [ ] **Step 3: Write minimal implementation**

Create stub files so the tree resolves (full impls in later tasks):

`crates/apfs-tray/src/supervisor.rs`:
```rust
//! Pure device→mount supervisor state machine (Task 2).
```
`crates/apfs-tray/src/detect.rs`:
```rust
//! APFS physical-drive detection (Task 3).
```
`crates/apfs-tray/src/child.rs`:
```rust
//! apfs-mount child process supervision (Task 4).
```
`crates/apfs-tray/src/winwin.rs`:
```rust
//! Hidden window + device-change notification (Task 5).
```
`crates/apfs-tray/src/tray.rs`:
```rust
//! Tray icon + menu + autostart (Task 6).
```
`crates/apfs-tray/src/main.rs`:
```rust
//! apfs-tray entry. Windows-only agent; no-op elsewhere.
#[cfg(not(windows))]
fn main() {
    eprintln!("apfs-tray is Windows-only.");
}

#[cfg(windows)]
fn main() {
    eprintln!("apfs-tray: not implemented yet (M5 Task 7).");
}
```

Replace `crates/apfs-tray/Cargo.toml` with:

```toml
[package]
name = "apfs-tray"
version = "0.0.0"
edition.workspace = true
license.workspace = true

[lints]
workspace = true

[[bin]]
name = "apfs-tray"
path = "src/main.rs"

[dependencies]
winfsp-host = { path = "../winfsp-host" }

[target.'cfg(windows)'.dependencies]
tray-icon = "0.19"

[target.'cfg(windows)'.dependencies.windows]
version = "0.58"
features = [
  "Win32_Foundation",
  "Win32_System_LibraryLoader",
  "Win32_System_Threading",
  "Win32_System_Registry",
  "Win32_UI_WindowsAndMessaging",
  "Win32_Devices_DeviceAndDriverInstallation",
]

[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p apfs-tray` (macOS) — Expected: PASS.
Run: `cargo build --workspace` — Expected: PASS (Windows deps not pulled on macOS).

- [ ] **Step 5: Commit**

```bash
git add crates/apfs-tray/Cargo.toml crates/apfs-tray/src/lib.rs crates/apfs-tray/src/main.rs crates/apfs-tray/src/supervisor.rs crates/apfs-tray/src/detect.rs crates/apfs-tray/src/child.rs crates/apfs-tray/src/winwin.rs crates/apfs-tray/src/tray.rs
git commit -m "feat(apfs-tray): crate skeleton with windows-target gated deps"
```

---

### Task 2: `supervisor` — pure device→mount state machine (macOS-tested)

**Files:**
- Modify: `crates/apfs-tray/src/supervisor.rs`

The supervisor is the brain: given the current set of present APFS drive numbers and the set it already has running children for, it decides which to MOUNT (newly arrived), which to UNMOUNT (removed), and tracks per-drive state incl. a restart counter for the watchdog. Pure (no Windows, no processes) → fully unit-tested on macOS.

- [ ] **Step 1: Write the failing test**

Write `crates/apfs-tray/src/supervisor.rs`:

```rust
//! Pure device→mount supervisor.
//!
//! Inputs: the set of APFS physical-drive numbers currently present, and
//! liveness of the child we spawned per drive. Output: the actions the
//! Windows layer must perform (spawn / kill). No Windows or process code
//! here so it is exhaustively unit-tested on any OS.

use std::collections::BTreeMap;

/// What the Windows layer should do for one drive this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Spawn `apfs-mount` for this physical drive number.
    Mount(u32),
    /// Kill the child for this physical drive number (device gone, or quit).
    Unmount(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Child spawned and believed alive.
    Mounted,
    /// Child died unexpectedly while the device is still present.
    Crashed { restarts: u32 },
}

/// Tracks intended mounts and produces actions. The Windows layer feeds it
/// observations (`present`, `child_alive`) and executes returned actions.
#[derive(Debug, Default)]
pub struct Supervisor {
    drives: BTreeMap<u32, State>,
}

/// Max automatic restarts before giving up on a crash-looping mount.
pub const MAX_RESTARTS: u32 = 3;

impl Supervisor {
    pub fn new() -> Self {
        Self {
            drives: BTreeMap::new(),
        }
    }

    /// Reconcile against the present APFS drive set. `present` = drive
    /// numbers detected now. Returns the actions to perform.
    pub fn reconcile(&mut self, present: &[u32]) -> Vec<Action> {
        let mut actions = Vec::new();
        // Removed devices: kill child, forget.
        let gone: Vec<u32> = self
            .drives
            .keys()
            .copied()
            .filter(|d| !present.contains(d))
            .collect();
        for d in gone {
            self.drives.remove(&d);
            actions.push(Action::Unmount(d));
        }
        // New devices: mount.
        for &d in present {
            if !self.drives.contains_key(&d) {
                self.drives.insert(d, State::Mounted);
                actions.push(Action::Mount(d));
            }
        }
        actions
    }

    /// Watchdog tick: the Windows layer reports, for each tracked drive,
    /// whether its child is still alive. A dead child for a still-present
    /// device is respawned up to `MAX_RESTARTS`.
    pub fn watchdog(&mut self, alive: &dyn Fn(u32) -> bool) -> Vec<Action> {
        let mut actions = Vec::new();
        let drives: Vec<u32> = self.drives.keys().copied().collect();
        for d in drives {
            if alive(d) {
                if let Some(s) = self.drives.get_mut(&d) {
                    *s = State::Mounted;
                }
                continue;
            }
            let restarts = match self.drives.get(&d) {
                Some(State::Crashed { restarts }) => *restarts,
                _ => 0,
            };
            if restarts >= MAX_RESTARTS {
                continue; // give up; leave tracked but no more respawns
            }
            self.drives.insert(
                d,
                State::Crashed {
                    restarts: restarts + 1,
                },
            );
            actions.push(Action::Mount(d));
        }
        actions
    }

    /// Drives we currently believe are mounted (for the tray menu).
    pub fn tracked(&self) -> Vec<u32> {
        self.drives.keys().copied().collect()
    }

    /// Quit: unmount everything.
    pub fn shutdown(&mut self) -> Vec<Action> {
        let actions = self
            .drives
            .keys()
            .map(|&d| Action::Unmount(d))
            .collect();
        self.drives.clear();
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mounts_new_unmounts_removed() {
        let mut s = Supervisor::new();
        assert_eq!(s.reconcile(&[3]), vec![Action::Mount(3)]);
        // Idempotent: already mounted -> no action.
        assert_eq!(s.reconcile(&[3]), vec![]);
        // New drive added alongside.
        assert_eq!(s.reconcile(&[3, 5]), vec![Action::Mount(5)]);
        // Drive 3 removed.
        assert_eq!(s.reconcile(&[5]), vec![Action::Unmount(3)]);
        assert_eq!(s.tracked(), vec![5]);
    }

    #[test]
    fn watchdog_respawns_dead_child_up_to_limit() {
        let mut s = Supervisor::new();
        s.reconcile(&[3]);
        let dead = |_d: u32| false;
        // 3 restarts then gives up.
        assert_eq!(s.watchdog(&dead), vec![Action::Mount(3)]);
        assert_eq!(s.watchdog(&dead), vec![Action::Mount(3)]);
        assert_eq!(s.watchdog(&dead), vec![Action::Mount(3)]);
        assert_eq!(s.watchdog(&dead), vec![]); // MAX_RESTARTS reached
    }

    #[test]
    fn watchdog_noop_when_alive() {
        let mut s = Supervisor::new();
        s.reconcile(&[3]);
        let alive = |_d: u32| true;
        assert_eq!(s.watchdog(&alive), vec![]);
    }

    #[test]
    fn alive_again_resets_crash_count() {
        let mut s = Supervisor::new();
        s.reconcile(&[3]);
        let dead = |_d: u32| false;
        let alive = |_d: u32| true;
        assert_eq!(s.watchdog(&dead), vec![Action::Mount(3)]);
        assert_eq!(s.watchdog(&alive), vec![]); // recovered -> Mounted
        // Crash again: counter restarted from 0, so it respawns again.
        assert_eq!(s.watchdog(&dead), vec![Action::Mount(3)]);
    }

    #[test]
    fn shutdown_unmounts_all() {
        let mut s = Supervisor::new();
        s.reconcile(&[3, 5]);
        let mut a = s.shutdown();
        a.sort_by_key(|x| match x {
            Action::Unmount(d) | Action::Mount(d) => *d,
        });
        assert_eq!(a, vec![Action::Unmount(3), Action::Unmount(5)]);
        assert!(s.tracked().is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p apfs-tray supervisor`
Expected: FAIL first (paste tests + signatures with `reconcile` returning `vec![]`, observe `mounts_new_unmounts_removed` FAIL), then add the real bodies.

- [ ] **Step 3: Write minimal implementation**

The bodies above are the implementation. No `unwrap/expect/panic` outside `#[cfg(test)]`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p apfs-tray supervisor` — Expected: PASS (5 tests).
Run: `cargo clippy -p apfs-tray --all-targets -- -D warnings` — Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/apfs-tray/src/supervisor.rs
git commit -m "feat(apfs-tray): pure device-to-mount supervisor with watchdog"
```

---

### Task 3: `detect` — enumerate APFS physical drives (Windows-only)

**Files:**
- Modify: `crates/apfs-tray/src/detect.rs`

- [ ] **Step 1: Write the implementation**

Write `crates/apfs-tray/src/detect.rs`:

```rust
//! Enumerate physical drives and return those whose GPT contains an APFS
//! container. Thin orchestration over the proven winfsp-host primitives —
//! no APFS parsing is re-implemented here.

use winfsp_host::gpt::find_apfs_container;
use winfsp_host::winraw::PhysicalDrive;

/// Physical-drive numbers (`\\.\PhysicalDriveN`) that currently hold an
/// APFS container. Scans 0..16 (same bound as `autodetect_apfs`).
pub fn present_apfs_drives() -> Vec<u32> {
    let mut out = Vec::new();
    for n in 0..16u32 {
        if let Ok(mut d) = PhysicalDrive::open(n) {
            if find_apfs_container(&mut d).is_ok() {
                out.push(n);
            }
        }
    }
    out
}
```

- [ ] **Step 2: Verify (deferred to box)**

macOS: `#[cfg(windows)]` — not compiled. Real verification in the final SSH task (it must list the real APFS USB's drive number, e.g. `[3]`).

- [ ] **Step 3: Confirm macOS workspace still builds**

Run: `cargo build --workspace` — Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/apfs-tray/src/detect.rs
git commit -m "feat(apfs-tray): APFS physical-drive detection via winfsp-host (windows)"
```

---

### Task 4: `child` — spawn/track/kill apfs-mount per drive (Windows-only)

**Files:**
- Modify: `crates/apfs-tray/src/child.rs`

Out-of-process isolation: each APFS drive is mounted by its own `apfs-mount.exe --drive N --wait-for-kill` child. A crash/hang in one mount cannot take down the agent or other mounts; killing the child makes WinFsp unmount.

- [ ] **Step 1: Write the implementation**

Write `crates/apfs-tray/src/child.rs`:

```rust
//! Per-drive `apfs-mount.exe` child supervision (Windows-only).

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Child, Command};

/// Tracks one `apfs-mount` child per physical-drive number.
#[derive(Default)]
pub struct Children {
    map: HashMap<u32, Child>,
}

/// Locate `apfs-mount.exe` next to the current exe (packaged layout) or
/// fall back to the cargo target dir during development.
fn apfs_mount_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("apfs-mount.exe");
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from("apfs-mount.exe") // rely on PATH/cwd as last resort
}

impl Children {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
        }
    }

    /// Spawn (or respawn) `apfs-mount` for `drive`. Any existing child for
    /// that drive is killed first (respawn path). Auto free drive-letter
    /// (no `--mount`).
    pub fn spawn(&mut self, drive: u32) -> std::io::Result<()> {
        self.kill(drive);
        let child = Command::new(apfs_mount_path())
            .arg("--drive")
            .arg(drive.to_string())
            .arg("--wait-for-kill")
            .spawn()?;
        self.map.insert(drive, child);
        Ok(())
    }

    /// Kill and reap the child for `drive` (WinFsp unmounts on exit).
    pub fn kill(&mut self, drive: u32) {
        if let Some(mut c) = self.map.remove(&drive) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Is the child for `drive` still running? (`try_wait` == Ok(None)).
    pub fn alive(&mut self, drive: u32) -> bool {
        match self.map.get_mut(&drive) {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    pub fn kill_all(&mut self) {
        let drives: Vec<u32> = self.map.keys().copied().collect();
        for d in drives {
            self.kill(d);
        }
    }
}
```

- [ ] **Step 2: Verify (deferred to box)** — `#[cfg(windows)]`; proven in the final SSH task.

- [ ] **Step 3:** `cargo build --workspace` (macOS) — Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/apfs-tray/src/child.rs
git commit -m "feat(apfs-tray): out-of-process apfs-mount child supervision (windows)"
```

---

### Task 5: `winwin` — hidden window + device-change notification (Windows-only)

**Files:**
- Modify: `crates/apfs-tray/src/winwin.rs`

- [ ] **Step 1: Write the implementation**

Write `crates/apfs-tray/src/winwin.rs` (exact `windows 0.58` API resolved on the box; shape per windows-docs-rs):

```rust
//! Hidden message-only window that receives WM_DEVICECHANGE for volume
//! arrival/removal, so the agent reacts to USB plug/unplug. Windows-only.

use std::sync::mpsc::Sender;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassW, HWND_MESSAGE,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_DEVICECHANGE, WNDCLASSW,
};

/// Sent on the channel when any device-change event arrives; the main loop
/// then re-scans drives (debounced) — we don't parse the broadcast struct,
/// a re-scan via `detect` is simpler and robust.
pub struct DeviceEvent;

static mut TX: Option<Sender<DeviceEvent>> = None;

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
) -> LRESULT {
    if msg == WM_DEVICECHANGE {
        // Any device-change: notify the loop to re-scan (debounced there).
        #[allow(static_mut_refs)]
        if let Some(tx) = TX.as_ref() {
            let _ = tx.send(DeviceEvent);
        }
        return LRESULT(1);
    }
    DefWindowProcW(hwnd, msg, w, l)
}

/// Create the hidden message-only window. Returns its HWND. Sends a
/// `DeviceEvent` on `tx` for every WM_DEVICECHANGE.
pub fn create_device_window(
    tx: Sender<DeviceEvent>,
) -> windows::core::Result<HWND> {
    unsafe {
        // SAFETY: single-threaded init before the message loop starts;
        // wndproc only reads TX after this is set.
        TX = Some(tx);
        let hinst = GetModuleHandleW(PCWSTR::null())?;
        let class_name = windows::core::w!("ApfsTrayDevWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            windows::core::w!("apfs-tray"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        )?;
        Ok(hwnd)
    }
}
```

Notes for the box build: `WM_DEVICECHANGE` with `HWND_MESSAGE` parent delivers `DBT_DEVICEARRIVAL`/`DBT_DEVICEREMOVECOMPLETE` for volume changes without an explicit `RegisterDeviceNotificationW` for the common removable-volume case; if testing shows events are missed, add `RegisterDeviceNotificationW` with `DBT_DEVTYP_DEVICEINTERFACE` + `GUID_DEVINTERFACE_VOLUME` (feature `Win32_Devices_DeviceAndDriverInstallation`). Resolve exact `CreateWindowExW`/`GetModuleHandleW` arg shapes against pinned `windows 0.58` (compiler arbiter); keep the re-scan-on-any-event design.

- [ ] **Step 2:** macOS not compiled; box-proven in final task.
- [ ] **Step 3:** `cargo build --workspace` (macOS) — Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git add crates/apfs-tray/src/winwin.rs
git commit -m "feat(apfs-tray): hidden window + WM_DEVICECHANGE notification (windows)"
```

---

### Task 6: `tray` — tray icon, menu, autostart (Windows-only)

**Files:**
- Modify: `crates/apfs-tray/src/tray.rs`

- [ ] **Step 1: Write the implementation**

Write `crates/apfs-tray/src/tray.rs` (exact `tray-icon 0.19` API resolved on the box):

```rust
//! System-tray icon + menu and per-user autostart. Windows-only.

use tray_icon::menu::{Menu, MenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

/// Stable menu item ids.
pub const ID_UNMOUNT_ALL: &str = "unmount_all";
pub const ID_QUIT: &str = "quit";

/// Build the tray icon with a status line + Unmount All + Quit. `status`
/// is a human string like "APFS: Z: (drive 3)" or "No APFS volume".
pub fn build_tray(status: &str) -> tray_icon::Result<TrayIcon> {
    let menu = Menu::new();
    let _ = menu.append(&MenuItem::new(status, false, None)); // disabled label
    let _ = menu.append(&MenuItem::with_id(
        ID_UNMOUNT_ALL,
        "Unmount All",
        true,
        None,
    ));
    let _ = menu.append(&MenuItem::with_id(ID_QUIT, "Quit", true, None));
    TrayIconBuilder::new()
        .with_tooltip("apfs-explorer")
        .with_menu(Box::new(menu))
        .build()
}

/// Register this exe for the current user's logon autostart (no admin).
/// Idempotent. `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.
pub fn install_autostart() -> windows::core::Result<()> {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    let exe = std::env::current_exe()
        .map_err(|_| windows::core::Error::from_win32())?;
    let mut wide: Vec<u16> =
        exe.to_string_lossy().encode_utf16().collect();
    wide.push(0);
    unsafe {
        let mut hkey = HKEY::default();
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run"),
            Some(0),
            PCWSTR_NULL,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        )
        .ok()?;
        let bytes = std::slice::from_raw_parts(
            wide.as_ptr() as *const u8,
            wide.len() * 2,
        );
        let _ = RegSetValueExW(
            hkey,
            w!("apfs-tray"),
            Some(0),
            REG_SZ,
            Some(bytes),
        );
        let _ = RegCloseKey(hkey);
    }
    Ok(())
}

use windows::core::PCWSTR as PCWSTR_NULL_TY;
#[allow(non_upper_case_globals)]
const PCWSTR_NULL: PCWSTR_NULL_TY = windows::core::PCWSTR(std::ptr::null());
```

Box-build notes: `tray-icon 0.19` menu/builder method names + `RegCreateKeyExW`/`RegSetValueExW` arg shapes are resolved against the pinned crates (compiler arbiter); keep the behavior (status label + Unmount All + Quit; HKCU Run autostart). An icon resource is optional — `TrayIconBuilder` without `.with_icon` shows a default; add an embedded icon later (cosmetic, not blocking).

- [ ] **Step 2:** macOS not compiled; box-proven in final task.
- [ ] **Step 3:** `cargo build --workspace` (macOS) — Expected: PASS.
- [ ] **Step 4: Commit**

```bash
git add crates/apfs-tray/src/tray.rs
git commit -m "feat(apfs-tray): tray icon/menu + per-user autostart (windows)"
```

---

### Task 7: `main` — wire the agent loop together (Windows-only)

**Files:**
- Modify: `crates/apfs-tray/src/main.rs`

- [ ] **Step 1: Write the implementation**

Replace `crates/apfs-tray/src/main.rs`:

```rust
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
    use apfs_tray::detect::present_apfs_drives;
    use apfs_tray::supervisor::{Action, Supervisor};
    use apfs_tray::tray::{build_tray, install_autostart, ID_QUIT, ID_UNMOUNT_ALL};
    use apfs_tray::winwin::create_device_window;
    use tray_icon::menu::MenuEvent;
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };

    let _ = install_autostart();

    let (tx, rx) = channel();
    if create_device_window(tx).is_err() {
        eprintln!("apfs-tray: failed to create device window");
        std::process::exit(1);
    }

    let mut sup = Supervisor::new();
    let mut kids = Children::new();
    let mut tray = build_tray("Starting…").ok();

    let apply = |acts: Vec<Action>, kids: &mut Children| {
        for a in acts {
            match a {
                Action::Mount(d) => {
                    let _ = kids.spawn(d);
                }
                Action::Unmount(d) => kids.kill(d),
            }
        }
    };

    // Initial scan (USB may already be plugged in at login).
    apply(sup.reconcile(&present_apfs_drives()), &mut kids);

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
            if ev.id.0 == ID_QUIT {
                apply(sup.shutdown(), &mut kids);
                kids.kill_all();
                return;
            } else if ev.id.0 == ID_UNMOUNT_ALL {
                apply(sup.shutdown(), &mut kids);
            }
        }
        // Device-change (debounced via the recv timeout) → re-scan.
        let rescan = match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(_) => true,
            Err(RecvTimeoutError::Timeout) => true, // also drives watchdog
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if rescan {
            apply(sup.reconcile(&present_apfs_drives()), &mut kids);
            let acts =
                sup.watchdog(&|d| kids.alive(d));
            apply(acts, &mut kids);
            let status = match sup.tracked().as_slice() {
                [] => "No APFS volume".to_string(),
                ds => format!("APFS mounted: drive(s) {ds:?}"),
            };
            if let Ok(t) = build_tray(&status) {
                tray = Some(t);
            }
        }
    }
}
```

Box-build notes: resolve `tray-icon 0.19` `MenuEvent::receiver()`/`ev.id` and the `PeekMessageW`/`MSG` `windows 0.58` shapes on the box (compiler arbiter). Keep the structure: pump messages → handle menu → on device-event/timeout re-scan + watchdog + refresh tray. The 2 s timeout doubles as the watchdog tick and a debounce.

- [ ] **Step 2:** `cargo build --workspace` (macOS) builds the `#[cfg(not(windows))]` arm — Expected: PASS.
- [ ] **Step 3: Commit**

```bash
git add crates/apfs-tray/src/main.rs
git commit -m "feat(apfs-tray): agent loop wiring device-watch + supervisor + tray (windows)"
```

---

### Task 8: cross-platform supervisor integration test

**Files:**
- Create: `crates/apfs-tray/tests/supervisor_it.rs`

- [ ] **Step 1: Write the test**

```rust
//! End-to-end of the PURE supervisor against a scripted device timeline
//! (no Windows): plug 3 → plug 5 → crash 3 (respawn) → unplug 5 → quit.

use apfs_tray::supervisor::{Action, Supervisor};

#[test]
fn full_lifecycle_timeline() {
    let mut s = Supervisor::new();
    assert_eq!(s.reconcile(&[3]), vec![Action::Mount(3)]);
    assert_eq!(s.reconcile(&[3, 5]), vec![Action::Mount(5)]);

    // Child 3 died, 5 alive → only 3 respawns.
    let alive = |d: u32| d == 5;
    assert_eq!(s.watchdog(&alive), vec![Action::Mount(3)]);

    // 5 unplugged.
    assert_eq!(s.reconcile(&[3]), vec![Action::Unmount(5)]);
    assert_eq!(s.tracked(), vec![3]);

    // Quit.
    assert_eq!(s.shutdown(), vec![Action::Unmount(3)]);
    assert!(s.tracked().is_empty());
}
```

- [ ] **Step 2: Run** `cargo test -p apfs-tray --test supervisor_it` — Expected: PASS.
- [ ] **Step 3: Commit**

```bash
git add crates/apfs-tray/tests/supervisor_it.rs
git commit -m "test(apfs-tray): supervisor full-lifecycle timeline"
```

---

### Task 9: Windows box build + real-USB auto-mount proof + finish

**Files:**
- Modify: `docs/ROADMAP.md`

Milestone-end decisive real-target proof (project methodology: real OS + real device is the arbiter). SSH alias `apfswin` (key auth, no password). Repo synced via `git bundle` → `scp` → box `git fetch/reset` (recipe in ROADMAP). The real APFS USB is on `\\.\PhysicalDrive3`.

- [ ] **Step 1: Sync + build on the box.** `cargo build -p apfs-tray --bin apfs-tray` (and ensure `apfs-mount.exe` is built next to it: `cargo build -p winfsp-host --bin apfs-mount`). Iterate ONLY on pinned `tray-icon 0.19` / `windows 0.58` API-shape mismatches in `winwin.rs`/`tray.rs`/`main.rs` (compiler arbiter); record resolved shapes in `// <crate> <ver>:` comments. Do not change `supervisor`/`detect`/`child` logic. End state: build PASS on the box.

- [ ] **Step 2: Cross-platform + box tests.** On the box: `cargo test -p apfs-tray` — supervisor unit + `supervisor_it` PASS.

- [ ] **Step 3: Real auto-mount proof (hang-safe, always-cleanup).** With the APFS USB on PhysicalDrive3, launch `apfs-tray.exe` detached (Win32_Process.Create won't show the drive — use `Start-Process` in the SSH user session, like M4b). Within a `Start-Job` + `Wait-Job -Timeout 90` (so a hang is detected, never hangs SSH): poll for a new drive letter appearing (the agent's initial scan must auto-mount the already-present USB); assert the drive exists and lists the real APFS root (`man`, `.Spotlight-V100`, …); then signal Quit (kill `apfs-tray.exe`) and assert the child `apfs-mount.exe` is reaped and the drive is gone. Capture: chosen letter, root entries, child-pid alive→dead, drive gone. Require: mounted=True, root non-empty, cleanup GONE=True. Always kill `apfs-tray`/`apfs-mount` at the end so SSH returns.

- [ ] **Step 4: Final consolidated review.** Dispatch one independent reviewer of the whole `feat/m5-apfs-tray-agent` branch (spec-compliance + quality + gates: `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace`; re-derive supervisor state-machine correctness incl. watchdog restart-cap & crash-count reset; confirm no `unwrap/expect/panic` in production, Windows deps target-gated, fault isolation = child crash can't kill agent). Fix any blocking findings.

- [ ] **Step 5: ROADMAP + KB + finish.** Update `docs/ROADMAP.md`: M5 status row + squash commit; "Compact sonrası DEVAM" next = **M6** (WiX/MSI packaging, WinFsp bundled, ship `apfs-tray` + `apfs-mount` together + autostart); durable M5 notes (user-session agent design rationale; `tray-icon 0.19`/`windows 0.58` resolved shapes; out-of-process child isolation; HKCU autostart; device-change = re-scan-on-any-event). Re-index `project:roadmap-state`. Commit ROADMAP. Then superpowers:finishing-a-development-branch: verify gates → squash-merge `feat/m5-apfs-tray-agent` → main → delete branch → fill `<squash>` SHA + re-index.

```bash
git add docs/ROADMAP.md
git commit -m "docs(roadmap): M5 user-session auto-mount agent + tray complete; next M6"
```

---

## Self-Review

**Spec coverage:** ROADMAP M5 = auto-mount/unmount on USB insert/remove (Tasks 2/3/5/7) ✓, watchdog (Task 2 `watchdog` + Task 7 loop) ✓, tray thin UI (Task 6) ✓, WM_DEVICECHANGE + APFS detect by GPT GUID + NXSB (Task 5 + Task 3 via winfsp-host) ✓, drive-letter visible to user (user-session agent — the chosen architecture; runs in the logged-in session) ✓, UAC-free (HKCU autostart, no service install) ✓, reuses M4b `apfs-mount` delegate surface (Task 4 spawns it) ✓. No APFS on-disk spec → no md.6. Drive-letter session-visibility (the M4b gap) is resolved structurally by running the mounter in the user session.

**Placeholder scan:** every code step is complete. The only deferrals are exact `tray-icon 0.19` / `windows 0.58` API shapes, explicitly framed as compiler-on-the-box-is-arbiter with a concrete resolution procedure (Task 9 Step 1) — consistent with the project's empirical-arbiter methodology, not placeholders.

**Type consistency:** `Supervisor::reconcile/watchdog/shutdown/tracked` return `Vec<Action>` / `Vec<u32>` used consistently in Task 7; `Children::spawn/kill/alive/kill_all` signatures match Task 7 call sites; `present_apfs_drives() -> Vec<u32>` feeds `reconcile`; `create_device_window(Sender<DeviceEvent>)` matches the channel in `main`. `apfs-mount` invoked with `--drive N --wait-for-kill` (flags exist per M4b). Watchdog `alive` closure `&dyn Fn(u32)->bool` matches `kids.alive(d)`.

**Scope:** one milestone, 9 bite-sized TDD tasks; pure brain (supervisor) fully macOS-tested; Windows shell proven on real hardware. Lean consolidated review per project process.
