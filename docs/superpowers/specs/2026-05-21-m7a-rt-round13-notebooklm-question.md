# M7a-RT round13 — fsck's expected btn_table_space.len for OMAP_SNAPSHOT (correction)

Round12 NotebookLM said "drop FIXED_KV_SIZE" but empirical fsck on
macOS 15.7.4 disagrees:

- With `btn_flags=0x03` (ROOT|LEAF, no FIXED_KV): fsck rejects with
  `invalid btn_flags (0x3), given bt_key_size (8) val_size (16)`.
- With `btn_flags=0x07` (ROOT|LEAF|FIXED_KV_SIZE) restored: fsck
  accepts the flags and moves to `btn_table_space.len` check.

So FIXED_KV_SIZE IS required when bt_key_size/val_size are non-zero.
Only the `btn_table_space.len` value remains unresolved.

## Current locked state (everything except table_space.len)

```
obj_phys.o_type    = 0x40000002 (OBJ_PHYSICAL | OBJECT_TYPE_BTREE)
obj_phys.o_subtype = 0x00000013 (OBJECT_TYPE_OMAP_SNAPSHOT)

btn_flags         = 0x0007 (ROOT|LEAF|FIXED_KV_SIZE)
btn_nkeys         = 1
btn_table_space   = {0, ???}
btn_free_space    = {KEY_SIZE, lowest_v_abs - key_end}
btn_key_free_list = {0xFFFF, 0}
btn_val_free_list = {0xFFFF, 0}

TOC[0] kvoff_t @56 = {key_off=0, val_off=16}
Key @ DATA_BASE+table_space.len: xid_t snap_xid (=3) u64 LE
Value @ val_area_end-16: omap_snapshot_t {oms_flags=0, oms_pad=0, oms_oid=0}

btree_info_t footer (4056..4096):
  bt_flags       = 0x00000011 (BTREE_PHYSICAL | BTREE_UINT64_KEYS)
  bt_node_size   = 4096
  bt_key_size    = 8
  bt_val_size    = 16
  bt_longest_key = 8
  bt_longest_val = 16
  bt_key_count   = 1 (u64)
  bt_node_count  = 1 (u64)
```

Values tried for btn_table_space.len (all rejected):
- 4 (1 × TOC_ENTRY_SIZE)
- 8 (8-byte alignment minimum)
- 256 (common reserve)
- 448 (container omap btree's value)
- 568 (max_records 142 × 4)

fsck consistently reports:
```
error: btn: invalid btn_table_space (0, N), given btn_flags (0x7)
```

## Q-JJ — what is the EXACT btn_table_space.len value fsck expects?

For an OBJECT_TYPE_OMAP_SNAPSHOT btree root with the layout above:
- bsz = 4096
- bt_key_size = 8, bt_val_size = 16
- btn_nkeys = 1
- btn_flags = 0x07 (FIXED_KV_SIZE)

What value of `btn_table_space.len` does fsck accept?

(a) Is there a hardcoded constant for OBJECT_TYPE_OMAP_SNAPSHOT?
(b) Or a formula different from `floor(usable / (k+v+toc)) * toc`?
(c) Does it depend on `bt_longest_key` / `bt_longest_val` values?

## Q-KK — could the right answer be writing fewer header fields?

E.g., set bt_key_size=0 (variable schema) AND btn_flags=0x03 AND
table_space=8? That triggered the contradiction error in round12
attempt. Is there a SPECIFIC combination that fsck accepts?

## Q-LL — what about bt_flags in btree_info?

We use 0x11 (PHYSICAL | UINT64_KEYS). Should it also include:
- BTREE_ALLOW_GHOSTS (0x04)?
- BTREE_KV_NONALIGNED (0x40)?

Maybe a specific bt_flags combination is what unlocks the
table_space.len check.

## Q-MM — what if btn_table_space.len IS nkeys-dependent?

(a) For nkeys=1, expected = 4 (or 8 aligned)?
(b) For nkeys=2, expected = 8?
(c) For nkeys=N, expected = next_8_aligned(N*4)?

All these were tried but maybe with the wrong bt_flags combo. With
the CORRECT bt_flags set, does table_space.len = nkeys * toc_entry
(aligned) work?

## Recommendation

Could NotebookLM:
1. Look up a real-world macOS-format USB with a Time Machine snapshot
   and dump the om_snapshot_tree root bytes verbatim?
2. OR cite Apple's `apfsutil` source code if available?
3. OR provide a minimal Python script that builds a valid
   om_snapshot_tree root that fsck accepts?

This is the FINAL invariant. Once cracked, M7a-RT completes.

## Round12 lessons (what was correct vs wrong)

CORRECT (kept):
- BTREE_UINT64_KEYS = 0x01 flag in bt_flags (UINT64_KEYS required for
  xid_t keyed trees)
- OBJECT_TYPE_OMAP_SNAPSHOT subtype 0x13
- omap_snapshot_t = 16 bytes {flags, pad, oid=0}
- One record keyed by snap_xid required

INCORRECT (had to roll back):
- "Drop FIXED_KV_SIZE": fsck strictly enforces FIXED_KV_SIZE when
  bt_key_size/val_size are set. The two flags are coupled, not
  independent as round12 implied.

Future rounds should call out this coupling explicitly when giving
multi-field answers.
