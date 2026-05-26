# Audit Wave-2 Findings — Error Recovery + Edge Cases

Scope: error-recovery paths, omap edge cases, transaction abort, rename corner cases,
xfield remove, drec hash collisions. Derived from specification cross-reference analysis
(linux-apfs-rw + APFS PDF) cross-checked against our codebase.

Symbols cross-checked:
`omap_get`, `create_or_modify_jobj`, `apfs_names_match`, `dir_rec_alloc_with_hash`,
`xf_remove`, `insert_jobj`, `apfs_internal_lookup_ext`, `fs_get_inum_for_name`,
`apfs_vnop_renamex` (partial, callees traced).

---

## Bugs found: 1 real, 1 potential (escalated to Wave 3)

### Bug W2-1 — Bitmap flush before NX superblock in commit() — crash-safety gap (REAL)
**Surface**: `apfs-write/src/txn.rs::commit()`
**Reference behavior**: The reference implementation's transaction commit uses the NX
superblock write as the atomic commit point (last write). All speculative writes before
it are invisible to a reader that mounts the previous checkpoint. Critically, bitmap
mutations that free old blocks must not be visible until after the new NX superblock
is written.
**Our behavior**: `commit()` writes `dirty_bitmaps` (Step 2 — marks old blocks free,
new blocks used) then writes the new NX superblock (Step 5). If a crash occurs between
Steps 2 and 5, the on-disk bitmap no longer matches either checkpoint: old blocks may
appear free (bitmap was updated) while the old checkpoint still references them, and the
new checkpoint's NX superblock was never written.
**Severity**: Medium. Manifests only on power loss mid-commit. WinFsp unmount is
clean-shutdown; the window is power loss during active commit. macOS fsck would likely
flag overallocation or missing blocks.
**Fix direction**: The bitmap writes for OLD block reclamation (freeing replaced VSB,
omap, fstree blocks) should happen AFTER the NX superblock is committed, not before.
New-block allocation (marking new blocks used) can stay before — those are safe to
mark early. Alternatively, batch all bitmap changes into a separate post-NX pass.
**Status**: 🐛 Bug — tracked for Wave 3 fix sweep.

### Potential Bug W2-2 — Alt-name xfield absent on case-only rename (NEEDS VERIFICATION)
**Surface**: `apfs-write/src/file.rs::rename()` on case-insensitive volumes
**Reference behavior**: `apfs_vnop_renamex` calls `apfs_check_and_alloc_alt_name` when
renaming to a differing-case name on a case-insensitive volume. This writes an ALT_NAME
xfield (type 0x0C) to the inode carrying the user-visible display name separately from
the hash key. Some macOS APIs (Finder, getattrlist ATTR_CMN_NAME) may read this field.
**Our behavior**: `rename()` calls `rebuild_inode_with_name()` which updates the inode
name xfield with the new name. We do NOT write a separate ALT_NAME xfield.
**Risk**: fsck does not flag the absence of ALT_NAME. Display name may show stale
capitalization in some macOS contexts if the volume is later read by macOS Finder after
our rename. No data corruption. Cosmetic risk only.
**Status**: ⚠️ Deliberate gap — investigate when case-insensitive support is hardened
(M9 #3 or later).

---

## Feature gaps confirmed (not bugs — existing code untouched)

### Gap W2-1 — Unicode case folding limited to ASCII (lookup path)
`apfs_names_match` uses `utf8_normalizeOptCaseFoldAndCompare` (full Unicode NFD + case).
Our `FsView::lookup` uses `to_ascii_lowercase()`. Misses Unicode case pairs (ä/Ä,
Turkish İ/i, etc). Existing gap from Wave 1, re-confirmed here. Low risk for Windows
workloads (mostly ASCII filenames). Tracked under M9 #3.
**Status**: ⚠️ Known gap (M9 #3).

### Gap W2-2 — DIRSTATS xfields not updated on rename (re-confirmed)
`apfs_vnop_renamex` calls `set_dir_stats_for_rename` which calls
`apfs_update_dir_stats_parent` on both source and destination parents.
Our `move_entry` / `rename` do not update DIRSTATS. Already tracked as M9 #4.
**Status**: ⚠️ Known gap (M9 #4).

### Gap W2-3 — Snapshot-aware omap reads (latent, M9 scope)
`omap_get` performs a snapshot-boundary re-query: if the found (oid, xid) falls
within an active snapshot window [snap_min_xid, snap_max_xid], it re-queries at
(oid, snap_min_xid - 1) to avoid exposing a snapshot-private mapping version.
Our `omap.rs::resolve()` uses find_le semantics (correct) but has no snapshot-window
guard. Not reachable in current code (no concurrent snapshot reads during write),
but a latent gap for snapshot-aware read paths (e.g., reading a file at a specific
snapshot's xid while a newer write is in progress).
**Status**: ⚠️ Deliberate gap — M9 snapshot read scope.

---

## Verified correct (no bug, no gap)

### V1 — omap (oid, xid) collision on write
`apply_omap_entries` finds existing entry by oid, updates xid + paddr atomically,
frees old paddr. One entry per oid at any time (container omap). Old blocks are freed
correctly. Matches reference behavior. **✅**

### V2 — omap tombstone (OMAP_VAL_DELETED) read path
Our `omap.rs::resolve()` checks `OMAP_VAL_DELETED` flag and returns `None` for deleted
entries. Matches reference `omap_get` behavior. Write path does not emit tombstones
(relies on checkpoint advancement for GC) — matches `linux-apfs-rw` behavior, fsck clean.
**✅ ⚠️ Deliberate divergence.**

### V3 — Case-only rename (Foo.txt → foo.txt) on case-insensitive volume
On a case-insensitive volume, `build_drec_key("Foo.txt")` == `build_drec_key("foo.txt")`
(same 22-bit hash + same folded name). In `rename()`: `collect_replace_target` detects
target_id == src_file_id (self-rename) and returns `None` (no replace needed).
In `rewrite_fstree`: `retain` (remove old_drec_key) happens BEFORE the duplicate-check
guard, so the re-insert of the same key passes cleanly. Inode name xfield is updated
with the new display name via `rebuild_inode_with_name`. **✅**

### V4 — DREC hash collision (two files, same 22-bit hash, different names)
`build_drec_key` appends the full name bytes after the hash — distinct names produce
distinct B-tree keys even with the same hash prefix. `list_dir` iterates ALL DRECs for
the directory (full leaf scan), returning all entries. `FsView::lookup` then does
`find` by exact name in the listing. Hash collisions are transparent. **✅**

### V5 — xf_remove present and correct
`xf_remove`: scans by type, two-phase memmove.
Our code uses fresh-blob rebuild per write — no in-place xf_remove needed. The
call sites in `apfs_vnop_renamex` are for purgeable/dataless edge cases (non-goal).
**✅ ⚠️ Deliberate divergence (fresh rebuild).**

### V6 — insert_jobj duplicate guard
Reference: `create_or_modify_jobj` dispatches `bt_update_with_hint` (existing key)
vs `bt_insert` (new key). Our `rewrite_fstree`: remove-before-insert order plus
`AlreadyExists` guard prevents duplicate insertion. **✅**

### V7 — Transaction abort (Drop path)
`Transaction::abort()` drops in-memory state without flushing anything. Pre-commit
in-memory state (pending blocks, dirty bitmaps, omap entries) never reaches disk.
The device remains at the previous checkpoint — safe and correct. The NX superblock
is only written as the last step of `commit()`. **✅**

---

## Non-goals confirmed (not audited)

- doc_tombstone_clear/save (document-ID tombstones) — no Windows analogue.
- size_tracking_ino_moved / purgeable state — macOS-specific purgeable file feature.
- apfs_check_and_alloc_alt_name deep audit — cosmetic, M9 scope.
- proc_is_forcing_hfs_case_sensitivity — process-level macOS override, non-goal.
- apfs_vnop_renamex ResourceFork xfield handling — macOS named streams, non-goal.

---

## Matrix rows added (wave-2)

See `cross-check-matrix.md` for new rows:
- `omap_get` snapshot-boundary re-query → ⚠️ Deliberate gap (M9)
- `apfs_vnop_renamex` case-only rename → ✅ (+ ⚠️ ALT_NAME gap)
- `commit()` bitmap-before-NX ordering → 🐛 Bug W2-1
- `xf_remove` → ✅ ⚠️
- `dir_rec_alloc_with_hash` hash collision → ✅
- `fs_get_inum_for_name` hash-collision fallback → ✅

---

## Wave 3 fix targets (from this audit)

| ID    | Location                          | Bug description                                    |
|-------|-----------------------------------|----------------------------------------------------|
| W2-1  | `apfs-write/src/txn.rs::commit()` | Bitmap flush before NX superblock — crash-safety   |
