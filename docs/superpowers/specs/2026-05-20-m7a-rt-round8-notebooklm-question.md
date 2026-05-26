# M7a-RT round8 NotebookLM — volume `om_snapshot_tree_oid` structure

Round 1-7 LOCKED. After applying every fix, scratch fsck STILL
reports "Snapshot metadata tree is invalid". Byte-level decode of
our volume omap_phys after a snapshot create shows:

```
volume omap_phys:
  om_snap_count        = 1   (we incremented it)
  om_most_recent_snap  = 0x3 (we set it to snap_xid)
  om_tree_oid          = 0xC2 (preserved from kernel, still valid)
  om_snapshot_tree_oid = 0x0 ← MISSING
```

Apple PDF page 116 defines:
```
om_snapshot_tree_oid: The object identifier of the snapshot tree that
                      gives access to snapshots.
```

We don't currently build a snapshot tree on snapshot create. The
suspicion is that an `om_snap_count > 0` with `om_snapshot_tree_oid
= 0` is the invariant fsck rejects via the chained "Snapshot
metadata tree is invalid" error.

## Q-Q: when is `om_snapshot_tree_oid` required to be non-zero?

(a) Always when `om_snap_count >= 1`?
(b) Only when there are revertible snapshots (some flag)?
(c) When a specific snapshot-related field in the volume reaches a
    threshold?

## Q-R: what exact on-disk structure does `om_snapshot_tree_oid` point to?

It's a B-tree root. We need the exact emit:

1. **`obj_phys.o_type`** — value?
   Probably `OBJ_PHYSICAL | OBJECT_TYPE_BTREE` (= 0x40000002)?

2. **`obj_phys.o_subtype`** — value?
   Apple lists `OBJECT_TYPE_OMAP_SNAPSHOT`? Or `OBJECT_TYPE_OMAP`?
   Or a different subtype constant?

3. **`btn_flags`** — value?
   ROOT | LEAF (single-node tree) = 0x0003? Plus FIXED_KV_SIZE
   (0x4) if records are fixed size?

4. **`btree_info_t.bt_flags`** — value?
   BTREE_PHYSICAL | BTREE_KV_NONALIGNED | ...?

## Q-S: `omap_snapshot_t` record format (records inside the snapshot-tree)

Apple PDF page 117 alludes to this. What is the exact struct? Likely
something like:

```c
struct omap_snapshot_key { xid_t xid; };           // 8 bytes?
struct omap_snapshot_val { uint32_t flags; ... };  // ?
```

Or is it `omap_phys_t` style? Or different?

What field offsets, sizes, encoding?

## Q-T: required `om_snapshot_tree_type` field in volume omap_phys?

`omap_phys_t` has separate `om_tree_type` (@40) and
`om_snapshot_tree_type` (@44) u32 fields. Currently the kernel sets
both to `0x40000002`. When we build a new snapshot tree, do we copy
the same value, or use a different one?

## Q-U: relationship to `om_most_recent_snap`?

When the first snapshot is created at xid=3:
- `om_snap_count` 0 → 1
- `om_most_recent_snap` 0 → 3
- `om_snapshot_tree_oid` 0 → ???

Does the snapshot tree need EXACTLY ONE record (`omap_snapshot_t`
keyed by xid=3)? Or zero records initially?

## How to use

NotebookLM, paste this file, capture answers with Apple PDF pages.
Save to `2026-05-20-m7a-write-spec-notes-round8.md`. Once we have
the answers, implementation is small:
- Allocate one block for the new snapshot tree root
- Build empty btree node with correct type/subtype/flags
- (Optional) insert one record keyed by snap_xid
- Set `om_snapshot_tree_oid` in the COW'd volume omap_phys
- Set `om_snapshot_tree_type` field too

Then re-prove scratch + (after USB reformat) real-USB CLEAN.

Proven at 48efacb baseline — all earlier fixes preserved.
