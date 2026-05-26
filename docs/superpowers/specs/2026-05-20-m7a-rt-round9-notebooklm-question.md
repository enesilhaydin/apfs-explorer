# M7a-RT round9 NotebookLM — definitive om_snapshot_tree subtype + records

Round8 first attempt (committed 7f9c097): provisioned an EMPTY btree
at om_snapshot_tree_oid with subtype 0x0B (OBJECT_TYPE_OMAP, per
NotebookLM round8). Scratch still fails with the SAME "Snapshot
metadata tree is invalid" error → either the subtype is wrong, the
btree is required to have records, or the bug is elsewhere entirely.

User flagged round8 NotebookLM reliability as lower. This round9 is
designed to disambiguate with multiple cross-checked questions.

## Q-V (deterministic): What subtype does Apple use for om_snapshot_tree?

Apple PDF page 107 already assigns:
- `OBJECT_TYPE_OMAP = 0x0B`
- `OBJECT_TYPE_SNAPMETATREE = 0x10`

But there is also `OBJECT_TYPE_OMAP_SNAPSHOT` mentioned in some
documentation. What is its EXACT numeric value and is it distinct
from OBJECT_TYPE_OMAP?

Please scan Apple PDF for ALL `OBJECT_TYPE_*` constants between 0x10
and 0x20 (or wherever the snapshot-index subtype falls) and provide
the full enumeration with values + PDF page citation.

## Q-W: Is om_snapshot_tree allowed to be empty, or must it contain ≥1 record?

Round8 said the tree must contain one `omap_snapshot_t` record per
snapshot accounted for in `om_snap_count`. Confirm with Apple PDF
citation that on the FIRST snapshot, the tree MUST contain exactly
one record. Or whether an empty tree (nkeys=0) is acceptable as long
as the OID is non-zero.

## Q-X: omap_snapshot_t exact byte layout (round8 may have been wrong)

Round8 NotebookLM gave:
```c
struct omap_snapshot_t {
    uint64_t snap_xid;
    uint64_t snap_create_time;
    uint64_t snap_metadata_tree_oid;
    uint64_t snap_name_len;       // uint64 — suspicious
    char     snap_name[];
};
```

Cross-check with Apple PDF. Specifically:
- Is `snap_name` truly part of `omap_snapshot_t`? (Looks redundant
  given snap_meta tree already has SNAP_NAME records.)
- Is `snap_name_len` `u64` or `u16`? (Other APFS string-length fields
  use u16.)
- Does the struct ACTUALLY contain just `{flags:u32, size:u32,
  oid:u64}` like a regular `omap_val_t`? (PDF page 130-ish?)
- Or is it `{snap_oid:u64, snap_metadata_tree_oid:u64}` — 16 bytes,
  no name?

Please dump the literal struct from Apple PDF VERBATIM (with field
declarations as written in the spec).

## Q-Y: Where in Apple PDF is `omap_snapshot_t` formally defined?

Round8 cited "page 117". But page 117 is `j_snap_metadata_val_t`
(part of the snap-meta tree, not om_snapshot_tree). Please find the
ACTUAL page in Apple PDF where `omap_snapshot_t` (or whatever the
om_snapshot_tree's value struct is named) is formally defined.

## Q-Z: Is the actual fsck rejection really om_snapshot_tree, or somewhere else?

The fsck error message is verbatim:
```
** Checking the snapshot metadata tree.
   Snapshot metadata tree is invalid.
```

"snapshot metadata tree" matches `apfs_snap_meta_tree_oid` (in VSB),
not `om_snapshot_tree_oid` (in omap_phys). Maybe round8's diagnosis
was WRONG and the actual bug is still in snap_meta_tree itself, not
om_snapshot_tree. Please confirm:

(a) Does fsck explicitly emit different error strings for these two
    trees (e.g. "Object map snapshot tree is invalid" vs "Snapshot
    metadata tree is invalid")?
(b) Or is "snapshot metadata tree" a generic error that fires for
    EITHER tree?

If (a), and our error is the snap_meta one specifically, then
om_snapshot_tree work was a wrong direction; the snap_meta tree
itself has a bug we haven't found.

## What we've proven byte-equivalent to kernel

For reference, here is everything we have verified matches the
kernel byte-for-byte (via Python hex diff):
- Container OMAP btree: header + TOC + key + value placement
- Snap-meta tree btree header (table_space=64, free_space=27/3842,
  free_lists=0xFFFF/0)
- Snap-meta tree TOC entries (k_off=0/8, k_len=8/19, v_off=59/67,
  v_len=59/8)
- Snap_metadata_val fields including extentref_tree_type=0x40000002
- All Fletcher-64 checksums

## How to use

Paste this file into NotebookLM after model quota reset. Save answers
to `2026-05-20-m7a-write-spec-notes-round9.md`. The Q-V (subtype)
and Q-Z (which-tree) answers are the highest-priority disambiguators.
