# WinFsp Callback Wiring Status

Tracks which WinFsp 0.13 callbacks are wired in `crates/winfsp-host/src/mount.rs`
and which are blocked by upstream limitations.

Last updated: 2026-05-26 (Wave 10 upstream research: hard links + FSCTL clone)

---

## Upstream version audit (2026-05-26)

| Component | Current (locked) | Latest available | Notes |
|---|---|---|---|
| `winfsp` Rust crate | `0.13.0+winfsp-2.1` | **0.13.0** (latest on crates.io) | 0.13.0 is the latest release (tagged 2026-05-13). No 0.14+ exists. |
| `winfsp-sys` Rust crate | `0.12.1+winfsp-2.1` | **0.12.1** | Matches winfsp 0.13 SDK. |
| WinFsp native C SDK | `2.1` (stable) | **2.2B1** (2026 Beta1, pre-release) | Beta only; no stable 2.2 released. |

**Repo:** `SnowflakePowered/winfsp-rs` (61 stars, 14 forks). Latest release tag: `0.13.0` (May 13 2026).
No winfsp-rs release after 0.13.0 exists. We are already on the latest stable.

---

## Wired callbacks

| Callback | APFS API | Status | Notes |
|---|---|---|---|
| `get_security_by_name` | `FsView::getattr` | Wired | |
| `open` | `FsView::getattr` | Wired | |
| `close` | — | Wired (no-op) | |
| `create` | `create_file` / `mkdir` | Wired | |
| `read` | `FsView::read` | Wired | Per-handle content cache |
| `write` | dirty buffer → `overwrite_existing_file` | Wired | Flush on Cleanup |
| `read_directory` | `FsView::read_dir` | Wired | DirMarker resume |
| `flush` | `FsView::getattr` | Wired | No-op on no context |
| `get_file_info` | `FsView::getattr` | Wired | |
| `get_volume_info` | `FsView::volume_size` | Wired | |
| `set_volume_label` | `set_volume_label` | Wired | |
| `set_basic_info` | `set_inode_attrs` | Wired | Timestamps + mode + bsd_flags |
| `set_file_size` | `truncate_file_fast` / `overwrite_existing_file` | Wired | Fast path + slow fallback |
| `set_delete` | handle flag | Wired | Actual unlink in `cleanup` |
| `cleanup` | `unlink` / `overwrite_existing_file` | Wired | Delete + write flush |
| `rename` | `rename` / `move_entry` | Wired | Same-parent + cross-dir |
| `overwrite` | `overwrite_existing_file` | Wired | CREATE_ALWAYS / supersede |
| `get_reparse_point` | `FsView::read_symlink` | Wired | IO_REPARSE_TAG_SYMLINK |
| `get_reparse_point_by_name` | `FsView::read_symlink` | Wired | Used by WinFsp resolver |
| `set_reparse_point` | `create_symlink` | Wired | Decode + re-create as S_IFLNK |
| `get_extended_attributes` | `FsView::list_xattrs` + `get_xattr` | **Wired (Wave 8/9)** | EA ↔ APFS xattr; XATTR_FILE_SYSTEM_OWNED filtered out |
| `set_extended_attributes` | `set_xattr` / `delete_xattr` | **Wired (Wave 8/9)** | Zero-length value = delete |
| `control` | dispatch skeleton | **Wired (Wave 8/9)** | FSCTL_DUPLICATE_EXTENTS_TO_FILE recognized; see upstream-blocked below |

---

## Upstream-blocked

### Hard links / FILE_LINK_INFORMATION — M9 #2

- **What it is:** Windows `CreateHardLinkW` → IRP `FileInformationClass =
  FileLinkInformation` → `FILE_LINK_INFORMATION { ReplaceIfExists; RootDirectory;
  FileNameLength; FileName[]; }`.
- **Our API:** `apfs_write_file::create_hard_link(txn, vsb, omap, parent_oid,
  existing_name, new_name)` — ready to wire.
- **Blocker (winfsp-rs layer):** `FileSystemContext` trait in winfsp-rs 0.13
  has NO `set_link` method. The trait defines: `get_security_by_name`, `open`,
  `close`, `create`, `read`, `write`, `flush`, `cleanup`, `read_directory`,
  `rename`, `set_basic_info`, `set_file_size`, `set_delete`, `overwrite`,
  `get_reparse_point`, `get_reparse_point_by_name`, `set_reparse_point`,
  `get_extended_attributes`, `set_extended_attributes`, `control`. No hard-link
  callback exists.
- **Blocker (native C layer):** `FSP_FILE_SYSTEM_INTERFACE` in WinFsp 2.1 and
  even the 2.2B1 beta header (verified 2026-05-26) has no `SetLink` callback
  slot. The struct ends at `(*SetEa)` + `(*Obsolete0)` + `(*Reserved[31])`.
  Hard links via `FILE_LINK_INFORMATION` are not part of the WinFsp user-mode
  interface at any released or beta version.
- **Root cause:** WinFsp intercepts `FileLinkInformation` at the FSD layer and
  returns `STATUS_NOT_SUPPORTED` unconditionally for user-mode file systems.
  This is a deliberate WinFsp design constraint (user-mode FSes are expected to
  be single-link; NTFS hard-link semantics require inode reference-count
  plumbing that WinFsp does not expose). See winfsp issue tracker: no open PR
  or issue requests `SetLink` as of 2026-05-26.
- **Resolution path:** No near-term upstream fix expected. Options:
  1. File a feature request against `winfsp/winfsp` requesting a `SetLink`
     callback slot in `FSP_FILE_SYSTEM_INTERFACE` (recommend to user — do not
     open on their behalf).
  2. Return `STATUS_NOT_SUPPORTED` from our side; Windows `CreateHardLinkW`
     will fail with `ERROR_NOT_SUPPORTED`. APFS hard links remain accessible
     read-only (they appear as normal files with nlink > 1 in `get_file_info`).
- **Current behaviour:** `STATUS_INVALID_DEVICE_REQUEST` (default) — hard link
  creation returns an error to the caller. No data loss.
- **Upgrade action:** None. winfsp-rs 0.13 is already the latest. Upgrade to
  0.14+ when/if `set_link` is added.

### FSCTL_DUPLICATE_EXTENTS_TO_FILE (0x000940CF) — W6-GAP-2

- **What it is:** Windows 11+ server-side copy FSCTL emitted by
  `Copy-Item -Reflink`, `robocopy /COPYALL`, and the `cp --reflink`
  WSL bridge.
- **Input:** `DUPLICATE_EXTENTS_DATA { HANDLE FileHandle; LARGE_INTEGER
  SourceFileOffset; TargetFileOffset; ByteCount; }` (32 bytes).
- **Our API:** `apfs_write_file::clone_file(txn, vsb, omap, src_parent,
  src_name, dst_parent, dst_name)` — fully implemented in Wave 8.
- **Blocker:** WinFsp 0.13 passes the raw kernel `HANDLE` value in the
  `input` buffer but does NOT resolve it to a file path before dispatching
  the `control` callback. From a user-mode WinFsp callback there is no
  platform helper to do `NtQueryInformationFile(FileNameInformation)` or
  `GetFinalPathNameByHandle` on an arbitrary kernel handle. The `control`
  callback is wired and recognises the FSCTL code; it returns
  `STATUS_NOT_SUPPORTED` so callers fall back to a regular byte copy
  transparently (no data loss, just no CoW extent sharing).
- **winfsp-rs 0.13 control signature (confirmed 2026-05-26):**
  `fn control(&self, context: &Self::FileContext, control_code: u32, input: &[u8], output: &mut [u8]) -> Result<u32>`
  No handle resolver parameter. No `FileSystemContext::resolve_handle` helper.
  No change since 0.12.
- **WinFsp 2.2B1 native status (confirmed 2026-05-26):** `FSP_FILE_SYSTEM_INTERFACE`
  `Control` slot unchanged; no `ResolveHandle` API added. 2.2B1 changes are
  CVE fix + FUSE improvements only — no filesystem API additions.
- **Resolution path:** Wait for WinFsp upstream to expose a
  handle-to-path helper in the `FileSystemContext` trait, OR contribute
  a patch to winfsp-rs. Recommended: file a feature request against
  `winfsp/winfsp` requesting `FspFileSystemResolveHandle(FileSystem, Handle)`
  (user should open — do not open on their behalf).
- **Upgrade action:** None. winfsp-rs 0.13 is the latest. No upgrade unblocks this.
- **Current behaviour:** `STATUS_NOT_SUPPORTED` → caller uses byte copy.

---

## Not applicable / non-goal

| Callback | Reason |
|---|---|
| `get_security` / `set_security` | APFS has no Windows ACLs; WinFsp default (world-readable) sufficient |
| `get_stream_info` | NTFS alternate data streams — no APFS equivalent; non-goal |
| `delete_reparse_point` | Symlink delete goes through `cleanup` → `unlink`; separate callback not needed |
| `get_dir_info_by_name` | WinFsp default (scan + filter) acceptable for APFS directory sizes |

---

## Feature status summary (2026-05-26 research)

| Feature | Milestone | winfsp-rs support | WinFsp native | Action |
|---|---|---|---|---|
| Hard links `FILE_LINK_INFORMATION` | M9 #2 | NOT in 0.13 (`set_link` absent) | NOT in v2.1 or v2.2B1 (`SetLink` absent) | Defer; user may file `winfsp/winfsp` feature request |
| FSCTL clone path resolver | W6-GAP-2 | NOT in 0.13 (`control` has no handle resolver) | NOT in v2.1 or v2.2B1 (no `ResolveHandle` API) | Defer; user may file `winfsp/winfsp` feature request |

Both features remain **upstream-blocked**. winfsp-rs 0.13.0 is the current
latest. No upgrade is available that unblocks either feature.

## Next steps

1. **Hard links — user action recommended:** File a feature request against
   `winfsp/winfsp` requesting a `SetLink` callback slot in
   `FSP_FILE_SYSTEM_INTERFACE`. Reference: M9 #2 (apfs-explorer).
2. **FSCTL clone — user action recommended:** File a feature request against
   `winfsp/winfsp` requesting `FspFileSystemResolveHandle(FileSystem, Handle)`
   so `FSCTL_DUPLICATE_EXTENTS_TO_FILE` can resolve the source `HANDLE` to a
   path in the `control` callback. Reference: W6-GAP-2 (apfs-explorer).
3. **Watch for winfsp-rs 0.14+:** When a new winfsp-rs release appears, check
   `FileSystemContext` trait for `set_link` and `control` signature changes.
   Current poll baseline: 0.13.0 (2026-05-13).
4. **Partial-range clone (M10+):** Once the handle gap is resolved, extend
   `clone_file` to support `SourceFileOffset != 0` or `ByteCount != whole_file`
   (range-clone semantics, needs extent split/trim).
5. **EA round-trip test:** Add a Windows integration test that sets an EA
   via `SetFileEa`, reads it back via `GetFileEa`, and verifies the APFS
   xattr persisted correctly (requires Windows runner in CI).
