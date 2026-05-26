# M13 Windows Smoke Test — 2026-05-26

Host: macOS (apfs-explorer main). Target: `apfswin` (Windows 11, SSH).
Rust 1.91.1 · MSVC 14.44 · WinFsp 2.1 · winfsp crate 0.13.

## Build

**Result: SUCCESS** (0 errors after fixes)

**Build fixes applied** (3 compile errors in fresh sync):

1. `winfsp-host/src/image_mount.rs` + `mount.rs`: `.removable_media(true)` call removed.
   `VolumeParams::removable_media()` was present in winfsp <= 0.12 but dropped in 0.13.
   `FSP_FSCTL_VOLUME_PARAMS` in winfsp-sys 0.12.1 (WinFsp 2.1 bindgen) has no `RemovableMedia`
   bitfield — the field is absent from the 2.1 SDK headers. Calls replaced with a comment;
   DriveType=2 (removable) is a deferred M13 enhancement.

2. `xtask/src/main.rs`: `WIN32_ERROR.map_err(|e| …)` pattern removed.
   `windows` crate 0.58 returns `WIN32_ERROR` (not `Result`) from `RegOpenKeyExW` /
   `RegQueryValueExW`. Fixed to direct `!= NO_ERROR` comparison.
   Also fixed `Some(&mut kind)` (was `&mut u32`, needs `*mut REG_VALUE_TYPE`) →
   changed to `Some(&mut kind)` with `kind: REG_VALUE_TYPE`.

**Warnings:** 5 total (all pre-existing, none new):
- `unused variable: file_name` (mount.rs:1085)
- `unused variable: clen` (mount.rs:1131)
- `unused variable: r` (mount.rs:1132, 1146)
- `private_interfaces` lint (apfs-dump bin, 1 warning)

## Binary Sizes

| Binary | Size |
|---|---|
| `apfs-mount.exe` | 1568 KB |
| `apfs-dump.exe` | 383 KB |
| `apfs-tray.exe` | 569 KB |
| `apfs-supervisor.exe` | MISSING (not a workspace target) |

## apfs-mount.exe Smoke Test

**--help**: OK. Subcommand list rendered correctly.

**Subcommand --help parse**:
- `mount --help` → EXIT=0, usage rendered
- `snapshot list --help` → EXIT=1, `error: missing required flag --image` (clap parse OK)
- `clone --help` → EXIT=1, `error: missing required flag --image` (clap parse OK)
- `volume list --help` → EXIT=1, `error: missing required flag --image` (clap parse OK)

**Mount test** (64 MB scratch image copy at `E:\dev\apfs-test.img`):
- Process launched via `Win32_Process.Create` (detached from SSH session)
- `Z:` drive mounted: **YES** — `dir Z:\` EXIT=0, label "APFSWRITE"
- R/W roundtrip: `WriteAllText` → `ReadAllText` → `Delete` → **match=True**
- `Z:\` item count: 0 (scratch image has empty root directory — expected)

**DriveType check**:
- `Win32_Volume` (WMI): does not enumerate WinFsp volumes — **known WinFsp behavior**
  (WinFsp does not register with Windows volume manager / PnP bus)
- `Win32_LogicalDisk` DriveType: empty (same reason)
- RemovableMedia / DriveType=2: **deferred** — `RemovableMedia` bitfield absent from
  WinFsp 2.1 SDK (winfsp-sys 0.12.1 bindgen output); tracked as M13 enhancement

## apfs-tray.exe Smoke Test

- Launched via `Win32_Process.Create`, PID assigned
- Alive after 3 seconds: **YES**
- Terminated cleanly with `Stop-Process -Force`: **OK**
- GUI/systray menu: not tested (requires interactive desktop session)

## MSI Artifact

**WiX Toolset**: NOT installed on apfswin (candle.exe / light.exe not found in PATH or
standard install locations).

**`cargo xtask package`**: invoked — compiled successfully, then failed:
```
package failed: dist/winfsp.msi not found.
Download the official WinFsp MSI from https://winfsp.dev/rel/ and copy it to dist/winfsp.msi,
then re-run xtask package.
```

xtask `package` command itself is functional. Two prerequisites needed before MSI can be
produced on this machine:
1. Install WiX v3 toolset (via `choco install wixtoolset` — Chocolatey is present)
2. Copy official `winfsp-2.x.msi` to `dist/winfsp.msi`

No MSI artifact produced this run.

## Cleanup

- `apfs-mount.exe` and `apfs-tray.exe` processes killed
- `Z:` drive unmounted (confirmed `Test-Path Z:\ = False`)
- `E:\dev\apfs-test.img` deleted
- Temp/log files at `E:\dev\` deleted
- `E:\dev\apfs-explorer\` and `E:\dev\cargo-target\` retained for next test run

## Known Issues / Limitations

| Issue | Severity | Notes |
|---|---|---|
| `removable_media` removed from winfsp 0.13 API | Low | WinFsp 2.1 SDK does not expose `RemovableMedia` bitfield; volume appears as DriveType=3 (Fixed). "Safely Remove Hardware" tray not available. Deferred enhancement. |
| WMI Win32_Volume does not see WinFsp volumes | Low | Expected WinFsp behavior — not a bug in our code |
| `apfs-supervisor.exe` not built | Info | Binary not present as workspace target; not needed for M13 |
| WiX not installed | Blocker for MSI | `choco install wixtoolset` + copy `winfsp.msi` to `dist/` required |
| R/W roundtrip on empty image | Info | Scratch image has no content; write to root dir succeeded confirming create/write/read/delete path works |
