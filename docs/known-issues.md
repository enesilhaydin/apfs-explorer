# Known Issues and Deferred Polish

Issues found during the proactive UX/runtime bug audit (2026-05-26).
Fixed items are tracked in git history. This file lists items that
require GUI/Windows-only manual testing or design work before they
can be closed.

## Deferred - requires Windows GUI test

### KI-001: Quit with active mounts - no confirmation dialog (MED)
File: `crates/apfs-tray/src/main.rs`, `ID_QUIT` handler.
Clicking Quit with volumes mounted performs a clean unmount silently.
A "are you sure?" `MessageBoxW` confirmation dialog would prevent
accidental data loss on unsaved writes. Deferred: requires manual
Windows smoke test to verify modal behaviour.

### KI-002: About dialog floats behind other windows (LOW)
File: `crates/apfs-tray/src/main.rs`, `show_about_dialog`.
`MessageBoxW(None, ...)` has no parent `HWND`, so the dialog can
appear behind other windows on multi-monitor setups. Fix: pass the
invisible device HWND created in `winwin.rs` as parent. Deferred:
needs Windows visual verification.

### KI-003: Mount Disk Image file-picker not implemented (MED)
File: `crates/apfs-tray/src/main.rs`, `ID_MOUNT_IMAGE` handler.
Menu item is currently disabled with label "coming soon". Full
implementation requires `IFileOpenDialog` (COM) integration and
driving a child `apfs-mount --image <path>` process. Planned for
the next milestone.

### KI-004: Volume label not shown in tray menu (LOW)
File: `crates/apfs-tray/src/main.rs`, `build_volume_list`.
`VolumeInfo.label` is always empty; the menu shows "Z: APFS" instead
of the real volume name. Fix: call `GetVolumeInformationW` on the
mounted drive letter after mount is confirmed live. Deferred: minor
polish, requires Windows environment.

### KI-005: Used/total bytes not shown in tray menu (LOW)
File: `crates/apfs-tray/src/main.rs`, `build_volume_list`.
`used_bytes` and `total_bytes` are always 0. Fix: call
`GetDiskFreeSpaceExW` on the drive letter. Deferred: minor polish.

### KI-006: Re-mount Read-Only missing from RW-mounted volumes (LOW)
File: `crates/apfs-tray/src/tray.rs`, `build_tray`.
The "Re-mount Read-Only" menu item is always rendered, including when
the volume is already mounted read-only. Should be hidden or disabled
when `VolumeInfo` indicates a read-only mount. Requires tray to track
mount writability per drive, which needs an additional status-file
field.

## Fixed in this audit session

| Commit | Severity | Description |
|--------|----------|-------------|
| (pending) | HIGH | `rebuild_tray` kept old icon alive on error instead of dropping it |
| (pending) | HIGH | About dialog URL contained placeholder `USER` token |
| (pending) | MED | `spawn_with_opts` error silently discarded in remount-RO path |
| (pending) | MED | `Mount Disk Image` menu item was enabled but did nothing visible |
| (pending) | MED | `default_icon()` used `expect` - replaced with graceful fallback |
