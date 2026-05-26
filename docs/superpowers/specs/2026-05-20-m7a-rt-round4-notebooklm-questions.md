# M7a-RT round4 NotebookLM follow-up (md.6 LOCKED)

Round3 (Q-C/Q-D) answered the STATIC layout (compacted N-key state +
spaceman_phys offsets). Round4 needs the DYNAMIC algorithm — how the
kernel actually rebuilds a populated B-tree leaf during a COW
insertion. Initial Q-C6 implementation regressed scratch because it
emitted a fully-compacted layout when the existing kernel-written
node had reserved TOC capacity (`btn_table_space.len` larger than
`nkeys * sizeof(toc_entry)`), and the kernel's actual COW pattern is
different.

## Q-E (omap insert: reserved TOC capacity)

On a populated container OMAP B-tree leaf (FIXED_KV, BTNODE_ROOT,
BTNODE_LEAF) where the kernel-written existing node has reserved more
TOC capacity than entries actually occupy — e.g.
`btn_table_space = {off: 0, len: 256}` with `btn_nkeys = 5` (so only
the first 5 × 4 = 20 bytes of TOC space are used) — when our writer
COW's that node and inserts a 6th entry:

(a) Should we PRESERVE the existing `btn_table_space.len = 256` and
    just write the new TOC entry into TOC[5] (= byte 76 within the
    node, at `DATA_BASE + 5*4`), leaving the reserved capacity intact?
(b) Should we COMPACT to `btn_table_space.len = 6 * 4 = 24`, losing the
    reserved capacity and shifting key_area_start accordingly?
(c) Some hybrid (e.g. compact only if there are stale free-list slots
    indicating prior deletions)?
(d) Does the kernel write `btn_table_space.len` as a specific
    function of `btn_nkeys` (e.g. next power-of-2, next multiple of
    16, fixed growth schedule)?

Please cite the Apple PDF page(s) or libfsapfs section(s) that
describe the kernel's actual behaviour. linux-apfs-rw `btree.c`
insert / `transaction.c` write paths should also document this.

## Q-F (COW write: layout preservation vs compaction)

When the writer rebuilds an existing fixed-kv leaf into a NEW physical
block as part of a COW transaction (e.g. an omap insert), the new
block's layout SHOULD be:

(a) Byte-for-byte identical to the old block except for the new
    entry's TOC slot, key, value (preserve everything else, including
    padding, reserved TOC capacity, key/value area placement)?
(b) Fully compacted per Q-C6 formulas regardless of the old block's
    layout?
(c) Compacted only when the old block has detectable fragmentation
    (free-list off != BTOFF_INVALID)?
(d) Something else — please describe.

## Q-G (free_space anchor double-check on compacted state)

Round3 Q-C6 said `btn_free_space.off = N * 16` measured from start of
key area. Cross-checking with a kernel-written empty (single-entry)
omap leaf would help confirm this is right under the kernel's
interpretation. Specifically:

Is `btn_free_space.off` an offset FROM where the keys start, OR FROM
where the next key WOULD be written (i.e. AFTER the last key, into
the free area)? These are arithmetically identical for a fully
compacted node but differ semantically if there is fragmentation.

Apple PDF cites would close this loop.

## Q-H (CIB COW invocation per snapshot transaction)

For our specific snapshot-create transaction (5 new blocks allocated:
live-VSB virtual, frozen-VSB physical, snap-meta tree node,
new-extentref-tree root, new-omap-btree-root):

(a) Which CIBs are guaranteed to be touched (i.e. need COW) per
    Q-D2's "only when bitmap mutated" rule?
(b) Are all 5 allocations from the same CIB (likely if they're in
    the same chunk-address range), or do they typically span multiple
    CIBs?
(c) If the spaceman's main pool has, say, 32 CIBs total, must we
    rewrite the full `sm_dev[MAIN].sm_addr_offset` paddr array (only
    a few entries change) or just patch the affected entries?

## Q-I (overallocation root cause)

fsck reports `overallocation (0x48b4+21) bitmap address (0x45d7)` —
this means the bitmap at 0x45d7 has 21 contiguous bits set starting at
0x48b4 that are not referenced by any active extent in the latest
checkpoint. The count grows by 7 every snapshot create run (each
allocation we do is leaving a bit set without a referencing extent).

(a) Is this expected behaviour when the spaceman is mutated in place
    (CIB-COW absent), or does it have a more specific cause?
(b) Is there a particular field whose absence causes the kernel to
    fail to count the new allocations toward `om_tree`, `vsb_extents`,
    or similar reachability roots?

## How the user should use this

1. Open NotebookLM with the apfs-reference notebook.
2. Ask each Q-E .. Q-I question with the context from this file.
3. Capture answers verbatim with Apple PDF page citations.
4. Save the answers into `2026-05-20-m7a-write-spec-notes-round4.md`.
5. Then ping me; I will implement carefully (one fix → scratch
   regression → real-USB → next fix), preserving the proven scratch
   baseline at every step.
