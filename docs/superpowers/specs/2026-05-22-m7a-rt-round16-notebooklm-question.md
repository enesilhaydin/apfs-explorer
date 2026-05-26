# M7a-RT round16 NotebookLM — Extent Ref Tree validation after snapshot

After round15 (do not free old vol omap btree paddr post-COW), fsck
clears ALL previous errors: container SB, checkpoint, spaceman, free
queue trees, container omap, volume SB, volume omap, snap-meta tree,
snap-meta entry, snapshot 1 of 1, fsroot tree. ONE error remains:

```
** Checking the extent ref tree.
   Extent ref tree is invalid.
** The volume /dev/rdiskNs1 with UUID ... was found to be corrupt
   and cannot be repaired.
```

## What we do for the extentref tree (Apple PDF F.3 model)

On `create_snapshot(xid=3)` on a fresh scratch (no user data, no dirty
extents):

1. Read `vsb.apfs_extentref_tree_oid` (= OLD kernel-formatted extentref
   tree paddr; the kernel set this at format time, xid=2, type=BTREE+PHYS,
   subtype=BLOCKREFTREE=0x09).
2. Allocate `new_extentref_paddr` and build a brand-new EMPTY
   blockreftree root at it for the LIVE volume:
   - obj_phys: o_oid=new_paddr, o_xid=3, type=BTREE|PHYSICAL,
     subtype=BLOCKREFTREE
   - btn_flags=ROOT|LEAF|FIXED_KV (0x07) — same as snap-meta empty pattern
   - nkeys=0, free_space covers the whole node, table_space=(0, small)
   - btree_info_t footer: `bt_flags=0x50` (BTREE_PHYSICAL | BTREE_KV_NONALIGNED),
     key_size=0, val_size=0, node_size=bsz, longest_key=0, longest_val=0,
     key_count=0, node_count=1
   - Valid Fletcher
3. `snap_metadata_val.extentref_tree_oid` = OLD paddr (snapshot inherits
   kernel-formatted tree)
4. `snap_metadata_val.extentref_tree_type` = u32 read verbatim from
   `vsb @ 0x78` (the kernel's apfs_extentref_tree_type, typically
   `0x40000009` = BTREE+PHYSICAL with subtype BLOCKREFTREE)
5. `new_vsb.apfs_extentref_tree_oid` = new_paddr (live gets new empty)

OLD extentref paddr is NEVER modified by our writer (no COW; we leave
it as-is so fsck can reach the snapshot's kernel-formatted block).

## Q-VV — which extentref tree does fsck reject?

fsck says "Extent ref tree is invalid" without specifying paddr. There
are TWO extentref trees after our transaction:

(a) NEW empty extentref @ `new_extentref_paddr` (xid=3, our build)
(b) OLD kernel-formatted extentref @ `old_extentref_oid` (xid=2,
    untouched)

Which one is fsck walking when it errors?

- If (a): our `build_empty_btree_root` for BLOCKREFTREE may have
  wrong bt_flags / btn_flags / footer fields for this specific subtype.
- If (b): we may need to COW it to xid=3 (same as we discovered for the
  volume omap btree in round14 — fsck enforces obj_phys.o_xid consistency
  between the snap_metadata_val.extentref_tree_oid pointer and the
  block's o_xid).

## Q-WW — for snapshot's snap_metadata_val.extentref_tree_oid: COW or pin?

Volume omap btree (round15): we COW it (xid bumps) but DO NOT free old
paddr (snapshot pins it for historical reachability). However, the new
omap_phys points to the NEW paddr; the snapshot's omap_phys references
the OLD paddr.

For the extentref tree, snap_metadata_val.extentref_tree_oid points to
the OLD paddr. Should we:

(a) Leave OLD paddr untouched (current behavior) — fsck accepts xid=2
    on the snapshot's referenced extentref tree
(b) COW OLD extentref tree to new paddr at xid=3, write new paddr into
    snap_metadata_val.extentref_tree_oid — same pattern as volume omap
(c) Modify OLD extentref tree IN-PLACE to bump o_xid to 3 (no COW)
(d) snap_metadata_val.extentref_tree_oid should be 0 / new empty tree
    when no dirty extents exist at snapshot time

In Apple's kernel, when newfs_apfs formats a volume, the extentref tree
is empty. When tmutil takes the first snapshot on an empty volume, what
does snap_metadata_val.extentref_tree_oid actually point to? Is it:
- The kernel's original empty extentref paddr?
- A freshly-COW'd copy?
- 0 (NULL)?

## Q-XX — BLOCKREFTREE-specific node format

For empty BLOCKREFTREE root (subtype 0x09):

(1) Are bt_flags 0x50 (BTREE_PHYSICAL | BTREE_KV_NONALIGNED) correct?
(2) Or does extentref need BTREE_KV_NONALIGNED unset (= 0x10 only)?
(3) Does the kernel use FIXED_KV_SIZE (btn_flags=0x07) for the EMPTY
    extentref root, or variable-kv (btn_flags=0x03)?
(4) For variable-kv empty: key_size=0, val_size=0 (we set this).
    For FIXED_KV empty: kernel uses key_size=0x14 (sizeof
    j_phys_ext_key) and val_size=0x10 (sizeof j_phys_ext_val)?

Apple PDF p.105-107 has `j_phys_ext_key_t` (key) and `j_phys_ext_val_t`
(val) for BLOCKREFTREE entries. The key is 8 bytes (j_key_t with type
APFS_TYPE_EXTENT shifted into the OID hi-bits), value is 16 bytes.

When BLOCKREFTREE is empty (no extents), is its root FIXED_KV or
variable-kv?

## Q-YY — btree_info_t footer for empty BLOCKREFTREE

Our footer for the new empty extentref tree:
- bt_flags=0x50, node_size=4096, key_size=0, val_size=0
- longest_key=0, longest_val=0, key_count=0, node_count=1

Should:
- key_size = 0x14 and val_size = 0x10 (struct sizes per Apple PDF)?
- longest_key = 0x14 and longest_val = 0x10 (max possible)?
- node_count = 0 (no leaves below root) or 1 (root counts)?

For snap-meta empty tree we have key_size=0, val_size=0 working — but
that tree uses variable-kv (BTREE_KV_NONALIGNED). BLOCKREFTREE may
strictly require FIXED_KV with the type's natural sizes.

## Recommendation

Could NotebookLM:

1. Quote Apple PDF section on extentref tree structure (BLOCKREFTREE,
   chap 9 typically) — exact bt_flags, btn_flags, key/val sizes for an
   EMPTY root?
2. Identify whether `snap_metadata_val.extentref_tree_oid` should be:
   - The kernel's original empty paddr (pin)
   - A new COW'd copy at snap_xid
   - 0 (NULL — no historical extents)
3. Provide a known-good byte dump of an empty BLOCKREFTREE root from
   any reference (libfsapfs, apfs-fuse, or extracted from a Time Machine
   sparseimage)?

After this round the LAST fsck error should resolve → scratch CLEAN →
real-USB final proof → M7a-RT complete.

## Context

- Commit `e5584fd` on branch `feat/m7a-write-snapshot` (42 atomic commits)
- See `2026-05-22-m7a-rt-SESSION-HANDOFF.md` (now outdated; this is the
  new frontier)
- Round15 NotebookLM answer LOCKED: COW does not free; snapshot pins
  old paddr
