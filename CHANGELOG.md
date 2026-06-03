# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.0.0-rc3] - 2026-06-02

### Added

- Multi-node volume omap read + write + free (#157): the catalog rewrite path
  now descends and rebuilds a split volume omap b-tree (table_space reserve
  matching macOS), so writes on volumes with thousands of files stay fsck-clean.
- Windows-runnable catalog self-verifier: `apfs_write::file::verify_fsroot` plus
  an env-gated (`APFS_SELF_VERIFY=1`) `self_verify` in winfsp-host that checks
  the fsroot after each committed op and FREEZES the mount on any structural
  violation (never propagates a pre-existing corruption).

### Fixed

- Catalog key comparator parity with Apple `apfs_key_compare` (verified against
  apfs.kext): per-type ordering - DIR_REC by hash then name, FILE_EXTENT /
  SIBLING_LINK numeric, XATTR by name (was a flat byte compare).

### Investigated

- #156 real-USB fsroot corruption - VERDICT: not our write-path. A 3-stage
  real-hardware handoff experiment with an independent verifier confirmed our
  output is structurally clean (all leaves monotonic), a clean macOS remount is
  clean, and a mid-write yank is clean (APFS crash-safety). The observed
  corruption sat in a macOS `.Spotlight-V100` cache dir with an Apple in-place
  inserter signature our packer cannot produce.

## [1.0.0-rc2] - 2026-05-26

### Added

- Windows installer: WiX Burn bundle chains the WinFsp MSI when the prerequisite
  is absent. Silent install parameters (`/qn /norestart`) suppress the WinFsp
  dialog. Product MSI gains Add/Remove Programs icon and Start Menu shortcut.
- Tray UI: minimal menu with per-volume actions (Open in Explorer, Eject,
  Re-mount Read-Only), auto-mount toggle, and default-read-only toggle.
  Settings persist to `%APPDATA%\APFS Explorer\config.toml`.
- Dirty indicator: tray polls a status file written by each mount process
  every 2 seconds; shows "unflushed writes" when batched ops are pending.
- CLI subcommands documented in `man/apfs-mount.1` and shell completions
  (bash, zsh, fish, PowerShell).
- Project hygiene: NOTICE, CONTRIBUTING, SECURITY, CHANGELOG, dependabot.yml,
  GitHub Actions CI + release workflows, bug/feature issue templates.
- Documentation: library usage guide, tested platforms matrix, APFS format
  compatibility matrix, icon design briefs.

### Fixed

- Cross-compile build error in winfsp-host (windows crate 0.58 API change).
- xtask registry probe now compares `WIN32_ERROR` values without `.map_err`
  on the bare type.

### Deferred

- Windows "Safely Remove Hardware" integration: WinFsp 2.1 SDK / winfsp-sys
  0.12.1 does not expose the `RemovableMedia` bitfield in
  `FSP_FSCTL_VOLUME_PARAMS`. Tracked for re-evaluation when WinFsp 2.2 stable
  ships or winfsp-sys updates its bindings.
- Hard links and `FSCTL_DUPLICATE_EXTENTS_TO_FILE`: blocked upstream
  (winfsp-rs 0.13 trait lacks `set_link`; `control` callback lacks a
  handle-to-path resolver).

## [1.0.0-rc1] - 2026-05-17

### Added

- M11 catalog incremental write path: delta-only B-tree node updates, reducing
  write amplification on large directories.
- M12 omap incremental patch in rename fast path: avoids full omap rewrite on
  single-object renames.
- Windows benchmark suite validated against physical APFS volumes (empirical
  performance floor confirmed).
- WinFsp EA bridge: extended attributes round-trip through Windows EA API.

### Fixed

17 bug fixes across the write path, including:
- debug-log spam on hot paths removed.
- Parent-chain EINVAL on deep directory renames corrected.
- Write-seed silent data loss on power-fail scenario fixed.
- Cache invalidate-on-error now clears stale entries correctly.
- `set_file_size` slow-fallback no longer silently drops the final block.
- `HANDLE` RAII wrapper prevents handle leak on early return.
- `getattr` timestamps corrected (Windows epoch 1601 base removed; real mtime
  reported).
- Case-insensitive lookup now returns consistent results for NFD-normalized names.

### Changed

- Minimum supported Rust version: 1.91.

## [0.9.9-rc1] - 2026-02-28

### Added

- M9 milestone features:
  - Extended attribute stream read/write (xattr data fork).
  - UUID resolver for volume cross-reference.
  - NFD case-fold normalization for case-insensitive volumes.
  - Clone (copy-on-write reflink) support.
  - Snapshot revert (`apfs snapshot revert <name>`).
  - CLI subcommands: `mount`, `unmount`, `snapshot`, `info`, `fsck-lite`.
  - WinFsp EA bridge for Windows extended attribute interoperability.

### Fixed

- Snapshot deletion drain (`sm_fq`) now safe with multiple simultaneous snapshots.
- Symlink round-trip through Windows reparse points preserves target path.

## [0.9.0-pre-windows-bench] - 2025-11-15

### Added

- Complete read path (B-tree traversal, extent resolution,
  directory enumeration, inode stat).
- WinFsp host crate (`winfsp-host`) initial integration.
- Windows installer skeleton (WiX Burn bundle, `xtask package`).
- Cargo workspace restructured: `apfs-core`, `apfs-write`, `winfsp-host`,
  `installer`, `xtask`.

### Changed

- Error handling migrated to `thiserror`; no `unwrap` in library paths.

## [0.5.0] - 2025-07-01

### Added

- M4-M8 milestones:
  - M4: Container/volume superblock parse, checkpoint selection.
  - M5: Object map B-tree traversal.
  - M6: Catalog B-tree: inode, directory record, dentry lookup.
  - M7: Extent-based file data read, compressed file detection.
  - M8: Write transaction skeleton, space manager allocation, free-run bitmap.

