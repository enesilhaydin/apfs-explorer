# M7a-RT round5a NotebookLM — byte-level emission diagnosis (Q-J)

Round3 + Round4 LOCKED but our writer's emission still breaks fsck
("Snapshot metadata tree is invalid") whenever `btn_free_space` is
written to any non-zero value. Show NotebookLM the exact bytes we
write and ask which one is wrong.

## Context

Container OMAP B-tree leaf (single new VSB entry) after our COW
write on a freshly-formatted 64 MiB scratch image. Block size 4096.
Root + leaf, fixed-kv-size (BTNODE_FIXED_KV_SIZE flag set).

## Q-J: which byte(s) in this emission are wrong?

```
@0..32   obj_phys (Fletcher-64 + oid + xid + o_type + o_subtype)
         o_type    = OBJ_PHYSICAL | OBJECT_TYPE_BTREE
         o_subtype = OBJECT_TYPE_OMAP

@32      btn_flags        = 0x0007  (ROOT | LEAF | FIXED_KV_SIZE)
@34      btn_level        = 0x0000
@36      btn_nkeys        = 0x00000001
@40      btn_table_space  off=0   len=4        (= nkeys * 4, kvoff_t)
@44      btn_free_space   off=16  len=3964     (Q-G: N*16, then formula)
@48      btn_key_free_list off=0xFFFF len=0    (BTOFF_INVALID, empty)
@52      btn_val_free_list off=0xFFFF len=0

TOC (1 kvoff_t entry @56..60):
@56  key_off = 0
@58  val_off = 16

Key area (starts at DATA_BASE + btn_table_space.len = 60):
@60..68    omap_key.oid = <vsb_oid>
@68..76    omap_key.xid = <snap_xid>

Value area (val_area_end = 4096 - 40 = 4056):
@4040..4044   omap_val.ov_flags = 0
@4044..4048   omap_val.ov_size  = 0x1000
@4048..4056   omap_val.ov_paddr = <vsb_paddr>

btree_info_t @4056..4096 (40 bytes, preserved from existing node):
@4056..4060  bt_flags
@4060..4064  bt_node_size   = 0x1000
@4064..4068  bt_key_size    = 0x10
@4068..4072  bt_val_size    = 0x10
@4072..4076  bt_longest_key
@4076..4080  bt_longest_val
@4080..4088  bt_key_count   = 1
@4088..4096  bt_node_count  = 1
```

After this write, `fsck_apfs -n` reports "Snapshot metadata tree is
invalid" — meaning fsck reaches the snap-meta tree via the VSB
virtual lookup through this omap, fails the lookup somehow, and
parses garbage as the snap-meta root.

Assuming we wrote bytes EXACTLY as shown above, which field/byte is
incorrect?

(a) Is `btn_free_space.off = 16` correct for a compacted single-entry
    fixed-kv root leaf, or should it be a different value? Cite the
    exact Apple PDF page that defines this.

(b) Is `btn_free_space.len = 3964` correct? If not, give the exact
    formula (using DATA_BASE, btn_table_space.len, key_area_start,
    val_area_end, N, KEY_SIZE, VAL_SIZE).

(c) Is placing the value at offset 4040..4056 correct (v_abs =
    val_area_end - val_off = 4056 - 16 = 4040)? Or should the value
    be at a different absolute offset?

(d) Is the val_off encoding (TOC val_off = 16 ⇒ v_abs = val_area_end
    - val_off) correct, or should it be a different formula?

## How to use

Open NotebookLM with the apfs-reference notebook. Paste this single
file. Capture answers verbatim with Apple PDF page citations. Save
to `2026-05-20-m7a-write-spec-notes-round5a.md`. Then ping me with
the answers.

The companion file `2026-05-20-m7a-rt-round5b-notebooklm-questions.md`
contains Q-K and Q-L (capacity edge case + minimum legal encoding) —
ask those separately.
