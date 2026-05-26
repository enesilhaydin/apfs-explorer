# M7b-RT Subtask E — extentref semantics for snapshot-shared extents (NotebookLM question)

> Locked spec gap revealed by post-Subtask-D3 decode of `repro_overalloc_snapshot_pinned`.
> Empirical findings + question for NotebookLM (md.6). Lock answer to KB
> `apfs-spec:extref-snapshot-semantics`.

## Empirical state (post-D3)

Sequence: `write_A (64 KiB)` → `create_snapshot` → `unlink_A` → `write_B (64 KiB)` → `verify`.

Real-disk decode of `/tmp/apfs-scratch-large.img` shows the extref trees in
a **structurally clean** state:

- **Snapshot EXTREF (EXT_pre @ 0x824, xid=3)**: 1 entry
  - paddr=2066, len=16, kind=1, owner=0x10 (=inode 16 = A.txt), `refcnt=1`
- **Live EXTREF (X3 @ 0x83a, xid=6)**: 4 entries (B's blocks only)
  - paddr=2061 / 2083 / 2085 / 2091 — owner=0x11 (=inode 17 = B.txt), `refcnt=1` each
  - **No entry for paddr=2066** (A is unlinked from live view)
- **Snapshot fsroot @ 0x822, xid=3**: A's INODE + DSTREAM_ID + FILE_EXTENT(2066, len=65536) intact
- **Live fsroot @ 0x838, xid=6**: B's INODE + DSTREAM_ID + 4× FILE_EXTENT for B's runs; no A records

Both views are internally consistent. snap_meta_tree's SNAP_METADATA points
to EXT_pre + frozen VSB. frozen VSB.apfs_extentref_tree_oid = 0x824 = EXT_pre.
All chains resolve.

Yet `fsck_apfs -n` reports:

```
warning: mismatch between extentref entry reference count (0) and calculated
         fsroot entry reference count (-1) for extent (2066 + 16)
warning: apfs_fs_alloc_count is not valid (expected 40, actual 39)
warning: missing/invalid physical extent (2066 + 16) with refcnt -1
```

(All other warnings — sfq_count mismatch, container overallocation — were
eliminated by Subtasks A/B/D1/D2/D3 + the sfq_count idempotency fix.)

## Question for NotebookLM

For an APFS volume with one active snapshot, when a file whose extent is
referenced ONLY by the snapshot is unlinked from the live view (via `unlink`),
what is the canonical state of the volume's **live** extentref tree
(`apfs_extentref_tree_oid` of the live VSB) and the snapshot's extentref tree
(`apfs_extentref_tree_oid` of the snapshot's frozen VSB)?

Specifically:

1. **Two separate extref trees** (current implementation): the snapshot
   freezes the pre-snapshot extref tree (EXT_pre, refcnt=1 for the extent);
   the live VSB gets a NEW empty extref tree (X1, COWed to X3 over later
   writes). The extent is in EXT_pre only. fsck flags "missing physical
   extent" and "fsroot reference count (-1)". Is this layout WRONG?

2. **Shared extref tree, refcnt-tracked**: both live and snapshot point at
   the SAME extref tree. The single entry for (2066, 16) holds the total
   refcnt across all fsroot views. At `create_snapshot`, every existing
   extref entry's refcnt is incremented by 1 (snapshot adds a reference).
   At `unlink`, the live decrement applies. If the result is `refcnt > 0`,
   the entry stays; if 0 AND no snapshot pins, free the extent. Is THIS
   the canonical model?

3. **Refcnt semantics on create_snapshot**: does the writer increment
   each extref entry's `refcnt` by 1 on snapshot creation (covering the
   snapshot's view), and decrement on `delete_snapshot`?

4. **Live extref-tree population**: when an extent is snapshot-only (live
   has no FILE_EXTENT pointing to it but the snapshot does), should the
   live extref tree CONTAIN the entry (with refcnt reflecting the snapshot
   pin) or NOT contain it (snapshot's separate extref carries it)?

5. **`fs_alloc_count` semantics for snapshot-pinned blocks**: are
   snapshot-only data extents counted in the LIVE VSB's `apfs_fs_alloc_count`
   (= volume-owned total) or only in the snapshot's view?

6. **Empty extref tree at create_snapshot**: kernel-formatted volumes
   appear to handle snapshots without allocating a fresh empty extref tree
   per snapshot. What does the kernel actually do at snapshot creation
   for `apfs_extentref_tree_oid`?

7. **fsck's expectations**: `fsck_apfs` reports
   "extentref entry reference count (0) ... fsroot entry reference count (-1)".
   What does fsck count when computing "calculated fsroot entry reference
   count" — does it sum across snapshot + live fsroots? Does -1 signal
   "snapshot fsroot ref (+1) minus expected refcnt counter not found in
   the live extref"?

## Hypothesis

Hypothesis (to be confirmed): the canonical model is **shared extref tree
with refcnt incremented on create_snapshot**. The current implementation
(separate empty live extref per snapshot) is structurally valid only for
the simple create-then-delete-snapshot case (which passes), but breaks
once unlink-after-snapshot races the refcnt accounting that fsck expects
on the live tree.

## What's done in this codebase

- Subtask A `999e97c` — single-omap arch
- Subtask B `c8580a2` — omap retention bounded by newest_snap
- Subtask D1 `ddf94a4` — sm_fq node builder + decoder helpers
- Subtask D2 `d0164be` — Transaction.enqueue_sm_fq + drain_sm_fq_main
- Subtask D3 `84c9245` — free_replaced_metadata reroutes pinned blocks to sm_fq
- Fix `6d8b879` — D2 sfq_count idempotent insert

Open: Subtask E (this question), Subtask C (OMAP_VAL_DELETED tombstones),
Subtask D4 (delete_snapshot drain integration).

## Pending real-disk verification

Once Subtask E semantics are locked, decode a kernel-created snapshot
(e.g. `tmutil snapshot /Volumes/<test-volume>`) and inspect the live
extref tree to confirm whether snapshot-pinned extents are present with
refcnt > 0 (shared-tree model) or absent (separate-tree model).

---

## ANSWERS (LOCKED — NotebookLM, 2026-05-23) — KB `apfs-spec:extref-snapshot-semantics`

> **Hypothesis confirmed (with refinement)**: SHARED Extref Tree, but NO
> bulk refcnt increment at snapshot creation. Snapshot creation is O(1):
> live and snapshot VSBs both record the SAME current extref `paddr`. The
> refcnt only mutates during active live operations. The "implicit pin"
> of the snapshot's frozen root protects all blocks reachable from it.

- **Q1+Q2 — model**: Shared extref tree. Current "separate empty live
  extref per snapshot" implementation is STRUCTURALLY WRONG. Apple's
  snapshot creation is O(1), not O(N).
- **Q3 — refcnt on create_snapshot**: NO bulk increment. The snapshot's
  frozen catalog implicitly references the extents.
- **Q4 — live extref population**: YES, the live extref tree MUST contain
  snapshot-only entries (carried forward across COW). The current
  refcnt typically remains 1 (single snapshot reference).
- **Q5 — fs_alloc_count**: counts ALL volume-owned blocks INCLUDING
  snapshot-pinned ones.
- **Q6 — at create_snapshot**: kernel does NOT allocate a fresh empty
  extref. VSB.apfs_extentref_tree_oid stays at the current physical
  block; frozen VSB and snap_metadata record the same paddr.
- **Q7 — fsck calculation**: fsck walks live + all snapshot fsroots,
  sums references, compares against the LIVE extref tree's refcnts.
  "-1" means: snapshot fsroot adds +1, live fsroot adds 0, total
  expected refcnt = 1; live extref reports 0 → diff = -1 = "missing
  one expected reference in the live extref tree".

### Actionable implementation (locked)

1. **`create_snapshot`**: REMOVE the new empty extref allocation. Live VSB
   and frozen VSB share the SAME `apfs_extentref_tree_oid` (= the current
   extref paddr at snapshot time). snap_metadata_val.extentref_tree_oid
   = same.
2. **`delete_snapshot`**: do NOT double-free the snapshot's frozen extref
   paddr when it equals the live extref paddr (i.e., no writes between
   create_snapshot and delete_snapshot).
3. **`unlink`**: when `apfs_num_snapshots > 0`, leave extref entries and
   data blocks alone (conservative — over-pins post-snapshot extents
   but is safe; precise per-extent xid check is a follow-up).
4. **Mutations (write_file)**: standard COW on the existing live extref
   tree — no change needed.

---

## Implementation attempt — reverted (2026-05-23)

A first attempt implementing Subtask E (1) + (2) + (3) above was applied
to `create_snapshot`, `delete_snapshot`, and `unlink`. It regressed
`decisive_create_delete_snapshot_on_scratch` (previously GREEN under
Subtask A).

**Why the minimal shared-tree edit was not enough** — after applying
(1) only, the `repro_overalloc_snapshot_pinned` post-create-snapshot
state showed:
- Live VSB.apfs_extentref_tree_oid = 0x824 (shared with snapshot) ✓
- Snapshot frozen VSB.apfs_extentref_tree_oid = 0x824 ✓
- snap_meta_tree.SNAP_METADATA.extref_oid = 0x824 ✓
- Extref @ 0x824 (xid=3) had A's entry with refcnt=1 ✓

Yet fsck reported:
```
warning: mismatch between extentref entry reference count (1) and
         calculated fsroot entry reference count (0) for extent (2066 + 16)
warning: orphan/invalid physical extent (2066)
```

The decoded fsroot @ 0x822 (resolved via omap) DOES contain A's
`FILE_EXTENT obj_id=16 phys=2066 len=65536`. But fsck calculates 0
fsroot references. Hypothesis: fsck distinguishes the snapshot's
fsroot view from the live's by requiring a SEPARATE `{root_oid, xid}`
omap entry per snapshot. With our single shared paddr (0x822), fsck
treats it as the live view only and misses the snapshot view's
contribution.

### What's needed for a full Subtask E implementation

- At `create_snapshot`, perhaps ALSO add a `{root_oid, snap_xid}` omap
  entry pointing at the SAME fsroot paddr as `{root_oid, xid_pre}`
  (since the snapshot view shares the fsroot at snap time). Or COW the
  fsroot to a new paddr stamped at snap_xid so fsck sees a distinct
  snapshot-owned root.
- Reconcile the `apfs_root_tree_type` / virtual-vs-physical semantics
  for the root_tree_oid across live and snapshot views.
- Per-extent age check in `unlink` to free post-snapshot extents while
  preserving pre-snapshot ones (the conservative "block all unlink-frees
  when snapshots exist" guard isn't enough).
- Likely also requires Subtask C (`OMAP_VAL_DELETED` tombstones) to
  signal removed-in-live, present-in-snap.

### Decision

For this session's checkpoint: revert the partial Subtask E code,
keep the locked KB answers (this doc), and continue Subtask E in a
fresh session with deeper investigation of fsck's snapshot-fsroot
counting model. The Subtask A+B+D1+D2+D3 chain remains GREEN; only
`repro_overalloc_snapshot_pinned` stays `#[ignore]`'d.

---

## 2026-05-24 — fsck's xid binding constraint (empirical)

Tried to surgically append a `{root_oid, snap_xid} -> existing_fsroot_paddr`
entry to the COW'd volume omap b-tree at create_snapshot. The append
itself was structurally correct (FIXED_KV TOC, sorted, footer.key_count
bumped), but fsck immediately reported:

```
error: (oid 0x404) apfs_root: bt: invalid o_xid (0x2, expected 0x3)
```

**Locked insight**: fsck cross-validates `omap_entry.xid` against the
target paddr's `obj_phys.o_xid`. They MUST match. Pointing a new
`{root_oid, snap_xid=3}` omap entry at the pre-existing fsroot block
(with `o_xid=2`) is a hard error.

### Corrected approach for the next session

At `create_snapshot`, also COW the fsroot to a new paddr stamped at
`snap_xid`:

1. Read the current fsroot at its existing paddr (`o_xid=write_A_xid` or
   baseline).
2. Allocate a new paddr.
3. Copy bytes; bump `o_oid = new_paddr`, `o_xid = snap_xid`; recompute
   checksum.
4. Stage at new paddr.
5. Add `{root_oid, snap_xid} -> new_fsroot_paddr` to the volume omap
   b-tree.
6. `fs_alloc_count` += 1 (one extra block now owned).
7. The OLD fsroot paddr stays referenced via the omap's pre-existing
   `{root_oid, older_xid}` entry — preserved as snapshot's earlier
   history (or pinned via `sm_fq` if intermediate).

Helpers `parse_omap_entries` + `build_omap_node` are already
`pub(crate)` from `985d4e6` for the next attempt — but the generic
`build_omap_node` produces wrong footer/flags for the volume omap,
so a surgical TOC append (like the reverted experiment in this doc)
is still needed.

---

## 2026-05-24 — Full Subtask E implementation done (3b9f1f4); fsroot=0 mystery persists

`create_snapshot` now performs:
1. ✅ Extref tree COW (snapshot retains old paddr, live gets new paddr
   stamped at `snap_xid`).
2. ✅ Fsroot COW (snapshot's view at `snap_xid` is a new paddr; old
   paddr stays for `xid_pre_snap` entry). CRITICAL detail: fsroot is
   VIRTUAL — `o_oid` stays = root_tree_virtual_oid (e.g. 0x404), NOT
   the paddr. Only `o_xid` is bumped to snap_xid.
3. ✅ Omap `{root_oid, snap_xid} -> new_fsroot_paddr` surgical TOC
   append (FIXED_KV omap b-tree, kvoff_t entries, 16B omap_key_t /
   omap_val_t).
4. ✅ fs_alloc_count delta corrected (+1 fsroot block, -1 extref-COW
   pair).

Test state after full implementation:
- `decisive_create_delete_snapshot_on_scratch`: GREEN (Subtask A baseline).
- `repro_overalloc_snapshot_pinned`: container OK + sfq CLEAN, but
  fsck STILL flags:
  `extentref entry reference count (1) and calculated fsroot entry
   reference count (0) for extent (2066 + 16)` + `orphan/invalid
   physical extent (2066)`.

Real-disk decode confirms both fsroots (xid=3 @ 0x822, xid=4 @ 0x827)
contain A.txt's `FILE_EXTENT obj_id=16 phys=2066 len=65536`. The
extref tree has refcnt=1 for (2066,16) with owner=inode-16. All chains
resolve. Yet fsck calculates 0 fsroot references.

### Leading hypothesis for next session

`live_xid > snap_xid` requirement. fsck might interpret "live entries"
as omap entries with `xid > om_most_recent_snap`. Currently our
create_snapshot commits at `txn.xid = snap_xid`, so all omap entries
are `xid ≤ snap_xid` → fsck sees them as snapshot-pinned and finds
NO live fsroot to count.

To test: structure `create_snapshot` so the snapshot commits at one
xid but the live VSB / new omap entry are stamped at `snap_xid + 1`.
This may require a two-xid commit model (or a single commit with
mixed xids — needs careful investigation of nx_superblock semantics).

Alternative: a single kernel-formatted post-tmutil-snapshot volume
decode would resolve this definitively. We don't have direct access
to a macOS box for this in-session.
