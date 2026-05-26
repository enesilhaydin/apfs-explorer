# M7a-RT round3 spec notes (LOCKED — NotebookLM 2026-05-20)

Source: NotebookLM with Apple File System Reference loaded
(`apfs-reference-apple-file-syst` notebook), cross-checked against
`libfsapfs` documentation. Every answer below is page-cited and is the
final arbiter for the M7a-RT writer (populated-real-USB COW correctness).
Subsequent code MUST match. Implementation guesses are forbidden under
md.6.

## Q-C series — Container OMAP B-tree insertion

### Q-C1 fixed-kv TOC entry struct (Apple PDF page 129)
```c
struct kvoff { uint16_t k; uint16_t v; };  // 4 bytes
```
Used when "the keys and values are both fixed in size". Strictly
distinct from `kvloc_t` (8 bytes) used when keys or values are
variable.

### Q-C2 key_off reference point (Apple PDF page 122-123) — LOCKED
`key_off` is measured **from the start of the key area**. The key area
begins after the end of the table of contents. With `btn_table_space.off
= 0`, the absolute anchor is `DATA_BASE + btn_table_space.len`.

### Q-C3 val_off direction & anchor (Apple PDF page 122-123) — LOCKED
`val_off` is counted from the END of the value area to the BEGINNING
of the value (grows backward / toward lower addresses). Anchor:
- **Root node** (BTNODE_ROOT, has 40-byte btree_info_t footer):
  `v_abs = (bsz - 40) - val_off`.
- **Non-root node** (no footer): `v_abs = bsz - val_off`.

Empirical observation `v_abs = val_area_end - val_off` is CORRECT.

### Q-C4 TOC sort + sort rule (Apple PDF page 123) — LOCKED
"The entries in the table of contents are sorted by key." For OMAP keys
the sort is `(obj_id ASC, xid ASC)`. On insert into a populated leaf,
the TOC entry MUST be placed in its sorted position; later TOC entries
shift to make room.

### Q-C5 sparse leaf placement vs compaction (Apple PDF page 122) — LOCKED
Compaction is NOT required on every insert. APFS natively supports
fragmentation within the key area and value area via the free-list
fields (`btn_key_free_list`, `btn_val_free_list`). A new key can be
placed at the end of the packed-key region without compacting, provided
there is enough contiguous shared free space (or a large-enough slot in
the free list).

### Q-C6 EXACT header field values for compacted N-key fixed-kv root leaf — LOCKED
For a perfectly compacted (no fragmentation) root leaf with N keys,
KEY_SIZE = 16 (omap), VAL_SIZE = 16 (omap), DATA_BASE = 56,
BTREE_INFO_SIZE = 40, bsz = 4096:

```
@40 btn_table_space:   off = 0           len = N * 4
@44 btn_free_space:    off = N * 16      len = 4000 - N * 36
@48 btn_key_free_list: off = 0xFFFF      len = 0
@52 btn_val_free_list: off = 0xFFFF      len = 0
```

Derivation: total block 4096 − 40 (btree_info) = 4056 (end of value
area). Values occupy N × 16 from the top, so values start at
4056 − N × 16. Key area starts at DATA_BASE + TOC = 56 + N × 4 = 56 +
N × 4. Keys occupy N × 16, ending at 56 + N × 4 + N × 16 = 56 + N × 20.
Shared free space length = (4056 − N × 16) − (56 + N × 20) = 4000 −
N × 36.

`btn_free_space.off` is measured from the **start of the key area**
(same anchor as `key_off` in TOC entries).

BTOFF_INVALID = 0xFFFF universally for empty `nloc_t` lists (page 122,
`#define BTOFF_INVALID 0xffff`).

## Q-D series — Spaceman CIB copy-on-write per transaction

### Q-D1 spaceman_phys_t byte layout (Apple PDF page 160-161; libfsapfs §7.3) — LOCKED
```
@0   obj_phys_t           (32 bytes)
@32  uint32_t sm_block_size
@36  uint32_t sm_blocks_per_chunk
@40  uint32_t sm_chunks_per_cib
@44  uint32_t sm_cibs_per_cab
@48  spaceman_device_t sm_dev[SD_MAIN]  // 48 bytes
   @48  sm_block_count   (u64)
   @56  sm_chunk_count   (u64)
   @64  sm_cib_count     (u32)
   @68  sm_cab_count     (u32)
   @72  sm_free_count    (u64)
   @80  sm_addr_offset   (u32)  ← byte offset within the spaceman block
                                  where the CIB paddr array starts
```

### Q-D2 CIB COW policy (Apple PDF; libfsapfs) — LOCKED
- Only CIBs whose bitmap was ACTUALLY mutated this transaction are
  COW'd to a new paddr. Unchanged CIBs stay at their existing paddr
  (referenced from the new spaceman's CIB paddr array).
- Old CIB paddrs (the ones that got COW'd) are released into the
  spaceman free queue `sm_fq[APFS_SFQ_MAIN]` for deferred reclamation
  via the reaper.

### Q-D3 ci_bitmap_addr block COW (Apple PDF page on OBJ_NOHEADER) — LOCKED
The allocation bitmap block (referenced by `chunk_info_t.ci_bitmap_addr`)
IS itself COW'd to a new paddr when its bits change. The bitmap object
uses OBJ_NOHEADER (no obj_phys), but is still subject to the universal
"never modify in place" rule. When the bitmap is COW'd, `ci_bitmap_addr`
in the parent CIB is updated → that CIB itself is COW'd → spaceman's
CIB paddr array is updated.

### Q-D4 sm_free_count + ci_free_count update timing (Apple PDF on ephemeral) — LOCKED
- spaceman is OBJ_EPHEMERAL: mutated in RAM during the transaction,
  serialised atomically at commit into a NEW physical location.
- `sm_dev[SD_MAIN].sm_free_count` decrements in the new spaceman block
  by the exact count of blocks allocated this txn.
- Each affected `chunk_info_t.ci_free_count` also decrements, which
  causes that CIB to be COW'd per Q-D2.

### Q-D5 internal-pool bitmap rotation (Apple PDF; SFQ_IP, SPACEMAN_IP_BM_TX_MULTIPLIER) — LOCKED
- IP (internal pool) is a SEPARATE allocation zone from the main pool.
  Tracked by `sm_ip_bm_base`, `sm_ip_bitmap_offset`, and the dedicated
  free queue `sm_fq[APFS_SFQ_IP]`.
- IP bitmaps must ROTATE (not in-place mutate) because the spaceman
  itself is serialised using IP-allocated space; in-place mutation
  would create an infinite recursive COW loop. Apple reserves
  `SPACEMAN_IP_BM_TX_MULTIPLIER = 16` pre-allocated rotating bitmap
  blocks for safe serialisation.
- Main-pool CIB bitmaps do NOT use this rotation pattern; they use
  standard B-tree-backed COW allocation (Q-D2 / Q-D3 above).

### Q-D6 sm_fq free queue record format (Apple PDF page 130, 160) — LOCKED
```c
typedef struct {
    xid_t   sfqk_xid;     // 8 bytes
    paddr_t sfqk_paddr;   // 8 bytes
} spaceman_free_queue_key_t;  // total 16 bytes, packed

typedef uint64_t spaceman_free_queue_val_t;  // block count
```

GHOST records: the free queue uses `BTREE_ALLOW_GHOSTS`. A ghost is
indicated by `kvloc.val.off = BTOFF_INVALID (0xFFFF)`. In sm_fq context,
a ghost record means a free extent of exactly **1 block** long
(value absent ⇒ implicit count = 1).
