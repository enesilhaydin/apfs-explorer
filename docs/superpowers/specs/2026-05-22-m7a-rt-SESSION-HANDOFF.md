# M7a-RT Session Handoff — 2026-05-22

## Current state (last commit: 61168e7)

Branch: `feat/m7a-write-snapshot` (clean working tree after 61168e7).
40 atomic commits today across rounds 10-14 of NotebookLM-LOCKED fixes.

### Test status — scratch_tests::decisive_create_delete_snapshot_on_scratch

`fsck_apfs -n` on post-snapshot scratch image reports ONLY 2 remaining
errors (down from 4 cascading errors this morning):

```
error: (oid 0xc2) om: bt: invalid o_xid (0x4)
   Object map is invalid.
error: sm: sm_free_count (16166) is not valid (16167) (sm_dev 0)
```

Both are spaceman free-count audit issues. Block 0xc2 is physically
intact (kernel-original xid=2, untouched) — Python verified. Our
allocator no longer hands out 0xc2. Old paddr was correctly freed
via txn.free_block. Yet fsck still complains.

### Today's progression (rounds 10-14)

| Commit | Round | What was fixed |
|---|---|---|
| 4d60b92 | 10 | APFS_TYPE_SNAP_METADATA=1, SNAP_NAME=11 (Apple PDF p.84) |
| a467480 | 11 | inum = ROOT_DIR_INO_NUM (2) |
| ec90f21 | 11c | om_snapshot_tree provisioned with omap_snapshot_t record |
| f1c3959 | 12 | BTREE_UINT64_KEYS + bt_key_size/val_size coupling |
| (committed) | 13 | toc_alloc=576 for om_snapshot_tree (variable-kv kvloc_t TOC) |
| a423fc6 | 14 | alloc_block SKIPS bitmap_addr=0 chunks (no double-alloc) |
| 61168e7 | 14b | COW volume omap btree + free old paddr |

### What's working (proven)

- snap-meta tree (key+name_len+~0ULL, inum=2, extentref_tree_type from VSB)
- container omap btree (BTOFF_INVALID free-list, free_space calc)
- om_snapshot_tree (subtype 0x13, single omap_snapshot_t record,
  bt_flags=0x52, key_size=8, val_size=16, table_space.len=576)
- 8-byte alignment, frozen VSB OID preservation, UNIX-epoch timestamps
- Volume omap btree COW (read old → copy → update obj_phys → write new
  → free old)
- alloc_block no longer hands out blocks from bitmap_addr=0 chunks

### Two remaining errors — TEK kaynaktan (single root cause)

Both errors share spaceman bookkeeping mismatch:

**Error 1**: `(oid 0xc2) om: bt: invalid o_xid (0x4)`
- Block 0xc2 contents UNCHANGED (xid=2, type=BTREE+OMAP, valid Fletcher)
- New chain (xid=3) doesn't reference 0xc2 anymore (we COW'd to 0xcd)
- We freed 0xc2 via txn.free_block(0xc2) ⇒ ci_free_count++, sm_free_count++
- But fsck still says 0xc2 has invalid o_xid

Hypothesis: the `(0x4)` is fsck reporting EXPECTED o_xid relative to
NX next_xid=4 (after our xid=3 transaction). fsck might require all
"free" blocks to have o_xid == 0 (cleared) instead of stale data, OR
the `0x4` is some context-flag not a real xid number.

**Error 2**: `sm_free_count (16166) is not valid (16167)`
- Our spaceman says 16166 free, fsck recomputes 16167
- Off-by-one: we're claiming ONE MORE block allocated than fsck thinks
  should be allocated
- Likely caused by our free_block(0xc2) — bitmap_addr=0 path increments
  ci_free_count but doesn't actually clear any bitmap bit (no bitmap
  exists); fsck's free-count recomputation may not align with ours

### Open hypothesis for round15

The two errors are likely the SAME bug: when bitmap_addr=0, our
alloc/free maintenance is fundamentally incomplete because there's no
real bitmap to mutate. Possible fixes:
- (a) When freeing a block in a bitmap_addr=0 chunk, ALSO clear the
  block's obj_phys (zero out o_xid) so fsck doesn't see stale xid
- (b) Allocate a real bitmap block at create_snapshot time, populate it
  with the current implicit allocation pattern, then mutate normally
- (c) Don't free 0xc2 at all — keep it allocated; fsck may accept that

### Action items for next session

1. Ask round15 NotebookLM about:
   - fsck error format `(oid X) om: bt: invalid o_xid (Y)` — what does Y mean?
   - When bitmap_addr=0, how does Apple's kernel maintain free_count
     consistency with fsck's audit?
   - Should freed-block obj_phys be zeroed?

2. If round15 confirms (a): zero out 0xc2's obj_phys when freeing
3. If (b): implement bitmap-block allocation + materialization
4. If (c): remove our free_block(0xc2) call — keep allocated

### Files modified this session

- `crates/apfs-write/src/snapshot.rs` (rounds 10-14, om_snapshot_tree
  builder, volume omap btree COW)
- `crates/apfs-write/src/txn.rs` (round14: alloc_block skip
  bitmap_addr=0 chunks)
- `docs/superpowers/specs/2026-05-20-m7a-rt-round{8..14}-*.md`
  (NotebookLM Q&A files)

### Important context for post-compact

- The Apple PDF (2020 edition) is the authoritative source; macOS 15.7.4
  fsck enforces additional invariants not always documented
- NotebookLM answers have been ~95% accurate but occasionally hallucinate
  field types or struct layouts; ALWAYS cross-check with Apple PDF page
  citations
- Block 0xc2 is the kernel-formatted volume omap btree paddr on every
  fresh `hdiutil gen-scratch` run — used as reference debug target
- Real-USB testing is disabled until scratch is fully CLEAN (we're 2
  errors away)

### Recovery commands

```
# Resume work after compact:
cd /Users/eiaydin/Documents/projects/apfs-explorer
git log --oneline -10
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_scratch \
    scratch_tests::decisive_create_delete_snapshot_on_scratch -- --nocapture

# Inspect post-test fsck error:
hdiutil attach -nomount /tmp/apfs-scratch-m7a.img
fsck_apfs -n /dev/diskNs1   # find via diskutil list

# Round15 question file:
docs/superpowers/specs/2026-05-22-m7a-rt-round15-notebooklm-question.md
```
