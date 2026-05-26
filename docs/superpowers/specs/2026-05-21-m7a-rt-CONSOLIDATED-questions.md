# M7a-RT — TÜM AÇIK NotebookLM Soruları (Consolidated)

After 11+ rounds of NotebookLM Q&A and 27 commits, only the
`om_snapshot_tree` validation remains. fsck rejects `btn_table_space.len`
regardless of value tried. This consolidated file collects EVERY open
question across rounds so you can ask them once.

---

## Context — What's already locked

| Round | Locked answer | Apple PDF page |
|---|---|---|
| 1 | SNAP_NAME key = j_key_t + name_len:u16 + name; obj_id=~0ULL | 119 |
| 2 | BTOFF_INVALID = 0xFFFF for empty free-lists | 122 |
| 5 | btn_table_space.len 8-aligned (FIXED_KV trees) | 297 |
| 6 | fsck silent-rollback when checkpoint malformed | 26 |
| 7 | extentref_tree_type = VSB.apfs_extentref_tree_type (u32) | 117 |
| 9 | OBJECT_TYPE_OMAP_SNAPSHOT = 0x13 (distinct from OMAP=0xB) | 15 |
| 9 | UNIX-epoch nanoseconds for timestamps | 118 |
| 10 | APFS_TYPE_SNAP_METADATA = 1; APFS_TYPE_SNAP_NAME = 11 | 84 |
| 11 | inum = ROOT_DIR_INO_NUM (2) | (empirical) |
| 11c | om_snapshot_tree must contain N records matching om_snap_count | (empirical) |

---

## Current state — what fsck reports

After ALL above fixes, on a fresh `hdiutil`-created 64 MB scratch
image with our writer creating one snapshot:

```
** Checking the snapshot metadata tree.
** Checking the snapshot metadata.
** Checking the fsroot tree.
** Checking the extent ref tree.
** Verifying volume object map space.
   (everything before passes)

error: btn: oid (204), xid (3), type (0x40000002), subtype (0x13),
       flags (0x7) level (0)
error: btn: invalid btn_table_space (0, N), given btn_flags (0x7)
   Snapshot is invalid.
```

The `(0, N)` is `(btn_table_space.off, btn_table_space.len)`. fsck
rejects every N value we've tried:

| N attempted | Reasoning | fsck verdict |
|---|---|---|
| 4 | One TOC entry (kvoff_t) | invalid |
| 8 | 8-aligned minimum (round5 alignment rule) | invalid |
| 448 | Same as kernel's container omap btree reserve | invalid |
| 568 | max_records(142) × toc_entry_size(4) | invalid |

---

## Our om_snapshot_tree block (verified byte-by-byte)

64 MiB scratch image (no records initially), then create one snapshot:

```
obj_phys.o_oid     = paddr (current value, 0xCC or similar)
obj_phys.o_xid     = 3 (= snap_xid)
obj_phys.o_type    = 0x40000002 (OBJ_PHYSICAL | OBJECT_TYPE_BTREE)
obj_phys.o_subtype = 0x00000013 (OBJECT_TYPE_OMAP_SNAPSHOT, p.15)

btree_node_phys header (bytes 32..56):
  btn_flags         = 0x0007 (ROOT|LEAF|FIXED_KV_SIZE)
  btn_level         = 0
  btn_nkeys         = 1
  btn_table_space   = {0, ???}   ← THE UNKNOWN
  btn_free_space    = {key_bytes, lowest_v - key_end}
  btn_key_free_list = {0xFFFF, 0}
  btn_val_free_list = {0xFFFF, 0}

TOC[0] (kvoff_t @ DATA_BASE=56):
  key_off = 0  (rel to key_area_start)
  val_off = 16 (rel from val_area_end backward)

Key (at DATA_BASE + table_space.len + 0):
  xid_t snap_xid LE = 3

Value (at val_area_end - 16 = 4040):
  omap_snapshot_t = { oms_flags:u32=0, oms_pad:u32=0, oms_oid:u64=0 }

btree_info_t footer (bytes 4056..4096, 40 bytes):
  bt_flags       = 0x00000052 (PHYSICAL|KV_NONALIGNED|SEQUENTIAL_INSERT)
  bt_node_size   = 4096
  bt_key_size    = 8  (sizeof xid_t)
  bt_val_size    = 16 (sizeof omap_snapshot_t)
  bt_longest_key = 8
  bt_longest_val = 16
  bt_key_count   = 1 (u64)
  bt_node_count  = 1 (u64)
```

fsck accepted bt_flags=0x52 (it moved past that check to
table_space). Earlier values (0x40, 0x42) were rejected with the
`mt_btflags` complaint.

---

## QUESTION 1 (Q-EE) — btn_table_space.len exact expected value

What value does fsck expect for `btn_table_space.len` on an
**OBJECT_TYPE_OMAP_SNAPSHOT** btree root node with:
- btn_flags = 0x07 (ROOT|LEAF|FIXED_KV_SIZE)
- bt_key_size = 8, bt_val_size = 16
- btn_nkeys = 1
- bsz = 4096

Options:

(a) **Tree-subtype-specific constant**. fsck has a hardcoded value
    per OBJECT_TYPE_*. If so, what is the exact constant for
    OBJECT_TYPE_OMAP_SNAPSHOT?

(b) **Formula derived from bt_key_size + bt_val_size + bsz**. The
    formula we tried — `floor((bsz - DATA_BASE - BTREE_INFO_SIZE) /
    (k + v + toc_entry)) * toc_entry` — gives 568, rejected. Give the
    CORRECT formula with all rounding/alignment rules.

(c) **Different per-tree-flavour**. Maybe `OMAP` and `OMAP_SNAPSHOT`
    use different constants. Container `OMAP` uses 448; what does
    `OMAP_SNAPSHOT` use?

Please cite Apple PDF page or libfsapfs section.

---

## QUESTION 2 (Q-FF) — bt_flags should include any opt-out flag?

Apple PDF p.131 flags include:
- BTREE_NONPERSISTENT (0x20)
- BTREE_NOHEADER (0x100)

(a) Should om_snapshot_tree's bt_flags include either of these (or
    others not in 0x52) to opt out of the table_space.len validation?

(b) Or is 0x52 (PHYSICAL|KV_NONALIGNED|SEQUENTIAL_INSERT) the wrong
    set entirely for this tree? Maybe should be 0x10 (PHYSICAL only)
    or 0x11 (PHYSICAL|UINT64_KEYS) since keys are xid_t u64?

---

## QUESTION 3 (Q-GG) — fsck's `mt_btflags` term

The earlier fsck error said `given mt_btflags`. What is mt_btflags?

(a) A volume-level "metadata btree flags" stored in apfs_superblock?
(b) A per-object-type kernel constant fsck cross-checks against?
(c) The bt_flags field within btree_info_t (which we set to 0x52)?
(d) Something else?

Understanding `mt_btflags` unlocks the table_space.len mystery
because fsck's rejection depends on it.

---

## QUESTION 4 (Q-HH) — omap_snapshot_t struct verify

Round9 said `omap_snapshot_t` is 16 bytes (PDF p.47):
```c
struct omap_snapshot {
    uint32_t oms_flags;
    uint32_t oms_pad;
    oid_t    oms_oid;
};
```

(a) Is `oms_oid = 0` truly correct for first snapshot creation, or
    should it be the snap_xid value (= 3 here)? PDF said
    "Populate this field with zero when you create a new snapshot,
    and preserve its value when you modify an existing snapshot" —
    confirm this still applies.

(b) Is `oms_flags = 0` correct? Or are there mandatory bits to set?

(c) Does the snap-tree's val ENCODING (val_off, val_len in TOC) need
    val_len = 16 exactly, or should we pad to a larger size?

---

## QUESTION 5 (Q-II) — alternative: skip om_snapshot_tree entirely?

When we set `om_snapshot_tree_oid = 0` in the volume omap_phys
(disabling our snapshot-tree provisioning), fsck reports:
```
error: om: om_snap_count (1) is not equal to the number of snapshots
       found (0)
```

(a) Is there ANY way to legally have `om_snap_count = 1` without a
    populated om_snapshot_tree? (e.g. opt-out flag in omap_phys.om_flags?)

(b) Or is the only path to provide a valid om_snapshot_tree (which is
    Q-EE territory)?

---

## How to use this consolidated file

1. Paste this ENTIRE file into NotebookLM (apfs-reference notebook).
2. Ask all 5 questions in one conversation if possible, otherwise
   sequentially.
3. Capture answers verbatim with Apple PDF page citations.
4. Save into `2026-05-21-m7a-write-spec-notes-round12.md`.
5. Ping me with the answers.

The unblocker is most likely Q-EE (exact table_space.len value). If
that's a hardcoded fsck constant tied to OBJECT_TYPE_OMAP_SNAPSHOT,
this is a 1-line fix and M7a-RT finishes.

## What rounds 1-11c locked (will remain after round12)

All previous answers are **kept** — they survived multi-stage testing
and Apple-PDF cross-validation. Round12 only seeks the final ONE
unknown: how does fsck compute (or hardcode) the expected
`btn_table_space.len` for an empty/single-entry OMAP_SNAPSHOT btree?
