# Architecture Reference

## Layers and single responsibility

```
apfs-tray        thin UI client (separate process)
   │  named pipe IPC (read status / send commands)
apfs-service     Windows Service (LocalSystem)
   │
winfsp-host      WinFsp callback implementation
   │
apfs             safe high-level API
   │
apfs-core        on-disk parse + BlockDevice trait
   │
\\.\PhysicalDriveN   raw block device
```

Dependencies are strictly one-directional (upper layers do not know lower
layers exist): `apfs-core → apfs → {apfs-write, winfsp-host} → apfs-service →
apfs-tray`. `xtask` is independent (fixture/harness).

### `apfs-core`
- NX superblock, checkpoint area, object map (omap), B-tree (catalog/extent/
  snapshot), Fletcher-64 checksum, oid/xid resolution.
- `BlockDevice` trait: `read_block`/`block_size`/`size`. Production = raw `\\.\`
  handle; tests = file-backed image. **The parser has no knowledge of the
  underlying device type** (testability).
- Target `#![forbid(unsafe_code)]`. `unwrap/expect/panic` are forbidden. Corrupt
  input → `Result`. APFS structure references: the APFS specification.

### `apfs`
- Open container, list volumes + **system-volume filter** (Preboot/Recovery/
  VM/Update hidden; derived from role flags), traverse directories, read file
  extents, xattr + macOS/Finder metadata (tags, color labels, resource forks).
- Compression codecs: ZLIB (DEFLATE), LZVN, LZFSE - dispatched from the
  `decmpfs` header.
- FileVault (software) decryption: keybag → wrapped VEK → AES-XTS.
- Snapshot reading (xid-based). All public functions return `Result<_, ApfsError>`.

### `apfs-write` (`write` feature, v2)
- Copy-on-write (COW) mutation. **Verify-before-commit architecture is mandatory:**
  1. Automatic snapshot before any write
  2. Write
  3. Independent re-read (code path separate from the writer) + checksum differential
  4. On mismatch: rollback to snapshot + `Result::Err` + user notification
- linux-apfs-rw serves as a correctness *reference* (not a code copy).
- Does not compile at all until v1 is stable on real disks (feature disabled).

### `winfsp-host`
- v1 read-only callbacks: `GetVolumeInfo`, `GetSecurityByName` (static
  "everyone read" DACL), `Open`, `Close`, `Read`, `ReadDirectory`,
  `GetFileInfo`.
- `Create/Write/Rename/SetBasicInfo/Flush` are **absent at the code level** in
  v1 → read-only guarantee enforced by the compiler.
- Every callback is wrapped in `std::panic::catch_unwind` → a corrupt file
  returns an error for that operation only, without taking down the volume.
- Write callbacks are added in v2 as a separate feature; each passes through
  the `apfs-write` verification pipeline.

### `apfs-service`
- Windows Service (LocalSystem). Starts at boot, no UAC prompt, runs while
  sessions are closed.
- `RegisterDeviceNotification` + `WM_DEVICECHANGE`
  (`DBT_DEVICEARRIVAL`/`DBT_DEVICEREMOVECOMPLETE`).
- On disk arrival: `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` → GPT type GUID
  `7C3457EF-0000-11AA-AA11-00306543ECAC` → validate `NXSB` (`0x4253584E`) magic
  → mount user volumes to available drive letters read-only.
- Watchdog: panic/exit → automatic restart; mount state is persistent
  (config/state) and is restored on restart.
- Config (TOML): drive letter assignments, auto-mount enable/disable, log level.

### `apfs-tray`
- Separate process. Connects to the service via named pipe: disk/volume list,
  capacity, mount status, manual mount/unmount, read-only warning, log viewer.
- A crash here does **not affect mounts** (the service is independent).

## Data flow (mount)

disk inserted → service receives event → validate partition/GPT/NXSB →
volume filter → `winfsp-host` mount → File Explorer calls → `winfsp-host`
callback → `apfs` API → `apfs-core` parse → raw `BlockDevice` read.
Removal → clean unmount.

## WinFsp / installer

- `winfsp` crate (SnowflakePowered/winfsp-rs ≥0.13), native API.
- Microsoft-signed WinFsp MSI **bundled in our installer** (WiX). One-time
  install, no reboot required. The user installs nothing manually.
- WinFsp version pinned to stable 2.x; update when a new stable is released.
