# RESUME HERE — M7b-RT Subtask A+B done; D/C/E pending (2026-05-23)

## Where we are

Branch `feat/m7a-write-snapshot`, working tree CLEAN, tag
`checkpoint/m7a-rt-realusb-clean-2026-05-22` protects the pre-M7b state.

### Subtasks completed this session

| # | Commit | Effect |
|---|---|---|
| **A** single-omap snapshot architecture | `999e97c` | Frozen VSB shares live omap_phys; old vomap/vol-tree/snap-meta freed at commit. `decisive_create_delete_snapshot_on_scratch` un-ignored, GREEN. Bonus: `decisive_create_delete_snapshot_on_real_usb` GREEN. |
| **B** omap retention bounded by `newest_snap` | `c8580a2` | `rewrite_fstree` carry-forward now drops post-snapshot intermediates. Matches `free_replaced_metadata`'s `o_xid > newest_snap` boundary. Eliminates the dual-boundary bug where carry-forward retained an entry whose backing block was freed → fsck overlap. |
| **D1** sm_fq node builder + decoder | `ddf94a4` | New `apfs-write::sm_fq` module: `build_empty_sm_fq_node`, `parse_sm_fq_entries`, `insert_sm_fq_entry`, `drain_sm_fq_upto`, `smfq_max_keys`. 8 unit tests green. Layout locked to kernel-empirical decode (`2026-05-23-m7b-rt-subtask-d-smfq-layout.md`). |
| **D2** Transaction sm_fq APIs | `d0164be` | `enqueue_sm_fq(snap_xid, paddr)` + `drain_sm_fq_main(upto_xid)` on Transaction. |
| **D3** reroute pinned blocks | `84c9245` | `free_replaced_metadata` routes snapshot-pinned blocks to sm_fq instead of silent preserve. Container OK + sfq tree CLEAN. |
| **D2 fix** | `6d8b879` | sfq_count idempotent (don't bump on duplicate insert). |
| **E doc lock** | `ec5db89` | NotebookLM answers locked + first partial impl reverted. |
| **E partial** | `5801305` | COW extentref tree at create_snapshot (snapshot retains old paddr, live gets new). decisive_create_delete still GREEN. fs_alloc_count delta corrected. |

Plan + postmortem docs (also committed):
- `2026-05-23-m7b-rt-snapshot-reclaim-plan.md` (`5dccfce`)
- `2026-05-23-m7b-rt-subtask-a-postmortem.md` (`a2d4caa`)

### Tests

- **54 lib + 21 scratch decisive + clippy 0** PASS. No regression. (lib count
  bumped from 46 → 54 by the 8 sm_fq unit tests.)
- Real USB: `decisive_create_delete_snapshot_on_real_usb` PASS,
  `decisive_file_write_on_real_usb` PASS.
- Still `#[ignore]`: `repro_overalloc_snapshot_pinned` (write-after-snap),
  `decisive_faz_a_on_real_usb` (USB harness race).

## What's left — pick one

### 1. Subtask D — Spaceman free queue (sm_fq) + reaper [D1 DONE; D2-D4 remaining]

**D1 (sm_fq builder + decoder, commit `ddf94a4`)** is in place. Next:

**Why it's next**: the post-B `repro_overalloc_snapshot_pinned` is no longer
structurally corrupt (no overlap, no invalid snap_meta_ext_oid, no extent
refcnt -1, container "appears to be OK"). The only remaining fsck signals
are 3 overallocations on legitimately-pinned blocks (V_snap @ 0x827,
T_snap @ 0x811, post-create live extref @ 0x80e) + fs_alloc_count drift.
These flags exist because fsck's container-level allocation verifier does
not traverse the snapshot's frozen-VSB chain; Apple's design tracks
snapshot-pinned blocks at the SPACEMAN level via sm_fq + reaper.

**Spec**: locked in KB `apfs-spec:cow-block-reclaim` Q4/Q6.
- sm_fq is a physical b-tree (subtype 0x0e SPACEMAN_FREE_QUEUE), keyed
  `{xid, paddr}` asc.
- When `free_replaced_metadata` encounters a block with `o_xid <= newest_snap`,
  enqueue `{snap_xid, paddr}` to sm_fq INSTEAD of leaving it silently
  allocated.
- Reaper (`OBJECT_TYPE_NX_REAPER`, container-level) drains sm_fq entries
  with `xid <= deleted_snap_xid` on `delete_snapshot`.
- fs_alloc_count update: pinned blocks remain in the volume's owned
  count UNTIL drained.

**Concrete remaining steps** (TDD red→green per layer):
1. ✅ **D1** sm_fq builder + decoder + drain helper (commit `ddf94a4`).
2. **D2** Transaction integration: locate sm_fq[SFQ_MAIN] via the ephemeral
   table (oid=0x403 in baseline scratch), wire COW into the commit path so
   the staged sm_fq node is written each txn (the existing ephemeral loop
   handles reaper + reaper-list; sm_fq slots in the same way). Update
   `spaceman_phys.sm_fq[SFQ_MAIN].sfq_count` / `sfq_oldest_xid` per spec.
3. **D3** Reroute `free_replaced_metadata`: when `o_xid <= newest_snap`,
   call `txn.enqueue_sm_fq(newest_snap, paddr)` (bitmap stays SET — block
   remains "allocated" from the spaceman's view, just tracked for deferred
   reclaim).
4. **D4** `delete_snapshot` drain: walk sm_fq for entries with
   `xid <= deleted_snap_xid` (via `drain_sm_fq_upto`), `txn.free_block`
   each drained paddr, write the new sm_fq node, update sfq_count.
5. Un-ignore `repro_overalloc_snapshot_pinned`; iterate until green.

### 2. Subtask C — `OMAP_VAL_DELETED` tombstones [smaller, prerequisite for some paths]

Needed for delete-while-snapshotted (e.g., create file, snap, unlink,
mount snap → see file via tombstone). Not strictly required for
`repro_overalloc_snapshot_pinned` to be green (that test does
unlink-then-write_B; tombstone only affects snapshot mount visibility).

### 3. Subtask E — extentref refcount (`j_phys_ext_val_t.refcnt`) [middle]

Canonical ownership for multi-snapshot / clone scenarios. The current
live-extentref-presence check works for single-snapshot single-clone but
breaks once 2+ snapshots cover the same extent.

### 4. Squash-merge to main [user decision, IRREVERSIBLE]

Only Subtask A+B would merge; D/C/E continue on the branch. NOT
recommended yet — write-after-snapshot is the most user-visible path
and still has overallocation flags (legitimate, but the test stays
ignored so a real user could hit volume warnings post-snapshot).

## Key paths

- `crates/apfs-write/src/snapshot.rs` — `create_snapshot` (Subtask A patch),
  `delete_snapshot` (Subtask A patch).
- `crates/apfs-write/src/file.rs:1465-1485` — `rewrite_fstree` carry-forward
  (Subtask B patch).
- `crates/apfs-write/src/file.rs:1213` — `free_replaced_metadata`
  (snapshot-aware boundary, untouched this round but the source-of-truth).
- `crates/apfs-write/src/txn.rs:759` — `apply_omap_entries` (container
  omap COW, untouched).
- `crates/apfs-write/tests/snapshot_scratch.rs` — 21 decisive scratch +
  USB tests.
- `docs/superpowers/specs/2026-05-22-m7-rt-snapshot-omap-architecture-finding.md`
- `docs/superpowers/specs/2026-05-22-m7-rt-cow-block-reclaim-questions.md`
  (locked Q1-Q6, source of sm_fq spec).
- `docs/superpowers/specs/2026-05-22-m7-rt-omap-snapshot-versioning-questions.md`
  (locked Q1-Q6).

## Resume commands

```
git status                         # CLEAN
git log --oneline -8               # see Subtask A+B in chain
cargo test -p apfs-write --lib     # 46 unit tests
cargo clippy -p apfs-write -- -D warnings  # 0
# Full decisive suite (~2 min):
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_scratch -- --test-threads=1

# To probe Subtask D progress (currently fails with 3 overalloc + alloc_count drift):
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_scratch \
  repro_overalloc_snapshot_pinned -- --nocapture --test-threads=1 --include-ignored
```

## Real-disk decode workflow (proven this session)

Whenever fsck flags a block, decode the post-test image to know the EXACT
structure:

```python
# /tmp/apfs-scratch-large.img — left behind by repro test
# APFS_BASE = 0x5000 (GPT offset), BSZ = 4096
# 1. Find latest nx via xp_desc ring
# 2. From nx, read container omap (paddr at nx+160)
# 3. Container omap b-tree at omap.om_tree_oid → entries map fs_oid -> VSB paddr
# 4. From VSB: apfs_omap_oid (V), apfs_snap_meta_tree_oid (SM)
# 5. From V: om_tree_oid (T), om_snapshot_tree_oid, om_most_recent_snap
# 6. From SM: SNAP_METADATA records → sblock_oid (frozen VSB)
# 7. From frozen VSB: apfs_omap_oid (V_snap), apfs_extentref_tree_oid (EXT_snap)
# 8. From V_snap: om_tree_oid (T_snap)
# 9. Block at suspect paddr: read header (o_oid, o_xid, o_type low 16, o_subtype)
```

This caught Subtask B's root cause in ONE iteration (T3 retained
{root, xid=5} -> 2058 while 2058 was freed → realloc → overlap).
