# M7a-RT round15 NotebookLM — fsck error semantics + bitmap-less chunk freeing

After round14 (volume omap btree COW + alloc_block skip bitmap_addr=0
chunks), 2 errors remain. Both relate to spaceman bookkeeping in a
chunk that has `ci_bitmap_addr = 0` (no persistent bitmap allocated
yet). The 2 errors likely share ONE root cause.

## Current state

Block 0xc2 is the kernel-formatted volume omap btree (paddr 0xc2,
xid=2). Our snapshot transaction:
1. Reads block 0xc2 (still kernel-original)
2. Allocates new paddr (0xcd) — NOT in the same chunk as 0xc2
3. Copies bytes verbatim
4. Updates obj_phys: o_oid=0xcd, o_xid=3, Fletcher recomputed
5. Writes new block at 0xcd
6. Calls txn.free_block(0xc2) — increments ci_free_count + sm_free_count
7. New volume omap_phys.om_tree_oid = 0xcd

Block 0xc2 contents AFTER our transaction (verified via Python):
- o_oid=0xc2 o_xid=0x2 type=0x40000002 subtype=0x0B (UNCHANGED)
- btn_flags=0x07 nkeys=1 table_space=(0,448) free_space=(16,3520)
- Valid Fletcher
- New chain (xid=3) does NOT reference 0xc2

The chunk containing 0xc2 has `ci_bitmap_addr=0` (no persistent bitmap).

## fsck output (verbatim)

```
** Checking the object map.
error: (oid 0xc2) om: bt: invalid o_xid (0x4)
   Object map is invalid.
** The volume /dev/rdiskNs1 with UUID ... was found to be corrupt
** Verifying allocated space.
error: sm: sm_free_count (16166) is not valid (16167) (sm_dev 0)
Fix sm_free_count (16166)? NO
   Space Verification failed.
```

## Q-RR — fsck error format `(oid X) om: bt: invalid o_xid (Y)` semantics

What does the `(Y)` value mean exactly?

(a) Y is the EXPECTED o_xid that fsck wanted to find at block X
(b) Y is the ACTUAL o_xid found at block X
(c) Y is some context-flag (e.g. next_xid, latest checkpoint xid)
(d) Y is the OID of the parent reference that pointed to X

In our case: block 0xc2 actually has o_xid=2 (verified). fsck reports
`(0x4)` even though no block in the image has o_xid=4 (verified via
disk scan — delete_snapshot didn't run, only create at xid=3). What
is fsck telling us?

## Q-SS — bitmap_addr=0 chunk freeing semantics

When `ci_bitmap_addr == 0`, the kernel uses "implicit allocation"
(no persistent bitmap block exists yet). How does Apple's kernel
correctly free a block in such a chunk?

(a) Just decrement ci_free_count (what we do) — sufficient?
(b) Allocate a real bitmap block, populate it with current allocation
    pattern, then mark the freed block in that bitmap
(c) Some other mechanism (per-CIB lazy bitmap creation, etc)

Our current implementation just increments ci_free_count + sm_free_count
without modifying any bitmap (since no bitmap exists). fsck's free
count recomputation may walk the structure differently and arrive at
a different count.

## Q-TT — free vs unreachable: spaceman policy

After we COW the volume omap btree from 0xc2 to 0xcd, the old paddr
0xc2 is no longer reachable from the LATEST checkpoint (new VSB →
new omap_phys → 0xcd, not 0xc2). But the OLD checkpoint (xid=2) still
has paddr 0xc1 → 0xc2.

Should our spaceman free 0xc2 in this case?

(a) YES — APFS spaceman tracks LATEST-checkpoint reachability only;
    0xc2 should be freed because new checkpoint doesn't reach it
(b) NO — old checkpoint still references 0xc2; freeing it would lose
    historical data
(c) YES but use the spaceman free queue (sm_fq) for reaper-deferred
    free, not immediate free
(d) Apple's policy is implementation-defined; either approach works

If (c), we need to implement free queue insertion instead of immediate
ci_free_count++.

## Q-UU — obj_phys zeroing on free

Should we ZERO the obj_phys (o_oid, o_xid, type, subtype, Fletcher)
of a freed block? Or leave the stale data?

fsck may scan all blocks in the spaceman's free range and reject any
block with stale obj_phys headers (since a "free" block shouldn't
have valid obj_phys data). If so, on free we should zero or null out
the first 32 bytes of the block.

## Recommendation

Could NotebookLM:
1. Find Apple PDF section on spaceman free-queue vs bitmap mechanics?
2. Decode `fsck_apfs` error format for `(oid X) ... (Y)` parenthetical?
3. Provide minimal Apple-compliant `free_block` algorithm for the
   `ci_bitmap_addr=0` case?

Once these clarify, the final 2 fsck errors should resolve and scratch
becomes CLEAN. From there → real-USB reformat + final proof → M7a-RT
complete.

## Today's session ended at commit `61168e7` with 40 atomic commits

See `2026-05-22-m7a-rt-SESSION-HANDOFF.md` for full handoff details.
