# PRE-COMPACT RESUME — M7b-RT Subtasks A/B/D1-D3/E almost done (2026-05-24)

## State at compact point

- Branch: `feat/m7a-write-snapshot`
- Working tree: CLEAN
- Tag: **`checkpoint/m7a-rt-realusb-clean-2026-05-22`** (safe return point)
- Latest commit: `cc972ea`
- Tests: **54 lib + 21 scratch decisive + clippy 0 + USB GREEN**; regression 0
- Only ignored: `repro_overalloc_snapshot_pinned` (1 fsck warning left), `decisive_faz_a_on_real_usb` (USB harness race)

## What's done (20 commits this round)

| # | Commit | Subtask |
|---|---|---|
| 1 | `5dccfce` | plan |
| 2 | `999e97c` | **A** single-omap arch |
| 3 | `a2d4caa` | A postmortem |
| 4 | `c8580a2` | **B** omap retention prune |
| 5 | `1bd56be` | RESUME |
| 6 | `14b6e54` | D layout spec |
| 7 | `ddf94a4` | **D1** sm_fq builder/decoder (8 lib tests) |
| 8 | `b7fb815` | RESUME |
| 9 | `d0164be` | **D2** Transaction.enqueue_sm_fq + drain |
| 10 | `84c9245` | **D3** free_replaced_metadata reroute |
| 11 | `6d8b879` | D2 sfq_count idempotent fix |
| 12 | `96ebbf8` | E NotebookLM question |
| 13 | `ec5db89` | E locked answers + revert partial |
| 14 | `5801305` | E partial (extref COW only) |
| 15 | `6f8770c` | RESUME |
| 16 | `985d4e6` | pub(crate) parse_omap_entries + build_omap_node |
| 17 | `8ab4b0d` | E empirical lock: fsck xid binding constraint |
| 18 | `3b9f1f4` | **E full** (fsroot COW + omap {root,snap_xid} entry) |
| 19 | `cc972ea` | E full status + 'live_xid > snap_xid' hypothesis |

## Outstanding problem (post-compact target)

`repro_overalloc_snapshot_pinned` (write_A → snap → unlink → write_B → fsck)
fails at step [3] (post-create-snapshot, xid=4) with TWO related fsck
warnings:

```
warning: mismatch between extentref entry reference count (1) and
         calculated fsroot entry reference count (0) for extent (2066 + 16)
warning: orphan/invalid physical extent (2066)
```

**Verified structurally correct (real-disk decode):**
- Live VSB at xid=4, omap=0x828, extref=0x824(COW @snap_xid), snap_meta=0x80f.
- Volume omap b-tree T@0x811 has 2 entries:
  - `{root=0x404, xid=3} -> paddr=0x822` (write_A's fsroot)
  - `{root=0x404, xid=4} -> paddr=0x827` (COW'd snap fsroot)
- Both fsroots @ 0x822 and 0x827 contain A.txt's
  `INODE obj_id=16` + `DSTREAM_ID` + `FILE_EXTENT obj_id=16 phys=2066 len=65536`.
- extref @ 0x824 (snapshot's, xid=3): refcnt=1 for paddr=2066 owner=16.
- extref @ COW'd live (xid=4): same content, refcnt=1.
- frozen VSB @ 0x80c: apfs_omap_oid=0x828 (live, single-omap arch),
  apfs_root_tree_oid=0x404, apfs_extentref_tree_oid=0x824 (snap-pinned).

All chains resolve, all xid bindings match (omap.xid == paddr.o_xid).
Yet fsck calculates 0 fsroot references for A's extent.

## Leading hypothesis: live_xid > snap_xid

fsck may interpret "live entries" as omap entries with
`xid > om_most_recent_snap`. Currently both live and snapshot resolve
via `max{xid <= snap_xid}` because we commit at `txn.xid = snap_xid`.
There's no live omap entry > snap_xid → fsck "sees" no live fsroot
to count.

### Implementation sketch for next session

In `create_snapshot`:
1. Do all snap-side staging at `txn.xid = snap_xid` (frozen VSB,
   snap_meta, COW'd fsroot for snap at o_xid=snap_xid, extref COW,
   omap b-tree {root, snap_xid} entry).
2. **Bump `txn.xid` to `snap_xid + 1`** (a new helper or direct
   mutation; field is `pub`).
3. Do live-side staging at the new xid:
   - COW the fsroot AGAIN (a second copy) with o_xid=snap_xid+1.
   - APPEND omap entry `{root, snap_xid+1} -> new_live_fsroot_paddr`.
   - new live vomap header at xid=snap_xid+1.
   - new live vol-tree at xid=snap_xid+1.
   - `stage_virtual(vsb_oid, ...)` → live VSB at xid=snap_xid+1
     (uses `self.xid` internally).
4. `commit()` → nx_superblock at xid=snap_xid+1 (the highest).
5. fs_alloc_count delta: +1 (the additional live fsroot block).

### Risk

- decisive_create_delete_snapshot_on_scratch passes today; the two-xid
  restructure may break it (or other tests). Apply carefully + iterate.
- The nx_superblock will be at xid=snap_xid+1 — verify checkpoint
  layout integrity.

### Alternative

A real macOS `tmutil snapshot /` + Python decode of the resulting
volume would empirically confirm whether the kernel actually does
this two-xid pattern, OR if there's a simpler structural difference
we're missing.

## Locked KB additions this session

- `apfs-spec:extref-snapshot-semantics`: shared extref tree (COW at
  snapshot creation, NOT a fresh empty tree).
- `apfs-spec:cow-block-reclaim` Q4/Q6 (sm_fq + reaper for deferred
  reclaim).
- `apfs-spec:omap-snapshot-versioning` (omap b-tree {oid,xid} keys,
  om_snapshot_tree).
- Empirical: fsck xid binding constraint (omap.xid == paddr.o_xid).
- Empirical: virtual object o_oid stays = oid, not paddr (for fsroot
  COW).
- Empirical: sm_fq[SFQ_MAIN] at spaceman_phys block offset 0xf0.

## Real-disk decode workflow (proven)

```python
# /tmp/apfs-scratch-large.img — left behind by repro test (2GB)
# /tmp/apfs-scratch-m7a.img — left behind by decisive tests (64MB)
# APFS_BASE = 0x5000, BSZ = 4096
# Walk: nx → container omap → vsb → vol omap → vol omap b-tree → fsroot
#       → also: snap_meta tree → SNAP_METADATA → sblock_oid → frozen VSB
```

This caught Subtask B + the o_xid binding constraint in ONE iteration
each. Use for every fsck mystery.

## Resume commands

```
git status                          # CLEAN
git log --oneline -22               # see all 20 commits
cargo test -p apfs-write --lib      # 54 unit tests, all pass
cargo clippy -p apfs-write -- -D warnings   # 0
# Full decisive suite:
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_scratch -- --test-threads=1
# Repro probe (only 1 fsck warning at xid=4 left):
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_scratch \
  repro_overalloc_snapshot_pinned -- --nocapture --test-threads=1 --include-ignored
```

## Open subtasks after fsroot=0 resolution

1. **D4** `delete_snapshot` drain (sm_fq integration; APIs ready in D2).
2. **C** OMAP_VAL_DELETED tombstones (delete-while-snapshotted).
3. **F** un-ignore tests + real USB final.
4. Squash-merge to main (IRREVERSIBLE — needs explicit user OK).
