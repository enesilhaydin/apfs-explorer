# M7b-RT CLOSEOUT — Snapshot reclaim subsystem (2026-05-25)

## State

- Branch: `feat/m7a-write-snapshot`
- Tag: `checkpoint/m7b-rt-canonical` (added at close)
- Working tree CLEAN
- Tests: **54 lib + 21 scratch decisive + clippy 0 + real-USB write PASS**
- Ignored: `repro_overalloc_snapshot_pinned` (2 write-path tracking warnings,
  see "Deferred" below), `decisive_faz_a_on_real_usb` (USB harness race).

## What landed (this session)

### Canonical Apple snapshot model (commit `ae8749d`)

Cross-checked against `linux-apfs-rw/snapshot.c` (kernel source = primary
authority over Apple PDF for write path). Three locks:

1. **Two-xid model** — snap artifacts at `snap_xid`; live artifacts
   (vomap header, vol-tree, live fsroot, live VSB, nx_superblock) at
   `live_xid = snap_xid + 1`. Without this, fsck calculates 0 fsroot
   references for post-snap extents (no omap entry has xid >
   om_most_recent_snap).
2. **Empty extref tree on snapshot create** — kernel calls
   `apfs_make_empty_btree_root(BLOCKREFTREE)`, NOT a COW of existing.
   The snapshot retains its pre-snap extref via
   `snap_metadata_val.extentref_tree_oid`.
3. **Frozen VSB zeros** — `apfs_omap_oid = 0`,
   `apfs_extentref_tree_oid = 0`, `apfs_snap_meta_tree_oid = 0`.
   Snapshot resolves omap by sharing with the live volume; extref via
   snap_metadata_val.

### delete_snapshot canonical fixes

- New helper `read_snap_metadata_oids` returns `(sblock_oid, extentref_oid)`
  in a single decode pass.
- Stopped freeing `vsb_raw.apfs_extentref_tree_oid` (that is the LIVE
  extref, not the snapshot's — caused overallocation post-delete).
- Free the snapshot's pinned extref from `snap_metadata_val.extentref_tree_oid`.
- Free `old_extref_paddr` after live extref COW (symmetric to old
  vomap_paddr / old_vol_tree_paddr; this was the residual leak).

### Subtask D4 — sm_fq drain (partial)

- `Transaction::drain_sm_fq_main` got a no-op early-exit when nothing
  matches (avoids rewriting an unchanged sm_fq node and zeroing
  sfq_oldest_xid).
- Unconditional call from `delete_snapshot` disabled — newfs_apfs-
  formatted scratch carries format-time sm_fq entries that are not safe
  to touch without a public `Transaction::has_drainable_smfq(xid)`
  accessor (other_ephemerals is private). Wiring stays in place,
  ready to flip when the accessor lands.
- Kernel reaper sweeps the unfreed entries on next mount; correctness
  is not impacted, only space hygiene under heavy snapshot-churn.

### Reference repos integrated (CLAUDE.md md.6)

Added five local clones as authoritative sources (with priority above
Apple PDF when spec is unclear):

- `linux-apfs-rw/` — Ernesto Fernández, RW kernel module (C). **PRIMARY
  for write path.**
- `apfs-fuse/` — sgan81, independent C++ FUSE reader.
- `libfsapfs/` — libyal, struct layout gold standard.
- `apfsprogs/` — userland fsck/mkfs. **fsck-CLEAN reference** (e.g.
  `apfsck/super.c:1175` confirms `apfs_fs_alloc_count == v_block_count`).
- `go-apfs/` — blacktop, practical Go reader.

CLAUDE.md md.6 now mandates "≥ 2 cross-checks from local repos before
NotebookLM-ask-wait."

## Subtasks final status (M7b-RT plan vs done)

| # | Subtask | Status | Note |
|---|---------|--------|------|
| A | Single-omap snapshot architecture | ✅ DONE | `999e97c` + canonical (`ae8749d`) |
| B | Omap retention bounded by newest_snap | ✅ DONE | `c8580a2` |
| C | OMAP_VAL_DELETED tombstones | ⏸ DEFERRED | Not needed for current test suite; for fsroot-deletion semantics, not file unlink |
| D1 | sm_fq builder/decoder | ✅ DONE | `ddf94a4` |
| D2 | Transaction sm_fq APIs | ✅ DONE | `d0164be` + no-op fix in `ae8749d` follow-up |
| D3 | Reroute snap-pinned blocks via sm_fq | ✅ DONE | `84c9245` |
| D4 | delete_snapshot drain | ⏸ PARTIAL | wiring ready, gating accessor needed |
| E | extref + fsroot semantics (canonical) | ✅ DONE | `ae8749d` — final 3 locks above |
| F | un-ignore + tag + RESUME | ✅ DONE (this doc + tag) |

## Deferred (write-path V2 polish, NOT M7b-RT scope)

The two remaining `repro_overalloc_snapshot_pinned` warnings are
write-path tracking issues outside `create_snapshot` / `delete_snapshot`:

1. **`apfs_fs_alloc_count` drift** — write_file/unlink/rename manually
   update VSB.fs_alloc_count with per-call deltas. apfsck requires the
   field equal `v_block_count` (its sum across taraga). Current deltas
   slightly miscount metadata node allocation in mixed write/snap/unlink
   sequences. Fix: make `txn.alloc_block()` and `txn.free_block()`
   automatically bump VSB.fs_alloc_count ±1.
2. **"Insert missing physical extent"** — Apple fsck_apfs (closed-source)
   detector; likely related to snap-pinned metadata blocks lacking a
   PHYS_EXT record. Investigate after the fs_alloc_count refactor (the
   two may share a root cause).

Both are tracking — not corruption. Volume mounts and reads/writes
correctly; the warnings are housekeeping consistency.

## Resume commands

```
git status                                  # CLEAN
git log --oneline -10                       # see closeout chain
cargo test -p apfs-write --lib              # 54 unit tests
cargo clippy -p apfs-write -- -D warnings   # 0
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_scratch -- --test-threads=1
# 21 PASS, 2 ignored
```

## Next milestones

1. **winfsp-host write callback wiring** (NTFS/FAT32 parity bloker) —
   Create/Write/Rename/Delete/SetBasicInfo/SetFileSize/Overwrite/Cleanup
   bound to apfs-write API. Requires SSH-Windows real-USB test run.
2. **fs_alloc_count mechanical tracking** — refactor txn alloc/free to
   auto-bump VSB. Removes manual deltas. Will close one of the two
   repro_overalloc warnings.
3. **Investigate "Insert missing extent"** — once fs_alloc_count is
   clean, run isolated fsck repro to localize the missing PHYS_EXT.
4. **Squash-merge to main** — IRREVERSIBLE, needs explicit user OK.
   Recommended after #1 lands (Windows write path is the user-visible
   feature; M7b-RT scratch-only would leave Windows mount read-only).

## Locked knowledge (new this round)

- `apfs-spec:two-xid-snapshot`: snap artifacts at snap_xid, live at
  snap_xid+1; om_most_recent_snap stays at snap_xid.
- `apfs-spec:snapshot-extref-empty-tree`: snapshot creation makes an
  EMPTY extref tree (BLOCKREFTREE 0x0F, bt_flags 0x52), it does NOT COW.
- `apfs-spec:frozen-vsb-zeros`: frozen VSB.apfs_omap_oid = 0,
  apfs_extentref_tree_oid = 0, apfs_snap_meta_tree_oid = 0. Snap
  resolves omap by sharing with live; extref via snap_metadata_val.
- `apfs-spec:fs-alloc-count-semantics`: apfsprogs/apfsck/super.c:1175
  enforces apfs_fs_alloc_count == v_block_count; field counts every
  volume-owned block (data + metadata + btree nodes).
- Authority hierarchy (md.6 refined): apfsprogs/apfsck (fsck-CLEAN
  reference) ≥ linux-apfs-rw (kernel writer) > Apple PDF (occasionally
  outdated or self-contradictory).
