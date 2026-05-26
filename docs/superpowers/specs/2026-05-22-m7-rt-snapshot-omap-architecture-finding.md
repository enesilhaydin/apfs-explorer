# M7a-RT — snapshot reclaim blocker: frozen-omap_phys vs om_snapshot_tree (finding)

> 2026-05-22. After the non-snapshot write engine reached real-USB fsck CLEAN,
> the two snapshot scratch tests remain #[ignore]'d. This records WHY they
> overallocate and what the canonical fix is, so the follow-up is unambiguous.
> Sources to lock: extends apfs-spec:omap-snapshot-versioning (Q2 om_snapshot_tree).

## Empirical decode (repro_overalloc_snapshot_pinned: writeA→snap→unlinkA→writeB)

Three volume superblocks on disk after the sequence:

| paddr | o_xid | omap_oid | extref | role |
|------:|------:|---------:|-------:|------|
| 2060  | 4     | **2085** | 2084   | FROZEN snapshot VSB (snap xid 4) |
| 2086  | 3     | 2085     | 2084   | superseded live VSB (write A)    |
| 2088  | 4     | 2087     | 2062   | live VSB after create_snapshot   |

fsck overallocation: blocks 2061 (xid2 btree subtype 0x10 = old snap-meta),
2083 (xid3 omap b-tree node), 2085 (xid3 omap_phys) — bitmap 0x7d8.

## Root cause

`create_snapshot` freezes a SEPARATE old `omap_phys` (2085) for the snapshot and
hands the live volume a NEW one (2087). So there are TWO volume object maps:
- snapshot's frozen omap_phys 2085 → its own b-tree node 2083, and
- the live omap_phys 2087 → its own (evolving) b-tree.

The omap-version-retention fix (rewrite_fstree carries forward prior `{oid,xid}`
entries when a snapshot exists) then re-adds the snapshot-era catalog mappings
(e.g. `{root_oid, xid3} -> paddrA`) into the LIVE omap 2087's tree — while the
snapshot's frozen omap 2085 ALSO maps them. The shared catalog/omap nodes are
now reachable from BOTH object maps → fsck counts them referenced more than once
→ "overallocation". (Symmetrically, 2085/2083/2061 — the snapshot's own frozen
omap + old snap-meta — are pinned by the snapshot yet collide with the live
omap's carried-forward view.)

This is NOT a leak that immediate-free can fix, and NOT the spaceman free queue
alone — it is an OMAP ARCHITECTURE mismatch.

## Canonical fix (Apple model)

APFS keeps ONE volume object map. Snapshots do NOT get a private frozen
`omap_phys`; the snapshot's frozen VSB references the SAME (current) omap, and
resolution is purely by xid: an object is resolved with the largest
`xid <= snapshot_xid`. The set of live snapshots is tracked in the omap's
`om_snapshot_tree` (subtype 0x13, key `xid_t`, val `omap_snapshot_t`), and
block reclaim is deferred to the spaceman free queue + reaper.

So the snapshot-reclaim milestone is:
1. `create_snapshot`: stop freezing a separate old omap_phys. Keep ONE omap;
   the frozen VSB points at the live omap_phys; record the snapshot xid in
   `om_snapshot_tree`. (om_most_recent_snap / om_snap_count already updated.)
2. omap retention bounded by the oldest snapshot xid (Q1), with OMAP_VAL_DELETED
   tombstones on delete-while-snapshotted (Q3).
3. Spaceman free queue (`sm_fq`, `{xid,paddr}` b-tree) + reaper for deferred
   reclaim on snapshot delete (Q4/Q6).
4. extentref refcount for blocks shared across snapshots / clones (Q5).

## Status / what is safe now

- Non-snapshot writes (create/write/mkdir/unlink/rename, single + cycles) are
  fsck CLEAN on scratch AND a real 32 GB USB. SHIPPABLE.
- `free_replaced_metadata` is now snapshot-aware (frees only blocks whose version
  xid > newest snapshot xid; identical to before when no snapshot exists), a
  correct partial step toward (2)/(3).
- The two snapshot tests stay #[ignore]'d until the om_snapshot_tree rework.
  They do NOT indicate a regression: snapshot create/delete + volume structure
  are valid; only container space accounting overallocates due to the dual-omap.
