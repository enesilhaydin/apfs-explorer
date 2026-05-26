# Wave 6 — Production-path audit findings (2026-05-26)

## Scope

10 APFS reference implementation surfaces audited against our codebase.
Reference: linux-apfs-rw + APFS specification cross-reference analysis.
Follows Waves 1–5 (94 matrix rows, 10 bugs shipped).

## Surface inventory

| # | Surface | Reference symbols | Status |
|---|---|---|---|
| W6-1 | jhash deep audit (insert/remove/lookup/collision/concurrency) | `_apfs_jhash_insertvnode`, `_apfs_jhash_removevnode`, `_apfs_jhash_getvnode_internal`, `_apfs_jhash_try_insert_stream`, `_object_in_jhash` | ⚠️ |
| W6-2 | Defragmentation / apfs_relocate | none (userspace daemon, non-goal) | 🚫 |
| W6-3 | Decoder corrupted input (LZVN/ZLIB/LZFSE) | via `_decmpfs_read_compressed` plugin dispatch | ✅ |
| W6-4 | apfs_vnop_clonefile / COW reflink | `_apfs_vnop_clonefile`, `_apfs_clone_internal` | ✅ |
| W6-5 | apfs_check / live fsck path | none (external fsck_apfs tool) | ✅ |
| W6-6 | revert_to_snapshot / snapshot rollback | `_revert_to_snapshot`, `_handle_revert_mount` | ✅ |
| W6-7 | UUID/role resolver helpers | not found as standalone symbols | ✅ |
| W6-8 | apfs_vnop_lookup / path-walking | `_apfs_vnop_lookup` | ✅ |
| W6-9 | vfsop_unmount edge (dirty/force) | `_apfs_vfsop_unmount` | ⚠️ |
| W6-10 | apfs_vfsop_sync / sync_progress | `_apfs_vfsop_sync` | ⚠️ |

## Findings detail

### W6-1: jhash — deliberate divergence, no bugs

Apple's jhash is a kernel vnode hash table (per-bucket `lck_rw_t` + gate
pattern for create/reclaim serialization). Key comparison uses `{dev_t,
inode_id, strncmp(name)}` — no NFD/case-fold at hash level (normalisation
happens upstream on create/rename).

Our model has no jhash: WinFsp manages vnode lifetime; `FsView` is stateless
and re-opened per transaction. This is a correct, deliberate divergence.
No logic bug, no missing feature.

### W6-2: Defragmentation — confirmed non-goal

No `defrag_*` or `relocate_*` symbol exists in the reference implementation. Defragmentation is
a userspace daemon or hibernation-WBC path (Fusion-drive only). Confirmed
as non-goal; matrix entry updated.

### W6-3: Decoder corrupted input — crash-safe, verified

- `lzvn_decode`: all truncation paths return `Err(ParseError::Short{..})`.
  No `panic!`, no `unsafe`, no `unwrap`. Quota-guarded per iteration.
- `decmpfs::decompress`: `Result`-propagated. `LengthMismatch` guard on
  decoded-vs-declared size. `unwrap_or(&[])` on line ~122 is safe
  (returns empty slice on 0-byte verbatim chunk — correct semantics).
- Test `bad_magic_and_unsupported_error_not_panic` explicitly verifies no
  panic on corrupted inputs.

No bug found.

### W6-4: COW clone / reflink — implemented (W6-GAP-1 DONE)

Apple's clone is metadata-only: `apfs_clone_internal` writes duplicate
`j_file_extent` keys pointing to the same physical blocks. No block copy.
Physical CoW copy happens on first write.

**W6-GAP-1 DONE** (2026-05-26): `clone_file` implemented in
`crates/apfs-write/src/file.rs`. New inode shares the source's physical
blocks via identical `j_file_extent` paddr values. Extref `refcnt`
incremented for each shared non-sparse block. `unlink` is now refcnt-aware:
decrement when refcnt > 1, remove + free when refcnt == 1. Zero data copy —
pure metadata operation. 7 integration tests (C1-C7) in
`tests/clone_file.rs`, gated by `APFS_WRITE_SCRATCH=1`. `FSCTL_DUPLICATE_EXTENTS_TO_FILE`
WinFsp surface deferred to M10.

### W6-5: Live fsck — architecturally equivalent, verified

Apple does not run fsck inside the APFS driver for user-data volumes. Live integrity
check is seal-based (signed system volumes only). External `fsck_apfs` is
the check tool — exactly our approach. ✅

### W6-6: Snapshot revert — ✅ IMPLEMENTED (W6-GAP-2)

`revert_to_snapshot` is a mount-time operation in the reference implementation triggered by
VSB fields `apfs_revert_to_xid` + `apfs_revert_to_sblock_oid`.

**W6-GAP-2 IMPLEMENTED** (`feat(apfs-write): revert_to_snapshot`):

- `revert_to_snapshot(txn, vsb_raw, omap_raw, snap_xid) -> Result<(), TxnError>`
  in `crates/apfs-write/src/snapshot.rs`.
- Validates snap_xid against the snap_meta tree; returns `SnapshotNotFound`
  on miss.
- Mid-chain guard: only the most-recent snapshot (om_most_recent_snap) may
  be reverted; older xids return `InvalidArgument`.
- Restores live extentref tree to the snapshot's pinned extref (COW to
  revert_xid for xid consistency).
- Clears VSB `apfs_snap_meta_tree_oid` and decrements `apfs_num_snapshots`.
- COWs volume omap: zeros `om_snap_count`, `om_most_recent_snap`,
  `om_snapshot_tree_oid`; zeroes the live_xid omap-btree entry for
  root_tree_oid so snap_xid entry becomes the highest visible xid.
- Frees frozen VSB block and snap_meta block.
- **Conservative reclaim**: blocks allocated after snap_xid are not freed
  (over-allocation, not under-allocation; fsck-recoverable; no corruption risk).
  Same conservative policy as multi-snapshot delete.
- `SnapshotNotFound(u64)` added to `TxnError`.
- 4 integration tests (R1-R4) in `tests/integration_snapshot_revert.rs`,
  gated by `APFS_WRITE_SCRATCH=1`.

### W6-7: UUID/role resolver — implemented (W6-GAP-3 ✅)

No standalone `apfs_resolve_filesystem_uuid` / `apfs_resolve_role` symbols
exist. UUID/role selection happens in `apfs_vfsop_mount` via mount-args struct
and `apfs_root_snapshot_select`.

**W6-GAP-3 IMPLEMENTED** (commit bb88801 + b016256 + ea61d7c):
- `NxSuperblock::uuid` — container UUID from `nx_uuid`@0x30 (cross-checked:
  linux-apfs-rw + apfsprogs, two independent sources).
- `VolumeSuperblock::uuid` — volume UUID from `apfs_vol_uuid`@0xF0=240.
- `VolumeRole` enum — typed decode of `apfs_role` u16 bitfield; `from_raw()`
  + `from_name()` for CLI use.
- `VolumeInfo` struct — index, uuid, name, role, features per volume.
- `Container::container_uuid()`, `list_volumes()`, `open_volume_by_uuid()`,
  `open_volume_by_role()`, `volumes_by_role()`, `open_volume_by_index()`.
- `apfs-mount` CLI: `--volume-uuid`, `--volume-role`, `--volume-index` flags
  with mutual-exclusion validation and role-name validation at startup.
- 9 integration tests in `tests/uuid_resolver.rs`; all 80 tests green.

### W6-8: vnop_lookup path-walking — verified

Apple's lookup: VFS cache → `fs_get_inum_for_name` B-tree → case-fold
(`utf8_normalizeOptCaseFoldAndCompare`) → alt-name → firmlinks → retry loop
(up to 1024 times on EAGAIN).

Our `FsView::lookup` covers the core path (B-tree + ASCII case-fold).
Firmlinks: non-goal. Full NFD normalization: deferred (W4 pending table).
VFS name cache: handled by WinFsp layer. No logic bug. ✅

### W6-9: vfsop_unmount dirty/force — deliberate divergence

Apple's force-unmount: logs EBUSY on `vflush` failure but continues teardown
when `MNT_FORCE` is set. Calls `tx_finish` before `container_unload`.

Our model: each `with_writable` commits. `flush()` (WinFsp FlushFileBuffers
callback) calls `commit()`. Drop closes image file. No explicit pre-close
flush hook needed beyond existing `flush()`. ✅ Deliberate divergence.

### W6-10: apfs_vfsop_sync — deliberate divergence

Apple's `vfsop_sync` iterates all vnodes calling `cluster_push` +
`buf_flushdirtyblks`, then flushes the transaction (synchronous or lazy
based on `MNT_WAIT`). No `apfs_sync_progress` symbol found.

Our model: commit-per-write. No lazy dirty buffer accumulation. WinFsp
`flush()` maps to commit. No periodic background sync needed. ✅ Deliberate
divergence.

## New bugs found

**None.** Wave 6 surfaces are either verified correct, confirmed non-goals,
or deliberate divergences with documented gaps.

## New gaps documented

| ID | Description | Risk | Scope |
|---|---|---|---|
| W6-GAP-1 | COW clone / reflink (`apfs_vnop_clonefile`) — ✅ IMPLEMENTED (refcnt-aware unlink, 7 tests) | — | done |
| W6-GAP-2 | Snapshot revert/rollback — ✅ IMPLEMENTED (conservative reclaim) | — | done |
| W6-GAP-3 | UUID/role-based volume selection — ✅ IMPLEMENTED | — | done |

## Wave 7 fix targets

Wave 6 found **0 logic bugs**. No Wave 7 fixes needed from this wave.
Wave 7 should address remaining pending gaps from Waves 1–6:
- W4-GAP-1: large xattr XATTR_DATA_STREAM write path
- W4-GAP-3: ResourceFork write (depends on W4-GAP-1)
- W4-FRAG-1: xfield blob preserve-unknown on update
- W6-GAP-2: snapshot revert — DONE (implemented)
- W6-GAP-3: UUID/role resolver — DONE (implemented in this milestone)

## Running totals after Wave 6

| Status | Count |
|---|---|
| ✅ Verified | 58 (+1 W6-GAP-2) |
| 🐛 Bug found/fixed | 10 (unchanged) |
| ⚠️ Deliberate divergence | 24 (unchanged) |
| ⏳ Pending feature gap | 6 (-1 W6-GAP-2 done) |
| 🚫 Non-goal | 8 (unchanged) |
| **Total rows** | **106** |
