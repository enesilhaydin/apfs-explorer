# M7a-RT — COW-replaced block reclamation: NotebookLM questions (English)

> Written 2026-05-22. ROOT CAUSE FOUND empirically; this asks for the CANONICAL
> reclamation mechanism so the fix is permanent (not a special case). Paste
> verbatim into NotebookLM; lock answers to KB `apfs-spec:cow-block-reclaim`.
> md.6: NotebookLM/Apple PDF are hypotheses; the real macOS disk is the arbiter.

## Root cause (empirically proven on real USB + scratch)

Every catalog / volume-omap / extentref / volume-superblock (VSB) mutation
copy-on-writes to FRESH blocks. Our writer NEVER freed the blocks it replaced.
The old blocks stay marked allocated in the per-chunk allocation bitmap but are
unreferenced by the live volume, so `fsck_apfs` reports container
"overallocation detected on Main device" and `apfs_fs_alloc_count is not valid`.

This was hidden the whole M7 series because our test oracle (`xtask
verify-scratch`) treated fsck warnings as CLEAN (fsck returns exit 0 + prints
"appears to be OK" even with overallocation). Oracle now fixed → the bug is
visible on both a fresh 32 GB USB AND scratch images.

Decoded proof (64 MiB scratch): baseline VSB xid2 had omap_phys @193, extref
@195; after one write_file (xid3) the new structures are at 202-205 and fsck
flags exactly blocks 193-200 (the old omap/extref/catalog/VSB) as overallocated.

Partial fix applied: freeing the VOLUME-level replaced blocks (old omap header,
old omap b-tree node, old catalog node(s), old extentref node) at commit when no
snapshot exists shrank the leak from ~7 blocks to 3. The remaining 3 are
CONTAINER-level: the old VSB block (a virtual object in the container omap) and
the old container-omap b-tree node + container `omap_phys` header, which the
container omap COWs every transaction in `commit()`.

## Questions

1. **When may a COW-replaced block be freed?** The previous checkpoint(s) in the
   `nx_xp_desc` ring still reference the old block. How many checkpoints does
   APFS retain in the descriptor ring, and is a block freeable only once NO
   checkpoint in the ring references it?

2. **Immediate free vs free queue — what does fsck require?** Is the canonical
   reclamation strictly the spaceman free queue (`sfq`), holding the block under
   `{xid, paddr}` until the referencing checkpoint rolls off? Or does a minimal
   implementation that frees immediately at commit (clears the bitmap bit)
   ALSO yield an fsck-clean container — i.e. does `fsck_apfs` validate ONLY the
   latest checkpoint, or every checkpoint in the ring? (This decides whether we
   must implement the free queue now or can defer it.)

3. **Container-level COW blocks.** The old VSB block (virtual object in the
   container omap), the old container-omap b-tree node, and the old container
   `omap_phys` header are COW'd every transaction. Are these reclaimed the same
   way (free queue) as volume-level blocks? Any ordering constraint vs the
   volume structures? Is the old VSB block reclaimed via the container omap's
   own snapshot/tombstone mechanism rather than an explicit free?

4. **Device-/structure-dependence.** Does the SET of blocks that must be freed
   per transaction vary device-to-device or by structure (multi-node vs
   single-node trees; real-bitmap vs implicit `ci_bitmap_addr==0` chunks)? Or is
   it deterministic per op: {old omap header + old omap tree + all old catalog
   nodes + old extentref + old VSB + old container-omap node + old container-omap
   header}? Does anything EXTRA need to be written on larger devices?

5. **`apfs_fs_alloc_count` exact definition.** Which blocks does it count — only
   volume-owned file-data + volume metadata (catalog/omap/extentref), or also
   the container-level VSB/container-omap blocks? Our value drifts by ±4 because
   we mis-count COW churn. We need the precise rule: is it incremented per data
   block and held constant across pure-metadata COW (since old==new count)?

6. **Free-queue record format / "fix value".** If the free queue is required,
   what is the exact on-disk record (e.g. `spaceman_free_queue` b-tree key/value:
   `{ xid, paddr }` -> count)? Is there a `sm_fq` head/tail/count to update in
   the spaceman? What does the fsck "space manager free queue trees" check
   validate, and is there a specific flag/count we must set so a freed-but-
   pinned block is accepted rather than flagged?

## Current hypotheses (to confirm/correct)

- Q2: I suspect fsck validates only the LATEST checkpoint, so freeing immediately
  at commit (no snapshot) yields a clean container — making the free queue an
  optimization for crash-safety/snapshots rather than an fsck requirement. If
  TRUE, completing the immediate-free across all COW sites (incl. container
  level) is the permanent fix for non-snapshot writes. (NEEDS CONFIRMATION.)
- Q3: container-omap old blocks are freed the same way; the old VSB block is the
  previous `{vsb_oid, xid}` container-omap mapping's paddr and can be freed once
  no checkpoint references it.
- Q5: fs_alloc_count = volume-owned blocks; constant across metadata-only COW
  (old freed == new allocated), +N for N new data blocks, -N on unlink of
  live-owned data. Our drift is from not freeing old metadata symmetrically.
