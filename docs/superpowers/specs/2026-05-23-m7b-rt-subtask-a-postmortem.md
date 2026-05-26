# M7b-RT — Subtask A green; B/D/E failure breakdown (2026-05-23)

## Subtask A — DONE (commit `999e97c`)

- `decisive_create_delete_snapshot_on_scratch` GREEN
- `decisive_create_delete_snapshot_on_real_usb` GREEN (32 GB USB, 4 stages)
- 46 lib + 21 scratch + clippy 0; no regression
- Architectural change: single-omap (frozen VSB's `apfs_omap_oid` points at
  the same COW'd volume omap as live)

## Post-Subtask-A `repro_overalloc_snapshot_pinned` failure (un-ignore probe)

Sequence: write A (64 KiB) → create_snapshot → unlink A → write B → fsck.

Raw `fsck_apfs -n -d` output captures 4 distinct issues:

1. **Overlap at paddr 2058**:
   - `oid 0x80a (type=2 BTREE, subtype=11 OMAP)` — snapshot's omap b-tree T1
   - `oid 0x404 (type=3 BTREE_NODE, subtype=14 FSTREE)` — write_B's new catalog
   - → snapshot-pinned block was reclaimed somewhere, then re-allocated.

2. **`apfs_snap_meta_ext_oid invalid`** (VSB offset, untouched by writer).

3. **Extent (2066+16) refcnt = -1**:
   - A's data extent dropped from fsroot, but extentref still references it.
   - Canonical fix: per-extent `j_phys_ext_val_t.refcnt` (Subtask E).

4. **fs_alloc_count drift (expected 41, actual 39)** + 3 overallocation blocks
   (0x80e, 0x811, 0x827) — intermediate omap/extref versions not pruned.

## Diagnosis trail so far

- `free_replaced_metadata` (file.rs:1213) snapshot-aware logic looks correct:
  `if rd_u64(&b, 16) > newest_snap → free else keep`. With `newest_snap=4`
  and T1 at xid=4, the check returns FALSE → preserve. Yet 2058 ends up
  overlapped → the actual free path is elsewhere.
- `apply_omap_entries` (txn.rs:759) operates on the CONTAINER omap; it frees
  old container omap header, old container omap b-tree, and the previous VSB
  paddr. It does NOT touch the volume omap or its b-tree. So that's not the
  culprit either.
- Hypothesis (untested): write_B's `rewrite_fstree` carry-forward logic may
  drop a snapshot-relevant `{oid, xid}` entry, releasing the catalog node
  paddr. Or: T1 is at 2058 and write_B's `free_replaced_metadata` sees T1's
  paddr as a candidate (because T2 still references T1's old node area?).
  Need a real-disk dump after each step to disambiguate.

## Next investigation steps (TDD-disciplined)

1. **Real-disk decode** — dump post-test `/tmp/apfs-scratch-large.img`:
   - Find T1's paddr (frozen VSB → V1 → T1).
   - Find what live omap b-tree T3 maps; check for stale `{root, xid=5}`.
   - Confirm whether T1 = 2058.
2. **Carry-forward audit** — read `rewrite_fstree` retention logic; verify
   that `{oid, xid_pre_snap}` entries are preserved across multiple writes
   after the snapshot.
3. **NotebookLM check** (md.6) — if real-disk evidence contradicts our
   reading of the locked spec, draft a question on the exact retention
   semantics (e.g. is the intermediate `{root, xid=5}` retained or pruned?).

## Subtask B/D/E phasing (locked plan)

- **B (omap retention)**: keep, per oid, the latest version with
  `xid ≤ each active snap.xid` + the live version; drop intermediates.
  Source of truth for active snap xids: om_snapshot_tree.
- **D (sm_fq + reaper)**: snapshot-pinned blocks go to spaceman free queue
  keyed `{xid, paddr}`; reaper drains entries with `xid ≤ deleted_snap_xid`
  on `delete_snapshot`.
- **E (extentref refcnt)**: canonical ownership = `j_phys_ext_val_t.refcnt`;
  increment on snapshot-cover, decrement on unlink; sm_fq only on refcnt→0
  AND no snapshot pins.

Each layer → separate TDD red→green commit. The next session opens with
step 1 above (real-disk decode of the failing image).

## Resume command

```
git log --oneline -5      # see commit chain through 999e97c (Subtask A)
git status                # CLEAN
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write \
  --test snapshot_scratch decisive_create_delete_snapshot_on_scratch \
  -- --nocapture --test-threads=1
# repro probe (will fail with 4 errors above):
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write \
  --test snapshot_scratch repro_overalloc_snapshot_pinned \
  -- --nocapture --test-threads=1 --include-ignored
```
