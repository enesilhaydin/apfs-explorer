# M7a-RT round4 spec notes (LOCKED — NotebookLM 2026-05-20 follow-up)

These answers are DEFINITIVE for the COW-insert dynamic algorithm.
Combined with round3 (static layout), they unblock the M7a-RT writer.

## Q-E LOCKED — Preserve existing `btn_table_space.len` (NEVER compact)

Apple PDF: "When the `BTNODE_FIXED_KV_SIZE` flag is set, Apple's
implementation allocates enough space for the table of contents to
avoid the need to expand it later. This is possible because the
maximum number of entries is known, as well as the size of an entry."

**Implication for writer**: when COWing a fixed-kv leaf and inserting
a new TOC entry, the new node MUST preserve the existing
`btn_table_space.len` (whatever the kernel-formatted volume reserved
upfront). DO NOT compact to `nkeys * 4` — the kernel deliberately
over-allocates so subsequent inserts don't shift the key area.

This is exactly why the round3 Q-C6 implementation regressed scratch:
the kernel-written omap had reserved TOC capacity, and our compacted
emit broke the layout invariant the kernel relies on.

## Q-F LOCKED — Layout: preserve existing with free-list, compact only when needed

Apple PDF: "Free space within the key area and within the value area
is organized using a free list. For example, free space appears
outside the shared free space when an entry is removed from a B-tree."

**Implication for writer**: COW writes should preserve the existing
layout (existing TOC capacity + existing key/value placement). Only
compact when forced (no contiguous shared free space available and
free-list entries exhausted).

## Q-G LOCKED — `btn_free_space.off` semantic anchor

Apple PDF: "The location's offset is counted from the beginning of
the key area to the beginning of the free space."

`btn_free_space.off` = byte offset FROM `key_area_start` TO the
beginning of the shared free space (i.e. to the byte immediately AFTER
the last packed key when there is no fragmentation).

For a compacted N-key leaf with KEY_SIZE = 16:
`btn_free_space.off = N * 16` (correct).

For a fragmented leaf where keys occupy non-contiguous slots in the
key area, `btn_free_space.off` is still "byte offset to where shared
free space begins" — i.e. the first byte of the actual unallocated
gap, not a sum of key sizes.

## Q-H LOCKED — CIB COW + CAB COW propagation

- (a) Only CIBs whose bitmap or `ci_free_count` actually mutated this
  txn are COW'd to a new paddr.
- (b) The 5 allocations for a snapshot-create transaction may touch 1
  or N CIBs depending on the free-block layout at txn start; cannot
  assume single-CIB.
- (c) When a CIB is COW'd, the **CAB** (Chunk Info Address Block,
  `OBJECT_TYPE_SPACEMAN_CAB`) that points to it ALSO must be COW'd to
  a new paddr; its `cab_cib_addr[]` array updates the changed CIB
  paddrs. The ephemeral `spaceman_device_t.sm_addr_offset` then points
  to the newly allocated CAB. There is NO in-place patching of the
  paddr array.

This means the COW chain for one snapshot-create is:
```
new_nx_superblock
  → new_xp_desc + new_xp_data
  → new_spaceman (ephemeral, in-memory until commit)
       → new_CAB
            → new_CIB(s)
                 → new_bitmap_block(s)
  → new_container_omap_phys
       → new_container_omap_btree_node
  → new_VSB (virtual; reachable via container omap)
       → new_volume_omap_phys
            → new_volume_omap_btree_node
       → new_snap_meta_tree_node
       → new_extentref_tree_root (empty, for live volume)
  → new_frozen_VSB_physical (snapshot's frozen sblock_oid)
```

## Q-I LOCKED — Overallocation root cause

Apple PDF policy: "modified copies of an object are always written to
a new location on disk." Overallocation = bitmap bit set but no active
extent reaches the bit from the latest checkpoint.

Root causes:
- Spaceman bitmap mutated in place to allocate space, but the chain
  from `nx_superblock` down to those new blocks did NOT complete.
- A NEW B-tree root paddr (e.g. new omap btree, new extentref tree)
  was allocated but not linked into the VSB (`apfs_omap_oid`,
  `apfs_extentref_tree_oid`, `apfs_root_tree_oid`).
- The checkpoint descriptor ring (`nx_xp_desc_base`) didn't advance to
  point to the new nx_superblock that references the new VSB.

**For our M7a-RT writer**: every allocation we make MUST be either
- linked into the new checkpoint chain (referenced by some active
  on-disk object reachable from the new `nx_superblock`), OR
- released via the spaceman free queue (`sm_fq[APFS_SFQ_MAIN]`) so
  the kernel knows the bit is free in the new checkpoint.

If either rule is violated, the bitmap bit becomes orphaned ⇒
overallocation.

## What changes in apply_omap_entries (the immediate fix)

Per Q-E and Q-G, the correct emit:
- `btn_table_space.off = 0`
- `btn_table_space.len = max(existing_toc_space_len, nkeys * 4)`
  (PRESERVE existing capacity; only grow if we exceed it)
- `btn_free_space.off = nkeys * KEY_SIZE` (= N * 16 for omap)
  (per Q-G: offset to beginning of shared free space from
  key_area_start)
- `btn_free_space.len = lowest_v_abs - (key_area_start_new + nkeys *
  KEY_SIZE)`
- `btn_key_free_list = {0xFFFF, 0}` and `btn_val_free_list = {0xFFFF,
  0}` (no fragmentation entries to chain)

`key_area_start_new = DATA_BASE + btn_table_space.len`. With the
preserve-existing-capacity rule, key_area_start_new MATCHES the
position the kernel-formatted volume already uses.

## Open follow-up (for round5 if needed)

The CIB-COW + CAB-COW + sm_fq-orphan-release pipeline is documented
above at high level. The byte-level fields (CAB layout, sm_fq btree
insertion, ip-bitmap rotation timing) are still mostly unimplemented
and will need their own careful round of scratch + real-USB testing
once the omap insert lands cleanly.
