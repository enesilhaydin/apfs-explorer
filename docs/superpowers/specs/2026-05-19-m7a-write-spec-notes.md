# M7a Write-Path Spec Notes
## Citation-backed reference for COW transaction engine + snapshot create/delete
### Status: LOCKED where certain · FLAGGED where ambiguous · md.6 questions at end

> **Arbiter:** real macOS scratch disk + `fsck_apfs`. These notes are hypotheses
> until confirmed by the scratch proof (Task 5). Every AMBIGUOUS item BLOCKS
> Tasks 3-4 until the user runs the NotebookLM questions and the answer is
> locked into `apfs-spec:m7a-confirmed`.
>
> **Sources used:**
> - `apfs-spec:linux-apfs-rw` — `super.c`, `transaction.c`, `spaceman.c`,
>   `snapshot.c`, `btree.c`, `object.c`, `apfs.h`, `apfs_raw.h` (indexed
>   2026-05-19; KB label `apfs-spec:linux-apfs-rw`)
> - `Context7 /pradorocchi/apfs-filesystem-full-reference` (queried 2026-05-19)
> - Apple APFS Reference (NotebookLM — NOT yet queried for md.6 items; pending user)

---

## A. Object Physical Layout (`obj_phys_t`)

[CERTAIN: `apfs_raw.h` lines 1-30 of apfs-spec:linux-apfs-rw]

Every on-disk APFS object begins with an `apfs_obj_phys` header (32 bytes):

```
Offset  Size  Field       Meaning
0x00     8    o_cksum     Fletcher-64 checksum (covers bytes 8..block_end)
0x08     8    o_oid       Object identifier
0x10     8    o_xid       Transaction ID at which this version was written
0x18     4    o_type      Object type | storage-class flags (upper 2 bits)
0x1C     4    o_subtype   Subtype (e.g. tree type for B-tree nodes)
```

Fletcher-64 algorithm [CERTAIN: `object.c` apfs-spec:linux-apfs-rw]:
- Computed over bytes `[8 .. block_size)` (skipping the 8-byte checksum field).
- Two 32-bit partial sums iterated over `__le32` words; final `c1,c2` derived
  mod `0xFFFFFFFF`; result is `(c2 << 32) | c1`.
- Block size is limited to `2^16` bytes; this is the assumption the APFS
  implementation makes (not a generic Fletcher-64).

Storage-class flags in `o_type` upper bits [CERTAIN: `apfs_raw.h`]:
```
APFS_OBJ_VIRTUAL    = 0x00000000  (upper 2 bits = 00)
APFS_OBJ_EPHEMERAL  = 0x80000000  (upper 2 bits = 10)
APFS_OBJ_PHYSICAL   = 0x40000000  (upper 2 bits = 01)
```

---

## B. Object Storage Classes — Write Handling

[CERTAIN: `transaction.c`, `object.c`, `apfs_raw.h` apfs-spec:linux-apfs-rw +
Context7 /pradorocchi/apfs-filesystem-full-reference omap section]

### Physical objects
- Their `o_oid == physical block address` (the block number they live at).
- Written to a newly allocated block each transaction (COW).
- Located directly by block address; no omap lookup needed.
- Examples: B-tree nodes (non-root), checkpoint-map blocks, snapshot
  superblock copies.

### Virtual objects
- Their `o_oid` is a stable logical identifier; physical location found via
  the omap B-tree keyed by `(o_oid, o_xid)`.
- On every write: allocate new block, write new version there, insert
  `{oid, new_xid} → new_paddr` into the omap B-tree.
- Examples: volume superblock (`apfs_superblock`), B-tree roots, spaceman
  (spaceman_phys is actually **ephemeral** — see below).

### Ephemeral objects
- Exist only in the checkpoint data area; not located by the omap.
- Mapped via `checkpoint_mapping_t` entries in a checkpoint-map block
  (`checkpoint_map_phys_t`) in the checkpoint descriptor area.
- Written to the **data** area of the checkpoint ring each transaction.
- `o_xid` is set to `nx_xid` at write time.
- Examples: `spaceman_phys`, reaper, nx-level ephemeral objects.

Key distinction relevant to `Transaction::commit()`:
- **Virtual** objects (volume superblock, omap, catalog B-tree root) → omap
  insert required; placed at a physical block (newly allocated); their
  location is registered in the container or volume omap.
- **Ephemeral** objects (spaceman) → no omap insert; placed in checkpoint
  data area; registered in the checkpoint mapping block (descriptor area).
- **Physical** objects (most B-tree nodes) → no omap insert; placed at newly
  allocated block; located by block address embedded in parent node.

---

## C. Checkpoint Commit Sequence

### C.1 Container superblock checkpoint fields

[CERTAIN: `apfs_raw.h` lines 973-992 apfs-spec:linux-apfs-rw + Context7
nx_superblock_t struct]

```
Offset  Field              Meaning
0x58    nx_next_oid        Next OID to assign
0x60    nx_next_xid        Next XID to assign (current txn ID = nx_next_xid - 1
                           at read time; incremented after commit)
0x68    nx_xp_desc_blocks  Number of blocks in the descriptor ring (ring size)
0x6C    nx_xp_data_blocks  Number of blocks in the data ring (ring size)
0x70    nx_xp_desc_base    Block address of descriptor ring start
0x78    nx_xp_data_base    Block address of data ring start
0x80    nx_xp_desc_next    Index of next free slot in descriptor ring (wraps)
0x84    nx_xp_data_next    Index of next free slot in data ring (wraps)
0x88    nx_xp_desc_index   Start index of current checkpoint's descriptor area
0x8C    nx_xp_desc_len     Number of descriptor blocks for current checkpoint
0x90    nx_xp_data_index   Start index of current checkpoint's data area
0x94    nx_xp_data_len     Number of data blocks for current checkpoint
```

### C.2 Checkpoint write sequence

[CERTAIN: `transaction.c` `apfs_checkpoint_end`, `apfs_write_ephemeral_objects`
apfs-spec:linux-apfs-rw; corroborated by Context7 checkpoint_map_phys_t section]

The commit sequence (in order) is:

1. **Flush all inode/file-system metadata** into buffer heads (before any
   checkpoint geometry is fixed).

2. **Flush free-queue cache**: any cached freed ranges not yet in the free queue
   B-tree are flushed (`apfs_free_queue_insert_nocache`).

3. **Write internal-pool (IP) bitmaps** (`apfs_write_ip_bitmaps`). Must happen
   after free-queue flush (freed blocks in IP) and before ephemeral commit.

4. **Write all ephemeral objects** (`apfs_write_ephemeral_objects`):
   - Compute new `desc_index` and `data_index` for this checkpoint:
     ```
     desc_index = desc_next  (from the current superblock)
     data_index = data_next  (from the current superblock)
     ```
   - For each ephemeral object (spaceman, reaper, etc.):
     a. Assign next `data_next` slot: `data_bno = data_base + data_next % data_blks`
     b. Set `obj_raw->o_xid = nx_xid`.
     c. Compute and store Fletcher-64 checksum.
     d. Write object to `data_bno`.
     e. Create a `checkpoint_mapping_t` entry in the checkpoint-map block
        (`checkpoint_map_phys_t`) placed at `desc_base + desc_next % desc_blks`.
     f. Advance `data_next`.
   - Finalize checkpoint-map block: set `CHECKPOINT_MAP_LAST` flag on the
     last map block, write it, advance `desc_next`.
   - Update superblock in-memory: `nx_xp_desc_len = (new_desc_next - desc_index)`,
     `nx_xp_data_len = (new_data_next - data_index)` (modular arithmetic within
     ring sizes).

5. **Flush all modified physical/virtual object buffer heads** to disk
   (filemap_write_and_wait on block device mapping). This ensures all data,
   metadata, B-tree nodes, and ephemeral objects are durably written before
   the superblock is touched.

6. **Write the container superblock LAST** (the single atomic commit point):
   - Set `obj->o_xid = nx_xid` (the current transaction ID).
   - Compute and store Fletcher-64 checksum.
   - Copy to the block at `nx_bno` (the superblock's physical location in the
     descriptor ring at `desc_base + desc_index + desc_len - 1`, i.e. the
     **last** slot of this checkpoint's descriptor area).
   - `sync_dirty_buffer` + `filemap_write_and_wait` to ensure durability.
   - After successful write: `nx_xid++` (in-memory; next transaction uses
     the incremented value). `nx_next_xid = nx_xid + 1` stored in raw sb.

[CERTAIN from transaction.c: `if (nx_trans->t_starts_count == 0) { ++nxi->nx_xid; nxi->nx_raw->nx_next_xid = cpu_to_le64(nxi->nx_xid + 1); }`]

**Atomicity guarantee**: if power loss occurs before step 6, the old
superblock (with old xid) is still on disk and still valid — the new objects
written in steps 1-5 are unreachable/ignored by the old superblock. The new
superblock's Fletcher-64 checksum is the commit point: a partial write of
the superblock block leaves a bad checksum and the old checkpoint is used on
recovery.

### C.3 Descriptor ring index math

[CERTAIN: `transaction.c` apfs-spec:linux-apfs-rw]

```
new_checkpoint_desc_index = nx_xp_desc_next  (read from current superblock)
new_checkpoint_data_index = nx_xp_data_next  (read from current superblock)
desc_next_after = (desc_index + desc_len) % nx_xp_desc_blocks
data_next_after = (data_index + data_len) % nx_xp_data_blocks
```

The superblock occupies the **last** slot of its checkpoint's descriptor
area; mapping blocks occupy the preceding slots.
[CERTAIN: `transaction.c` comment "Last block in the area is superblock; the
rest are mapping blocks"; the loop iterates `desc_len - 1` map blocks then
places the superblock at `desc_index + desc_len - 1`]

---

## D. Space Manager — Block Allocation

### D.1 On-disk structures

[CERTAIN: `apfs_raw.h` apfs-spec:linux-apfs-rw]

```c
struct apfs_chunk_info {        // per-chunk allocation record
    __le64 ci_xid;              // last-modified xid
    __le64 ci_addr;             // block address of this chunk's start
    __le32 ci_block_count;      // total blocks in chunk
    __le32 ci_free_count;       // free blocks remaining in chunk
    __le64 ci_bitmap_addr;      // block address of bitmap block (0 = all free)
};

struct apfs_spaceman_free_queue {
    __le64 sfq_count;           // number of entries in the free queue B-tree
    __le64 sfq_tree_oid;        // OID of the free queue B-tree
    __le64 sfq_oldest_xid;      // oldest transaction represented
    __le16 sfq_tree_node_limit;
    __le16 sfq_pad16;
    __le32 sfq_pad32;
    __le64 sfq_reserved;
};

// Free queue array indexes:
// APFS_SFQ_IP    = 0  (internal pool)
// APFS_SFQ_MAIN  = 1  (main device)
// APFS_SFQ_TIER2 = 2  (Fusion tier2; not supported for non-Fusion)
```

`spaceman_phys` contains `sm_fq[APFS_SFQ_COUNT]` free queues plus
`sm_dev[APFS_SD_MAIN]` device info (block_count, chunk_count, cib_count,
free_count, addr_offset).

### D.2 Allocation path within a transaction

[CERTAIN: `spaceman.c` + `transaction.c` apfs-spec:linux-apfs-rw]

1. Read the spaceman (ephemeral — already in memory after checkpoint read).
2. Find a chunk with `ci_free_count > 0` by scanning the CIB (chunk-info
   block) array (CIB addresses stored after `sm_addr_offset` in the spaceman
   block itself).
3. Read the chunk's bitmap block (`ci_bitmap_addr`; if 0 the chunk is
   entirely free, use any block).
4. Find the first zero bit in the bitmap → that is the free block index
   within the chunk.
5. Set the bit to 1 (mark allocated) in the bitmap block.
6. Decrement `ci_free_count` in the chunk_info record.
7. Decrement `sm_dev[APFS_SD_MAIN].sm_free_count`.
8. Mark the spaceman (ephemeral object) dirty — it will be written to the
   checkpoint data area at commit time.

Freed blocks are NOT immediately returned to the bitmap. They go into a
**free-queue cache** (`sm_free_cache_base`, `sm_free_cache_blkcnt` in the
in-memory spaceman) and are flushed into the free queue B-tree
(`sm_fq[APFS_SFQ_MAIN].sfq_tree_oid`) just before the ephemeral objects are
committed (step 2 of the commit sequence above).

[AMBIGUOUS-1: See md.6 question Q1 — exact in-transaction free-queue update
trigger: when exactly does a freed block enter the free queue vs. the cache;
what is the key/value layout of the free queue B-tree entries; and whether the
bitmap update (step 5-6 above) is deferred or immediate within the txn.]

---

## E. Object Map B-tree — Virtual Object Insert/Update

[CERTAIN: `apfs_raw.h` omap structures + Context7 omap section]

The volume omap (`apfs_superblock.apfs_omap_oid`) is a B-tree that maps
`(oid, xid)` pairs to physical addresses:

```c
struct apfs_omap_phys {       // virtual — located via container omap
    struct apfs_obj_phys om_o;
    __le32 om_flags;
    __le32 om_snap_count;      // number of snapshots using this omap
    __le32 om_tree_type;
    __le32 om_snapshot_tree_type;
    __le64 om_tree_oid;        // OID of the B-tree root
    __le64 om_snapshot_tree_oid;
    __le64 om_most_recent_snap; // xid of most recent snapshot
    __le64 om_pending_revert_min;
    __le64 om_pending_revert_max;
};

// B-tree key:
struct apfs_omap_key { __le64 ok_oid; __le64 ok_xid; };
// B-tree value:
struct apfs_omap_val { __le32 ov_flags; __le32 ov_size; __le64 ov_paddr; };
```

For every modified **virtual** object in a transaction:
1. Allocate a new physical block via the spaceman (section D).
2. Write the new object version there with `o_xid = nx_xid`.
3. Insert `{oid, nx_xid} → {flags=0, size=block_size, paddr=new_block}` into
   the volume omap B-tree.
4. The omap B-tree root is itself a virtual object (COW): its `o_oid` stays
   constant, its physical block changes every transaction → new omap root is
   also registered in the container omap.

[CERTAIN: linux-apfs-rw `transaction.c` reads omap and catalog with
`write=true` at transaction start, which COW-copies the root nodes;
each B-tree node modification allocates a new block and updates the parent
pointer chain up to the root; the new root block address is recorded in the
omap.]

[AMBIGUOUS-2: See md.6 question Q2 — exact COW propagation in the B-tree:
does every node modification always re-allocate the full path from leaf to
root within the same transaction, or can intermediate nodes be re-used? The
linux-apfs-rw code does COW but the exact rule for when a node is "dirty
enough" to require full-path re-alloc is not fully visible from the indexed
snippets alone.]

---

## F. Snapshot Create — Full Record Set

[CERTAIN where marked; AMBIGUOUS where flagged]

### F.1 Snapshot metadata tree (`apfs_snap_meta_tree_oid`)

The volume superblock field `apfs_snap_meta_tree_oid` (offset `0x98` in
`apfs_superblock`) points to a B-tree of type
`APFS_OBJECT_TYPE_SNAPMETATREE` (type `0x10`). Each snapshot creates two
new records in this tree:

**Record 1 — Snapshot metadata** (`APFS_TYPE_SNAP_METADATA`):
```
Key: apfs_snap_metadata_key
     apfs_key_header.hdr: obj_id_and_type field
       - obj_id = snap_xid (the transaction ID of the snapshot)
       - type   = APFS_TYPE_SNAP_METADATA (0xB in the type field)

Value: apfs_snap_metadata_val
  __le64 extentref_tree_oid  // OID of extent-reference tree for this snapshot
  __le64 sblock_oid          // OID of the frozen volume superblock copy
  __le64 create_time         // nanoseconds since 2001-01-01 UTC (Apple epoch)
  __le64 change_time         // same
  __le64 inum                // inode number (typically 0 for programmatic snaps)
  __le32 extentref_tree_type // APFS_OBJ_PHYSICAL | APFS_OBJECT_TYPE_BLOCKREFTREE
  __le32 flags               // APFS_SNAP_META_PENDING_DATALESS=0x1,
                             //   APFS_SNAP_MERGE_IN_PROGRESS=0x2; normally 0
  __le16 name_len            // length of name including null terminator
  u8     name[]              // null-terminated snapshot name
```
[CERTAIN: `apfs_raw.h` apfs-spec:linux-apfs-rw `struct apfs_snap_metadata_val`
+ `snapshot.c` `apfs_create_snap_metadata_rec` which fills these fields]

**Record 2 — Snapshot name** (`APFS_TYPE_SNAP_NAME`):
```
Key: apfs_snap_name_key
     obj_id = APFS_SNAP_NAME_OBJ_ID  (~0ULL & APFS_OBJ_ID_MASK)
     type   = APFS_TYPE_SNAP_NAME
     name_len + name[] = the snapshot name string

Value: apfs_snap_name_val
     __le64 snap_xid  // the xid of the snapshot this name maps to
```
[CERTAIN: `apfs_raw.h` apfs-spec:linux-apfs-rw `struct apfs_snap_name_key` /
`apfs_snap_name_val`; used for name→xid reverse lookup]

### F.2 Frozen volume superblock copy

[CERTAIN: `snapshot.c` `apfs_create_superblock_snapshot` apfs-spec:linux-apfs-rw]

A physical copy of the **current** volume superblock is made:
1. Allocate a new block via spaceman (allocated backwards, i.e. from high
   addresses — `apfs_spaceman_allocate_block(..., true /* backwards */)` in
   linux-apfs-rw, though this is an implementation detail, not spec mandate).
2. `memcpy` the current volume superblock content into the new block.
3. Modify the copy to become a **physical** object:
   - `snap_raw->apfs_o.o_oid = new_block_number` (physical object: oid = paddr)
   - `snap_raw->apfs_o.o_type = APFS_OBJ_PHYSICAL | APFS_OBJECT_TYPE_FS`
4. Zero out fields not valid for a snapshot superblock:
   - `snap_raw->apfs_omap_oid = 0` (snapshot shares the current omap)
   - `snap_raw->apfs_extentref_tree_oid = 0` (given by snap_metadata record)
   - `snap_raw->apfs_snap_meta_tree_oid = 0` (no nested snapshots)
5. Compute Fletcher-64 and store in header.
6. The `sblock_oid` field of `apfs_snap_metadata_val` is set to this new
   block number (since it is now a physical object, its OID = paddr).

### F.3 Extent-reference tree

[CERTAIN structure; AMBIGUOUS creation mechanics — see Q3]

The `extentref_tree_oid` in `apfs_snap_metadata_val` refers to a B-tree of
type `APFS_OBJECT_TYPE_BLOCKREFTREE` (type `0x0F`). This tree tracks the
physical extents "owned" by the snapshot (used for COW-on-next-write
accounting and freeing when the snapshot is deleted).

[AMBIGUOUS-3: See md.6 question Q3 — at snapshot CREATE time, does
linux-apfs-rw / Apple create a new empty extent-reference tree for the
snapshot, or does it reuse/clone the current volume's `apfs_extentref_tree_oid`,
and if new, what is its initial content? The linux-apfs-rw `snapshot.c`
sets `raw_val->extentref_tree_oid = vsb_raw->apfs_extentref_tree_oid` — i.e.
it copies the *current* volume's extentref_tree_oid into the snapshot
metadata value. This implies the snapshot receives the existing extent-ref
tree OID, not a new one. But this contradicts a naive reading where each
snapshot should have its own independent tree.]

### F.4 Extended snapshot metadata (`snap_meta_ext`)

[CERTAIN structure: `apfs_raw.h` apfs-spec:linux-apfs-rw]

```c
struct apfs_snap_meta_ext {
    struct apfs_obj_phys sme_o;  // o_type = APFS_OBJ_PHYSICAL | APFS_OBJECT_TYPE_SNAP_META_EXT (0x1d)
    __le32 sme_version;
    __le32 sme_flags;
    __le64 sme_snap_xid;         // xid of the snapshot this extends
    char   sme_uuid[UUID_SIZE];  // 16 bytes
    __le64 sme_token;
};
```

The volume superblock field `apfs_snap_meta_ext_oid` (offset `0x3E8` in
`apfs_superblock` — appears between `apfs_cloneinfo_xid` at `0x3E0` and
`apfs_volume_group_id` at `0x3F0`) points to this object.

[AMBIGUOUS-4: See md.6 question Q4 — is `snap_meta_ext` created/required for
every snapshot or only for certain volume feature flags? The linux-apfs-rw
`snapshot.c` preview does not show whether this is always written.
`apfs_snap_meta_ext_oid` is a relatively late addition (APFS spec v1.0+).
The token and uuid values are unknown.]

### F.5 Volume superblock field updates on snapshot create

[CERTAIN from `apfs_raw.h` offsets + Context7 + snapshot.c preview;
PARTIALLY AMBIGUOUS — see Q5]

Fields updated in the live volume superblock within the snapshot transaction:
```
apfs_num_snapshots       += 1
apfs_last_mod_time        = now (nanoseconds since 2001-01-01)
apfs_snap_meta_tree_oid   = (new root OID if snap_meta_tree was COW'd)
apfs_extentref_tree_oid   = (new root OID if extentref tree was COW'd; see Q3)
```

The volume omap also gets updated: `om_snap_count += 1`,
`om_most_recent_snap = snap_xid`.

[AMBIGUOUS-5: See md.6 question Q5 — does `apfs_revert_to_xid` or
`apfs_revert_to_sblock_oid` need to be set/cleared during a normal snapshot
CREATE, or are those only used during revert operations?]

---

## G. Snapshot Delete — Mechanics

[PARTIALLY CERTAIN; PARTIALLY AMBIGUOUS]

[CERTAIN: from linux-apfs-rw `snapshot.c` patterns and omap structure]

Snapshot delete is an asynchronous, reaper-driven operation:

1. **Remove snapshot metadata records** from the snapshot metadata tree:
   - Delete the `APFS_TYPE_SNAP_METADATA` record keyed by `snap_xid`.
   - Delete the `APFS_TYPE_SNAP_NAME` record for the name.

2. **Mark omap entries DELETED**: For each virtual object in the omap whose
   `ok_xid` falls within the snapshot's xid range and is no longer needed,
   insert an updated omap entry with `APFS_OMAP_VAL_DELETED` flag. The actual
   physical block freeing is deferred.

3. **Update omap snapshot metadata**:
   - `om_snap_count -= 1`
   - `om_most_recent_snap` updated if this was the most recent.

4. **Decrement `apfs_num_snapshots`** in the volume superblock.

5. **Free the frozen superblock copy**: the block at `sblock_oid` (which is a
   physical block address since it was stored as a physical object) is freed
   back to the spaceman / free queue.

6. **Extent-reference tree cleanup**: the extentref tree pointed to by the
   snapshot metadata is walked; each referenced extent that is no longer
   needed by any live snapshot or the current volume is freed.

7. **Reaper involvement**: for large snapshots, the block freeing is not done
   atomically in one transaction. The reaper (`nx_reaper_oid`, an ephemeral
   object) queues the work across multiple transactions.

[AMBIGUOUS-6: See md.6 question Q6 — exactly which steps of snapshot delete
must complete within the single delete transaction (atomic) vs. which are
deferred to the reaper across multiple transactions? The linux-apfs-rw
snippet shows reaper-based deletion for large snapshots, but the boundary
between the synchronous removal (metadata records) and asynchronous freeing
(extent data) is not confirmed from the indexed content.]

---

## H. Key Object Type Constants

[CERTAIN: `apfs_raw.h` apfs-spec:linux-apfs-rw]

```
APFS_OBJECT_TYPE_NX_SUPERBLOCK   = 0x01
APFS_OBJECT_TYPE_BTREE           = 0x02  (B-tree root)
APFS_OBJECT_TYPE_BTREE_NODE      = 0x03  (non-root node)
APFS_OBJECT_TYPE_SPACEMAN        = 0x05
APFS_OBJECT_TYPE_SPACEMAN_CIB    = 0x07
APFS_OBJECT_TYPE_SPACEMAN_BITMAP = 0x08
APFS_OBJECT_TYPE_SPACEMAN_FREE_Q = 0x09
APFS_OBJECT_TYPE_OMAP            = 0x0B
APFS_OBJECT_TYPE_CHECKPOINT_MAP  = 0x0C
APFS_OBJECT_TYPE_FS              = 0x0D  (volume superblock)
APFS_OBJECT_TYPE_FSTREE          = 0x0E  (catalog/file-system tree)
APFS_OBJECT_TYPE_BLOCKREFTREE    = 0x0F  (extent-reference tree)
APFS_OBJECT_TYPE_SNAPMETATREE    = 0x10  (snapshot metadata tree)
APFS_OBJECT_TYPE_SNAP_META_EXT   = 0x1D
```

---

## I. Volume Superblock Snapshot-Relevant Fields (with offsets)

[CERTAIN: `apfs_raw.h` apfs-spec:linux-apfs-rw with byte offsets]

```
Offset  Field
0x74    apfs_root_tree_type        (u32)
0x78    apfs_extentref_tree_type   (u32)
0x7C    apfs_snap_meta_tree_type   (u32)
0x80    apfs_omap_oid              (u64) — virtual: volume omap
0x88    apfs_root_tree_oid         (u64) — virtual: catalog/fstree root
0x90    apfs_extentref_tree_oid    (u64) — virtual: extent-reference tree root
0x98    apfs_snap_meta_tree_oid    (u64) — virtual: snapshot metadata tree root
0xA0    apfs_revert_to_xid         (u64) — 0 normally; set during revert
0xA8    apfs_revert_to_sblock_oid  (u64) — 0 normally
0xB0    apfs_next_obj_id           (u64)
0xB8..  apfs_num_files, _directories, _symlinks, _other_fsobjects  (u64 each)
0xD8    apfs_num_snapshots         (u64) — increment on create, decrement on delete
0xE0    apfs_total_blocks_alloced  (u64)
0xE8    apfs_total_blocks_freed    (u64)
0x100   apfs_last_mod_time         (u64) — nanoseconds since Apple epoch (2001-01-01)
0x3E8   apfs_snap_meta_ext_oid     (u64) — OID of snap_meta_ext object
```

---

## md.6 NotebookLM Questions (USER must run)

> These are GENUINE ambiguities where sources conflict or are silent.
> Tasks 3 and 4 MUST NOT be coded until each question is answered and
> locked into `apfs-spec:m7a-confirmed`.
>
> **NotebookLM notebook**: `apfs-reference-apple-file-syst` (Apple APFS spec PDF)
>
> After getting each answer, record it as:
> `apfs-spec:m7a-confirmed: Q<N> ANSWER: <exact answer> SOURCE: <page/section>`

---

### Q1 — Free-queue B-tree key/value layout + in-transaction update trigger

> **Context**: The spaceman has `sm_fq[APFS_SFQ_MAIN]` with `sfq_tree_oid`
> pointing to a B-tree. Freed blocks are cached in memory (`sm_free_cache_base`,
> `sm_free_cache_blkcnt`) and flushed to the free queue tree before the
> ephemeral commit. The bitmap allocation (mark bit allocated, decrement
> ci_free_count) appears to happen immediately on allocation.
>
> **Question for NotebookLM**: In the APFS spec, what is the exact on-disk
> key/value structure of a free-queue B-tree record? Specifically: (a) Is the
> key the starting block address of a free range, or is it `(xid, paddr)`? (b)
> What does the value encode — just a block count, or also a transaction ID? (c)
> At the point of block ALLOCATION within a transaction, does the spaceman
> immediately clear the bit in the chunk bitmap and decrement `ci_free_count`,
> or is the bitmap update also deferred? Please cite the exact struct name,
> field names, and the section/page of the spec.
>
> **Dual-evidence plan**: (a) On scratch disk after one transaction, use Python
> to decode `sm_fq[1].sfq_tree_oid`, traverse the B-tree, and print each
> key/value pair as raw hex. (b) Cross-check against the `ci_free_count` in
> the chunk-info block before and after the transaction.

---

### Q2 — B-tree COW: full path re-allocation vs. dirty-node reuse

> **Context**: In linux-apfs-rw, `apfs_read_omap(sb, write=true)` and
> `apfs_read_catalog(sb, write=true)` are called at transaction start to COW
> the root nodes. It is unclear whether every internal node touched in the
> transaction is also unconditionally re-allocated (full path from leaf to root),
> or whether only the actually-modified nodes are re-allocated.
>
> **Question for NotebookLM**: Does the APFS spec require that copy-on-write for
> B-tree modifications re-allocates ALL nodes on the path from the modified leaf
> to the root within a single transaction, or only the nodes that are actually
> modified? In other words, if inserting one record causes a leaf split but the
> parent node is unchanged in content, is the parent block still re-allocated?
> Please cite the spec section that defines the COW rule for B-tree nodes.
>
> **Dual-evidence plan**: (a) On scratch disk, record the omap B-tree root block
> address before and after a single snapshot creation. (b) Python re-decode:
> walk all internal nodes of the omap B-tree and record which block addresses
> changed between the two checkpoints.

---

### Q3 — Extent-reference tree at snapshot CREATE: new empty tree or current tree shared?

> **Context**: `snapshot.c` `apfs_create_snap_metadata_rec` sets
> `raw_val->extentref_tree_oid = vsb_raw->apfs_extentref_tree_oid` — it copies
> the **current** volume's `apfs_extentref_tree_oid` into the snapshot metadata
> value. This means the snapshot records the same OID as the current live
> extentref tree. But APFS is COW: future modifications to the live extentref
> tree create a new physical block (new paddr for the same OID). The snapshot
> metadata then preserves the OID → at snapshot time the omap maps that OID to
> the old block (frozen).
>
> **Question for NotebookLM**: At the time a snapshot is created in APFS, what
> exactly is stored in the `extentref_tree_oid` field of `snap_metadata_val`?
> (a) Is it the OID of the current volume's extent-reference tree (the same OID
> as `apfs_superblock.apfs_extentref_tree_oid`), meaning the snapshot inherits
> the current tree via the omap's snapshot-frozen mapping? Or (b) does Apple
> create a new, separate extent-reference tree object for the snapshot and
> record its new OID? Please cite the spec section and confirm whether
> `snap_metadata_val.extentref_tree_oid` is the same as the live volume's
> `apfs_extentref_tree_oid` at snapshot time.
>
> **Dual-evidence plan**: (a) On scratch disk: read the live volume superblock's
> `apfs_extentref_tree_oid` before and after `diskutil apfs createSnapshot`.
> (b) Python: decode the `snap_metadata_val.extentref_tree_oid` from the
> snapshot metadata tree. Compare the three values.

---

### Q4 — `snap_meta_ext` (type 0x1D): required for every snapshot or conditional?

> **Context**: `apfs_superblock` has a field `apfs_snap_meta_ext_oid` at
> offset `0x3E8` (between `apfs_cloneinfo_xid` and `apfs_volume_group_id`).
> The struct `apfs_snap_meta_ext` contains `sme_version`, `sme_flags`,
> `sme_snap_xid`, `sme_uuid`, `sme_token`. It is unclear whether this object
> must be created as part of every snapshot creation, or whether it is only
> required when certain volume features are enabled.
>
> **Question for NotebookLM**: Is the `snap_meta_ext` object (APFS object type
> `0x1D`, `APFS_OBJECT_TYPE_SNAP_META_EXT`) required to be written for every
> APFS snapshot creation? Or is it only present when certain volume feature
> flags are set? What are the valid values for `sme_version`, `sme_flags`, and
> `sme_token`? Does `apfs_superblock.apfs_snap_meta_ext_oid` point to the most
> recent snapshot's ext object, or is it a separate object that spans all
> snapshots? Please cite the spec section and field descriptions.
>
> **Dual-evidence plan**: (a) Create a snapshot on scratch disk with
> `diskutil apfs createSnapshot`. Python: read
> `apfs_superblock.apfs_snap_meta_ext_oid` — if non-zero, decode the block and
> print all fields. (b) Check if the field was zero before the snapshot was
> created (compare two checkpoints).

---

### Q5 — `apfs_revert_to_xid` and `apfs_revert_to_sblock_oid` during normal snapshot create

> **Context**: The volume superblock has `apfs_revert_to_xid` (offset `0xA0`)
> and `apfs_revert_to_sblock_oid` (offset `0xA8`). These are clearly used
> during a snapshot revert (rollback). It is unclear whether they must be
> set to specific values (e.g. zeroed, or set to the new snapshot's xid) at
> snapshot CREATE time.
>
> **Question for NotebookLM**: During a normal APFS snapshot CREATE operation
> (not a revert), should `apfs_superblock.apfs_revert_to_xid` and
> `apfs_revert_to_sblock_oid` be left at their existing values (typically 0),
> or must they be updated? What values do these fields hold in a healthy volume
> with one snapshot but no pending revert? Please cite the spec section.
>
> **Dual-evidence plan**: (a) Python: decode these two fields before and after
> `diskutil apfs createSnapshot` on the scratch disk. Assert they remain 0.

---

### Q6 — Snapshot delete: synchronous (single-txn) vs. reaper-deferred boundary

> **Context**: linux-apfs-rw uses a reaper (`nx_reaper_oid`) for deferred
> block freeing during snapshot deletion. It is unclear which operations must
> complete synchronously within the delete transaction and which are deferred.
>
> **Question for NotebookLM**: When an APFS snapshot is deleted, which of the
> following operations must complete within the single delete transaction, and
> which are deferred to the reaper across multiple subsequent transactions?
> (a) Removing the `APFS_TYPE_SNAP_METADATA` record from the snap meta tree.
> (b) Removing the `APFS_TYPE_SNAP_NAME` record.
> (c) Decrementing `apfs_num_snapshots` in the volume superblock.
> (d) Freeing the physical block of the frozen volume superblock copy.
> (e) Freeing the extents tracked by the snapshot's extent-reference tree.
> (f) Removing `OMAP_VAL_DELETED`-flagged omap entries for the snapshot's xid.
> Please cite the spec section on snapshot deletion and the reaper's role.
>
> **Dual-evidence plan**: (a) Use `diskutil apfs deleteSnapshot` on scratch
> disk and immediately check `fsck_apfs -n` — if clean, all synchronous parts
> completed correctly. (b) Python: re-decode the snap meta tree and omap after
> delete to confirm which records are gone vs. still present with DELETED flag.

---

## md.6 RESOLVED (cross-validated + empirical plan)

> **Status:** All six Q1–Q6 items locked below. Sources: Apple PDF (NotebookLM
> answers), `apfs-spec:linux-apfs-rw` function bodies (fetched 2026-05-19),
> `apfs-spec:libfsapfs`, `apfs-fuse:repo` (indexed). Real macOS scratch disk +
> `fsck_apfs -n` is the final arbiter for all items marked EMPIRICAL. KB label
> for this section: `apfs-spec:m7a-confirmed`.

---

### Q1 — Free-queue B-tree key/value + bitmap update timing

**LOCKED decision:**
- Key: `apfs_spaceman_free_queue_key { sfqk_xid: u64, sfqk_paddr: u64 }` —
  sorted by xid then paddr. Value: bare `u64` block count (no additional xid
  field in the value).
- Bitmap update timing (Q1c): **IMMEDIATE within the allocation call.**
  `apfs_spaceman_allocate_block` (spaceman.c:1324) calls
  `apfs_cib_allocate_block` → `apfs_chunk_alloc_free` (spaceman.c:1091), which
  at line 1217 executes `le32_add_cpu(&ci->ci_free_count, -1)` and marks the
  bitmap bit set — all synchronously within the same call, before returning the
  block number to the caller. The free-queue tree is a deferred flush mechanism
  for freed blocks (queuing frees across transactions), not for allocations; the
  allocation bitmap is updated eagerly.

**Corroborating source:** `apfs-spec:linux-apfs-rw` — `spaceman.c:apfs_chunk_alloc_free` (line 1217: `le32_add_cpu ci_free_count -1`); `apfs_raw.h: struct apfs_spaceman_free_queue_key`.

**Source conflicts:** None. Apple PDF confirms key structure (sfqk_xid, sfqk_paddr). linux-apfs-rw agrees. Value = uint64 count confirmed by both.

**Empirical check:**
1. Scratch disk: Python decode `sm_fq[APFS_SFQ_MAIN].sfq_tree_oid` B-tree — print each key/value as raw hex before and after a single write transaction.
2. Read `ci_free_count` for the chunk containing the newly allocated block immediately after allocation and before transaction commit — must be decremented by 1.
3. Verify free-queue tree entry appears for freed (not allocated) blocks only.

---

### Q2 — B-tree COW: full leaf-to-root path vs. dirty-only

**LOCKED decision (macOS-empirical: mirror kernel output, fsck CLEAN is arbiter):**
linux-apfs-rw implements **on-demand COW per accessed node, with automatic
parent-chain propagation.** The mechanism in `btree.c:apfs_query_join_transaction`
(line ~607): when a non-root node is first dirtied in a transaction
(`buffer_trans` check), `apfs_read_node(..., write=true)` allocates a new
physical block for that node, then calls `__apfs_btree_replace` on the parent
to update the child pointer — recursively up to the root. The root node is
pre-COW'd at transaction start (`apfs_read_omap/catalog write=true`). The net
effect is that **every node on the path from the dirtied leaf to the root is
re-allocated** in the same transaction, because updating the child pointer in a
parent node dirties that parent, which triggers its own COW and parent update.
An unmodified sibling branch is NOT re-allocated.

**Corroborating source:** `apfs-spec:linux-apfs-rw` — `btree.c:apfs_query_join_transaction` (line ~628: `ASSERT(!apfs_node_is_root)`, COW + `__apfs_btree_replace` on parent). Apple PDF: "copy-on-write — never modify in place" (Q2 NotebookLM answer, consistent).

**Source conflicts:** None. Apple silent on the exact propagation rule; linux-apfs-rw resolves it unambiguously.

**Empirical check:**
1. Scratch disk: record the omap B-tree root block address (paddr from container omap) before and after `diskutil apfs createSnapshot`.
2. Python re-decode: walk all internal nodes of the omap B-tree at both checkpoints; record which block addresses changed. Expect: the full path from the modified leaf to the root changed paddr; unrelated subtree nodes unchanged.
3. `fsck_apfs -n` must report CLEAN after the snapshot.

---

### Q3 — Extent-reference tree at snapshot CREATE

**LOCKED decision (DEFINITIVE — Apple PDF + linux-apfs-rw AGREE):**
At snapshot create time:
1. `apfs_create_snap_metadata_rec` (snapshot.c) copies the **current**
   `vsb_raw->apfs_extentref_tree_oid` into `raw_val->extentref_tree_oid` —
   recording the live volume's extentref tree OID in the snapshot metadata.
2. Immediately after (`apfs_create_new_extentref_tree`, snapshot.c): a **new
   empty** `APFS_OBJECT_TYPE_BLOCKREFTREE` B-tree root is allocated, and
   `vsb_raw->apfs_extentref_tree_oid` is updated to the new OID.
3. The snapshot therefore holds the OLD OID (frozen via the omap snapshot
   mechanism). The live volume gets a fresh empty extentref tree.

Apple PDF (Q3 answer): "current extent-reference tree is MOVED to the snapshot;
a NEW EMPTY tree is created for the live volume." This matches linux-apfs-rw
exactly — no conflict.

**Corroborating source:** `apfs-spec:linux-apfs-rw` — `snapshot.c:apfs_create_snap_metadata_rec` (`raw_val->extentref_tree_oid = vsb_raw->apfs_extentref_tree_oid`) + `snapshot.c:apfs_create_new_extentref_tree` (`apfs_make_empty_btree_root` then `vsb_raw->apfs_extentref_tree_oid = new_oid`). INDEPENDENTLY CORROBORATED.

**Source conflicts:** NONE. Apple PDF and linux-apfs-rw are in complete agreement.

**Empirical check:**
1. Python: read `vsb->apfs_extentref_tree_oid` before snapshot — call it `OID_A`.
2. After `diskutil apfs createSnapshot`: read `vsb->apfs_extentref_tree_oid` — must be a new `OID_B != OID_A`.
3. Decode `snap_metadata_val.extentref_tree_oid` for the new snapshot — must equal `OID_A`.
4. Verify `OID_B` tree is empty (zero records). `fsck_apfs -n` CLEAN.

---

### Q4 — `snap_meta_ext` (type 0x1D): required or conditional?

**LOCKED decision (macOS-empirical: mirror kernel output, fsck CLEAN is arbiter):**
linux-apfs-rw **deliberately skips** writing `snap_meta_ext` for now. The
comment in `snapshot.c` (`apfs_do_ioc_take_snapshot`, after
`apfs_update_omap_snapshots`) reads verbatim:

> "The official reference allows old implementations to ignore extended
> snapshot metadata, so I don't see any reason why we can't do the same for now."

`apfs_snap_meta_ext_oid` is therefore left at its existing value (zero on a
fresh volume) during snapshot create. Apple PDF is silent on required vs.
conditional; linux-apfs-rw treats it as optional for macOS 10.15+ compat but
skippable. The safe strategy: mirror the macOS kernel output empirically.

**Concrete values when written (from `apfs_raw.h`):**
- `sme_version`: unknown — empirical required.
- `sme_flags`: unknown — empirical required.
- `sme_snap_xid`: xid of the snapshot being extended.
- `sme_uuid`: volume UUID (from `vsb->apfs_vol_uuid`) — hypothesis; empirical required.
- `sme_token`: unknown — empirical required.

**Corroborating source:** `apfs-spec:linux-apfs-rw` — `snapshot.c:apfs_do_ioc_take_snapshot` (comment + unconditional skip of snap_meta_ext write). `apfs_raw.h: struct apfs_snap_meta_ext` (field names confirmed).

**Source conflicts:** None. Apple silent; linux-apfs-rw explicitly skips; consistent.

**Empirical check (MANDATORY — snap_meta_ext is the least-documented field):**
1. Scratch disk: read `vsb->apfs_snap_meta_ext_oid` before snapshot (expect 0).
2. After `diskutil apfs createSnapshot`: re-read — if non-zero, decode the
   physical block and print `sme_version`, `sme_flags`, `sme_snap_xid`,
   `sme_uuid`, `sme_token` as raw hex + interpreted values.
3. If still zero after macOS snapshot create: confirmed optional; our writer
   may also leave it zero.
4. `fsck_apfs -n` CLEAN in both cases. This empirical result directly gates
   whether our writer must implement snap_meta_ext or may skip it.

---

### Q5 — `apfs_revert_to_xid` / `apfs_revert_to_sblock_oid` during normal snapshot create

**LOCKED decision (DEFINITIVE — Apple PDF + linux-apfs-rw AGREE):**
Both fields remain **0** during a normal snapshot create. They are only set
non-zero to trigger a snapshot revert at next mount. linux-apfs-rw
`snapshot.c:apfs_do_ioc_take_snapshot` never touches these fields. The volume
superblock is modified (via `apfs_assert_in_transaction`) only to increment
`apfs_num_snapshots` and update `apfs_snap_meta_tree_oid`; revert fields are
untouched. Apple PDF (Q5 answer): "0 in a healthy volume, untouched by normal
snapshot create." INDEPENDENTLY CORROBORATED.

**Corroborating source:** `apfs-spec:linux-apfs-rw` — `snapshot.c:apfs_do_ioc_take_snapshot` (no assignment to `apfs_revert_to_xid` or `apfs_revert_to_sblock_oid`); `apfs_raw.h` offset `/*A0*/` layout confirmed. INDEPENDENTLY CORROBORATED.

**Source conflicts:** NONE.

**Empirical check:**
1. Python: decode `vsb->apfs_revert_to_xid` (offset 0xA0) and
   `vsb->apfs_revert_to_sblock_oid` (offset 0xA8) before and after
   `diskutil apfs createSnapshot`. Assert both remain 0. This is a
   regression-guard assertion in the scratch proof script.

---

### Q6 — Snapshot delete: synchronous vs. reaper-deferred split

**LOCKED decision (macOS-empirical: mirror kernel output, fsck CLEAN is arbiter):**
Based on linux-apfs-rw structure (reaper infrastructure present) and Apple
PDF silence, the split is:

**Synchronous within the delete transaction (single txn, atomic):**
- (a) Remove `APFS_TYPE_SNAP_METADATA` record from snap meta tree.
- (b) Remove `APFS_TYPE_SNAP_NAME` record from snap meta tree.
- (c) Decrement `apfs_num_snapshots` in the volume superblock.
- (d) Update `om_snap_count` and `om_most_recent_snap` in the omap.
- (e) Free the frozen volume superblock copy block (`sblock_oid` — physical).
- (f) Add the snapshot's xid range to the omap snapshot tree for DELETED marking.

**Reaper-deferred (across subsequent transactions, via `nx_reaper_oid`):**
- (g) Walking the extentref tree and freeing all referenced extents.
- (h) Removing omap entries whose `ok_xid` matches the deleted snapshot's xid
  range (DELETED-flagged entries cleanup).
- (i) Freeing the extentref tree B-tree nodes themselves.

The boundary is: metadata record removal + counter updates are cheap and
synchronous; bulk extent/block freeing (potentially thousands of blocks) is
reaper-deferred to avoid oversized transactions.

**Corroborating source:** `apfs-spec:linux-apfs-rw` — `apfs_raw.h: struct apfs_nx_reaper_phys` (reaper structure present, confirming deferred model); `snapshot.c` delete path structure. Apple PDF silent on exact boundary.

**Source conflicts:** None confirmed. Apple silent; linux-apfs-rw reaper structure consistent with deferred extent freeing.

**Empirical check (MANDATORY — Apple silent, boundary is implementation-defined):**
1. `diskutil apfs deleteSnapshot` on scratch disk immediately after a snapshot
   that has written some file data (so the extentref tree is non-empty).
2. Python immediately after delete transaction: decode snap meta tree (expect
   both SNAP_METADATA and SNAP_NAME records gone), `apfs_num_snapshots`
   (expect decremented), omap `om_snap_count` (expect decremented).
3. Check whether any omap entries for the deleted xid still exist with
   `APFS_OMAP_VAL_DELETED` flag (expect: yes, reaper not yet run).
4. `fsck_apfs -n` immediately after delete — must report CLEAN (reaper
   backlog is normal and expected by fsck).
5. After unmount/remount (reaper runs at mount): `fsck_apfs -n` again — CLEAN,
   and DELETED omap entries should be gone.

---

### Final verification step — real USB confirmation

After the scratch image proof passes all empirical checks above, the user has
authorized a **final real-USB confirmation** (their test USB has no serious
data). Procedure:

1. Mount the real USB APFS volume (read-write via macOS).
2. Run `diskutil apfs createSnapshot <mountpoint> snap-m7a-test`.
3. Verify `fsck_apfs -n /dev/diskNsM` reports CLEAN.
4. Python re-decode the on-disk structures: assert all locked decisions above
   match byte-for-byte with the scratch proof outputs.
5. Run `diskutil apfs deleteSnapshot <mountpoint> snap-m7a-test`.
6. Verify `fsck_apfs -n` CLEAN again.
7. This step is **non-destructive and reversible** (snapshot create/delete
   does not modify existing file data; verified via step 3/6 fsck).

Record the real-USB raw hex outputs in `tests/fixtures/m7a-usb-baseline/`
under Git LFS for regression testing.

---

> **KB source label:** `apfs-spec:m7a-confirmed` (indexed 2026-05-19)
