# M7a-RT round12 — fsck's expected btn_table_space.len for OBJECT_TYPE_OMAP_SNAPSHOT

After round10/11/11b/11c fixes the snap_meta tree validation passes
and fsck now validates the om_snapshot_tree (subtype 0x13). One
specific field still rejected:

```
error: btn: oid (204), xid (3), type (0x40000002), subtype (0x13),
       flags (0x7) level (0)
error: btn: invalid btn_table_space (0, N), given btn_flags (0x7)
```

We populated a SINGLE entry (snap_xid → omap_snapshot_t) and tried
the following N values for btn_table_space.len:

| N attempted | Reasoning | fsck verdict |
|---|---|---|
| 4 | Just 1 TOC entry (kvoff_t) | invalid |
| 448 | Same as kernel's omap btree reserve | invalid |
| 568 | max_records(142) * toc_entry_size(4), 8-aligned | invalid |

## Block context (everything else passes)

```
obj_phys.o_type    = 0x40000002 (OBJ_PHYSICAL | OBJECT_TYPE_BTREE)
obj_phys.o_subtype = 0x00000013 (OBJECT_TYPE_OMAP_SNAPSHOT)
btn_flags          = 0x0007 (BTNODE_ROOT|LEAF|FIXED_KV_SIZE)
btn_level          = 0
btn_nkeys          = 1
btn_key_free_list  = {0xFFFF, 0}
btn_val_free_list  = {0xFFFF, 0}
btn_free_space     = {N*KEY_SIZE, lowest_v - key_end} computed correctly

btree_info_t footer:
  bt_flags      = 0x52 (BTREE_PHYSICAL|KV_NONALIGNED|SEQUENTIAL_INSERT)
  bt_node_size  = 4096
  bt_key_size   = 8  (sizeof xid_t)
  bt_val_size   = 16 (sizeof omap_snapshot_t)
  bt_longest_key = 8
  bt_longest_val = 16
  bt_key_count  = 1
  bt_node_count = 1

TOC[0]: key_off=0, val_off=16
Key bytes: snap_xid u64 LE (= 3)
Value bytes: omap_snapshot_t { oms_flags=0, oms_pad=0, oms_oid=0 }
```

bt_flags=0x52 was accepted (fsck moved past it to table_space check).
Earlier failures included key_size mismatches and inum=0 — both fixed.

## Q-EE: what value of btn_table_space.len does fsck accept here?

(a) Is there a tree-subtype-specific constant fsck has hardcoded for
    OBJECT_TYPE_OMAP_SNAPSHOT? If so, what value?

(b) Or is the expected value derived from bt_key_size, bt_val_size,
    and bsz via a specific formula we haven't found? If so, give the
    EXACT formula (and any rounding/alignment rules).

(c) Or does fsck require additional metadata in btree_info_t footer
    (bt_longest_key/longest_val of different value, etc) that makes
    our table_space.len pass when set together?

## Q-FF: alternative — does fsck have a flag we should set to skip
this check?

For example, a btree-info flag that signals "fresh empty tree, don't
validate reserved capacity". Apple PDF flag list includes
BTREE_NONPERSISTENT (0x20), BTREE_NOHEADER (0x100). Could either
opt-out of the table_space.len validation?

## Q-GG: precise reading of fsck's `mt_btflags` term

The earlier fsck error mentioned `given mt_btflags`. What is
mt_btflags? Is it:

(a) A volume-level "metadata btree flags" stored in apfs_superblock?
(b) A kernel constant per object-type that fsck cross-checks?
(c) The bt_flags field within btree_info_t that we already set?

Understanding mt_btflags unlocks why fsck accepts/rejects specific
table_space values.

## What works so far (rounds 1-11c LOCKED)

- Round1: SNAP_NAME key format (j_key_t + name_len + name)
- Round2: BTOFF_INVALID 0xFFFF/0 free-lists
- Round3: snap-meta toc_len=64 reserve
- Round5: 8-byte alignment
- Round6: silent rollback illusion + container omap free_space
- Round7: extentref_tree_type preservation + frozen VSB OIDs
- Round9: OBJECT_TYPE_OMAP_SNAPSHOT = 0x13, UNIX-epoch timestamps
- Round10: APFS_TYPE_SNAP_METADATA=1, APFS_TYPE_SNAP_NAME=11
- Round11: inum=2 ROOT_DIR_INO_NUM
- Round11c: om_snapshot_tree populated with 1 omap_snapshot_t record

Only one specific invariant remains: btn_table_space.len for the
om_snapshot_tree root. Cracking this completes M7a-RT.
