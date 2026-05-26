# M7a-RT round14 — 4 cascading fsck errors after om_snapshot_tree crack

Round13 (toc_alloc=576) cracked om_snapshot_tree validation. fsck no
longer reports "Snapshot is invalid" — now reveals 4 errors in OTHER
trees/structures that were hidden behind the snap-tree rejection.

This file asks NotebookLM to identify whether these 4 errors share a
**root cause** (we're misunderstanding something fundamental) rather
than chasing each individually.

## fsck output (verbatim, macOS 15.7.4 fsck_apfs -n)

After scratch create_snapshot + delete_snapshot cycle:

```
error: btn: oid (194), xid (4), type (0x40000002), subtype (0xb),
       flags (0x7) level (0)
error: btn: invalid btn_key_free_list offset 0
warning: checkpoint 7 (xid 4) failed consistency check

error: btn: oid (206), xid (3), type (0x40000002), subtype (0xb),
       flags (0x7) level (0)
error: btn: invalid btn_key_free_list offset 0
warning: checkpoint 5 (xid 3) failed consistency check

warning: spaceman main free count 16166 does not match sum of free
         counts 16167

error: (oid 0xc2) om: bt: invalid o_xid (0x4)
   Object map is invalid.

error: sm: sm_free_count (16166) is not valid (16167) (sm_dev 0)
Fix sm_free_count (16166)? NO
```

om_snapshot_tree (subtype 0x13) is NOT in this list — fully accepted ✓.

## Q-NN — are these 4 errors a single root cause?

The 4 distinct error categories all surfaced after the snap-tree crack:

1. **btn_key_free_list offset 0** on TWO container OMAP btree blocks
   (subtype 0xb, paddrs 194 & 206) — we already fixed this in commit
   c4268af for `apply_omap_entries`, but new blocks show the same bug
2. **om btree o_xid invalid (0x4)** at oid 0xc2 (kernel-formatted
   volume omap btree's original paddr) — xid 0x4 = del_xid
3. **spaceman free_count off-by-one** (16166 vs expected 16167)
4. **checkpoint consistency check failed** at xid=3 and xid=4

Hypothesis: these are **all symptoms of one fundamental error** —
perhaps we're writing some shared field (like xid in obj_phys) at the
wrong moment, OR we're allocating a block that should not be touched
(reserved by spaceman bookkeeping), OR we're not freeing blocks
correctly during delete_snapshot.

Please analyse the error PATTERN as a whole. What single mistake
explains all 4 errors together?

## Q-OO — `(oid 0xc2) om: bt: invalid o_xid (0x4)` interpretation

oid 0xc2 is the kernel-formatted volume omap btree's ORIGINAL paddr
(set by hdiutil/newfs_apfs on scratch creation, xid=2). Our writer
COWs the volume omap_phys header to a new paddr but DOES NOT modify
the underlying btree at 0xc2. However, fsck now reports that 0xc2's
o_xid is 0x4 (= our del_xid).

How can a block we never touched have its xid bumped to 4?

(a) Is some other path overwriting paddr 0xc2 with a new btree node?
(b) Or is fsck's error message misleading — perhaps 0xc2 IS our
    new-COW'd block and fsck only reports an OID after-the-fact?
(c) Or did our spaceman free-block call mark 0xc2 as free, allowing
    it to be re-allocated and overwritten?

## Q-PP — spaceman free_count off-by-one

`sm_free_count (16166)` vs `expected 16167`. The off-by-one suggests
we're either:

(a) Decrementing free_count once too many times during allocation
(b) Not incrementing free_count once during a free operation
(c) Off-by-one in our chunk-info-block bitmap maintenance

What is the canonical Apple `sm_free_count` invariant? Should it
exactly equal `sum over all CIBs of ci_free_count`? Or is there
slack/reserve we're missing?

## Q-QQ — checkpoint consistency check failure

`warning: checkpoint 7 (xid 4) failed consistency check`. What
specific invariants does fsck verify for a checkpoint?

(a) That xp_desc_blocks and xp_data_blocks counts cover the actual
    ring usage?
(b) That all blocks referenced from the checkpoint are reachable
    from the NX superblock?
(c) That the spaceman in this checkpoint accounts for all extents?

Knowing which check failed lets us target the right fix.

## What we know is working (rounds 1-13 LOCKED)

- snap-meta tree (write fixed, fsck-clean past inum)
- container omap free_list sentinel (c4268af — for apply_omap_entries)
- omap_phys header COW (preserves om_tree_oid, updates snap counts)
- om_snapshot_tree (round13: variable-kv kvloc_t TOC + UINT64_KEYS
  + table_space=576 reserve)

## Recommendation

Could NotebookLM:
1. Dump a Time-Machine-snapshotted real APFS volume and compare
   ALL btree nodes that touched during a snapshot transaction?
2. Identify the SHARED root cause of these 4 errors?
3. Provide the exact sm_free_count maintenance rules?

This is likely 1-3 root causes (not 4 independent fixes). After this
round M7a-RT should be complete.
