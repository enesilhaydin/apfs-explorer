# M7b-RT Subtask D — sm_fq layout (empirical, locked 2026-05-23)

> Source: real-disk decode of `/tmp/apfs-scratch-large.img` (kernel-formatted
> APFS in GPT, baseline + 18 checkpoints). Cross-checked against KB
> `apfs-spec:cow-block-reclaim` Q4/Q6 and the architecture finding.

## spaceman_phys sm_fq embedding

Each `spaceman_free_queue_t` is **40 bytes**:

| Offset | Field | Type | Notes |
|:---:|---|---|---|
| +0  | `sfq_count`            | u64 | running entry count |
| +8  | `sfq_tree_oid`         | u64 | ephemeral oid of the sm_fq b-tree |
| +16 | `sfq_oldest_xid`       | u64 | smallest xid in the queue |
| +24 | `sfq_tree_node_limit`  | u16 | (then 2 B pad, 4 B pad) |
| +32 | `sfq_reserved`         | u64 | always 0 |

The kernel-formatted scratch has TWO active slots:
- `sm_fq[A]` @ block offset **0xc8** (start of spaceman_phys body after header
  + variable IP/dev fields): tree_oid=0x403, count=2, oldest_xid=2,
  node_limit=1.
- `sm_fq[B]` @ block offset **0xf0**: tree_oid=0x405, count=2, oldest_xid=2,
  node_limit=231.

(The exact base offset for `sm_fq[SFQ_IP]` depends on the IP-pool fields
which vary by container size; the locked layout for our scratch is the
two offsets above. Robust implementation reads the running offset from
the obj_phys end + known sm_dev[2] + IP fields.)

## sm_fq b-tree node (empirical, kernel-formatted)

Header (obj_phys 32 B):
- `o_oid` = paddr (physical, but typed ephemeral)
- `o_xid` = current txn xid
- `o_type` = `OBJ_EPHEMERAL | OBJECT_TYPE_BTREE` = **`0x80000002`**
- `o_subtype` = `OBJECT_TYPE_SPACEMAN_FREE_QUEUE` = **`0x09`**

Btree node (`btree_node_phys`):
- `btn_flags` = **`0x07`** (`ROOT | LEAF | FIXED_KV_SIZE`)
- `btn_level` = 0
- `btn_nkeys` = N (0 when empty)
- `btn_table_space` = (off=0, len=**576**) — fixed reserve, 576/4 = 144
  kvoff_t entries reserved (fits up to 144 records before split)
- `btn_free_space` = computed: `off = nkeys*key_size`, `len = bsz - 56 -
  toc_len - nkeys*16 - 40`. Empirical for nkeys=1: `(16, 3400)`.
- `btn_key_free_list` / `btn_val_free_list` = `(0xFFFF, 0)` (invalid
  sentinel; no free-list management at this level).

Btree footer (last 40 bytes — `btree_info_t`):
- `bt_flags` = **`0x0e`** (`SEQUENTIAL_INSERT | ALLOW_GHOSTS | EPHEMERAL`)
- `bt_node_size` = 4096
- `bt_key_size` = **16** (= `sizeof(spaceman_free_queue_key_t)`)
- `bt_val_size` = **8** (= `sizeof(uint64_t)`, treated as count)
- `bt_longest_key` = 16
- `bt_longest_val` = 8
- `bt_key_count` = N
- `bt_node_count` = 1

Key/val:
- `spaceman_free_queue_key_t` (16 B): `sfqk_xid u64`, `sfqk_paddr u64`
  — sorted ascending by `(xid, paddr)`.
- `spaceman_free_queue_val_t` (8 B): `count u64` — typically 1 (block
  count for run-length); we always use 1 (no extent coalescing yet).

## Subtask D plan (4 atomic commits)

### D1 — sm_fq builder + decoder helpers
- `build_empty_sm_fq_node(paddr, xid, bsz)` — emits the layout above.
- `parse_sm_fq_entries(node, bsz)` — returns `Vec<(xid, paddr, count)>`.
- Unit test (lib): round-trip empty → decode → empty.

### D2 — `Transaction::enqueue_sm_fq(snap_xid, paddr)`
- Locate sm_fq[SFQ_MAIN] (oid=0x403, paddr from ephemeral table).
- COW (rewrite) the b-tree node in-place (ephemerals are re-staged
  every commit; the new node's paddr stays the same).
- Insert `{snap_xid, paddr}` keeping `(xid asc, paddr asc)` order.
- Bump spaceman_phys.sm_fq[SFQ_MAIN].sfq_count, update sfq_oldest_xid
  if needed.

### D3 — `free_replaced_metadata` re-routing
- When `o_xid <= newest_snap`, instead of silent preserve, call
  `txn.enqueue_sm_fq(newest_snap, paddr)`.
- The bitmap bit stays SET (the block is still allocated), but it is
  now explicitly tracked → fsck sees no overallocation.
- fs_alloc_count: pinned blocks remain counted (they ARE owned by
  the volume's snapshot view).

### D4 — `delete_snapshot` drain
- After all other delete steps, walk sm_fq[SFQ_MAIN] for entries with
  `xid <= deleted_snap_xid`.
- For each: `txn.free_block(paddr)` (bitmap clear + free_count++),
  remove the entry from sm_fq.
- Update sfq_count, sfq_oldest_xid.
- fs_alloc_count: decrement by drained count.

### Test gate
- Un-ignore `repro_overalloc_snapshot_pinned`; expect fsck CLEAN
  post-write_B (and after a follow-up delete_snapshot, also CLEAN).

## Edge cases to handle

- **sm_fq node split**: 576-byte TOC = 144 entries max. For
  `repro_overalloc_snapshot_pinned` we enqueue ~3-5 entries, well
  under the limit. Multi-node sm_fq is a follow-up.
- **Multiple snapshots**: each snapshot pins its own set; sm_fq keys
  with `xid` distinguishes them. Drain uses `xid <= deleted_snap.xid`.
- **Reaper handoff**: per spec, NX reaper "drains in the background";
  for fsck-correctness we drain immediately at `delete_snapshot`. The
  reaper integration is a v2 follow-up.

## What to read first next session

1. This doc.
2. `crates/apfs-write/src/txn.rs` ephemerals re-write loop (search for
   `ephemerals` / `reaper` to find the existing pattern; sm_fq slots
   into it).
3. `crates/apfs-write/src/file.rs` free_replaced_metadata (line ~1213)
   — the single call site to retarget.
4. `crates/apfs-write/src/snapshot.rs` delete_snapshot — add the drain
   pass before fs_alloc_count delta.
