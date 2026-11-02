# Architecture Reference

## Layers and single responsibility

```
apfs-explorer-gui   desktop app (Tauri): lists drives, asks the WinFsp
   │                launcher to start / stop apfs-mount
   │  WinFsp launcher (service, LocalSystem)
winfsp-host         apfs-mount.exe: WinFsp callbacks, write grouping,
   │                block cache, raw drive / image devices, CLI subcommands
apfs-write          copy-on-write transactions and every write operation
   │
apfs                FsView: paths, listings, reads, xattrs, symlinks
   │
apfs-core           on-disk structures, B-trees, object maps, codecs, GPT,
   │                BlockDevice trait
\\.\PhysicalDriveN or an image file
```

Dependencies point one way: `apfs-core → apfs → apfs-write → winfsp-host`.
The app does not link the engine; it starts `apfs-mount.exe` through the
WinFsp launcher. `installer` holds the version helpers for the MSI, `xtask`
builds fixtures and the installer.

### `apfs-core`
- NX superblock, checkpoint area, object map, B-trees (catalog, extent
  reference, snapshot metadata), Fletcher-64, oid/xid resolution.
- Catalog records: inodes, directory records (hashed and plain names),
  extents, xattrs, sibling links; decmpfs with zlib, LZVN, LZFSE and LZBITMAP.
- `gpt::find_apfs_container`: the APFS partition of a whole-disk image.
- `BlockDevice` / `WritableBlockDevice`: byte-addressed reads and writes,
  plus the durability barrier (`flush_data`). The parser does not know the
  device kind.
- `#![forbid(unsafe_code)]`; no `unwrap`/`expect`/`panic`; corrupt input is
  a `Result` error.

### `apfs`
- `FsView`: opens a container, picks a volume (`VolumeSelector`: the user
  data volume by default, or by UUID, role or index), resolves paths with the
  volume's own name rules (full Unicode case fold and normalization, or exact),
  lists directories, reads byte ranges (only the extents or compressed chunks
  a range touches), xattrs, symlinks, snapshots.
- Caches: resolved paths and directory listings, dropped when the view is
  rebound to a newer volume superblock.
- Encrypted (FileVault) volumes are refused, not decrypted.

### `apfs-write` (`write` feature)
- `Transaction`: copy-on-write. Changed objects get new blocks, staged in
  memory (file data of 1 MiB or more goes to the disk at once, smaller pieces
  are gathered first); the object map maps virtual objects to their new
  blocks; the space manager allocates and frees.
- Commit: write every staged block, durability barrier, write the new
  checkpoint (superblock last), barrier. An interrupted commit leaves the
  previous checkpoint as the one a reader mounts.
- `atomically`: an operation either lands whole or is rolled back inside the
  open transaction, data written straight to the disk included.
- Operations: create, write (whole file or a byte range), truncate, rename,
  unlink, mkdir/rmdir, hard links, symlinks, xattrs, timestamps and flags,
  clones, unsharing a stream a clone shares, snapshots (create, list, delete,
  revert).
- `linux-apfs-rw` and `apfsprogs` are the correctness references (clean room:
  algorithms checked, no code copied); every change is checked with macOS
  `fsck_apfs` and against macOS editing the result.

### `winfsp-host` (`apfs-mount.exe`)
- WinFsp callbacks for the full read/write surface, each wrapped in
  `catch_unwind`: a fault fails that one call and leaves the mount up.
- Write grouping (`group.rs`): operations run in one open transaction, which
  commits after 100 ms without an operation, 1 s after it opened, at 4096
  operations or 64 MiB staged, on a flush request and on unmount.
- Devices: `PhysicalDrive` (raw `\\.\PhysicalDriveN`, refuses the disk
  Windows runs from), image files, an offset window onto the APFS partition,
  and a block cache (metadata LRU of 64 KiB blocks, plus a 1 MiB read-ahead
  window for file content read in order).
- File, directory, security and volume information expire after 1 s.
  Windows does not cache file data: WinFsp would only with file information
  that never expires, and with it a CopyFile of 4 GiB or more hung in the
  kernel. Single-name directory queries are answered by one lookup
  (`GetDirInfoByName`).
- A volume that has snapshots is mounted read-only even when writing was
  requested.
- Subcommands on images: `volume list`, `snapshot list|create|delete|revert`,
  `clone`.

### `apfs-explorer-gui`
- Lists physical drives (WMI) while its window is shown, marks the APFS ones,
  and mounts through the launcher classes `apfs-mount` (read-only) and
  `apfs-mount-rw` (`--writable`), registered by the installer. The launcher
  runs the mount in session 0, so the drive is visible to the user's Explorer
  without a UAC prompt per mount.

## Data flow (mount)

Mount in the app → launcher starts `apfs-mount --drive N --mount X:` → GPT →
APFS partition → container → volume → `FsView` → WinFsp mount at `X:` →
Explorer calls → callbacks → `FsView` (reads) or the open write group
(writes) → `apfs-core` → raw device. Unmount commits the open group first.

## WinFsp / installer

- `winfsp` crate 0.13 (WinFsp 2.1 API).
- The official, signed WinFsp MSI is chained by the WiX Burn bundle and
  installed silently when absent; no reboot.
