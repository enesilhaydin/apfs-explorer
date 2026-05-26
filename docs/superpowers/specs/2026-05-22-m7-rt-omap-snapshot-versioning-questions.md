# M7a-RT — Object-map snapshot versioning: open NotebookLM questions (English)

> Written 2026-05-22. These are the questions that the snapshot-pinned scratch
> red test (`repro_overalloc_snapshot_pinned`: write A → snapshot → unlink A →
> write B) surfaced and that I solved EMPIRICALLY (fsck CLEAN) but could NOT pin
> to the canonical APFS rule. My fix works but has known limitations (noted per
> question). Paste verbatim into NotebookLM; lock answers to KB
> `apfs-spec:omap-snapshot-versioning`.
>
> md.6: NotebookLM / Apple PDF are HYPOTHESES; the real macOS disk is the final
> arbiter. The current fix is already real-disk CLEAN, so these refine the model
> rather than block it.

## Context (what I implemented, and why it is incomplete)

When a volume has a snapshot at transaction `xid = S` and the live volume is
later mutated (catalog copy-on-write at `xid = L > S`), I observed that the
object map (`omap`, a B-tree keyed by `{oid, xid}`) must retain the
`{oid, xid <= S} -> paddr` mappings, otherwise the snapshot can no longer
resolve its catalog (fsck: "Snapshot is invalid"). My catalog-rewrite path
originally re-stamped every omap entry at the new xid, destroying those
mappings.

My fix: when a snapshot exists (`apfs_num_snapshots > 0`), the omap rebuild
carries forward ALL prior `{oid, xid}` entries and adds the new
`{oid, new_xid}` entries (old catalog blocks are never freed, so the carried
mappings stay valid). This is fsck CLEAN, but:
- It carries forward **every** old entry, including dead intermediate versions
  with no snapshot referencing them (unbounded omap growth).
- It never reclaims the pinned blocks or stale omap entries after a snapshot is
  deleted.
- It uses a single-node omap (no split yet).

## Questions

1. **Omap version retention bound.** When the live volume is mutated after a
   snapshot at xid `S`, which `{oid, xid}` omap entries must be retained and
   which may be pruned? Is retention bounded by the OLDEST live snapshot xid —
   i.e. for each `oid`, keep the newest entry with `xid <= S_oldest` (the
   version each snapshot needs) plus the live (latest) entry, and drop
   intermediate versions that no snapshot references? Or does APFS keep every
   version until the relevant snapshot is deleted?

2. **`om_snapshot_tree_oid` (omap_phys offset 56).** What is the structure
   (key/value layout) and exact role of the object map's snapshot tree? Is it
   required for snapshot resolution, or is the main `om_tree` (keyed
   `{oid, xid}`) sufficient on its own? When `create_snapshot` runs, must we
   populate `om_snapshot_tree`, and with what entries (e.g. one per snapshot
   xid)? My current `create_snapshot` does not build it explicitly — why does
   fsck still accept the snapshot?

3. **`omap_val` flags (OMAP_VAL_DELETED, OMAP_VAL_SAVED, …).** When an `oid` is
   replaced or deleted in the live volume while a snapshot pins the previous
   version, is the old omap value marked with a flag (e.g. `OMAP_VAL_SAVED`)
   rather than physically retained as a separate `{oid, xid}` key? How do these
   flags interact with the `xid`-based "largest xid <= target" resolution?

4. **Block + omap-entry reclaim on snapshot delete.** When `delete_snapshot`
   removes the only snapshot referencing a set of blocks (old catalog nodes,
   pre-snapshot data blocks from files that were unlinked while snapshotted, and
   the stale `{oid, xid}` omap entries), how are those reclaimed? Are the blocks
   enqueued on the spaceman free queue keyed by the deleted snapshot's xid, and
   are the orphaned omap entries simply dropped on the next omap rebuild? What
   exactly does fsck expect afterwards (no leaked blocks, no dangling
   `{oid, xid}` entries)?

5. **Live-ownership vs extentref refcount for unlink.** When unlinking a file
   while a snapshot exists, is "the physical-extent record is present in the
   LIVE extentref tree" the correct signal that a block is owned by the live
   volume (and therefore free-able)? After `create_snapshot` swaps in a fresh
   EMPTY live extentref tree, all pre-snapshot blocks are absent from the live
   tree — does APFS rely on exactly this (so the absence means "snapshot owns
   it, do not free"), or does the canonical model decrement an extentref
   reference count (`j_phys_ext_val.refcnt`) and free only when it reaches 0?
   If refcount is canonical, what bumps it at snapshot creation?

6. **Omap node split + key ordering.** The volume omap is a B-tree; with many
   snapshots and mutations the `{oid, xid}` entry set outgrows one node. What is
   the canonical key ordering for omap nodes and internal-node pivots — `oid`
   ascending then `xid` ascending? When the root node fills, does the omap split
   like the catalog (internal node holds child physical paddrs, since the omap
   itself is a physical tree)? Any per-node entry-count limit beyond block size?

## My current answer hypotheses (to confirm or correct)

- Q1: I suspect retention should be bounded by the oldest snapshot xid; carrying
  ALL versions is over-conservative but not incorrect. (Need confirmation.)
- Q5: I suspect live-extentref-presence is equivalent to refcount==live for the
  common case because `create_snapshot` resets the live extentref tree, but
  refcount is the general mechanism (matters for blocks shared by multiple
  snapshots or written before AND after a snapshot). (Need confirmation.)
- Q4: I suspect the spaceman free queue keyed by xid is the reclaim path
  (consistent with the earlier batch answers B8/B10). (Need confirmation.)

---

## ANSWERS (LOCKED — NotebookLM + Apple File System Reference, 2026-05-22)

> md.6: hypotheses; real macOS disk is the arbiter. The current fix is already
> real-disk CLEAN; these refine the model. KB: `apfs-spec:omap-snapshot-versioning`.

- **Q1 (retention bound) — CONFIRMED + refined.** Retain, for each OID, the
  latest version with `xid <= each active snapshot's xid`, PLUS the live
  version. Intermediate mutations between two snapshots may be pruned (no
  snapshot resolves to them). Resolution: search for the matching OID with the
  largest `xid <= requested xid`. (PDF p.110-111.) → my carry-ALL approach is
  over-conservative but correct; pruning by snapshot xids is the optimization.

- **Q2 (om_snapshot_tree_oid) — must populate for tmutil compliance.** Distinct
  PHYSICAL b-tree, subtype `0x13` OBJECT_TYPE_OMAP_SNAPSHOT. Key = `xid_t`
  (snapshot xid). Value = `omap_snapshot_t` (16B: `oms_flags` u32, `oms_pad`
  u32, `oms_oid` u64). fsck tolerates its absence (falls back to checkpoint
  records + historical XIDs), but a fully macOS/Time-Machine-compliant volume
  must populate it. (PDF p.47, p.115, p.15.) → my create_snapshot omits it;
  follow-up to build it.

- **Q3 (omap_val flags) — TOMBSTONE mechanism.** Deleting an oid that a snapshot
  pins does NOT just leave the old entry: insert a NEW key `{oid, live_xid}`
  with value flag `OMAP_VAL_DELETED` (0x1). Live lookup hits the tombstone (gone);
  snapshot lookup at `xid = old_xid` stops at the old key and resolves the old
  block. (PDF p.114.) → my approach leaves the old entry without a tombstone;
  works for the test, but the canonical model uses tombstones (matters when the
  live volume looks up a deleted oid).

- **Q4 (reclaim on snapshot delete) — CONFIRMED deferred.** Spaceman free queue
  (`sm_fq`) + Reaper (OBJECT_TYPE_NX_REAPER / NX_REAP_LIST). Snapshot delete
  hands the snapshot root + unique omap entries to the reaper, which walks
  historical trees and pushes blocks referenced ONLY by the deleted snapshot's
  xid epoch to the free queue; orphaned omap entries are removed. (PDF p.107,
  p.67.)

- **Q5 (unlink ownership) — CORRECTED: canonical is refcnt.** Each physical
  extent has a record in the extentref tree with `refcnt` (usually 1). A
  snapshot pins the whole extentref tree. Unlink in the live volume decrements
  the live extentref `refcnt`; a block is freed only when `refcnt == 0` AND not
  protected by a snapshot boundary. My empty-live-extentref-on-snapshot approach
  bypasses the decrement for historical blocks — works for a single snapshot /
  refcnt==1, but BREAKS for cloned blocks shared across snapshots (`refcnt > 1`).
  (PDF p.89 `j_phys_ext_val_t.refcnt`.) → real refcounting needed for clones.

- **Q6 (omap split + ordering) — CONFIRMED.** omap b-tree sorted by OID asc then
  XID asc. Volume omap is a PHYSICAL b-tree (OBJECT_TYPE_OMAP): on split,
  internal-node values are `paddr_t` child block addresses (NOT virtual oids).
  Pivot key = exact copy of the child's first `{oid, xid}` key. No entry-count
  limit beyond the 4096-byte block. (PDF p.113, p.121.) → my (oid,xid) sort is
  correct; omap split (future) uses physical child pointers.
