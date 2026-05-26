# M7a-RT round6 NotebookLM — the byte-identical-but-fsck-rejected anomaly

This question is the **most concrete** we've had: a full byte-by-byte
side-by-side hex diff of our omap btree node vs the kernel-formatted
omap btree node. They are **byte-identical except for the legitimate
COW-induced changes** (cksum, oid, xid, entry's xid, vsb_paddr), yet
`fsck_apfs -n` accepts the kernel's emission and rejects ours with
"Snapshot metadata tree is invalid".

## Setup

64 MiB scratch APFS-in-GPT image, freshly formatted by `hdiutil`
(macOS 15.7.4 `newfs_apfs 2332.140.13`). One volume APFSWRITE with
no snapshots. Our writer creates a single snapshot via the M7a code
path (`apfs-write::snapshot::create_snapshot`).

## Container OMAP B-tree node — verified header byte-identical

The kernel formats the initial 1-entry container omap btree leaf at
xid=2 with EXACTLY these bytes (Python decode of the on-disk block):

```
@32..56 (btree_node_phys header) hex:
  07000000 010000000000c001 1000c00d ffff0000 ffff0000
```

Parsed:
```
btn_flags        = 0x0007  (ROOT | LEAF | FIXED_KV_SIZE)
btn_level        = 0x0000
btn_nkeys        = 0x00000001
btn_table_space  = {off: 0, len: 448}
btn_free_space   = {off: 16, len: 3520}
btn_key_free_list = {off: 0xFFFF, len: 0}
btn_val_free_list = {off: 0xFFFF, len: 0}
```

After our writer COWs this node for the snapshot transaction at
xid=3, our emission (verified via Python decode of the on-disk
block) has IDENTICAL header bytes @32..56:

```
@32..56 (ours): 07000000 010000000000c001 1000c00d ffff0000 ffff0000
```

Byte-by-byte diff of the entire 4096-byte block between kernel-format
and our-emission only reveals the legitimate COW-induced changes:
- `@0..8`   obj_phys.o_cksum (new Fletcher because xid changed)
- `@8..16`  obj_phys.o_oid (new paddr after COW)
- `@16`     obj_phys.o_xid LSB (new xid)
- `@44..48` btn_free_space — **our value matches kernel's exactly**
- `@512`    one byte in TOC[0]'s key.xid (new xid)
- `@4048`   one byte in TOC[0]'s value.paddr (new vsb_paddr)

Every Fletcher-64 checksum verified MATCHING:
- new container omap btree node: ✓
- new container omap_phys: ✓
- new VSB: ✓
- new snap-meta tree node: ✓
- new NX superblock: ✓

## The anomaly

When our writer leaves `btn_free_space @44..48` as zero (the
uninitialised state from `vec![0u8; bsz]`), `fsck_apfs -n` ACCEPTS
the result and reports the container CLEAN. The kernel-equivalent
value `{16, 3520}` is also presumably valid (kernel itself wrote
it on a clean format).

When our writer writes `btn_free_space @44..48 = {16, 3520}` (= the
exact same values the kernel writes), `fsck_apfs -n` REJECTS the
result with "Snapshot metadata tree is invalid" (chained from a VSB
virtual lookup through the container omap to the snap-meta tree).

This is highly counterintuitive: writing the kernel's exact value
should not break what zero-bytes pass. Possibilities we cannot
rule out without the spec:

(a) fsck reads `btn_free_space` and uses it for a validation that's
    skipped when `{0, 0}` (treated as "no free-space tracked").
    Once `btn_free_space` is non-zero, the validation activates and
    checks an invariant we are violating elsewhere (perhaps the
    key/value packing density, the value placement direction, or
    the relationship between TOC entries and free-space gap).

(b) The kernel's btn_free_space encoding might NOT be `{16, 3520}`
    even though our Python decode reads those bytes — perhaps the
    actual semantic is different (e.g., a packed nloc_t with
    different field order, or a 32-bit value not two 16-bit fields).
    The Apple PDF page 122 nloc_t struct says `{off: u16, len: u16}`
    but maybe FIXED_KV_SIZE nodes use a different encoding?

(c) There's a kernel-only invariant linking some other field to
    `btn_free_space`: e.g. `bt_longest_key` in btree_info_t footer
    should be `KEY_SIZE` (= 16) when free-space is non-zero. Or
    `bt_longest_val` should reflect actual. Or both.

## Apple PDF citations needed

1. The exact byte semantic of `btn_free_space.off` and
   `btn_free_space.len` for a FIXED_KV_SIZE leaf — confirming or
   contradicting our reading.

2. Whether writing `btn_free_space` triggers additional fsck
   validation that `{0, 0}` skips.

3. Whether there are interdependent fields in `btree_info_t` footer
   (bt_longest_key, bt_longest_val, bt_node_size) that must match
   when `btn_free_space` is non-zero.

4. Any other invariant the kernel checks that involves
   `btn_free_space` together with other btree node fields.

## Concrete request

Given the above byte-by-byte equivalence between our writer's
emission and the kernel's emission, please identify which Apple
PDF section explains why fsck rejects our `{16, 3520}` write while
accepting both `{0, 0}` and the kernel's `{16, 3520}`. The answer
should let us write `btn_free_space` correctly without regressing
scratch.

The hex diff and fletcher verification details above are reproducible
from `tools/decode_macos_template.py` plus a fresh
`xtask gen-scratch` invocation on macOS 15.7.4 with apfs-write at
commit `356aa59`.
