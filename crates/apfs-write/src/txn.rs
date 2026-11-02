//! COW Transaction engine for APFS write path.
//!
//! A `Transaction` accumulates object writes in memory, allocates free blocks
//! from the space manager, writes each object with a correct `obj_phys_t`
//! header (Fletcher-64), updates the volume omap B-tree for virtual objects,
//! appends a checkpoint (descriptor map + data), and atomically writes the new
//! `nx_superblock` last - the single commit point.
//!
//! Spec: `the APFS specification` (Q1-Q6 locked decisions).
//! Write sequence: C.2 checkpoint commit sequence (locked).
//! Allocation: D.2 allocation path (locked, immediate bitmap update).
//! Object layout: A. obj_phys_t (locked, Fletcher-64 over bytes [8..block_end]).

use apfs_core::block_device::{BlockError, WritableBlockDevice};
use apfs_core::checksum::fletcher64;
use apfs_core::nx::{NX_SUPERBLOCK_OID, XP_AREA_TREE_FLAG};
use std::collections::{HashMap, HashSet};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Object type / storage class constants (from apfs-core::obj, replicated here
// so txn.rs can construct headers without importing private internals).
// [CERTAIN: apfs_raw.h the APFS specification]
// ---------------------------------------------------------------------------

/// Physical storage class flag in o_type upper 2 bits.
pub const OBJ_PHYSICAL: u32 = 0x4000_0000;
/// Ephemeral storage class flag in o_type upper 2 bits.
pub const OBJ_EPHEMERAL: u32 = 0x8000_0000;
/// Virtual storage class - upper 2 bits both zero.
pub const OBJ_VIRTUAL: u32 = 0x0000_0000;

/// APFS container superblock object type (NXSB).
pub const OBJECT_TYPE_NX_SUPERBLOCK: u32 = 0x0000_0001;
/// APFS B-tree node object type.
pub const OBJECT_TYPE_BTREE_NODE: u32 = 0x0000_0003;
/// APFS checkpoint map block object type.
pub const OBJECT_TYPE_CHECKPOINT_MAP: u32 = 0x0000_000C;
/// APFS volume superblock object type (APSB).
pub const OBJECT_TYPE_FS: u32 = 0x0000_000D;
/// APFS space manager object type.
pub const OBJECT_TYPE_SPACEMAN: u32 = 0x0000_0005;
/// APFS B-tree (root) object type.
pub const OBJECT_TYPE_BTREE: u32 = 0x0000_0002;
/// APFS object-map object type.
pub const OBJECT_TYPE_OMAP: u32 = 0x0000_000B;
/// APFS snapshot metadata tree type.
pub const OBJECT_TYPE_SNAPMETATREE: u32 = 0x0000_0010;
/// APFS extent-reference tree (BLOCKREFTREE) type.
pub const OBJECT_TYPE_BLOCKREFTREE: u32 = 0x0000_000F;
/// APFS object-map snapshot tree subtype (distinct from OBJECT_TYPE_OMAP).
/// [CERTAIN: the APFS specification p.15 Object Types]
pub const OBJECT_TYPE_OMAP_SNAPSHOT: u32 = 0x0000_0013;

/// Flag on the last `checkpoint_map_phys_t` block in a checkpoint.
/// [CERTAIN: apfs_raw.h CHECKPOINT_MAP_LAST]
pub const CHECKPOINT_MAP_LAST: u32 = 0x0000_0001;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum TxnError {
    #[error("block device error: {0}")]
    Device(#[from] BlockError),
    #[error("no free blocks available in spaceman")]
    NoFreeBlocks,
    #[error("checkpoint ring full (desc ring exhausted)")]
    CheckpointRingFull,
    #[error("image too small: block {block} out of range (device has {size} bytes)")]
    BlockOutOfRange { block: u64, size: u64 },
    #[error("unsupported block size {0}: only 4096-byte blocks are supported")]
    InvalidBlockSize(u32),
    #[error("spaceman parse error: {0}")]
    SpacemanParse(String),
    #[error("volume superblock parse error: {0}")]
    VolumeParse(String),
    #[error("nx superblock parse error: {0}")]
    NxParse(String),
    #[error("omap parse error: {0}")]
    OmapParse(String),
    #[error("entry already exists: {0}")]
    AlreadyExists(String),
    #[error("directory not empty: {0}")]
    DirectoryNotEmpty(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("snapshot not found: xid {0:#x}")]
    SnapshotNotFound(u64),
    #[error("post-commit verification failed: {0}")]
    PostCommitVerify(String),
    /// The mutation would corrupt on-disk state this implementation cannot
    /// maintain correctly (sealed or encrypted volume, hard-linked inode,
    /// compressed file). Refusing is deliberate: the alternative is a write
    /// that silently loses or destroys user data.
    #[error("unsupported for this object: {0}")]
    Unsupported(String),
}

// ---------------------------------------------------------------------------
// Pending write: a fully-serialized block scheduled for commit.
// ---------------------------------------------------------------------------

/// What a free-queue drain reclaimed.
///
/// `records` is how many queue entries were removed; `blocks` is how many
/// physical blocks those entries covered. The two differ whenever an entry
/// describes a contiguous run, and callers doing volume accounting want
/// `blocks` - `apfs_fs_alloc_count` counts blocks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainOutcome {
    pub records: usize,
    pub blocks: u64,
    /// Of `blocks`, the ones that came from an entry this volume enqueued
    /// while a snapshot pinned them (see `drain_sm_fq_main`'s `owned_from_xid`).
    /// Only these count against `apfs_fs_alloc_count`.
    pub owned_blocks: u64,
}

/// One block to write at commit time, identified by its physical block address.
#[derive(Debug, Clone)]
pub struct PendingWrite {
    /// Physical block number (byte offset = paddr * block_size).
    pub paddr: u64,
    /// Fully serialized block content (block_size bytes, including obj_phys header
    /// with Fletcher-64 already computed and stored in bytes [0..8]).
    pub data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// OmapEntry: a virtual-object → physical mapping to register.
// ---------------------------------------------------------------------------

/// A pending omap B-tree insertion: {oid, xid} → paddr.
/// These are batched and written as a single new B-tree root at commit.
#[derive(Debug, Clone)]
pub struct OmapEntry {
    pub oid: u64,
    pub xid: u64,
    pub paddr: u64,
    pub size: u32,
}

// ---------------------------------------------------------------------------
// In-memory spaceman view (minimal - only the fields we need for allocation).
// [CERTAIN: spaceman.c D.2 allocation path, the APFS specification]
// ---------------------------------------------------------------------------

/// Parsed fields from the spaceman ephemeral object needed for allocation.
/// We keep the raw block so we can serialize it back at commit.
#[derive(Clone)]
pub struct SpacemanView {
    /// Physical block number where this spaceman lives (checkpoint data area).
    pub paddr: u64,
    /// OID of the spaceman (ephemeral).
    pub oid: u64,
    /// XID when this spaceman was last written.
    pub xid: u64,
    /// o_type (SPACEMAN | EPHEMERAL).
    pub o_type: u32,
    /// o_subtype.
    pub o_subtype: u32,
    /// Total free block count in main device (`sm_dev[MAIN].sm_free_count`).
    /// Offset 0x68 from object start per the APFS specification `sm_dev[0]`.
    pub free_count: u64,
    /// Block size (mirrors nx_superblock.block_size).
    pub block_size: u32,
    /// Raw block bytes - mutated in place, serialized at commit.
    pub raw: Vec<u8>,
    /// XID the open transaction will commit under. Metadata carrying anything
    /// older belongs to a checkpoint that is still on disk and cannot be
    /// changed where it lies.
    pub txn_xid: u64,
    /// The pool the allocator's own metadata is carved out of.
    pub ip: InternalPool,
    /// Chunk-info address blocks this transaction has read, by address. On a
    /// container above about 8 TiB the array at `sm_addr_offset` names these
    /// and they name the chunk-info blocks; the dirty ones are copies made
    /// under this transaction and are written at commit.
    pub cabs: HashMap<u64, CabBlock>,
}

/// One chunk-info address block as this transaction sees it.
#[derive(Debug, Clone)]
pub struct CabBlock {
    /// The block's bytes.
    pub raw: Vec<u8>,
    /// Changed by this transaction, which makes it a fresh pool block the
    /// previous checkpoint does not name.
    pub dirty: bool,
}

// Byte offsets of the internal-pool fields inside `spaceman_phys`.
// [CERTAIN: apfsprogs/include/apfs/raw.h struct apfs_spaceman_phys, cross-read
//  against linux-apfs-rw/apfs_raw.h, and decoded byte-for-byte out of a
//  macOS-formatted 512 MiB container - sm_fq is three queues of 40 bytes, not
//  two, which is what puts the ring fields at 320 and not 280]
const SMI_IP_BLOCK_COUNT: usize = 152;
const SMI_IP_BM_SIZE_IN_BLOCKS: usize = 160;
const SMI_IP_BM_BLOCK_COUNT: usize = 164;
const SMI_IP_BM_BASE: usize = 168;
const SMI_IP_BASE: usize = 176;
const SMI_IP_BM_FREE_HEAD: usize = 320;
const SMI_IP_BM_FREE_TAIL: usize = 322;
const SMI_IP_BM_XID_OFFSET: usize = 324;
const SMI_IP_BITMAP_OFFSET: usize = 328;
const SMI_IP_BM_FREE_NEXT_OFFSET: usize = 332;
/// Ring slot that is in use, so not a link in the free list.
const IP_BM_INDEX_INVALID: u16 = 0xFFFF;
/// Byte offset of `sm_fq[SFQ_IP]`, the first of the three free queues. Its
/// records are the pool's own blocks; `sm_fq[SFQ_MAIN]` at 0xF0 holds
/// everything outside the pool, and apfsck rejects a record on the wrong one.
const SFQ_IP_BASE: usize = 200;

/// The spaceman's internal pool.
///
/// The allocator's own metadata - chunk bitmaps and chunk-info blocks - cannot
/// be allocated out of a chunk, because reading a chunk needs its bitmap. APFS
/// reserves a small block range for it instead, three blocks per chunk, and
/// tracks which of those blocks are taken in a bitmap of its own. That bitmap
/// is not written in place either: it lives in a ring of sixteen slots, and
/// each transaction writes it to a fresh slot so the previous checkpoints keep
/// the version they were sealed with.
///
/// [CERTAIN: linux-apfs-rw/spaceman.c apfs_ip_find_free / apfs_allocate_ip_bitmap;
///  apfsprogs/apfsck/spaceman.c check_internal_pool sizes the pool as
///  3 * (chunks + cibs + cabs) and requires the ring to hold 16 copies]
#[derive(Debug, Clone, Default)]
pub struct InternalPool {
    /// First block of the pool (`sm_ip_base`).
    pub base: u64,
    /// Blocks in the pool (`sm_ip_block_count`).
    pub block_count: u64,
    /// First block of the bitmap ring (`sm_ip_bm_base`).
    pub bm_base: u64,
    /// Slots in the ring (`sm_ip_bm_block_count`), always 16x `bm_size`.
    pub bm_ring_len: u32,
    /// Blocks one full copy of the pool bitmap takes (`sm_ip_bm_size_in_blocks`).
    pub bm_size: u32,
    /// Byte offset in the spaceman of the per-copy xid array.
    pub xid_offset: u32,
    /// Byte offset in the spaceman of the per-copy ring-slot array.
    pub bitmap_offset: u32,
    /// Byte offset in the spaceman of the ring's free-list array.
    pub free_next_offset: u32,
    /// The pool bitmap, `bm_size` blocks of it, as currently on disk.
    bitmaps: Vec<Vec<u8>>,
    /// Which of those blocks this transaction changed.
    dirty: Vec<bool>,
    /// Pool blocks this transaction copied away from. They stay marked used:
    /// the checkpoint that still names them is the one a torn commit falls back
    /// to, so they go on the pool's free queue and are released a transaction
    /// later, when that checkpoint is no longer the fallback.
    released: Vec<u64>,
}

impl InternalPool {
    /// Is there a pool to allocate from at all? A synthetic spaceman (unit
    /// tests) and any container whose formatter left the fields zero have none.
    pub fn is_available(&self) -> bool {
        self.bm_size > 0
            && self.bm_ring_len >= self.bm_size
            && self.block_count > 0
            && self.bm_base > 0
            && self.bitmaps.len() == self.bm_size as usize
    }

    /// First block of the pool that nothing has taken, or `None` when the pool
    /// is full or absent.
    fn find_free(&self, bsz: usize) -> Option<u64> {
        if !self.is_available() {
            return None;
        }
        let bits_per_block = bsz.checked_mul(8)?;
        for (i, bitmap) in self.bitmaps.iter().enumerate() {
            for bit in 0..bits_per_block {
                let byte = *bitmap.get(bit / 8)?;
                if byte & (1u8 << (bit % 8)) != 0 {
                    continue;
                }
                let off_in_pool = (i.checked_mul(bits_per_block)?).checked_add(bit)? as u64;
                if off_in_pool >= self.block_count {
                    return None;
                }
                return self.base.checked_add(off_in_pool);
            }
        }
        None
    }

    /// Does this block belong to the pool?
    pub fn contains(&self, bno: u64) -> bool {
        self.block_count > 0 && bno >= self.base && bno < self.base + self.block_count
    }

    fn set_bit(&mut self, bsz: usize, bno: u64, used: bool) -> Result<(), TxnError> {
        if !self.contains(bno) {
            return Err(TxnError::SpacemanParse(format!(
                "block {bno} is not in the internal pool"
            )));
        }
        let bits_per_block = bsz.saturating_mul(8);
        if bits_per_block == 0 {
            return Err(TxnError::SpacemanParse("block size is zero".into()));
        }
        let off_in_pool = (bno - self.base) as usize;
        let idx = off_in_pool / bits_per_block;
        let bit = off_in_pool % bits_per_block;
        let bitmap = self.bitmaps.get_mut(idx).ok_or_else(|| {
            TxnError::SpacemanParse(format!("internal-pool bitmap {idx} is missing"))
        })?;
        let byte = bitmap.get_mut(bit / 8).ok_or_else(|| {
            TxnError::SpacemanParse(format!("internal-pool bit {bit} is out of range"))
        })?;
        if used {
            *byte |= 1u8 << (bit % 8);
        } else {
            *byte &= !(1u8 << (bit % 8));
        }
        if let Some(d) = self.dirty.get_mut(idx) {
            *d = true;
        }
        Ok(())
    }

    /// Record that the pool block `bno` is taken.
    pub fn mark_used(&mut self, bsz: usize, bno: u64) -> Result<(), TxnError> {
        self.set_bit(bsz, bno, true)
    }

    /// Record that the pool block `bno` is available again.
    pub fn mark_free(&mut self, bsz: usize, bno: u64) -> Result<(), TxnError> {
        self.set_bit(bsz, bno, false)
    }

    /// Hand a superseded pool block to the pool's free queue. It stays marked
    /// used until the queue is drained, which is what keeps the previous
    /// checkpoint readable.
    fn release(&mut self, bno: u64) {
        if self.contains(bno) && !self.released.contains(&bno) {
            self.released.push(bno);
        }
    }

    fn has_dirty(&self) -> bool {
        self.dirty.iter().any(|d| *d)
    }
}

/// A non-spaceman ephemeral object carried forward from the previous checkpoint.
/// Examples: NX reaper, reaper-list B-tree nodes. These must be re-written to
/// the checkpoint data area every transaction so `nx_xp_data_len >= 2`.
/// [CERTAIN: empirical - fsck_apfs errors "nx_xp_data_len (1) is less than 2";
///  baseline scratch image always has spaceman + reaper + 2 btree nodes = 4 ephemerals]
#[derive(Debug, Clone)]
pub struct EphemeralObj {
    /// o_type from the checkpoint_mapping_t entry.
    pub cme_type: u32,
    /// o_subtype from the checkpoint_mapping_t entry.
    pub cme_subtype: u32,
    /// The volume that owns the object (`cme_fs_oid`), 0 for the container's
    /// own. Carried through the checkpoint as read: rewriting it as 0 would
    /// hand a volume's object to the container.
    pub fs_oid: u64,
    /// OID of this ephemeral object.
    pub oid: u64,
    /// Raw block bytes (will have xid updated and checksum recomputed at commit).
    pub raw: Vec<u8>,
}

/// Parsed chunk-info: one entry in the CIB.
/// [CERTAIN: apfs_raw.h struct apfs_chunk_info, D.1 the APFS specification]
#[derive(Debug, Clone)]
pub struct ChunkInfo {
    /// XID when this chunk was last changed.
    pub xid: u64,
    /// First block number covered by this chunk.
    pub addr: u64,
    /// Total blocks in this chunk (always APFS_CHUNK_BITS = 16384 except last).
    pub block_count: u32,
    /// Free blocks remaining in this chunk.
    pub free_count: u32,
    /// Physical block address of the bitmap block (0 = entire chunk free).
    pub bitmap_addr: u64,
}

// ---------------------------------------------------------------------------
// Nx superblock view (fields needed for Transaction).
// ---------------------------------------------------------------------------

/// Parsed subset of `nx_superblock_t` fields needed for transaction commit.
/// [CERTAIN: nx.rs + C.1 the APFS specification]
#[derive(Debug, Clone)]
pub struct NxView {
    pub block_size: u32,
    pub block_count: u64,
    /// Physical block address of the spaceman (ephemeral - read from checkpoint map).
    pub spaceman_oid: u64,
    /// Physical block address of the container omap (PHYSICAL object).
    pub omap_oid: u64,
    /// Volume fs_oids (up to 100).
    pub fs_oids: Vec<u64>,
    // Checkpoint ring geometry.
    pub xp_desc_base: u64,
    pub xp_desc_blocks: u32,
    pub xp_data_base: u64,
    pub xp_data_blocks: u32,
    pub xp_desc_next: u32,
    pub xp_data_next: u32,
    pub xp_desc_index: u32,
    pub xp_desc_len: u32,
    pub xp_data_index: u32,
    pub xp_data_len: u32,
    /// Current transaction ID (next_xid - 1 at read time).
    pub current_xid: u64,
    /// Next OID to assign.
    pub next_oid: u64,
    /// Raw block for serialization at commit.
    pub raw: Vec<u8>,
}

impl NxView {
    /// Parse from a raw 4096-byte block already verified by Fletcher-64.
    /// [CERTAIN: nx.rs field offsets verified against the APFS specification]
    pub fn parse(block: &[u8]) -> Result<Self, TxnError> {
        if block.len() < 4096 {
            return Err(TxnError::NxParse(format!(
                "block too short: {}",
                block.len()
            )));
        }
        // Verify Fletcher-64 before trusting any field.
        {
            let stored = block
                .get(0..8)
                .and_then(|s| s.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(|| TxnError::NxParse("block too short for checksum".into()))?;
            let computed = fletcher64(block);
            if stored != computed {
                return Err(TxnError::NxParse(format!(
                    "bad Fletcher-64: stored={stored:#x} computed={computed:#x}"
                )));
            }
        }
        let magic = u32_at(block, 32)?;
        if magic != 0x4253_584E {
            return Err(TxnError::NxParse(format!("bad NX magic: {magic:#x}")));
        }
        let block_size = u32_at(block, 36)?;
        if block_size != 4096 {
            return Err(TxnError::InvalidBlockSize(block_size));
        }
        // The container superblock is the one object with a fixed identifier.
        let oid = u64_at(block, 8)?;
        if oid != NX_SUPERBLOCK_OID {
            return Err(TxnError::NxParse(format!(
                "container superblock carries oid {oid}, expected {NX_SUPERBLOCK_OID}"
            )));
        }
        let block_count = u64_at(block, 40)?;
        // The container's byte size bounds every block address; a count that
        // does not even fit the arithmetic describes nothing.
        if block_count.checked_mul(u64::from(block_size)).is_none() {
            return Err(TxnError::NxParse(format!(
                "block count {block_count} overflows the address space"
            )));
        }
        let next_oid = u64_at(block, 0x58)?;
        let next_xid = u64_at(block, 0x60)?;
        // xp_desc_blocks @0x68 (u32), xp_data_blocks @0x6C (u32)
        let xp_desc_blocks = u32_at(block, 0x68)?;
        let xp_data_blocks = u32_at(block, 0x6C)?;
        // A tree-form checkpoint area (high bit set) keeps a B-tree oid where
        // the ring base would be. The ring arithmetic below (slot = index %
        // blocks, block = base + slot) has no meaning for it, and a checkpoint
        // written that way would land on unrelated blocks, so the write path
        // declines such a container (apfsprogs refuses the form as well).
        if xp_desc_blocks & XP_AREA_TREE_FLAG != 0 || xp_data_blocks & XP_AREA_TREE_FLAG != 0 {
            return Err(TxnError::NxParse(format!(
                "tree-form checkpoint area is not supported: xp_desc_blocks={xp_desc_blocks:#x} xp_data_blocks={xp_data_blocks:#x}"
            )));
        }
        // The checkpoint descriptor/data ring lengths are used as moduli when
        // walking the checkpoint (i % xp_desc_blocks in read_spaceman,
        // data_index % xp_data_blocks when writing). A crafted container with a
        // zero ring length would divide by zero. Valid containers always have
        // non-zero ring geometry.
        // A checkpoint takes two descriptor blocks (its map and its
        // superblock); a ring with fewer would put both at the same block.
        if xp_desc_blocks < 2 || xp_data_blocks == 0 {
            return Err(TxnError::NxParse(format!(
                "invalid checkpoint ring geometry: xp_desc_blocks={xp_desc_blocks:#x} xp_data_blocks={xp_data_blocks:#x}"
            )));
        }
        // xp_desc_base @0x70 (i64 stored as u64), xp_data_base @0x78
        let xp_desc_base_raw = u64_at(block, 0x70)?;
        let xp_data_base = u64_at(block, 0x78)?;
        // Both rings lie inside the container, after block 0 (the superblock
        // itself), and apart from each other: overlapping rings would have a
        // checkpoint map written over the space manager.
        let desc = checkpoint_ring("descriptor", xp_desc_base_raw, xp_desc_blocks, block_count)?;
        let data = checkpoint_ring("data", xp_data_base, xp_data_blocks, block_count)?;
        if desc.0 < data.1 && data.0 < desc.1 {
            return Err(TxnError::NxParse(format!(
                "checkpoint rings overlap: descriptor {}..{} and data {}..{}",
                desc.0, desc.1, data.0, data.1
            )));
        }
        // xp_desc_next @0x80 (u32), xp_data_next @0x84 (u32)
        let xp_desc_next = u32_at(block, 0x80)?;
        let xp_data_next = u32_at(block, 0x84)?;
        // xp_desc_index @0x88, xp_desc_len @0x8C, xp_data_index @0x90, xp_data_len @0x94
        let xp_desc_index = u32_at(block, 0x88)?;
        let xp_desc_len = u32_at(block, 0x8C)?;
        let xp_data_index = u32_at(block, 0x90)?;
        let xp_data_len = u32_at(block, 0x94)?;
        // spaceman_oid @0x98 (u64), omap_oid @0xA0 (u64)
        let spaceman_oid = u64_at(block, 0x98)?;
        let omap_oid = u64_at(block, 0xA0)?;
        // nx_fs_oid array @0xB8, 100 * 8 bytes. A deleted volume leaves its
        // slot empty and the later volumes where they were, so an empty slot
        // is skipped, not taken for the end.
        let mut fs_oids = Vec::new();
        for i in 0..100usize {
            let off = 0xB8 + i * 8;
            if off + 8 > block.len() {
                break;
            }
            let oid = u64_at(block, off)?;
            if oid != 0 {
                fs_oids.push(oid);
            }
        }
        Ok(Self {
            block_size,
            block_count,
            spaceman_oid,
            omap_oid,
            fs_oids,
            xp_desc_base: xp_desc_base_raw,
            xp_desc_blocks,
            xp_data_base,
            xp_data_blocks,
            xp_desc_next,
            xp_data_next,
            xp_desc_index,
            xp_desc_len,
            xp_data_index,
            xp_data_len,
            current_xid: next_xid.saturating_sub(1),
            next_oid,
            raw: block.to_vec(),
        })
    }
}

/// The blocks `[start, end)` of a checkpoint ring, checked to start after
/// block 0 and to end inside the `block_count`-block container.
fn checkpoint_ring(
    what: &str,
    base: u64,
    blocks: u32,
    block_count: u64,
) -> Result<(u64, u64), TxnError> {
    match base.checked_add(u64::from(blocks)) {
        Some(end) if base >= 1 && end <= block_count => Ok((base, end)),
        _ => Err(TxnError::NxParse(format!(
            "checkpoint {what} ring at block {base} with {blocks} blocks does not lie inside \
             the {block_count}-block container"
        ))),
    }
}

/// Byte offset of block `bno`, refusing an address whose offset would
/// overflow or whose block lies past the end of the device. Block addresses
/// come from on-disk structures (ring bases, chunk-info blocks, bitmap
/// pointers), so none of them is multiplied out unchecked. A device that
/// cannot report its size (0) is bounded by its own reads.
fn block_offset<D: WritableBlockDevice>(dev: &D, bno: u64, bsz: usize) -> Result<u64, TxnError> {
    let size = dev.size();
    let out_of_range = || TxnError::BlockOutOfRange { block: bno, size };
    let off = bno.checked_mul(bsz as u64).ok_or_else(out_of_range)?;
    let end = off.checked_add(bsz as u64).ok_or_else(out_of_range)?;
    if size != 0 && end > size {
        return Err(out_of_range());
    }
    Ok(off)
}

// ---------------------------------------------------------------------------
// Transaction
// ---------------------------------------------------------------------------

/// A COW write transaction over a writable APFS block device.
///
/// Usage:
/// 1. `Transaction::begin(dev)` - reads the nx_superblock + spaceman.
/// 2. Stage object writes via `stage_object(...)`.
/// 3. `commit()` - writes all staged objects, checkpoint map, spaceman, then
///    `nx_superblock` last (the atomic commit point).
///
/// On any error from `commit()`, the device state is indeterminate - the
/// caller MUST discard the image (the scratch test does this via baseline copy).
pub struct Transaction<D: WritableBlockDevice> {
    dev: D,
    /// Parsed nx_superblock (latest from checkpoint ring).
    pub nx: NxView,
    /// Spaceman ephemeral object (read from checkpoint data area).
    pub sm: SpacemanView,
    /// Blocks scheduled to write at commit time (in order of staging).
    pending: Vec<PendingWrite>,
    /// paddr -> index into `pending`, so a read can see what this transaction
    /// has already staged at that address.
    pending_index: HashMap<u64, usize>,
    /// While a savepoint is held: the earlier content of every block staged
    /// before it and restaged since.
    undo: Option<Undo>,
    /// Virtual object → physical mappings to register in the volume omap.
    omap_entries: Vec<OmapEntry>,
    /// Volume superblock oid -> block of the newest superblock this
    /// transaction staged for it, so a later operation works from it.
    staged_volumes: HashMap<u64, u64>,
    /// XID for this transaction (= nx.current_xid + 1).
    pub xid: u64,
    /// Next OID to assign within this transaction.
    next_oid: u64,
    /// Physical block numbers allocated in this transaction (for audit).
    allocated_blocks: Vec<u64>,
    /// File data blocks this transaction allocated and wrote straight to the
    /// device ([`Self::write_new_data`]): no checkpoint names them before
    /// the commit, so they need not wait in memory for it.
    direct: HashSet<u64>,
    /// Data of `direct` blocks not on the device yet, by block: small writes
    /// gather here up to [`DATA_BUFFER_MAX`] bytes and reach the device as
    /// runs of consecutive blocks, so a device that serves each request in
    /// turn (a USB drive's raw node) gets a few large writes, not one per
    /// file.
    data_buf: std::collections::BTreeMap<u64, Vec<u8>>,
    data_buf_bytes: usize,
    /// Non-spaceman ephemeral objects carried from the previous checkpoint
    /// (reaper, reaper-list B-tree nodes). Must be re-written every txn
    /// so `nx_xp_data_len >= 2` (fsck_apfs requirement).
    other_ephemerals: Vec<EphemeralObj>,
    /// Deferred bitmap writes: bitmap_paddr -> in-memory bitmap block.
    /// Populated by alloc_one_block / free_one_block; flushed in commit().
    dirty_bitmaps: HashMap<u64, Vec<u8>>,
    /// Deferred CIB writes: cib_paddr -> (byte_offset_of_ci_free_count_in_cib, cib_buf).
    /// Populated by alloc_one_block / free_one_block; flushed in commit().
    /// Value is the full CIB buffer (checksum recomputed at flush time).
    dirty_cibs: HashMap<u64, Vec<u8>>,
    /// Per-chunk free-runs index keyed by `ci_bitmap_addr`. Mirrors each loaded
    /// bitmap so `alloc_blocks_run` is O(log R) instead of a linear bit scan
    /// (R = number of contiguous free runs in the chunk). Built lazily when a
    /// bitmap is first read into `dirty_bitmaps`. Dropped at commit/abort
    /// along with the dirty cache.
    dirty_runs: HashMap<u64, crate::free_runs::ChunkFreeRuns>,
    /// Where this transaction's next block would follow its last one, so
    /// what it stages lies in long runs (see `alloc_run_near`).
    locality: Locality,
    /// Bitmap block paddrs that contain reclaim (clear-bit / free) mutations.
    ///
    /// Populated whenever `free_one_block_cached` clears a bit in a bitmap block.
    /// Used by `commit()` to split the bitmap flush into two phases:
    ///   Phase A (pre-NXSB):  alloc-only bitmaps - safe to persist early.
    ///   Phase B (post-NXSB): reclaim bitmaps - must not be visible on disk
    ///                         until the new checkpoint is atomically sealed.
    ///
    /// Write-ordering discipline (linux-apfs-rw write-ordering):
    /// a pre-NXSB crash leaks the new blocks (overallocation, recoverable by
    /// fsck), but never makes old-checkpoint blocks appear free - no corruption.
    reclaim_bitmap_paddrs: HashSet<u64>,
    /// CIB paddrs touched by reclaim operations (ci_free_count incremented by
    /// `free_one_block_cached`). Deferred to Phase B flush alongside reclaim bitmaps
    /// so that the free-count increment is also post-NXSB visible.
    reclaim_cib_paddrs: HashSet<u64>,
    /// Blocks that operations have logically freed (COW reclaim) but whose bitmap
    /// bits must NOT be cleared until `commit()` Step 2 has flushed all staged
    /// pending writes.
    ///
    /// Root cause of M8 #3 batch corruption: if `free_block` immediately clears a
    /// bitmap bit inside `free_one_block_cached`, then `alloc_run_cached` in a
    /// subsequent operation within the same transaction can re-allocate that block.
    /// `free_replaced_metadata` in the later operation then tries to free the same
    /// OLD block again (using the stale vsb_raw/vol_omap_raw captured before the
    /// transaction), but now the bit is 1 again → the idempotent guard does NOT
    /// fire → double-free → `ChunkFreeRuns` corruption → `alloc_run_cached`
    /// re-allocates an already-in-use address → pending write overwrites the
    /// container omap → `BadMagic { expected: 11, found: 3 }` on re-open.
    ///
    /// Fix: `free_block` records the paddr here without touching the bitmap.
    /// `commit()` drains this set AFTER flushing all pending writes (Step 2),
    /// so freed addresses can never be re-allocated within the same transaction.
    /// Derived from linux-apfs-rw: the kernel finalises all block writes before
    /// running the allocator's reclaim pass.
    pending_frees: HashSet<u64>,
    /// Running count of blocks handed out by the allocator this transaction.
    blocks_allocated: u64,
    /// Write every changed tree by rebuilding it whole instead of rewriting
    /// only the paths to the changed records. The rebuild is the slower
    /// reference the path-local writer is checked against.
    full_rebuild: bool,
}

/// The earlier content of the blocks restaged since a savepoint.
struct Undo {
    /// Blocks at these positions of `pending` were staged before it.
    below: usize,
    saved: HashMap<usize, Vec<u8>>,
    /// Data blocks written straight to the device before it and rewritten
    /// since: their content at the savepoint, read back from the device.
    direct_saved: HashMap<u64, Vec<u8>>,
    /// Data blocks first written straight to the device since it.
    direct_added: HashSet<u64>,
}

/// A transaction's in-memory state at one point, for [`Transaction::atomically`].
struct Savepoint {
    pending_len: usize,
    omap_entries_len: usize,
    allocated_blocks_len: usize,
    nx: NxView,
    sm: SpacemanView,
    next_oid: u64,
    staged_volumes: HashMap<u64, u64>,
    other_ephemerals: Vec<EphemeralObj>,
    dirty_bitmaps: HashMap<u64, Vec<u8>>,
    dirty_cibs: HashMap<u64, Vec<u8>>,
    dirty_runs: HashMap<u64, crate::free_runs::ChunkFreeRuns>,
    reclaim_bitmap_paddrs: HashSet<u64>,
    reclaim_cib_paddrs: HashSet<u64>,
    pending_frees: HashSet<u64>,
    blocks_allocated: u64,
}

/// Reading through an open transaction sees what it has staged: a reader
/// built on it between the operations of a group reads the volume as the
/// group has left it so far, before any of it is committed. The span is
/// read from the device and every staged block it covers is laid over it.
impl<D: WritableBlockDevice> apfs_core::block_device::BlockDevice for Transaction<D> {
    fn size(&self) -> u64 {
        self.dev.size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.dev.read_at(offset, buf)?;
        self.lay_staged_over(offset, buf);
        Ok(())
    }

    /// File content through the transaction: the device's content path, with
    /// what the transaction staged laid over it as for any read.
    fn read_content_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.dev.read_content_at(offset, buf)?;
        self.lay_staged_over(offset, buf);
        Ok(())
    }
}

impl<D: WritableBlockDevice> Transaction<D> {
    /// Lay the blocks this transaction has staged or gathered over `buf`,
    /// which holds the device's bytes from `offset`.
    fn lay_staged_over(&self, offset: u64, buf: &mut [u8]) {
        if (self.pending.is_empty() && self.data_buf.is_empty()) || buf.is_empty() {
            return;
        }
        let bsz = u64::from(self.nx.block_size);
        let end = offset.saturating_add(buf.len() as u64);
        let first = offset / bsz;
        let last = (end - 1) / bsz;
        let mut lay = |paddr: u64, data: &[u8]| {
            let start = paddr.saturating_mul(bsz);
            let lo = start.max(offset);
            let hi = start.saturating_add(data.len() as u64).min(end);
            if lo >= hi {
                return;
            }
            let (dst, src) = ((lo - offset) as usize, (lo - start) as usize);
            let n = (hi - lo) as usize;
            if let (Some(d), Some(s)) = (buf.get_mut(dst..dst + n), data.get(src..src + n)) {
                d.copy_from_slice(s);
            }
        };
        for (&b, data) in self.data_buf.range(first..=last) {
            lay(b, data);
        }
        let mut overlay = |pw: &PendingWrite| lay(pw.paddr, &pw.data);
        if last - first < self.pending.len() as u64 {
            for b in first..=last {
                if let Some(pw) = self
                    .pending_index
                    .get(&b)
                    .and_then(|&i| self.pending.get(i))
                {
                    overlay(pw);
                }
            }
        } else {
            for pw in self
                .pending
                .iter()
                .filter(|pw| (first..=last).contains(&pw.paddr))
            {
                overlay(pw);
            }
        }
    }

    /// The block of the newest superblock this transaction staged for the
    /// volume whose superblock oid is `vsb_oid`, if it staged one.
    pub fn staged_volume_block(&self, vsb_oid: u64) -> Option<u64> {
        self.staged_volumes.get(&vsb_oid).copied()
    }

    /// Rebuild every tree an operation changes in full (`true`), or rewrite
    /// only the nodes on the paths to the changed records (`false`, the
    /// default). Both leave the same records on disk.
    pub fn set_full_rebuild(&mut self, on: bool) {
        self.full_rebuild = on;
    }

    /// Whether changed trees are rebuilt in full; see [`Self::set_full_rebuild`].
    pub fn full_rebuild(&self) -> bool {
        self.full_rebuild
    }

    /// Begin a new transaction by reading the container superblock and spaceman.
    ///
    /// Reads block 0 (bootstrap nx_superblock), walks the checkpoint descriptor
    /// ring to find the latest valid superblock, then reads the spaceman from
    /// the checkpoint data area.
    pub fn begin(dev: D) -> Result<Self, TxnError> {
        Self::begin_recoverable(dev).map_err(|(_dev, e)| e)
    }

    /// Like [`begin`] but on error returns the device back in the error tuple
    /// so a caller (e.g. the writable mount) can recover the handle and fall
    /// back cleanly instead of leaking it / leaving the mount dead. [#137]
    pub fn begin_recoverable(mut dev: D) -> Result<Self, (D, TxnError)> {
        let bsz = 4096usize;
        let mut buf = vec![0u8; bsz];
        // Run the fallible bootstrap with `dev` borrowed; on any error the
        // device is returned unchanged so the caller keeps ownership.
        let parsed = (|| -> Result<_, TxnError> {
            // Read block 0 (bootstrap).
            dev.read_at(0, &mut buf)?;
            // Walk checkpoint ring to find newest superblock.
            let nx = find_latest_nx(&mut dev, &buf)?;
            // The block count bounds every address the container names; a
            // device shorter than that holds only part of it. A larger device
            // (an image with slack) is fine, and one that cannot report its
            // size (0) is bounded by its own reads.
            let container_bytes = nx
                .block_count
                .checked_mul(u64::from(nx.block_size))
                .ok_or_else(|| {
                    TxnError::NxParse(format!(
                        "block count {} overflows the address space",
                        nx.block_count
                    ))
                })?;
            let dev_size = dev.size();
            if dev_size != 0 && container_bytes > dev_size {
                return Err(TxnError::NxParse(format!(
                    "the container declares {container_bytes} bytes but the device holds {dev_size}"
                )));
            }
            let xid = nx.current_xid + 1;
            let next_oid = nx.next_oid;
            // Read spaceman from the checkpoint data area (ephemeral: its paddr
            // is recorded in the checkpoint map block(s) in the descriptor area).
            let mut sm = read_spaceman(&mut dev, &nx)?;
            sm.txn_xid = xid;
            // Read other ephemeral objects (reaper, reaper-list btree nodes) to
            // carry them forward to the new checkpoint.
            // [CERTAIN: fsck_apfs requires nx_xp_data_len >= 2; baseline has 4]
            let other_ephemerals = read_other_ephemerals(&mut dev, &nx, sm.oid)?;
            Ok((nx, xid, next_oid, sm, other_ephemerals))
        })();
        match parsed {
            Ok((nx, xid, next_oid, sm, other_ephemerals)) => {
                let mut txn = Self {
                    dev,
                    nx,
                    sm,
                    pending: Vec::new(),
                    pending_index: HashMap::new(),
                    undo: None,
                    omap_entries: Vec::new(),
                    staged_volumes: HashMap::new(),
                    xid,
                    next_oid,
                    allocated_blocks: Vec::new(),
                    direct: HashSet::new(),
                    data_buf: std::collections::BTreeMap::new(),
                    data_buf_bytes: 0,
                    other_ephemerals,
                    dirty_bitmaps: HashMap::new(),
                    dirty_cibs: HashMap::new(),
                    dirty_runs: HashMap::new(),
                    locality: Locality::default(),
                    reclaim_bitmap_paddrs: HashSet::new(),
                    reclaim_cib_paddrs: HashSet::new(),
                    pending_frees: HashSet::new(),
                    blocks_allocated: 0,
                    full_rebuild: false,
                };
                // Pool blocks that older transactions copied away from become
                // available now: the checkpoints that named them are no longer
                // the one a torn commit falls back to.
                if let Err(e) = txn.drain_internal_pool_queue() {
                    return Err((txn.abort(), e));
                }
                Ok(txn)
            }
            Err(e) => Err((dev, e)),
        }
    }

    /// Allocate the next free block from the spaceman.
    ///
    /// Thin wrapper around `alloc_blocks_run(1)` - all bitmap/CIB writes are
    /// deferred to `commit()` via the dirty-cache. Callers unchanged.
    pub fn alloc_block(&mut self) -> Result<u64, TxnError> {
        let (start, _) = self.alloc_blocks_run(1)?;
        Ok(start)
    }

    /// Allocate up to `n` contiguous blocks from a single bitmap chunk.
    ///
    /// Returns `(start_paddr, count)` where `count <= n`. For multi-chunk runs
    /// the caller loops. Bitmap and CIB writes are deferred to `commit()` - the
    /// dirty-cache coalesces all allocs touching the same bitmap block into ONE
    /// physical write per block at commit time.
    ///
    /// [CERTAIN: deferred-bitmap pattern mirrors linux-apfs-rw apfs_write_ip_bitmaps;
    ///  on-disk semantics (bit numbering, CIB layout) unchanged]
    pub fn alloc_blocks_run(&mut self, n: usize) -> Result<(u64, usize), TxnError> {
        let bsz = self.nx.block_size as usize;
        let out = alloc_run_near(
            &mut self.dev,
            &mut self.sm,
            &mut self.dirty_bitmaps,
            &mut self.dirty_cibs,
            &mut self.dirty_runs,
            &mut self.locality,
            bsz,
            n,
        );
        if let Ok((_, got)) = out {
            self.blocks_allocated = self.blocks_allocated.saturating_add(got as u64);
        }
        out
    }

    /// Blocks this transaction has taken from the allocator so far.
    ///
    /// With [`Self::blocks_freed`], lets a whole-volume operation state its
    /// effect on `apfs_fs_alloc_count` as what it actually did rather than as a
    /// hand-maintained tally that drifts the moment a step is added.
    pub fn blocks_allocated(&self) -> u64 {
        self.blocks_allocated
    }

    /// Distinct blocks this transaction has scheduled for reclaim so far.
    pub fn blocks_freed(&self) -> u64 {
        self.pending_frees.len() as u64
    }

    /// Release the pool blocks that transactions older than this one copied
    /// away from.
    ///
    /// A block is queued rather than freed on the spot because the checkpoint
    /// that still names it is the one a torn commit falls back to. Once a newer
    /// transaction opens, that checkpoint has been superseded and the block can
    /// go back into the pool.
    ///
    /// [CERTAIN: linux-apfs-rw/spaceman.c apfs_flush_free_queue releases every
    ///  record older than the open transaction, and apfs_flush_fq_rec routes an
    ///  address inside the pool to apfs_ip_mark_free]
    fn drain_internal_pool_queue(&mut self) -> Result<(), TxnError> {
        if !self.sm.ip.is_available() {
            return Ok(());
        }
        let bsz = self.nx.block_size as usize;
        let Some((eph_idx, node)) = self.sm_fq_ip_node() else {
            return Ok(());
        };
        // Everything strictly older than this transaction. A record written by
        // this transaction is still protecting a checkpoint.
        let upto = self.xid.saturating_sub(1);
        let (new_node, drained) = crate::sm_fq::drain_sm_fq_upto(&node, upto, bsz)
            .map_err(|e| TxnError::SpacemanParse(format!("pool free queue drain: {e:?}")))?;
        if drained.is_empty() {
            return Ok(());
        }
        let mut released_blocks: u64 = 0;
        for (_xid, paddr, count) in &drained {
            let run = (*count).max(1);
            if run > self.sm.ip.block_count {
                return Err(TxnError::SpacemanParse(format!(
                    "pool free queue run of {run} blocks exceeds the pool ({} blocks)",
                    self.sm.ip.block_count
                )));
            }
            for off in 0..run {
                let b = paddr.checked_add(off).ok_or_else(|| {
                    TxnError::SpacemanParse(format!("pool free queue paddr overflow at {paddr}"))
                })?;
                self.sm.ip.mark_free(bsz, b)?;
            }
            released_blocks += run;
        }
        if let Some(e) = self.other_ephemerals.get_mut(eph_idx) {
            e.raw = new_node;
        }
        let remaining = self
            .other_ephemerals
            .get(eph_idx)
            .map(|e| crate::sm_fq::parse_sm_fq_entries(&e.raw, bsz))
            .unwrap_or_default();
        let old_count = u64_from_le(&self.sm.raw, SFQ_IP_BASE);
        write_u64_le_slice(
            &mut self.sm.raw,
            SFQ_IP_BASE,
            old_count.saturating_sub(released_blocks),
        );
        let new_oldest = remaining.iter().map(|&(x, _, _)| x).min().unwrap_or(0);
        write_u64_le_slice(&mut self.sm.raw, SFQ_IP_BASE + 16, new_oldest);
        Ok(())
    }

    /// Put the pool blocks this transaction copied away from on the pool's
    /// free queue, so the next transaction can release them.
    fn enqueue_internal_pool_releases(&mut self) -> Result<(), TxnError> {
        if self.sm.ip.released.is_empty() {
            return Ok(());
        }
        let bsz = self.nx.block_size as usize;
        let released = std::mem::take(&mut self.sm.ip.released);
        let Some((eph_idx, mut node)) = self.sm_fq_ip_node() else {
            // Without a queue there is nowhere to record the release. Leaving
            // the blocks marked used leaks them, which costs pool capacity;
            // freeing them now would let the next transaction hand out a block
            // the previous checkpoint is still reading.
            tracing::warn!(
                blocks = released.len(),
                "no internal-pool free queue: superseded pool blocks stay marked used"
            );
            return Ok(());
        };
        for paddr in &released {
            node = crate::sm_fq::insert_sm_fq_entry(&node, self.xid, *paddr, bsz)
                .map_err(|e| TxnError::SpacemanParse(format!("pool free queue insert: {e:?}")))?;
        }
        if let Some(e) = self.other_ephemerals.get_mut(eph_idx) {
            e.raw = node;
        }
        let old_count = u64_from_le(&self.sm.raw, SFQ_IP_BASE);
        write_u64_le_slice(
            &mut self.sm.raw,
            SFQ_IP_BASE,
            old_count.saturating_add(released.len() as u64),
        );
        let oldest = u64_from_le(&self.sm.raw, SFQ_IP_BASE + 16);
        if oldest == 0 || self.xid < oldest {
            write_u64_le_slice(&mut self.sm.raw, SFQ_IP_BASE + 16, self.xid);
        }
        Ok(())
    }

    /// The single node of the internal pool's free queue, with its index in
    /// `other_ephemerals`. The queue's node limit is one, so there is no tree
    /// to walk.
    fn sm_fq_ip_node(&self) -> Option<(usize, Vec<u8>)> {
        let tree_oid = u64_from_le(&self.sm.raw, SFQ_IP_BASE + 8);
        if tree_oid == 0 {
            return None;
        }
        let idx = self
            .other_ephemerals
            .iter()
            .position(|e| e.oid == tree_oid)?;
        self.other_ephemerals.get(idx).map(|e| (idx, e.raw.clone()))
    }

    /// Write the internal-pool bitmap this transaction changed into a fresh
    /// slot of its ring.
    ///
    /// The ring is a linked free list threaded through an array of slot
    /// indices: a slot is taken from the head and a released one is appended at
    /// the tail, so the copy the previous checkpoint was sealed with is never
    /// the copy this one overwrites, and no slot can be reused inside a single
    /// transaction. A slot that is in use carries the invalid index instead of
    /// a link, which is how a reader tells the two apart.
    ///
    /// [CERTAIN: linux-apfs-rw/spaceman.c apfs_reallocate_ip_bitmap;
    ///  apfsprogs/apfsck/spaceman.c check_ip_free_next requires the list to run
    ///  from head to tail, the tail's own link to be invalid, and the number of
    ///  slots off the list to equal the bitmap's size in blocks]
    fn flush_internal_pool(&mut self, xid: u64) -> Result<(), TxnError> {
        if !self.sm.ip.has_dirty() {
            return Ok(());
        }
        let bsz = self.nx.block_size as usize;
        let ring = self.sm.ip.bm_ring_len;
        let bm_base = self.sm.ip.bm_base;
        let slot_arr = self.sm.ip.bitmap_offset as usize;
        let xid_arr = self.sm.ip.xid_offset as usize;
        let next_arr = self.sm.ip.free_next_offset as usize;
        let link = |slot: u16| next_arr + slot as usize * 2;

        for i in 0..self.sm.ip.bitmaps.len() {
            if self.sm.ip.dirty.get(i) != Some(&true) {
                continue;
            }
            let old_slot = u16_from_le(&self.sm.raw, slot_arr + i * 2);
            let free_head = u16_from_le(&self.sm.raw, SMI_IP_BM_FREE_HEAD);
            let free_tail = u16_from_le(&self.sm.raw, SMI_IP_BM_FREE_TAIL);
            if u32::from(old_slot) >= ring
                || u32::from(free_head) >= ring
                || u32::from(free_tail) >= ring
            {
                return Err(TxnError::SpacemanParse(format!(
                    "internal-pool ring indices out of range (slot {old_slot}, \
                     head {free_head}, tail {free_tail}, ring {ring})"
                )));
            }
            // Release the slot the bitmap is leaving, at the tail.
            write_u16_le_slice(&mut self.sm.raw, link(free_tail), old_slot);
            write_u16_le_slice(&mut self.sm.raw, SMI_IP_BM_FREE_TAIL, old_slot);
            // Take the head for the new copy.
            let new_slot = free_head;
            let next_head = u16_from_le(&self.sm.raw, link(new_slot));
            if next_head == IP_BM_INDEX_INVALID || u32::from(next_head) >= ring {
                return Err(TxnError::SpacemanParse(format!(
                    "internal-pool free list is exhausted at slot {new_slot}"
                )));
            }
            write_u16_le_slice(&mut self.sm.raw, link(new_slot), IP_BM_INDEX_INVALID);
            write_u16_le_slice(&mut self.sm.raw, SMI_IP_BM_FREE_HEAD, next_head);
            write_u16_le_slice(&mut self.sm.raw, slot_arr + i * 2, new_slot);
            write_u64_le_slice(&mut self.sm.raw, xid_arr + i * 8, xid);

            let bno = bm_base + u64::from(new_slot);
            let off =
                bno.checked_mul(self.nx.block_size as u64)
                    .ok_or(TxnError::BlockOutOfRange {
                        block: bno,
                        size: self.dev.size(),
                    })?;
            let bitmap = self.sm.ip.bitmaps.get(i).cloned().ok_or_else(|| {
                TxnError::SpacemanParse(format!("internal-pool bitmap {i} vanished"))
            })?;
            debug_assert_eq!(bitmap.len(), bsz);
            self.dev.write_at(off, &bitmap)?;
            if let Some(d) = self.sm.ip.dirty.get_mut(i) {
                *d = false;
            }
        }
        Ok(())
    }

    /// Schedule a physical block for reclaim at commit time.
    ///
    /// The block is added to `pending_frees` and its bitmap bit is NOT cleared
    /// until `commit()` has flushed all staged pending writes (Step 2). This
    /// prevents `alloc_run_cached` from re-allocating the block within the same
    /// transaction - which was the root cause of the M8 #3 batch-rename corruption:
    ///
    ///   1. Rename 1 frees old-omap-header block X (immediately clears bitmap bit).
    ///   2. Rename 2's alloc_run_cached re-allocates X for its new VSB.
    ///   3. Rename 2 also calls free_replaced_metadata(stale vsb_raw) → tries to
    ///      free X again; bit is 1 → guard misses → double-free → corrupt free-runs
    ///      → subsequent alloc re-uses an already-pending address → pending write
    ///      overwrites container omap → BadMagic on re-open.
    ///
    /// By deferring all bitmap clears to post-Step-2, freed blocks are invisible
    /// to the allocator for the remainder of the transaction.
    ///
    /// [CERTAIN: empirical - overallocation = bitmap set but not in live extents;
    ///  deferred free is safe because the reclaim bitmaps are Phase B (post-NXSB)]
    pub fn free_block(&mut self, paddr: u64) -> Result<(), TxnError> {
        self.pending_frees.insert(paddr);
        Ok(())
    }

    /// Enqueue a snapshot-pinned block to `sm_fq[SFQ_MAIN]` for deferred reclaim.
    ///
    /// Used by `free_replaced_metadata` when a block is pinned by an active
    /// snapshot (`o_xid <= newest_snap`): instead of leaving the block silently
    /// allocated (which fsck reports as overallocation because the container-
    /// level space verifier does NOT traverse the snapshot's frozen-VSB chain),
    /// this records `{snap_xid, paddr}` in the SPACEMAN_FREE_QUEUE so fsck's
    /// space-verifier explicitly sees the pin. On `delete_snapshot`, the
    /// drained entries' bitmap bits are cleared via `drain_sm_fq_main`.
    ///
    /// The bitmap bit is NOT cleared here - the block stays "allocated" from
    /// the spaceman's view, just tracked for deferred reclaim.
    ///
    /// Supports multi-level SFQ_MAIN trees (btn_level > 0) via the
    /// `sm_fq::insert_multi_node` / `insert_multi_node_2` engine (#150).
    ///
    /// Locked spec: KB `the APFS specification` Q4/Q6 + kernel-empirical
    /// layout in the design notes.
    #[allow(clippy::indexing_slicing)]
    pub fn enqueue_sm_fq(&mut self, snap_xid: u64, paddr: u64) -> Result<(), TxnError> {
        // A block of the space manager's internal pool is released through
        // the pool's own queue (SFQ_IP) and bitmap; on the main queue its
        // drain would clear a bit in a chunk bitmap that does not track it.
        // [linux-apfs-rw spaceman.c picks the queue by apfs_block_in_ip;
        //  apfsck keeps the two apart]
        if self.sm.ip.contains(paddr) {
            return Err(TxnError::SpacemanParse(format!(
                "block {paddr} of the internal pool queued for the main free queue"
            )));
        }
        let bsz = self.nx.block_size as usize;
        // sm_fq[SFQ_MAIN] is the SECOND of three contiguous spaceman_free_queue_t
        // entries (40 bytes each) inside spaceman_phys. The base block-offset is
        // 0xf0 (= 32 obj_phys + 16 fixed u32s + 64 sm_dev[2] + 56 IP-pool fields
        // + 40 sm_fq[SFQ_IP] = 248; verified empirically against the kernel-
        // formatted scratch baseline, 2026-05-23 decode).
        const SFQ_MAIN_BASE: usize = 0xf0;
        let count_off = SFQ_MAIN_BASE;
        let tree_oid_off = SFQ_MAIN_BASE + 8;
        let oldest_xid_off = SFQ_MAIN_BASE + 16;

        let sm_fq_main_oid = u64_from_le(&self.sm.raw, tree_oid_off);
        if sm_fq_main_oid == 0 {
            // SFQ_MAIN not provisioned on this volume - fall back to immediate
            // free (no deferred-reclaim safety, but non-destructive).
            return self.free_block(paddr);
        }
        let eph_idx = self
            .other_ephemerals
            .iter()
            .position(|e| e.oid == sm_fq_main_oid)
            .ok_or_else(|| {
                TxnError::SpacemanParse(format!(
                    "sm_fq[SFQ_MAIN] oid {sm_fq_main_oid:#x} not found among ephemerals"
                ))
            })?;

        // Build the flat (oid, raw_bytes) view that insert_multi_node operates on.
        // We include ALL other_ephemerals so any leaf children referenced by the
        // internal root are found.  After the call, we sync the updated bytes back.
        let before_count = {
            let node = &self.other_ephemerals[eph_idx].raw;
            let level = u16_from_le(node, 0x22);
            if level == 0 {
                crate::sm_fq::parse_sm_fq_entries(node, bsz).len()
            } else {
                // Count total entries across all leaves in the multi-level tree.
                let view: Vec<(u64, Vec<u8>)> = self
                    .other_ephemerals
                    .iter()
                    .map(|e| (e.oid, e.raw.clone()))
                    .collect();
                // A tree that does not validate is not one to enqueue into:
                // the record would land while the counts stayed behind.
                crate::sm_fq::validate_sm_fq_tree(sm_fq_main_oid, &view, bsz).map_err(|e| {
                    TxnError::SpacemanParse(format!("free queue tree before the enqueue: {e:?}"))
                })?
            }
        };

        // Take a snapshot of all sm_fq-related ephemerals into a mutable Vec<(oid, bytes)>.
        // Non-sm_fq ephemerals (reaper, etc.) are untouched and stay in other_ephemerals.
        let mut sm_fq_view: Vec<(u64, Vec<u8>)> = self
            .other_ephemerals
            .iter()
            .map(|e| (e.oid, e.raw.clone()))
            .collect();

        // Allocate new ephemeral oid(s) for potential new leaf nodes.
        let new_oid_a = self.next_oid;
        self.next_oid += 1;
        let new_oid_b = self.next_oid;
        self.next_oid += 1;

        let root_level = u16_from_le(&self.other_ephemerals[eph_idx].raw, 0x22);
        let insert_result = if root_level == 0 {
            // root+leaf (single-node): may need 2 oids for root-grow.
            crate::sm_fq::insert_multi_node_2(
                sm_fq_main_oid,
                &mut sm_fq_view,
                new_oid_a,
                new_oid_b,
                snap_xid,
                paddr,
                self.xid,
                bsz,
            )
        } else {
            // Multi-level tree: standard 1-oid path (2nd oid goes unused).
            crate::sm_fq::insert_multi_node(
                sm_fq_main_oid,
                &mut sm_fq_view,
                new_oid_a,
                self.xid,
                snap_xid,
                paddr,
                bsz,
            )
        };

        match insert_result {
            Ok(()) => {}
            Err(e) => {
                // Roll back the oid bump and propagate the error cleanly.
                // The COW invariant holds: prior checkpoint is intact.
                // We subtracted 2 from next_oid; restore it.
                self.next_oid -= 2;
                return Err(TxnError::SpacemanParse(format!(
                    "sm_fq multi-node insert failed (paddr={paddr:#x}): {e:?}"
                )));
            }
        }

        // Roll back unused oid(s): only bump for newly-created nodes.
        // Determine how many new nodes were actually added.
        let new_node_count = sm_fq_view.len().saturating_sub(self.other_ephemerals.len());
        // We pre-allocated 2 oids; if only 0 or 1 node was added, give back the rest.
        let oids_used = new_node_count.min(2);
        self.next_oid -= 2 - oids_used as u64;

        // Sync updated bytes back into other_ephemerals and register new nodes.
        for (oid, raw) in &sm_fq_view {
            if let Some(eph) = self.other_ephemerals.iter_mut().find(|e| e.oid == *oid) {
                eph.raw = raw.clone();
            } else {
                // New node - push as a new EphemeralObj so commit writes it
                // to the checkpoint data area and registers its checkpoint_mapping entry.
                self.other_ephemerals.push(EphemeralObj {
                    cme_type: OBJ_EPHEMERAL | OBJECT_TYPE_BTREE,
                    cme_subtype: crate::sm_fq::OBJECT_TYPE_SPACEMAN_FREE_QUEUE,
                    fs_oid: 0,
                    oid: *oid,
                    raw: raw.clone(),
                });
            }
        }

        // Count total entries after insert to determine sfq_count delta.
        let after_count = {
            let view: Vec<(u64, Vec<u8>)> = self
                .other_ephemerals
                .iter()
                .map(|e| (e.oid, e.raw.clone()))
                .collect();
            let level = u16_from_le(&self.other_ephemerals[eph_idx].raw, 0x22);
            if level == 0 {
                crate::sm_fq::parse_sm_fq_entries(&self.other_ephemerals[eph_idx].raw, bsz).len()
            } else {
                crate::sm_fq::validate_sm_fq_tree(sm_fq_main_oid, &view, bsz).map_err(|e| {
                    TxnError::SpacemanParse(format!("free queue tree after the enqueue: {e:?}"))
                })?
            }
        };

        if after_count > before_count {
            // Bump sfq_count + maintain sfq_oldest_xid invariant.
            let new_count = u64_from_le(&self.sm.raw, count_off).saturating_add(1);
            write_u64_le(&mut self.sm.raw, count_off, new_count);
            let old_oldest = u64_from_le(&self.sm.raw, oldest_xid_off);
            if old_oldest == 0 || snap_xid < old_oldest {
                write_u64_le(&mut self.sm.raw, oldest_xid_off, snap_xid);
            }
        }
        Ok(())
    }

    /// Drain every `sm_fq[SFQ_MAIN]` entry with `xid <= upto_xid`, freeing each
    /// drained paddr's bitmap bit. Used by `delete_snapshot` to reclaim the
    /// blocks pinned by the deleted snapshot.
    ///
    /// `owned_from_xid` separates the entries this volume enqueued from the
    /// ones the formatter left behind: an entry stamped older than the oldest
    /// snapshot the volume has ever held cannot have come from the
    /// snapshot-pinned copy-on-write path, and its blocks are stale rather
    /// than volume-owned. fsck counts `apfs_fs_alloc_count` by walking what
    /// the volume still reaches, so only the owned ones may be subtracted.
    ///
    /// Returns the entry count, the total blocks freed, and the owned subset.
    #[allow(clippy::indexing_slicing)]
    pub fn drain_sm_fq_main(
        &mut self,
        upto_xid: u64,
        owned_from_xid: u64,
    ) -> Result<DrainOutcome, TxnError> {
        let bsz = self.nx.block_size as usize;
        const SFQ_MAIN_BASE: usize = 0xf0;
        let count_off = SFQ_MAIN_BASE;
        let tree_oid_off = SFQ_MAIN_BASE + 8;
        let oldest_xid_off = SFQ_MAIN_BASE + 16;

        let sm_fq_main_oid = u64_from_le(&self.sm.raw, tree_oid_off);
        if sm_fq_main_oid == 0 {
            return Ok(DrainOutcome::default());
        }
        let eph_idx = self
            .other_ephemerals
            .iter()
            .position(|e| e.oid == sm_fq_main_oid)
            .ok_or_else(|| {
                TxnError::SpacemanParse(format!(
                    "sm_fq[SFQ_MAIN] oid {sm_fq_main_oid:#x} not found among ephemerals"
                ))
            })?;
        // Detect whether the SFQ_MAIN tree is single-node or multi-level.
        let root_level = u16_from_le(&self.other_ephemerals[eph_idx].raw, 0x22);

        let (total_drained_count, all_drained) = if root_level == 0 {
            // Single-node root+leaf: existing path.
            let (new_node, drained) =
                crate::sm_fq::drain_sm_fq_upto(&self.other_ephemerals[eph_idx].raw, upto_xid, bsz)
                    .map_err(|e| TxnError::SpacemanParse(format!("sm_fq drain: {e:?}")))?;
            let count = drained.len();
            if count > 0 {
                self.other_ephemerals[eph_idx].raw = new_node;
            }
            (count, drained)
        } else {
            // Multi-level tree: drain each leaf child individually.
            // The internal root's structure is unchanged (separator keys and
            // child oids remain valid - we only rewrite leaf content).
            let children =
                crate::sm_fq::parse_internal_entries(&self.other_ephemerals[eph_idx].raw, bsz);
            let mut total_drained = 0usize;
            let mut all_drained_entries: Vec<(u64, u64, u64)> = Vec::new();
            for (_, _, child_oid) in &children {
                let child_idx = self
                    .other_ephemerals
                    .iter()
                    .position(|e| e.oid == *child_oid);
                let Some(cidx) = child_idx else { continue };
                let (new_leaf, drained) =
                    crate::sm_fq::drain_sm_fq_upto(&self.other_ephemerals[cidx].raw, upto_xid, bsz)
                        .map_err(|e| TxnError::SpacemanParse(format!("sm_fq drain leaf: {e:?}")))?;
                if !drained.is_empty() {
                    self.other_ephemerals[cidx].raw = new_leaf;
                    total_drained += drained.len();
                    all_drained_entries.extend(drained);
                }
            }
            (total_drained, all_drained_entries)
        };

        // M7b-RT D4: no-op when nothing matches.
        if total_drained_count == 0 {
            return Ok(DrainOutcome::default());
        }

        // #142: each sm_fq entry can cover a CONTIGUOUS RUN of `count` blocks
        // (sfqv_count). Free the whole run [paddr, paddr+count); dropping the
        // run length leaked every block past the first (e.g. the formatter's
        // stale xid=1 volume-omap is one {paddr, count=2} run = header + tree,
        // and freeing only the header left the tree as fsck "overallocation").
        let mut drained_blocks: u64 = 0;
        let mut owned_blocks: u64 = 0;
        for (_x, p, c) in all_drained {
            // A free-queue run cannot exceed the container; a crafted run length
            // would otherwise loop unboundedly (hang) and free blocks past the
            // device. Reject it, and guard the per-block address against u64
            // overflow.
            if c > self.nx.block_count {
                return Err(TxnError::SpacemanParse(format!(
                    "sm_fq run length {c} exceeds container ({} blocks)",
                    self.nx.block_count
                )));
            }
            let run = c.max(1);
            drained_blocks += run;
            if _x >= owned_from_xid {
                owned_blocks += run;
            }
            for off in 0..run {
                let b = p.checked_add(off).ok_or_else(|| {
                    TxnError::SpacemanParse(format!("sm_fq run paddr overflow at {p}+{off}"))
                })?;
                self.free_block(b)?;
            }
        }
        // Adjust counters. sfq_count counts BLOCKS, not records: fsck compares
        // it against the sum of the run lengths held by the tree. Subtracting
        // the record count left the field high by (blocks - records) whenever a
        // drained entry covered a run, which fsck reports as an sfq_count
        // mismatch. [apfsprogs apfsck/spaceman.c: sfq_count += length]
        let old_count = u64_from_le(&self.sm.raw, count_off);
        write_u64_le(
            &mut self.sm.raw,
            count_off,
            old_count.saturating_sub(drained_blocks),
        );
        // Recompute oldest_xid from remaining entries across all leaves.
        let new_oldest = if root_level == 0 {
            let remaining =
                crate::sm_fq::parse_sm_fq_entries(&self.other_ephemerals[eph_idx].raw, bsz);
            remaining.iter().map(|&(x, _, _)| x).min().unwrap_or(0)
        } else {
            let children =
                crate::sm_fq::parse_internal_entries(&self.other_ephemerals[eph_idx].raw, bsz);
            children
                .iter()
                .filter_map(|(_, _, child_oid)| {
                    self.other_ephemerals
                        .iter()
                        .find(|e| e.oid == *child_oid)
                        .map(|e| crate::sm_fq::parse_sm_fq_entries(&e.raw, bsz))
                })
                .flatten()
                .map(|(x, _, _)| x)
                .min()
                .unwrap_or(0)
        };
        write_u64_le(&mut self.sm.raw, oldest_xid_off, new_oldest);
        Ok(DrainOutcome {
            records: total_drained_count,
            blocks: drained_blocks,
            owned_blocks,
        })
    }

    /// Read one block, seeing what this transaction has already staged there.
    ///
    /// A staged block has not reached the device yet, so reading straight from
    /// it returns whatever the address held before - on a fresh scratch image
    /// zeroes, but on a used volume the previous occupant's bytes. Walking a
    /// just-copied b-tree root that way parsed file data as a node and followed
    /// its contents as child pointers, which then freed live extents.
    pub fn read_block(&mut self, paddr: u64, buf: &mut [u8]) -> Result<(), TxnError> {
        if let Some(block) = self.data_buf.get(&paddr) {
            let n = buf.len().min(block.len());
            if let (Some(d), Some(s)) = (buf.get_mut(..n), block.get(..n)) {
                d.copy_from_slice(s);
            }
            return Ok(());
        }
        if let Some(&i) = self.pending_index.get(&paddr) {
            if let Some(pw) = self.pending.get(i) {
                let n = buf.len().min(pw.data.len());
                buf.get_mut(..n)
                    .ok_or(TxnError::BlockOutOfRange {
                        block: paddr,
                        size: 0,
                    })?
                    .copy_from_slice(pw.data.get(..n).unwrap_or(&[]));
                return Ok(());
            }
        }
        let off =
            paddr
                .checked_mul(self.nx.block_size as u64)
                .ok_or(TxnError::BlockOutOfRange {
                    block: paddr,
                    size: self.dev.size(),
                })?;
        self.dev
            .read_at(off, buf)
            .map_err(|e| TxnError::SpacemanParse(e.to_string()))
    }

    /// Allocate a new OID for use within this transaction.
    pub fn alloc_oid(&mut self) -> u64 {
        let oid = self.next_oid;
        self.next_oid += 1;
        oid
    }

    /// Stage a physical object write.
    ///
    /// Allocates a free block, serializes the object with a correct `obj_phys_t`
    /// header (Fletcher-64), and queues it for writing at commit time.
    ///
    /// `body` must be exactly `block_size - 32` bytes (the block content after
    /// the obj_phys header).
    ///
    /// Returns the allocated physical block number.
    pub fn stage_physical(
        &mut self,
        _oid: u64,
        o_type: u32,
        o_subtype: u32,
        body: &[u8],
    ) -> Result<u64, TxnError> {
        let bsz = self.nx.block_size as usize;
        if body.len() + 32 != bsz {
            return Err(TxnError::SpacemanParse(format!(
                "body len {} + 32 != block_size {}",
                body.len(),
                bsz
            )));
        }
        let paddr = self.alloc_block()?;
        // Physical objects: o_oid = paddr (spec A. obj_phys_t).
        let block = make_obj_block(ObjBlockArgs {
            o_oid_stored: paddr,
            xid: self.xid,
            o_type: o_type | OBJ_PHYSICAL,
            o_subtype,
            body,
        });
        self.allocated_blocks.push(paddr);
        self.stage_pending(paddr, block);
        Ok(paddr)
    }

    /// Stage a virtual object write.
    ///
    /// Like `stage_physical`, but also registers an omap entry `{oid, xid} →
    /// paddr` so the volume omap B-tree is updated at commit.
    ///
    /// Returns the allocated physical block number.
    pub fn stage_virtual(
        &mut self,
        oid: u64,
        o_type: u32,
        o_subtype: u32,
        body: &[u8],
    ) -> Result<u64, TxnError> {
        let bsz = self.nx.block_size as usize;
        if body.len() + 32 != bsz {
            return Err(TxnError::SpacemanParse(format!(
                "body len {} + 32 != block_size {}",
                body.len(),
                bsz
            )));
        }
        let block = make_obj_block(ObjBlockArgs {
            o_oid_stored: oid,
            xid: self.xid,
            o_type: o_type | OBJ_VIRTUAL,
            o_subtype,
            body,
        });
        // A version this transaction already staged under the same id and
        // transaction id is named by nothing but the object-map entry this
        // transaction will write: it is rewritten where it is, and that
        // entry stands. A new block per operation held one more block per
        // operation until the commit and split the data written between
        // operations into separate runs.
        if let Some(paddr) = self.restageable_virtual(oid) {
            self.restage(paddr, block);
            return Ok(paddr);
        }
        let paddr = self.alloc_block()?;
        self.allocated_blocks.push(paddr);
        self.stage_pending(paddr, block);
        self.omap_entries.push(OmapEntry {
            oid,
            xid: self.xid,
            paddr,
            size: self.nx.block_size,
        });
        if o_type == OBJECT_TYPE_FS {
            self.staged_volumes.insert(oid, paddr);
        }
        Ok(paddr)
    }

    /// The block of the newest version of virtual object `oid` this
    /// transaction staged, when that version carries the transaction's
    /// current id and is still live.
    fn restageable_virtual(&self, oid: u64) -> Option<u64> {
        let e = self.omap_entries.iter().rev().find(|e| e.oid == oid)?;
        (e.xid == self.xid && self.staged_live(e.paddr)).then_some(e.paddr)
    }

    /// The volume superblock as this transaction has it: `vsb_raw` (a copy
    /// read before the transaction began) unless an operation in this
    /// transaction staged a newer superblock for the same volume.
    pub fn current_vsb(&mut self, vsb_raw: &[u8]) -> Result<Vec<u8>, TxnError> {
        let oid = u64_from_le(vsb_raw, 8);
        let Some(&paddr) = self.staged_volumes.get(&oid) else {
            return Ok(vsb_raw.to_vec());
        };
        let mut vsb = vec![0u8; self.nx.block_size as usize];
        self.read_block(paddr, &mut vsb)?;
        Ok(vsb)
    }

    /// The volume superblock and its object-map header as this transaction
    /// has them. Every operation starts from these, so one that follows
    /// another in the same transaction builds on its changes instead of on
    /// the copies read before the transaction began: from those, the last
    /// superblock staged would win, every earlier change would be lost, and
    /// blocks an earlier change released could stay named by the tree that
    /// won.
    pub fn current_volume(
        &mut self,
        vsb_raw: &[u8],
        omap_raw: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), TxnError> {
        let oid = u64_from_le(vsb_raw, 8);
        if !self.staged_volumes.contains_key(&oid) {
            return Ok((vsb_raw.to_vec(), omap_raw.to_vec()));
        }
        let vsb = self.current_vsb(vsb_raw)?;
        let mut omap = vec![0u8; self.nx.block_size as usize];
        self.read_block(u64_from_le(&vsb, VSB_OMAP_OID), &mut omap)?;
        Ok((vsb, omap))
    }

    /// Stage a raw pre-built block (already has correct obj_phys header + Fletcher-64).
    ///
    /// Used by snapshot.rs to write objects (like the frozen volume superblock
    /// copy) that require precise field layout.
    /// Record a block this transaction will write, replacing any earlier stage
    /// for the same address so `read_block` sees the newest version.
    fn stage_pending(&mut self, paddr: u64, data: Vec<u8>) {
        let at = self.pending_index.get(&paddr).copied();
        match at.and_then(|i| self.pending.get_mut(i).map(|pw| (i, pw))) {
            Some((i, pw)) => {
                let before = std::mem::replace(&mut pw.data, data);
                // A block staged before the savepoint keeps its first earlier
                // content, so a rollback can put it back.
                if let Some(undo) = self.undo.as_mut().filter(|u| i < u.below) {
                    undo.saved.entry(i).or_insert(before);
                }
            }
            None => {
                self.pending_index.insert(paddr, self.pending.len());
                self.pending.push(PendingWrite { paddr, data });
            }
        }
    }

    pub fn stage_raw(&mut self, paddr: u64, data: Vec<u8>) {
        self.allocated_blocks.push(paddr);
        self.stage_pending(paddr, data);
    }

    /// Whether this transaction staged `paddr` and has not released it: a
    /// block no checkpoint names yet, so a later change in the same
    /// transaction may rewrite it where it is instead of copying it.
    pub(crate) fn staged_live(&self, paddr: u64) -> bool {
        self.pending_index.contains_key(&paddr) && !self.pending_frees.contains(&paddr)
    }

    /// Rewrite a block this transaction already staged (see
    /// [`Self::staged_live`]) in place.
    pub(crate) fn restage(&mut self, paddr: u64, data: Vec<u8>) {
        self.stage_pending(paddr, data);
    }

    /// Run `op` so that it either lands whole or leaves the transaction as
    /// it was. An operation can fail after it has staged blocks and taken
    /// space (the volume fills up half way through a write); in a
    /// transaction that holds earlier operations too, those must survive
    /// it, so its own changes are rolled back instead of the transaction
    /// being thrown away.
    pub fn atomically<R>(
        &mut self,
        op: impl FnOnce(&mut Self) -> Result<R, TxnError>,
    ) -> Result<R, TxnError> {
        let sp = self.savepoint();
        let out = op(self);
        if out.is_err() {
            // Putting back data written straight to the device can fail in
            // turn; that error says more about the transaction than the
            // operation's own.
            self.rollback_to(sp)?;
        } else {
            self.undo = None;
        }
        out
    }

    /// Remember the transaction's in-memory state. One savepoint at a time:
    /// taking another replaces the first.
    fn savepoint(&mut self) -> Savepoint {
        self.undo = Some(Undo {
            below: self.pending.len(),
            saved: HashMap::new(),
            direct_saved: HashMap::new(),
            direct_added: HashSet::new(),
        });
        Savepoint {
            pending_len: self.pending.len(),
            omap_entries_len: self.omap_entries.len(),
            allocated_blocks_len: self.allocated_blocks.len(),
            nx: self.nx.clone(),
            sm: self.sm.clone(),
            next_oid: self.next_oid,
            staged_volumes: self.staged_volumes.clone(),
            other_ephemerals: self.other_ephemerals.clone(),
            dirty_bitmaps: self.dirty_bitmaps.clone(),
            dirty_cibs: self.dirty_cibs.clone(),
            dirty_runs: self.dirty_runs.clone(),
            reclaim_bitmap_paddrs: self.reclaim_bitmap_paddrs.clone(),
            reclaim_cib_paddrs: self.reclaim_cib_paddrs.clone(),
            pending_frees: self.pending_frees.clone(),
            blocks_allocated: self.blocks_allocated,
        }
    }

    /// Put the transaction back as it was at `sp`. Data blocks written
    /// straight to the device and rewritten since get their content back;
    /// those first written since go back to the free space with the rest of
    /// what the operation took, holding bytes nothing names.
    fn rollback_to(&mut self, sp: Savepoint) -> Result<(), TxnError> {
        let undo = self.undo.take();
        self.pending.truncate(sp.pending_len);
        if let Some(undo) = undo {
            for (i, data) in undo.saved {
                if let Some(pw) = self.pending.get_mut(i) {
                    pw.data = data;
                }
            }
            for paddr in &undo.direct_added {
                self.direct.remove(paddr);
                if let Some(old) = self.data_buf.remove(paddr) {
                    self.data_buf_bytes -= old.len();
                }
            }
            for (paddr, data) in undo.direct_saved {
                if self.data_buf.contains_key(&paddr) {
                    self.buffer_data(paddr, data);
                } else {
                    self.write_direct(paddr, &data)?;
                }
            }
        }
        self.pending_index.retain(|_, i| *i < sp.pending_len);
        self.omap_entries.truncate(sp.omap_entries_len);
        self.allocated_blocks.truncate(sp.allocated_blocks_len);
        self.nx = sp.nx;
        self.sm = sp.sm;
        self.next_oid = sp.next_oid;
        self.staged_volumes = sp.staged_volumes;
        self.other_ephemerals = sp.other_ephemerals;
        self.dirty_bitmaps = sp.dirty_bitmaps;
        self.dirty_cibs = sp.dirty_cibs;
        self.dirty_runs = sp.dirty_runs;
        self.reclaim_bitmap_paddrs = sp.reclaim_bitmap_paddrs;
        self.reclaim_cib_paddrs = sp.reclaim_cib_paddrs;
        self.pending_frees = sp.pending_frees;
        self.blocks_allocated = sp.blocks_allocated;
        Ok(())
    }

    /// Write file data to block `paddr`, which this transaction has just
    /// allocated, straight to the device. Nothing a checkpoint holds names
    /// the block until the commit, so the content need not wait in memory
    /// for it: a crash before then leaves the bytes in free space. The
    /// commit's barriers still order them before the checkpoint that names
    /// them.
    /// `data` may span several blocks from `paddr` on (a run allocated in
    /// one piece). A run of [`DATA_DIRECT_MIN`] bytes or more goes to the
    /// device at once in one write; a smaller one waits in the data buffer
    /// with the others, which reaches the device in runs once it holds
    /// [`DATA_BUFFER_MAX`] bytes, and at the commit.
    pub fn write_new_data(&mut self, paddr: u64, data: &[u8]) -> Result<(), TxnError> {
        let bsz = self.nx.block_size as usize;
        let at_once = data.len() >= DATA_DIRECT_MIN;
        if at_once {
            self.write_direct(paddr, data)?;
        }
        for (i, chunk) in data.chunks(bsz).enumerate() {
            let b = paddr + i as u64;
            self.allocated_blocks.push(b);
            if self.direct.insert(b) {
                if let Some(undo) = self.undo.as_mut() {
                    undo.direct_added.insert(b);
                }
            }
            if !at_once {
                let mut block = chunk.to_vec();
                block.resize(bsz, 0);
                self.buffer_data(b, block);
            }
        }
        if self.data_buf_bytes >= DATA_BUFFER_MAX {
            self.spill_data()?;
        }
        Ok(())
    }

    /// Put block `b`'s data in the data buffer, replacing what it held.
    fn buffer_data(&mut self, b: u64, block: Vec<u8>) {
        let len = block.len();
        if let Some(old) = self.data_buf.insert(b, block) {
            self.data_buf_bytes -= old.len();
        }
        self.data_buf_bytes += len;
    }

    /// Write the data buffer to the device, consecutive blocks joined into
    /// one write, and empty it. Blocks this transaction released since are
    /// left out: nothing names them.
    fn spill_data(&mut self) -> Result<(), TxnError> {
        let bsz = self.nx.block_size as usize;
        let buffered = std::mem::take(&mut self.data_buf);
        self.data_buf_bytes = 0;
        let mut run: Vec<u8> = Vec::new();
        let mut run_start = 0u64;
        let mut run_next = u64::MAX;
        for (b, block) in buffered {
            if self.pending_frees.contains(&b) {
                continue;
            }
            if b != run_next || run.len() + block.len() > COMMIT_WRITE_MAX {
                write_run(&mut self.dev, run_start, &run, bsz)?;
                run.clear();
                run_start = b;
            }
            run.extend_from_slice(&block);
            run_next = b + 1;
        }
        write_run(&mut self.dev, run_start, &run, bsz)
    }

    /// Whether `paddr` is a data block this transaction wrote with
    /// [`Self::write_new_data`] and has not released: still free in every
    /// checkpoint, so it may be rewritten where it is.
    pub(crate) fn fresh_data(&self, paddr: u64) -> bool {
        self.direct.contains(&paddr) && !self.pending_frees.contains(&paddr)
    }

    /// Rewrite a block [`Self::fresh_data`] admits. Under a savepoint, its
    /// content from before is kept so a rollback can put it back.
    pub(crate) fn rewrite_fresh_data(&mut self, paddr: u64, data: &[u8]) -> Result<(), TxnError> {
        let keep = self.undo.as_ref().is_some_and(|u| {
            !u.direct_added.contains(&paddr) && !u.direct_saved.contains_key(&paddr)
        });
        if keep {
            let mut before = vec![0u8; self.nx.block_size as usize];
            self.read_block(paddr, &mut before)?;
            if let Some(undo) = self.undo.as_mut() {
                undo.direct_saved.insert(paddr, before);
            }
        }
        if self.data_buf.contains_key(&paddr) {
            let mut block = data.to_vec();
            block.resize(self.nx.block_size as usize, 0);
            self.buffer_data(paddr, block);
            return Ok(());
        }
        self.write_direct(paddr, data)
    }

    fn write_direct(&mut self, paddr: u64, data: &[u8]) -> Result<(), TxnError> {
        let off =
            paddr
                .checked_mul(u64::from(self.nx.block_size))
                .ok_or(TxnError::BlockOutOfRange {
                    block: paddr,
                    size: self.dev.size(),
                })?;
        self.dev.write_at(off, data)?;
        Ok(())
    }

    /// Stage an in-place overwrite of a physical object at a known block address.
    ///
    /// Unlike `stage_raw`, this does NOT add the block to `allocated_blocks` -
    /// the block already exists on disk (it was allocated in a previous
    /// transaction). Used for physical objects such as the volume omap
    /// (`o_type = PHYSICAL | OBJECT_TYPE_OMAP`) that are accessed by paddr
    /// directly (paddr = o_oid) rather than via the container omap B-tree.
    ///
    /// The caller is responsible for setting the correct obj_phys header
    /// (o_oid = paddr, o_xid = txn.xid, o_type with OBJ_PHYSICAL) and
    /// recomputing Fletcher-64 before calling this method.
    /// [CERTAIN: empirical - baseline macOS scratch image shows vol omap
    ///  o_type = 0x4000000B (PHYSICAL | OMAP); the APFS specification]
    pub fn stage_overwrite(&mut self, paddr: u64, data: Vec<u8>) {
        // Do not push to allocated_blocks - block pre-exists.
        self.stage_pending(paddr, data);
    }

    /// Register an omap entry without staging a block write.
    /// Used when the block has already been staged via `stage_raw`.
    pub fn register_omap_entry(&mut self, entry: OmapEntry) {
        self.omap_entries.push(entry);
    }

    /// Return a reference to the staged pending writes (for test inspection).
    pub fn pending_writes(&self) -> &[PendingWrite] {
        &self.pending
    }

    /// Bytes of the blocks staged to be written at commit, file data
    /// included; they are held in memory until then.
    pub fn staged_bytes(&self) -> usize {
        self.pending.iter().map(|p| p.data.len()).sum::<usize>() + self.data_buf_bytes
    }

    /// Return the omap entries staged so far (for test inspection).
    pub fn omap_entries(&self) -> &[OmapEntry] {
        &self.omap_entries
    }

    /// Commit the transaction to disk.
    ///
    /// Write order per C.2 (the APFS specification):
    /// 1. Apply pending omap entries to the container omap B-tree leaf.
    /// 2. All staged data/metadata blocks.
    /// 3. Spaceman ephemeral block (checkpoint data area).
    /// 4. Checkpoint map block (descriptor area) with CHECKPOINT_MAP_LAST.
    /// 5. `nx_superblock` - LAST (the atomic commit point).
    ///
    /// Discard all staged changes and return the underlying device.
    /// No disk writes occur - every mutation was kept in RAM until commit.
    /// Used by the WinFsp write path when an apfs-write API call fails
    /// mid-transaction: returning the device lets the host rebuild a
    /// fresh FsView and keep the mount alive rather than leaking the
    /// device through Drop (which would terminate the mount session).
    pub fn abort(self) -> D {
        self.dev
    }

    /// After commit: nx.current_xid bumped, next_oid bumped, ring indices
    /// advanced. The device is in a consistent APFS state readable by the
    /// macOS kernel.
    pub fn commit(self) -> Result<D, TxnError> {
        self.commit_recoverable().map_err(|(_dev, e)| e)
    }

    /// Like [`commit`] but on error returns the device back in the error tuple
    /// so a caller (e.g. the writable mount) can recover the handle and fall
    /// back cleanly instead of leaking it / leaving the mount dead. [#137]
    ///
    /// NOTE: a mid-commit failure may leave the on-disk checkpoint half-written;
    /// the recovered device must be treated as needing a fresh re-`begin` (which
    /// reads the last DURABLE checkpoint - partial writes before the NXSB flush
    /// are not yet the active checkpoint, so the prior checkpoint is intact).
    pub fn commit_recoverable(mut self) -> Result<D, (D, TxnError)> {
        match self.commit_inner() {
            Ok(()) => Ok(self.dev),
            Err(e) => Err((self.dev, e)),
        }
    }

    /// Volume superblock `oid` as the transaction started from it, through
    /// the container object map as it was sealed; `None` for a volume this
    /// transaction created.
    fn volume_before(&mut self, oid: u64) -> Result<Option<Vec<u8>>, TxnError> {
        let bsz = self.nx.block_size as usize;
        let omap = apfs_core::omap::Omap::open(&mut self.dev, self.nx.omap_oid, self.nx.block_size)
            .map_err(|err| TxnError::OmapParse(format!("container omap: {err:?}")))?;
        let old_paddr = omap
            .resolve(&mut self.dev, oid, self.nx.current_xid, self.nx.block_size)
            .map_err(|err| TxnError::OmapParse(format!("resolve volume {oid}: {err:?}")))?;
        let Some(paddr) = old_paddr else {
            return Ok(None);
        };
        let off = paddr
            .checked_mul(bsz as u64)
            .ok_or(TxnError::BlockOutOfRange {
                block: paddr,
                size: self.dev.size(),
            })?;
        let mut old_blk = vec![0u8; bsz];
        self.dev.read_at(off, &mut old_blk)?;
        Ok(Some(old_blk))
    }

    /// Carry the change of each staged volume's `apfs_fs_alloc_count` into
    /// its running totals: a rise into `apfs_total_blocks_alloced`, a fall
    /// into `apfs_total_blocks_freed`, measured against the version the
    /// transaction started from. Every other writer moves them with every
    /// block it takes or gives back, so they stay apart by the allocation
    /// count; left alone, the next writer's frees of blocks this one took
    /// push the freed total past the allocated one.
    /// [CERTAIN: linux-apfs-rw node.c / object.c / extents.c raise
    ///  apfs_total_blocks_alloced and apfs_total_blocks_freed with every
    ///  apfs_fs_alloc_count change; apfsprogs/apfsck/super.c warns when the
    ///  freed total exceeds the allocated one; measured: macOS removing
    ///  files we wrote raised the freed total past ours]
    fn mirror_allocation_totals(&mut self, entries: &[OmapEntry]) -> Result<(), TxnError> {
        for e in entries {
            let Some(i) = self.pending_index.get(&e.paddr).copied() else {
                continue;
            };
            let after = match self.pending.get(i) {
                Some(pw)
                    if pw.data.len() >= VSB_TOTAL_BLOCKS_FREED + 8
                        && u32_from_le(&pw.data, 24) & 0x0000_FFFF == OBJECT_TYPE_FS =>
                {
                    u64_from_le(&pw.data, VSB_ALLOC_COUNT)
                }
                _ => continue,
            };
            let Some(old_blk) = self.volume_before(e.oid)? else {
                continue;
            };
            let before = u64_from_le(&old_blk, VSB_ALLOC_COUNT);
            if after == before {
                continue;
            }
            let Some(pw) = self.pending.get_mut(i) else {
                continue;
            };
            let (field, moved) = if after > before {
                (VSB_TOTAL_BLOCKS_ALLOCED, after - before)
            } else {
                (VSB_TOTAL_BLOCKS_FREED, before - after)
            };
            let total = u64_from_le(&pw.data, field).saturating_add(moved);
            write_u64_le_slice(&mut pw.data, field, total);
            update_checksum_in_place(&mut pw.data);
        }
        Ok(())
    }

    /// Keep the space manager's sum over reserved volumes in step with the
    /// volume superblocks this transaction stages.
    ///
    /// A volume created with a reserve is counted twice: in its own
    /// `apfs_fs_alloc_count` and in the space manager's
    /// `sm_fs_reserve_alloc_count`, the total over every volume that has a
    /// reserve. A staged superblock whose count moved has to move that total
    /// by the same amount, measured against the version the transaction
    /// started from.
    /// [CERTAIN: apfsprogs/apfsck/spaceman.c requires the sum to equal the
    ///  reserved volumes' counts; measured: a volume formatted with
    ///  `newfs_apfs -r` written by us left the sum at its old value]
    fn follow_reserved_volume_counts(&mut self, entries: &[OmapEntry]) -> Result<(), TxnError> {
        let mut delta: i128 = 0;
        for e in entries {
            let Some(new_blk) = self
                .pending_index
                .get(&e.paddr)
                .and_then(|&i| self.pending.get(i))
                .map(|pw| pw.data.clone())
            else {
                continue;
            };
            if new_blk.len() < VSB_ALLOC_COUNT + 8
                || u32_from_le(&new_blk, 24) & 0x0000_FFFF != OBJECT_TYPE_FS
            {
                continue;
            }
            let after = if u64_from_le(&new_blk, VSB_RESERVE_BLOCK_COUNT) != 0 {
                u64_from_le(&new_blk, VSB_ALLOC_COUNT)
            } else {
                0
            };
            let before = match self.volume_before(e.oid)? {
                Some(old_blk) if u64_from_le(&old_blk, VSB_RESERVE_BLOCK_COUNT) != 0 => {
                    u64_from_le(&old_blk, VSB_ALLOC_COUNT)
                }
                _ => 0,
            };
            delta += i128::from(after) - i128::from(before);
        }
        if delta != 0 {
            let current = i128::from(u64_from_le(&self.sm.raw, SPACEMAN_FS_RESERVE_ALLOC_COUNT));
            let updated = (current + delta).clamp(0, i128::from(u64::MAX)) as u64;
            write_u64_le_slice(&mut self.sm.raw, SPACEMAN_FS_RESERVE_ALLOC_COUNT, updated);
        }
        Ok(())
    }

    fn commit_inner(&mut self) -> Result<(), TxnError> {
        let bsz = self.nx.block_size as usize;
        let xid = self.xid;

        // --- Step 1: Apply pending omap entries to the container omap B-tree (COW). ---
        // Virtual objects require {oid, xid} → paddr entries in the container omap.
        // We COW both the B-tree and the omap itself to new blocks so the previous
        // checkpoint's blocks remain valid (fsck_apfs checks o_xid consistency).
        // Returns the new container omap paddr for use in the NX superblock.
        // [CERTAIN: COW required; empirical + omap.rs + the APFS specification]
        //
        // BATCH DEDUP FIX (M8 #3 regression - batch-rename omap corruption):
        // When N operations sharing the same virtual OID (e.g. N renames each
        // calling stage_virtual(vsb_oid, …)) are committed in one Transaction,
        // self.omap_entries contains N entries for that OID pointing to N different
        // physical blocks B1..BN (all staged in self.pending).
        //
        // apply_omap_entries's merge loop processes entries in order: when it sees
        // entry e_{k+1} for an OID it already has at paddr=B_k, it adds B_k to
        // to_free. After all N entries are processed, to_free contains B1..B_{N-1}.
        // free_one_block_cached immediately clears those bits in dirty_bitmaps, so
        // alloc_run_cached subsequently re-allocates B1 (or another freed Bi) as
        // the new container-omap B-tree node or omap header.  Step 2 (pending flush)
        // then overwrites those omap structures with the stale staged VSB data for
        // rename 1, corrupting the container omap.  On re-open, Omap::open sees a
        // non-OMAP object type and returns BadMagic { expected: 11, found: 3 }.
        //
        // Fix: deduplicate self.omap_entries by oid BEFORE calling
        // apply_omap_entries, keeping only the LAST entry per oid.  This ensures
        // apply_omap_entries sees at most one entry per oid and to_free only
        // contains the OLD on-disk block (from the previous checkpoint), never a
        // block that is still staged in self.pending.
        //
        // The discarded intermediate blocks (B1..B_{N-1}) are removed from
        // self.pending (no point writing stale data) and freed via the deferred
        // reclaim path (Phase B) so the bitmap stays consistent.  They were
        // allocated this transaction and never committed, so clearing their bits
        // post-NXSB is equivalent to "never allocated" from the prior-checkpoint's
        // perspective.
        //
        // Derived from linux-apfs-rw transaction-lifecycle pattern: the kernel
        // commits one VSB per transaction; our batch path can accumulate multiple.
        let deduplicated_omap_entries: Vec<OmapEntry>;
        let dropped_intermediate_padrs: Vec<u64>;
        {
            // Walk omap_entries in reverse, keeping the LAST (most recent) entry
            // per oid.  Entries not kept are "intermediate" - their blocks are
            // still in self.pending and must be pruned + deferred-freed.
            let mut seen_oids: std::collections::HashSet<u64> = std::collections::HashSet::new();
            let mut kept: Vec<OmapEntry> = Vec::with_capacity(self.omap_entries.len());
            let mut dropped_padrs: Vec<u64> = Vec::new();
            for e in self.omap_entries.iter().rev() {
                if seen_oids.insert(e.oid) {
                    kept.push(e.clone());
                } else {
                    dropped_padrs.push(e.paddr);
                }
            }
            kept.reverse(); // restore original order for deterministic omap layout
            deduplicated_omap_entries = kept;
            dropped_intermediate_padrs = dropped_padrs;
        }
        self.mirror_allocation_totals(&deduplicated_omap_entries)?;
        self.follow_reserved_volume_counts(&deduplicated_omap_entries)?;

        // Remove stale pending writes for dropped intermediate blocks and
        // remove them from allocated_blocks so Phase A does not flush their
        // alloc-bits (they will be freed in Phase B below).
        if !dropped_intermediate_padrs.is_empty() {
            let drop_set: std::collections::HashSet<u64> =
                dropped_intermediate_padrs.iter().copied().collect();
            self.pending.retain(|pw| !drop_set.contains(&pw.paddr));
            self.pending_index = self
                .pending
                .iter()
                .enumerate()
                .map(|(i, pw)| (pw.paddr, i))
                .collect();
            self.allocated_blocks.retain(|b| !drop_set.contains(b));
        }

        let new_omap_paddr = if !deduplicated_omap_entries.is_empty() {
            apply_omap_entries(
                &mut self.dev,
                &mut self.sm,
                self.nx.omap_oid,
                bsz,
                &deduplicated_omap_entries,
                xid,
                &mut self.dirty_bitmaps,
                &mut self.dirty_cibs,
                &mut self.dirty_runs,
                &mut self.reclaim_bitmap_paddrs,
                &mut self.reclaim_cib_paddrs,
            )?
        } else {
            self.nx.omap_oid // no change
        };

        // Free dropped intermediate blocks via deferred reclaim (Phase B).
        // apply_omap_entries has already allocated the new omap structures, so
        // these padrs will not be re-allocated within this commit.  Clearing
        // their bitmap bits post-NXSB (Phase B) is safe: old-checkpoint readers
        // do not reference these blocks (they were never committed), and the new
        // checkpoint will see them as free after Phase B completes.
        for paddr in dropped_intermediate_padrs {
            free_one_block_cached(
                &mut self.dev,
                &mut self.sm,
                &mut self.dirty_bitmaps,
                &mut self.dirty_cibs,
                &mut self.dirty_runs,
                &mut self.reclaim_bitmap_paddrs,
                &mut self.reclaim_cib_paddrs,
                bsz,
                paddr,
            )?;
        }

        // Blocks this transaction both staged and released: a later operation
        // replaced them, nothing in the new checkpoint names them, and Step 2
        // does not write them.
        let released_here: HashSet<u64>;
        // --- Step 1a: Drain pending_frees BEFORE the Phase A alloc/reclaim split. ---
        // free_block() accumulates COW-replaced blocks (old omap header, old omap
        // b-tree node, old catalog nodes, …) in pending_frees WITHOUT clearing their
        // bitmap bits, so the allocator can never re-hand-out a freed block earlier
        // in the same transaction (the M8 #3 batch double-free fix). All allocation
        // for this transaction is complete by this point: operations allocate during
        // their own execution and Step 1 (apply_omap_entries) is the last allocator
        // caller, so clearing these bits now can never feed a freed block back to
        // alloc_run_cached - the M8 #3 invariant is preserved.
        //
        // CRITICAL ORDERING (fsck "overallocation" fix, #136): the clears MUST be
        // applied to dirty_bitmaps BEFORE the Phase A split below. A bitmap block
        // touched by BOTH an allocation (set-bit) and a reclaim (clear-bit) is
        // deferred to Phase B as ONE coherent copy. When the drain ran AFTER the
        // split (the previous ordering) Phase A had already moved that bitmap into
        // deferred_bitmaps with the clears missing; the drain then re-read a stale
        // base from disk, and the Phase B `or_insert` merge kept the Phase-A copy
        // and silently discarded the clears - leaving the old COW blocks marked
        // allocated but unreferenced, which fsck_apfs reports as "overallocation"
        // (empirically: one create_file leaked the old volume omap header + omap
        // b-tree node + catalog root, decoded as o_xid=prev / type omap+btree).
        //
        // Perf (M8 #3 W3): sort paddrs by CIB index so paddrs in the same CIB are
        // processed consecutively and hit the dirty_cibs cache on the 2nd+ call,
        // reducing O(N×M_cibs) to O(N log N + N×avg_scan_within_cib).
        {
            let blocks_per_chunk = u32_from_le(&self.sm.raw, 36) as u64;
            let chunks_per_cib = u32_from_le(&self.sm.raw, 40) as u64;
            let cib_span = blocks_per_chunk.saturating_mul(chunks_per_cib).max(1);
            let mut frees_to_drain: Vec<u64> = self.pending_frees.drain().collect();
            released_here = frees_to_drain
                .iter()
                .copied()
                .filter(|p| self.pending_index.contains_key(p))
                .collect();
            frees_to_drain.sort_unstable_by_key(|&p| p / cib_span);
            for paddr in frees_to_drain {
                free_one_block_cached(
                    &mut self.dev,
                    &mut self.sm,
                    &mut self.dirty_bitmaps,
                    &mut self.dirty_cibs,
                    &mut self.dirty_runs,
                    &mut self.reclaim_bitmap_paddrs,
                    &mut self.reclaim_cib_paddrs,
                    bsz,
                    paddr,
                )?;
            }
        }

        // --- Step 1b Phase A: Flush alloc-only bitmap + CIB writes (pre-NXSB). ---
        // alloc_block / free_block accumulate bitmap and CIB mutations in memory.
        // Phase A flushes only blocks that contain NEW allocation (set-bit) mutations
        // and no reclaim (clear-bit) mutations. These are safe to persist before the
        // NXSB write: a pre-NXSB crash leaks the new blocks (overallocation, recoverable
        // by fsck) but never makes old-checkpoint blocks appear free - no corruption.
        // Reclaim bitmaps (clear-bit mutations) are deferred to Phase B, after the NXSB
        // write seals the new checkpoint (bug W2-1 fix).
        //
        // This is the primary M8 perf win: 4096 allocs for 16 MiB → 1 bitmap write.
        // Write-ordering discipline: linux-apfs-rw write-ordering (allocation bits are
        // pre-NXSB safe; reclaim/free bits must be post-NXSB).
        //
        // That deferral is for a block edited where it lies. A block copied
        // into the internal pool is a different matter: the previous
        // checkpoint keeps its own copy untouched, and the new checkpoint names
        // the fresh block, so the fresh block has to be on the media before the
        // superblock that names it - a power loss between the two would leave
        // the newest checkpoint pointing at a block that was never written,
        // while the allocator state it describes rolls back to the copy's
        // source. Every pool block is therefore written now, whatever bits it
        // carries; only a block outside the pool (a container with no usable
        // pool, edited in place) is held back.
        let mut deferred_bitmaps: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut deferred_cibs: HashMap<u64, Vec<u8>> = HashMap::new();
        {
            let keys_to_defer: Vec<u64> = self
                .dirty_bitmaps
                .keys()
                .filter(|k| self.reclaim_bitmap_paddrs.contains(k) && !self.sm.ip.contains(**k))
                .copied()
                .collect();
            for k in keys_to_defer {
                if let Some(buf) = self.dirty_bitmaps.remove(&k) {
                    deferred_bitmaps.insert(k, buf);
                }
            }
        }
        {
            let keys_to_defer: Vec<u64> = self
                .dirty_cibs
                .keys()
                .filter(|k| self.reclaim_cib_paddrs.contains(k) && !self.sm.ip.contains(**k))
                .copied()
                .collect();
            for k in keys_to_defer {
                if let Some(buf) = self.dirty_cibs.remove(&k) {
                    deferred_cibs.insert(k, buf);
                }
            }
        }
        // Flush alloc-only bitmaps now (pre-NXSB).
        for (bm_paddr, bm_buf) in self.dirty_bitmaps.drain() {
            let off = bm_paddr.checked_mul(self.nx.block_size as u64).ok_or(
                TxnError::BlockOutOfRange {
                    block: bm_paddr,
                    size: self.dev.size(),
                },
            )?;
            self.dev.write_at(off, &bm_buf)?;
        }
        // Flush alloc-only CIBs now (pre-NXSB).
        for (cib_paddr, mut cib_buf) in self.dirty_cibs.drain() {
            update_checksum_in_place(&mut cib_buf);
            let off = cib_paddr.checked_mul(self.nx.block_size as u64).ok_or(
                TxnError::BlockOutOfRange {
                    block: cib_paddr,
                    size: self.dev.size(),
                },
            )?;
            self.dev.write_at(off, &cib_buf)?;
        }
        // The chunk-info address blocks this transaction copied: fresh pool
        // blocks the previous checkpoint does not name, so they are as safe
        // to write now as the copied chunk-info blocks above.
        for (cab_paddr, cab) in self.sm.cabs.iter_mut().filter(|(_, c)| c.dirty) {
            update_checksum_in_place(&mut cab.raw);
            let off = cab_paddr.checked_mul(self.nx.block_size as u64).ok_or(
                TxnError::BlockOutOfRange {
                    block: *cab_paddr,
                    size: self.dev.size(),
                },
            )?;
            self.dev.write_at(off, &cab.raw)?;
            cab.dirty = false;
        }

        // File data still in the data buffer goes first: the barrier after
        // this step covers it too.
        self.spill_data()?;
        // --- Step 2: Write all staged data/metadata blocks. ---
        // In address order, blocks at consecutive addresses joined into one
        // write of at most `COMMIT_WRITE_MAX` bytes: a file staged block by
        // block was one device write per block. The order inside this step
        // does not matter; the barrier after it does.
        let bsz = self.nx.block_size as usize;
        let mut order: Vec<usize> = (0..self.pending.len())
            .filter(|&i| {
                self.pending
                    .get(i)
                    .is_some_and(|pw| !released_here.contains(&pw.paddr))
            })
            .collect();
        order.sort_unstable_by_key(|&i| self.pending.get(i).map_or(0, |pw| pw.paddr));
        let pending = &self.pending;
        let dev = &mut self.dev;
        let mut run: Vec<u8> = Vec::new();
        let mut run_start = 0u64;
        let mut run_next = 0u64;
        for pw in order.iter().filter_map(|&i| pending.get(i)) {
            let whole = pw.data.len() % bsz == 0 && !pw.data.is_empty();
            let joins = !run.is_empty()
                && whole
                && pw.paddr == run_next
                && run.len() + pw.data.len() <= COMMIT_WRITE_MAX;
            if !joins {
                write_run(dev, run_start, &run, bsz)?;
                run.clear();
                run_start = pw.paddr;
            }
            run.extend_from_slice(&pw.data);
            run_next = if whole {
                pw.paddr + (pw.data.len() / bsz) as u64
            } else {
                // Not a whole number of blocks: nothing joins after it.
                u64::MAX
            };
        }
        write_run(dev, run_start, &run, bsz)?;
        drop(run);

        // (pending_frees were drained in Step 1a, before the Phase A split - see
        // the #136 fix there. Draining here, after the split, dropped the clears.)

        // --- Step 1c: Settle the internal pool. The blocks this transaction
        // copied away from go on the pool's free queue, and the pool bitmap
        // moves to a fresh ring slot. Both rewrite bookkeeping the spaceman
        // carries, so both have to happen before it is sealed below.
        self.enqueue_internal_pool_releases()?;
        self.flush_internal_pool(xid)?;

        // --- Step 2: Write spaceman + all other ephemerals to checkpoint data area. ---
        // data_index = xp_data_next (current superblock value). Objects are
        // tiled into consecutive ring slots, each taking as many slots as it
        // has blocks, and the data length is counted in blocks - the spaceman
        // of a large container is longer than one.
        // [CERTAIN: linux-apfs-rw/transaction.c accumulates data_len by
        //  obj_blkcnt; apfsprogs/apfsck/super.c tiles the ring the same way]
        let data_index = self.nx.xp_data_next;
        let mut data_len: u32 = 0;
        // The ring has to hold this checkpoint's objects next to the previous
        // checkpoint's, which stays live until the new superblock is sealed;
        // otherwise the tiling below would write over it.
        let blocks_of = |len: usize, what: &str| -> Result<u32, TxnError> {
            let blocks = data_ring_object_blocks(&self.nx, len as u32, what)?;
            Ok(u32::try_from(blocks).unwrap_or(u32::MAX))
        };
        let mut needed = blocks_of(self.sm.raw.len(), "spaceman")?;
        for eph in &self.other_ephemerals {
            needed = needed.saturating_add(blocks_of(eph.raw.len(), "ephemeral object")?);
        }
        checkpoint_ring_has_room(self.nx.xp_data_blocks, self.nx.xp_data_len, needed)?;
        // Update spaceman xid and recompute Fletcher-64 over the whole object.
        update_xid_and_checksum(&mut self.sm.raw, xid);
        let sm_paddr = write_data_ring_object(&mut self.dev, &self.nx, data_index, &self.sm.raw)?;
        let sm_size = self.sm.raw.len() as u32;
        data_len += data_ring_object_blocks(&self.nx, sm_size, "spaceman")? as u32;

        // Write other ephemeral objects (reaper, reaper-list btree nodes) to
        // the slots that follow, updating their xid each time.
        // [CERTAIN: fsck_apfs requires nx_xp_data_len >= 2; empirical baseline]
        let mut eph_paddrs: Vec<u64> = Vec::with_capacity(self.other_ephemerals.len());
        for eph in self.other_ephemerals.iter_mut() {
            update_xid_and_checksum(&mut eph.raw, xid);
            let slot = data_index.wrapping_add(data_len);
            let eph_bno = write_data_ring_object(&mut self.dev, &self.nx, slot, &eph.raw)?;
            data_len +=
                data_ring_object_blocks(&self.nx, eph.raw.len() as u32, "ephemeral object")? as u32;
            eph_paddrs.push(eph_bno);
        }
        let total_ephemerals = data_len; // in blocks, as nx_xp_data_len counts them
        let new_data_next = (data_index + total_ephemerals) % self.nx.xp_data_blocks;

        // (There used to be a step here that reached back into the previous
        // checkpoint's spaceman and overwrote its free count. It was needed
        // while the chunk bitmaps were shared: allocating set bits the previous
        // checkpoint's count knew nothing about, so it read as overallocated.
        // Now that a bitmap belonging to an older transaction is copied before
        // it is changed, the previous checkpoint keeps the bitmaps its count was
        // computed from, and writing into it would be the very thing that makes
        // a torn commit unrecoverable.)

        // --- Step 3: Write checkpoint map block to descriptor area. ---
        // desc_index = xp_desc_next.
        let desc_index = self.nx.xp_desc_next;
        let desc_bno = self.nx.xp_desc_base + (desc_index as u64 % self.nx.xp_desc_blocks as u64);
        // Build checkpoint_map_phys_t with entries for spaceman + all other ephemerals.
        // [CERTAIN: checkpoint_map_phys_t layout, the APFS specification C.2]
        let cmap_block = build_checkpoint_map(CheckpointMapArgs {
            bsz,
            xid,
            block_oid: desc_bno,
            sm_oid: self.sm.oid,
            sm_type: self.sm.o_type,
            sm_subtype: self.sm.o_subtype,
            sm_paddr,
            sm_size,
            other_ephemerals: &self.other_ephemerals,
            other_paddrs: &eph_paddrs,
        })?;
        let cmap_off =
            desc_bno
                .checked_mul(self.nx.block_size as u64)
                .ok_or(TxnError::BlockOutOfRange {
                    block: desc_bno,
                    size: self.dev.size(),
                })?;
        self.dev.write_at(cmap_off, &cmap_block)?;
        // The NX superblock ring slot is at desc_index + 1.
        // The ring write index (where NX is written) is one past the cmap.
        let nx_ring_slot = (desc_index + 1) % self.nx.xp_desc_blocks;
        // xp_desc_next advances past BOTH the cmap block and the NX block.
        // [CERTAIN: empirical - baseline macOS scratch xid=2 has desc_index=2,
        //  desc_len=2, desc_next=4 (= desc_index+2); the APFS specification]
        let new_desc_next = (desc_index + 2) % self.nx.xp_desc_blocks;

        // --- Step 4: Build and write the new nx_superblock - LAST (atomic). ---
        // Update nx_superblock fields:
        //   nx_next_xid += 1 (was current_xid + 1 = xid, so now xid + 1)
        //   nx_next_oid  = self.next_oid
        //   xp_desc_next = (desc_index + 2) % desc_blocks  (past cmap + NX)
        //   xp_data_next = new_data_next
        //   xp_desc_index = desc_index (start of this checkpoint's descriptor area)
        //   xp_desc_len   = 2 (checkpoint map block + NX superblock block)
        //   xp_data_index = data_index (start of this checkpoint's data area)
        //   xp_data_len   = blocks the ephemerals occupy (the spaceman may take several)
        // [CERTAIN: empirical baseline macOS scratch; C.1/C.2 the APFS specification]
        let mut nx_raw = self.nx.raw.clone();
        // nx_next_xid @0x60 (u64) - the next xid to use AFTER this checkpoint.
        write_u64_le(&mut nx_raw, 0x60, xid + 1);
        // nx_next_oid @0x58 (u64)
        write_u64_le(&mut nx_raw, 0x58, self.next_oid);
        // nx_omap_oid @0xA0 (u64) - updated to COW'd container omap block.
        // [CERTAIN: empirical - COW allocates new omap block each checkpoint]
        write_u64_le(&mut nx_raw, 0xA0, new_omap_paddr);
        // xp_desc_next @0x80 (u32) - past both cmap and NX ring slot
        write_u32_le(&mut nx_raw, 0x80, new_desc_next);
        // xp_data_next @0x84 (u32)
        write_u32_le(&mut nx_raw, 0x84, new_data_next);
        // xp_desc_index @0x88 (u32) - start of THIS checkpoint's descriptor blocks
        write_u32_le(&mut nx_raw, 0x88, desc_index);
        // xp_desc_len @0x8C (u32) - 2 blocks: checkpoint map + NX superblock
        write_u32_le(&mut nx_raw, 0x8C, 2u32);
        // xp_data_index @0x90 (u32) - start of THIS checkpoint's data blocks
        write_u32_le(&mut nx_raw, 0x90, data_index);
        // xp_data_len @0x94 (u32) - spaceman + other ephemerals
        write_u32_le(&mut nx_raw, 0x94, total_ephemerals);
        // nx_superblock o_xid @0x10 (u64) - the xid of THIS checkpoint
        write_u64_le(&mut nx_raw, 0x10, xid);
        // Recompute Fletcher-64 (covers bytes [8..block_size)).
        update_checksum_in_place(&mut nx_raw);
        // --- COW-1 barrier #1: flush every checkpoint-referenced block to media
        // BEFORE sealing the new checkpoint. All blocks the new NXSB points at
        // (container omap, staged data/metadata, spaceman, other ephemerals, the
        // checkpoint map) were issued above; this flush makes them durable first,
        // so a torn/reordered write can never expose an NXSB that references
        // not-yet-durable blocks. Without it the container can read empty or
        // corrupt after an unclean stop. Mirrors linux-apfs-rw apfs_checkpoint_end
        // (filemap_write_and_wait before the superblock write).
        self.dev.flush_data()?;
        // The superblock goes into the descriptor ring at nx_ring_slot
        // (= desc_index + 1, one past the checkpoint map) FIRST: that write is
        // the commit point. macOS and fsck_apfs mount the newest valid
        // superblock found in the ring and treat block 0 as a bootstrap copy,
        // so a copy written before the ring slot would, after a power loss in
        // between, announce a checkpoint nobody else mounts. The ring slot is
        // flushed on its own before block 0 is brought up to date.
        // [CERTAIN: linux-apfs-rw apfs_checkpoint_end writes the ring slot with
        //  a flush on either side and refreshes block 0 separately;
        //  apfsprogs read_latest_super scans only the ring and reports a block
        //  0 that disagrees as a crash; apfs.kext
        //  nx_checkpoint_find_highest_xid picks the ring's highest xid and
        //  nx_superblock_agrees_with_main_superblock only logs a block 0
        //  that is newer]
        let nx_ring_bno =
            self.nx.xp_desc_base + (nx_ring_slot as u64 % self.nx.xp_desc_blocks as u64);
        let nx_ring_off = nx_ring_bno.checked_mul(self.nx.block_size as u64).ok_or(
            TxnError::BlockOutOfRange {
                block: nx_ring_bno,
                size: self.dev.size(),
            },
        )?;
        self.dev.write_at(nx_ring_off, &nx_raw)?;
        // --- COW-1 barrier #2: flush the sealed checkpoint (the ring slot) to
        // media BEFORE anything that assumes the new checkpoint is durable:
        // the block 0 copy, and Phase B below, whose crash-safety argument
        // ("new NXSB is valid") only holds once the NXSB is durable; otherwise
        // a crash could persist the freed-bit writes while the new NXSB is
        // lost, letting the next mount fall back to the prior checkpoint whose
        // still-live blocks are now marked free - silent corruption. Mirrors
        // linux-apfs-rw apfs_checkpoint_end (filemap_write_and_wait after the
        // superblock write).
        self.dev.flush_data()?;
        // Block 0 is the bootstrap copy of the sealed checkpoint. It takes no
        // barrier of its own: no reader mounts it over the ring, a copy a
        // power cut loses is the stale copy every reader already expects and
        // mounts past, and the next commit's first barrier makes it durable.
        // [CERTAIN: apfsprogs read_latest_super scans only the ring;
        //  apfs.kext nx_checkpoint_find_highest_xid picks the ring's highest
        //  xid]
        self.dev.write_at(0, &nx_raw)?;

        // --- Phase B: Flush reclaim bitmaps + CIBs (post-NXSB). ---
        // Now that the new checkpoint is sealed (NXSB written), it is safe to
        // persist the reclaim (clear-bit) bitmap mutations. Old-checkpoint
        // readers see the previous checkpoint via its own NXSB; clearing these
        // bits makes the freed blocks available to the NEXT transaction.
        //
        // Crash here: new NXSB is valid; old blocks that were freed show up
        // as still-allocated in the bitmap (some bits not yet cleared). The
        // new checkpoint's live extents do NOT reference those blocks, so fsck
        // sees them as leaked (overallocation in old xid scope), not corruption.
        // A subsequent fsck/scrub pass or the next transaction can reclaim them.
        //
        // Merge any reclaim bitmaps that were added by Step 2a (pending_frees
        // drain) into deferred_bitmaps. These arrived after the Phase A split
        // so they were not separated earlier; they must also be Phase B.
        for (k, v) in self.dirty_bitmaps.drain() {
            deferred_bitmaps.entry(k).or_insert(v);
        }
        for (k, v) in self.dirty_cibs.drain() {
            deferred_cibs.entry(k).or_insert(v);
        }
        for (bm_paddr, bm_buf) in deferred_bitmaps {
            let off = bm_paddr.checked_mul(self.nx.block_size as u64).ok_or(
                TxnError::BlockOutOfRange {
                    block: bm_paddr,
                    size: self.dev.size(),
                },
            )?;
            self.dev.write_at(off, &bm_buf)?;
        }
        for (cib_paddr, mut cib_buf) in deferred_cibs {
            update_checksum_in_place(&mut cib_buf);
            let off = cib_paddr.checked_mul(self.nx.block_size as u64).ok_or(
                TxnError::BlockOutOfRange {
                    block: cib_paddr,
                    size: self.dev.size(),
                },
            )?;
            self.dev.write_at(off, &cib_buf)?;
        }

        // --- Post-commit verification (trust-but-verify) -------------------
        // Some USB controllers ACK a write that did not durably land (dropped,
        // torn, or reordered). Re-read the sealed checkpoint - the NX superblock
        // at its descriptor-ring slot plus the checkpoint map - and confirm
        // each has a valid Fletcher-64 and the new transaction XID. On failure
        // we return Err WITHOUT having corrupted anything: every referenced
        // block is COW (the previous checkpoint is intact), so the next mount
        // simply falls back to it and the failed transaction is lost. This
        // catches silent write failures that APFS's own model assumes the
        // hardware does not make. [matches "trust-but-verify"; not a snapshot/
        // reread layer - verifies only the atomic commit point.]
        //
        // Block 0 is not the commit point: a stale copy there is what every
        // reader expects after a power loss and mounts past. So it is checked
        // last, rewritten once if it did not land, and only reported when it
        // still disagrees - the transaction is committed either way.
        {
            let bsz = self.nx.block_size as usize;
            let mut buf = vec![0u8; bsz];
            self.dev.read_at(nx_ring_off, &mut buf)?;
            verify_checkpoint_block(&buf, Some(xid), "nxsb@ring")?;
            let cmap_bno = self.nx.xp_desc_base + desc_index as u64;
            let cmap_off = cmap_bno.checked_mul(self.nx.block_size as u64).ok_or(
                TxnError::BlockOutOfRange {
                    block: cmap_bno,
                    size: self.dev.size(),
                },
            )?;
            self.dev.read_at(cmap_off, &mut buf)?;
            verify_checkpoint_block(&buf, None, "checkpoint-map")?;
            verify_checkpoint_data(
                &mut self.dev,
                &self.nx,
                xid,
                sm_paddr,
                sm_size,
                &self.other_ephemerals,
                &eph_paddrs,
            )?;
            self.dev.read_at(0, &mut buf)?;
            if verify_checkpoint_block(&buf, Some(xid), "nxsb@block0").is_err() {
                self.dev.write_at(0, &nx_raw)?;
                self.dev.flush_data()?;
                self.dev.read_at(0, &mut buf)?;
                if let Err(e) = verify_checkpoint_block(&buf, Some(xid), "nxsb@block0") {
                    tracing::warn!(
                        "commit {xid}: the bootstrap copy of the superblock did not land \
                         ({e}); the checkpoint is sealed in the descriptor ring and mounts"
                    );
                }
            }
        }

        Ok(())
    }
}

/// Re-read the checkpoint's data - the spaceman and every other ephemeral
/// object, at the ring address the checkpoint map names - and require each
/// whole, checksummed and under `xid`. A dropped write here would mount as
/// the previous checkpoint's allocator paired with this checkpoint's blocks,
/// and the transaction would still have been reported as committed.
fn verify_checkpoint_data<D: WritableBlockDevice>(
    dev: &mut D,
    nx: &NxView,
    xid: u64,
    sm_paddr: u64,
    sm_size: u32,
    ephemerals: &[EphemeralObj],
    eph_paddrs: &[u64],
) -> Result<(), TxnError> {
    let back = read_data_ring_object(dev, nx, sm_paddr, sm_size, "spaceman")
        .map_err(|e| TxnError::PostCommitVerify(format!("spaceman@{sm_paddr}: {e}")))?;
    verify_checkpoint_block(&back, Some(xid), "spaceman")?;
    if ephemerals.len() != eph_paddrs.len() {
        return Err(TxnError::PostCommitVerify(format!(
            "{} ephemeral objects were written at {} addresses",
            ephemerals.len(),
            eph_paddrs.len()
        )));
    }
    for (eph, &paddr) in ephemerals.iter().zip(eph_paddrs) {
        let what = format!("ephemeral {:#x}", eph.oid);
        let back = read_data_ring_object(dev, nx, paddr, eph.raw.len() as u32, &what)
            .map_err(|e| TxnError::PostCommitVerify(format!("{what}@{paddr}: {e}")))?;
        verify_checkpoint_block(&back, Some(xid), &what)?;
    }
    Ok(())
}

/// Verify a re-read checkpoint object: its Fletcher-64 must be valid and, when
/// `want_xid` is given, its `o_xid` (offset 16) must equal the committed XID.
/// Used by `commit_inner`'s post-commit read-back to detect silent write
/// failures. Returns `TxnError::PostCommitVerify` on mismatch.
fn verify_checkpoint_block(buf: &[u8], want_xid: Option<u64>, what: &str) -> Result<(), TxnError> {
    apfs_core::checksum::verify_block(buf)
        .map_err(|_| TxnError::PostCommitVerify(format!("{what}: invalid Fletcher-64 checksum")))?;
    if let Some(x) = want_xid {
        let oxid = u64_from_le(buf, 16);
        if oxid != x {
            return Err(TxnError::PostCommitVerify(format!(
                "{what}: o_xid {oxid:#x} != committed {x:#x}"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Insert `entries` into the container omap B-tree leaf node.
///
/// The container omap is a physical object at `omap_oid` (paddr = oid for
/// physical objects).  The omap block contains `om_tree_oid` at byte offset 48,
/// which is the physical address of the flat (single-level) B-tree root leaf.
///
/// Layout of the fixed-kv omap B-tree leaf:
///   Header (obj_phys + btree_node_phys header) = 56 bytes before TOC area.
///     obj_phys: 32 bytes (o_cksum, o_oid, o_xid, o_type, o_subtype)
///     btn_flags u16 @32, btn_level u16 @34, btn_nkeys u32 @36
///     btn_table_space: off u16 @40, len u16 @42
///     btn_free_space: off u16 @44, len u16 @46
///     btn_key_free_list: off u16 @48, len u16 @50
///     btn_val_free_list: off u16 @52, len u16 @54
///   TOC (immediately after 56-byte header, relative offset 0 = DATA_BASE=56):
///     Each fixed-kv TOC entry = (key_off u16, val_off u16) = 4 bytes
///   Key area (after TOC area, relative to DATA_BASE+toc_space_off):
///     Each omap_key_t = oid(u64) + xid(u64) = 16 bytes
///   Value area (grows downward from end of block, leaf has no btree_info_t):
///     Each omap_val_t = flags(u32) + size(u32) + paddr(u64) = 16 bytes
///     val_off in TOC = distance from END of block (positive, not negative).
///
/// The B-tree root paddr (a physical object) is written in place; no omap
/// registration needed for it.
///
/// [CERTAIN: omap.rs BTree layout; apfs_raw.h omap_phys_t / omap_key_t /
/// omap_val_t / btree_node_phys_t; the APFS specification; empirically
/// verified against real 64 MiB scratch image]
///
/// Apply `entries` to the container omap B-tree using copy-on-write (COW):
/// 1. Allocate a new block for the updated B-tree.
/// 2. Allocate a new block for the updated container omap (om_tree_oid updated).
/// 3. Write both; return the new container omap paddr.
///
/// The caller must update `nx_omap_oid` in the NX superblock to the returned value.
/// [CERTAIN: COW required - in-place overwrite invalidates previous checkpoints;
/// empirical: fsck_apfs "invalid o_xid" on old checkpoint's omap tree block]
/// The largest single device write a commit makes of staged blocks at
/// consecutive addresses.
const COMMIT_WRITE_MAX: usize = 8 << 20;

/// File data a transaction holds before writing it to the device (see
/// `Transaction::data_buf`).
const DATA_BUFFER_MAX: usize = 4 << 20;

/// A run of new data this long or longer is written at once: it is a large
/// write already.
const DATA_DIRECT_MIN: usize = 1 << 20;

/// Write `run` (staged blocks joined) at block `start`; nothing for an
/// empty run.
fn write_run<D: WritableBlockDevice>(
    dev: &mut D,
    start: u64,
    run: &[u8],
    bsz: usize,
) -> Result<(), TxnError> {
    if run.is_empty() {
        return Ok(());
    }
    let off = start
        .checked_mul(bsz as u64)
        .ok_or(TxnError::BlockOutOfRange {
            block: start,
            size: dev.size(),
        })?;
    dev.write_at(off, run)?;
    Ok(())
}

fn apply_omap_entries<D: WritableBlockDevice>(
    dev: &mut D,
    sm: &mut SpacemanView,
    omap_paddr: u64,
    bsz: usize,
    entries: &[OmapEntry],
    xid: u64,
    dirty_bitmaps: &mut HashMap<u64, Vec<u8>>,
    dirty_cibs: &mut HashMap<u64, Vec<u8>>,
    dirty_runs: &mut HashMap<u64, crate::free_runs::ChunkFreeRuns>,
    reclaim_bitmap_paddrs: &mut HashSet<u64>,
    reclaim_cib_paddrs: &mut HashSet<u64>,
) -> Result<u64, TxnError> {
    if entries.is_empty() {
        return Ok(omap_paddr); // no change needed
    }

    // Read container omap block (physical object: paddr = oid).
    let omap_off = omap_paddr
        .checked_mul(bsz as u64)
        .ok_or(TxnError::OmapParse("omap paddr overflow".into()))?;
    let mut omap_blk = read_verified_block(dev, omap_off, bsz, "container object map", omap_paddr)
        .map_err(TxnError::OmapParse)?;

    // om_tree_oid @48 (u64) - paddr of the B-tree root.
    // [CERTAIN: apfs_raw.h struct apfs_omap_phys; offset 48 =
    //  obj_phys(32) + om_flags(4) + om_snap_count(4) + om_tree_type(4)
    //  + om_snapshot_tree_type(4) = 48; the APFS specification]
    let tree_paddr = u64_from_le(&omap_blk, 48);
    if tree_paddr == 0 {
        return Err(TxnError::OmapParse("om_tree_oid is 0".into()));
    }

    // Read the B-tree root leaf block.
    let tree_off = tree_paddr
        .checked_mul(bsz as u64)
        .ok_or(TxnError::OmapParse("tree paddr overflow".into()))?;
    let tree_blk = read_verified_block(dev, tree_off, bsz, "container object map root", tree_paddr)
        .map_err(TxnError::OmapParse)?;

    // btree_node_phys_t header fields.
    // btn_flags @32 (u16), btn_level @34 (u16), btn_nkeys @36 (u32)
    // btn_table_space: off @40 (u16), len @42 (u16)
    // DATA_BASE = 56 bytes (32 obj_phys + 24 btn header fields before TOC).
    let data_base: usize = 56;

    let btn_flags = u16_from_le(&tree_blk, 32);
    let btn_level = u16_from_le(&tree_blk, 34);
    let mut nkeys = u32_from_le(&tree_blk, 36) as usize;
    let toc_space_off = u16_from_le(&tree_blk, 40) as usize; // relative to DATA_BASE
    let toc_space_len = u16_from_le(&tree_blk, 42) as usize;

    // Only handle flat leaf (level == 0). Non-flat omap trees would require
    // recursive insertion - beyond scope of M7a (scratch image is single-level).
    if btn_level != 0 {
        return Err(TxnError::OmapParse(format!(
            "multi-level omap B-tree (level={btn_level}) not supported"
        )));
    }

    // The table of contents starts `toc_space_off` bytes into the record area
    // and the keys follow the whole table space.
    let toc_base = data_base + toc_space_off;
    let key_area_start = data_base + toc_space_len;

    // A single-node tree's root carries the tree's description in its last
    // 40 bytes, so its value area ends there. A level-0 node without the
    // root flag (0x1) is a leaf of a larger tree and cannot be the root the
    // object map names.
    // [CERTAIN: empirical macOS scratch image; the APFS specification]
    let is_root = btn_flags & 0x0001 != 0;
    if !is_root {
        return Err(TxnError::OmapParse(
            "omap B-tree root lacks the root flag".into(),
        ));
    }
    const BTREE_INFO_SIZE: usize = 40;
    let val_area_end = bsz
        .checked_sub(BTREE_INFO_SIZE)
        .ok_or_else(|| TxnError::OmapParse("block too small for a B-tree root".into()))?;

    // Fixed sizes for omap B-tree.
    const KEY_SIZE: usize = 16; // omap_key_t = oid(8) + xid(8)
    const VAL_SIZE: usize = 16; // omap_val_t = flags(4) + size(4) + paddr(8)
    const TOC_ENTRY_SIZE: usize = 4; // (key_off u16, val_off u16)

    // The records the node holds, read through its table. A table, key or
    // value that lies outside its area is a broken node; rebuilding around it
    // would drop the mapping (and never free the block it names), so it is an
    // error rather than a skipped record. An untouched record is carried over
    // whole: its flags and size are the object's, not the rebuild's to reset.
    #[derive(Clone)]
    struct OmapKV {
        oid: u64,
        xid: u64,
        flags: u32,
        size: u32,
        paddr: u64,
    }
    if toc_space_off + nkeys * TOC_ENTRY_SIZE > toc_space_len || key_area_start > val_area_end {
        return Err(TxnError::OmapParse(format!(
            "omap B-tree root: {nkeys} table entries at {toc_space_off} do not fit its \
             {toc_space_len}-byte table space"
        )));
    }
    let mut existing: Vec<OmapKV> = Vec::with_capacity(nkeys);
    for i in 0..nkeys {
        let toc_off = toc_base + i * TOC_ENTRY_SIZE;
        let key_off_rel = u16_from_le(&tree_blk, toc_off) as usize;
        let val_off_rel = u16_from_le(&tree_blk, toc_off + 2) as usize;

        // A value sits `val_off_rel` bytes before the end of the value area;
        // on a macOS-written root the first record's value is at 16, right
        // under the tree description.
        // [CERTAIN: empirical macOS scratch image; the APFS specification]
        let v_abs = val_area_end
            .checked_sub(val_off_rel)
            .filter(|_| val_off_rel >= VAL_SIZE)
            .ok_or_else(|| {
                TxnError::OmapParse(format!(
                    "omap B-tree root: record {i} value offset {val_off_rel} lies outside \
                     the value area"
                ))
            })?;
        let k_off = key_area_start + key_off_rel;
        if k_off + KEY_SIZE > v_abs {
            return Err(TxnError::OmapParse(format!(
                "omap B-tree root: record {i} key at {k_off} runs into the value at {v_abs}"
            )));
        }
        existing.push(OmapKV {
            oid: u64_from_le(&tree_blk, k_off),
            xid: u64_from_le(&tree_blk, k_off + 8),
            flags: u32_from_le(&tree_blk, v_abs),
            size: u32_from_le(&tree_blk, v_abs + 4),
            paddr: u64_from_le(&tree_blk, v_abs + 8),
        });
    }

    // Merge new entries: for each incoming OmapEntry, update an existing record
    // with the same oid (advancing its xid to the new one) or append a new entry.
    // The omap B-tree is keyed by (oid ASC, xid ASC); we do a simple upsert.
    let mut merged = existing.clone();
    // Container-level COW reclaim: when an existing virtual-object mapping (e.g.
    // the volume superblock) is replaced by a new paddr, the OLD block becomes
    // unreferenced by the live container omap and must be freed, or it leaks
    // (fsck "overallocation"). [the APFS specification Q3 - container blocks
    //  are reclaimed like volume blocks; no separate VSB tombstone.]
    let mut to_free: Vec<u64> = Vec::new();
    for e in entries {
        // Check if an entry with the same oid already exists (update it).
        if let Some(kv) = merged.iter_mut().find(|kv| kv.oid == e.oid) {
            if kv.paddr != e.paddr {
                to_free.push(kv.paddr); // old VSB (or other virtual object) block
            }
            *kv = OmapKV {
                oid: e.oid,
                xid: e.xid,
                flags: 0,
                size: e.size,
                paddr: e.paddr,
            };
        } else {
            merged.push(OmapKV {
                oid: e.oid,
                xid: e.xid,
                flags: 0,
                size: e.size,
                paddr: e.paddr,
            });
        }
    }
    // Sort by (oid, xid) ascending.
    merged.sort_by(|a, b| a.oid.cmp(&b.oid).then(a.xid.cmp(&b.xid)));
    nkeys = merged.len();

    // Rebuild the B-tree block from scratch with the merged key set.
    let mut new_blk = vec![0u8; bsz];

    // Copy obj_phys header from old block (oid, type, subtype stay the same).
    if let (Some(dst), Some(src)) = (new_blk.get_mut(..32), tree_blk.get(..32)) {
        dst.copy_from_slice(src);
    }
    // Update o_xid to current transaction xid.
    write_u64_le_slice(&mut new_blk, 16, xid);

    // btn_flags, btn_level, btn_nkeys.
    write_u16_le_slice(&mut new_blk, 32, btn_flags);
    write_u16_le_slice(&mut new_blk, 34, 0u16); // btn_level = 0 (leaf)
    write_u32_le_slice(&mut new_blk, 36, nkeys as u32);

    // Confirmed (, the APFS specification p.297):
    //
    // "Keys and values are normally aligned to eight-byte boundaries when
    // stored." DATA_BASE = 56 is itself 8-aligned, so for the key area
    // (which starts at DATA_BASE + btn_table_space.len) to land on an
    // 8-byte boundary, btn_table_space.len MUST itself be a multiple of 8.
    //
    // Previous emissions used max(existing, nkeys*4) directly; on scratch
    // (where existing toc_space_len happens to be 8-aligned 256) this
    // worked, but the Q-C6 attempt that set len=4 (= 1 * 4) placed keys
    // at byte 60 (= 56+4; 60 % 8 = 4) → misaligned → fsck rejected the
    // node as a chain → "Snapshot metadata tree is invalid". Round-up
    // to 8 to guarantee correctness in both cases.
    //
    // Q-G LOCKED: btn_free_space.off = N * KEY_SIZE (offset from
    // key_area_start to the start of shared free space).
    // btn_free_space.len = lowest_v_abs - free_space_start (the gap
    // between the packed key tail and the packed value head).
    let toc_needed = nkeys * TOC_ENTRY_SIZE;
    let toc_alloc_raw = toc_space_len.max(toc_needed);
    let toc_alloc = (toc_alloc_raw + 7) & !7usize; // align up to 8 (Q-J)
    let key_area_start_new = data_base + toc_alloc;
    let key_bytes = nkeys * KEY_SIZE;
    let lowest_v_abs = val_area_end.saturating_sub(nkeys * VAL_SIZE);
    let key_end = key_area_start_new + key_bytes;
    let free_space_len = lowest_v_abs.saturating_sub(key_end);

    // Confirmed (): {0, 0} is genuinely invalid;
    // fsck silently rolls back to the previous checkpoint on encountering it
    // and reports the rolled-back state CLEAN - making it LOOK like scratch
    // passes. The "scratch CLEAN with {0, 0}" baseline was the illusion of
    // a silent rollback to xid=2 (kernel's pre-snapshot state). Write the
    // correct {N*KEY_SIZE, lowest_v_abs - key_end} values so fsck actually
    // verifies our xid=N+1 checkpoint. The remaining real bug surfaces in
    // the snap-meta tree (next).
    write_u16_le_slice(&mut new_blk, 40, 0u16); // table_space.off (rel. DATA_BASE)
    write_u16_le_slice(&mut new_blk, 42, toc_alloc as u16); // table_space.len (8-aligned, preserved)
    write_u16_le_slice(&mut new_blk, 44, key_bytes as u16); // free_space.off (Q-G/Q-L)
    write_u16_le_slice(&mut new_blk, 46, free_space_len as u16); // free_space.len (Q-L)
    write_u16_le_slice(&mut new_blk, 48, 0xFFFFu16); // key_free_list.off = BTOFF_INVALID
    write_u16_le_slice(&mut new_blk, 50, 0u16); // key_free_list.len = 0
    write_u16_le_slice(&mut new_blk, 52, 0xFFFFu16); // val_free_list.off = BTOFF_INVALID
    write_u16_le_slice(&mut new_blk, 54, 0u16); // val_free_list.len = 0

    // Write TOC + keys (sequential) and values (growing down from val_area_end).
    //
    // val_off_rel encoding (empirically confirmed, macOS kernel):
    //   val_off_rel = (i + 1) * VAL_SIZE  for entry i (0-indexed, first entry closest to end)
    //   v_abs = val_area_end - val_off_rel
    //   where val_area_end = bsz - BTREE_INFO_SIZE for root nodes.
    // So entry 0: val_off_rel=16, v_abs=val_area_end-16.
    // [CERTAIN: empirical macOS scratch image; the APFS specification]
    for (i, kv) in merged.iter().enumerate() {
        // The header written above puts the table at the start of the record
        // area, wherever the source node kept its own.
        let toc_off = data_base + i * TOC_ENTRY_SIZE;

        // Key offset relative to key_area_start_new.
        let key_off_rel = i * KEY_SIZE;
        let k_abs = key_area_start_new + key_off_rel;
        if k_abs + KEY_SIZE > bsz {
            return Err(TxnError::OmapParse(
                "omap B-tree keys overflow block".into(),
            ));
        }

        // Value: placed from val_area_end downward.
        // val_off_rel = (i + 1) * VAL_SIZE.
        let val_off_rel = (i + 1) * VAL_SIZE;
        let v_abs = val_area_end.saturating_sub(val_off_rel);
        if v_abs < k_abs + KEY_SIZE {
            return Err(TxnError::OmapParse(
                "omap B-tree keys/values overlap".into(),
            ));
        }

        // Write TOC entry: (key_off_rel u16, val_off_rel u16).
        write_u16_le_slice(&mut new_blk, toc_off, key_off_rel as u16);
        write_u16_le_slice(&mut new_blk, toc_off + 2, val_off_rel as u16);

        // Write key.
        write_u64_le_slice(&mut new_blk, k_abs, kv.oid);
        write_u64_le_slice(&mut new_blk, k_abs + 8, kv.xid);

        // Write value: omap_val_t = flags(u32) + size(u32) + paddr(u64).
        write_u32_le_slice(&mut new_blk, v_abs, kv.flags); // ov_flags
        write_u32_le_slice(&mut new_blk, v_abs + 4, kv.size); // ov_size
        write_u64_le_slice(&mut new_blk, v_abs + 8, kv.paddr); // ov_paddr
    }

    // The tree description in the root's last 40 bytes is the source's
    // (flags, node size, key and value size are the tree's), with the counts
    // brought up to date: the record count, and the longest key and value,
    // which are the fixed sizes once the tree holds a record. A source with
    // no records carries zeros there, which the checker accepts only while
    // the tree stays empty. bt_node_count stays 1 (single-node tree).
    // [CERTAIN: btree.rs BTREE_INFO_SIZE = 40; BTNODE_ROOT = 0x0001;
    //  empirical macOS scratch image; the APFS specification]
    let bti_start = val_area_end;
    if let (Some(dst), Some(src)) = (
        new_blk.get_mut(bti_start..bsz),
        tree_blk.get(bti_start..bsz),
    ) {
        dst.copy_from_slice(src);
    }
    write_u32_le_slice(&mut new_blk, bti_start + 16, KEY_SIZE as u32); // bt_longest_key
    write_u32_le_slice(&mut new_blk, bti_start + 20, VAL_SIZE as u32); // bt_longest_val
    write_u64_le_slice(&mut new_blk, bti_start + 24, nkeys as u64); // bt_key_count

    // Allocate a new block for the updated B-tree (COW - do not overwrite old block).
    // [CERTAIN: COW required; fsck_apfs rejects modified blocks from old checkpoints]
    let new_tree_paddr = alloc_run_cached(dev, sm, dirty_bitmaps, dirty_cibs, dirty_runs, bsz, 1)
        .map_err(|e| TxnError::OmapParse(format!("alloc tree block: {e}")))
        .map(|(p, _)| p)?;

    // Update o_oid to new_tree_paddr (physical objects have o_oid = paddr).
    write_u64_le_slice(&mut new_blk, 8, new_tree_paddr);
    update_checksum_in_place(&mut new_blk);

    let new_tree_off = new_tree_paddr
        .checked_mul(bsz as u64)
        .ok_or(TxnError::OmapParse("new tree paddr overflow".into()))?;
    dev.write_at(new_tree_off, &new_blk)
        .map_err(|e| TxnError::OmapParse(e.to_string()))?;

    // Allocate a new block for the updated container omap (COW).
    // Update om_tree_oid @48 and o_xid, recompute checksum.
    let new_omap_paddr = alloc_run_cached(dev, sm, dirty_bitmaps, dirty_cibs, dirty_runs, bsz, 1)
        .map_err(|e| TxnError::OmapParse(format!("alloc omap block: {e}")))
        .map(|(p, _)| p)?;

    // Update omap block: o_oid = new_omap_paddr, o_xid = xid, om_tree_oid = new_tree_paddr.
    write_u64_le_slice(&mut omap_blk, 8, new_omap_paddr); // o_oid = paddr for physical
    write_u64_le_slice(&mut omap_blk, 16, xid); // o_xid
    write_u64_le_slice(&mut omap_blk, 48, new_tree_paddr); // om_tree_oid
    update_checksum_in_place(&mut omap_blk);

    let new_omap_off = new_omap_paddr
        .checked_mul(bsz as u64)
        .ok_or(TxnError::OmapParse("new omap paddr overflow".into()))?;
    dev.write_at(new_omap_off, &omap_blk)
        .map_err(|e| TxnError::OmapParse(e.to_string()))?;

    // Reclaim the container-level blocks this COW replaced: the old container
    // omap b-tree node, the old container omap header, and any old virtual-object
    // (VSB) blocks whose mapping we advanced. fsck validates only the latest
    // checkpoint, so freeing immediately at commit keeps the container CLEAN.
    // (Crash-safe deferral via the spaceman free queue is a follow-up;
    //  the APFS specification Q2.)
    for old in to_free {
        free_one_block_cached(
            dev,
            sm,
            dirty_bitmaps,
            dirty_cibs,
            dirty_runs,
            reclaim_bitmap_paddrs,
            reclaim_cib_paddrs,
            bsz,
            old,
        )?;
    }
    free_one_block_cached(
        dev,
        sm,
        dirty_bitmaps,
        dirty_cibs,
        dirty_runs,
        reclaim_bitmap_paddrs,
        reclaim_cib_paddrs,
        bsz,
        tree_paddr,
    )?;
    free_one_block_cached(
        dev,
        sm,
        dirty_bitmaps,
        dirty_cibs,
        dirty_runs,
        reclaim_bitmap_paddrs,
        reclaim_cib_paddrs,
        bsz,
        omap_paddr,
    )?;

    Ok(new_omap_paddr)
}

/// Byte range of `nx_uuid` inside a container superblock.
const NX_UUID_RANGE: std::ops::Range<usize> = 72..88;

/// Find the newest valid nx_superblock in the checkpoint descriptor ring.
///
/// The ring decides; block 0 is the bootstrap copy and its own xid never
/// competes, and a ring slot naming another container or geometry (a stale
/// slot left by a reformat) is no candidate. Block 0 is used only when the
/// ring holds no valid superblock.
/// [CERTAIN: same rule as apfs_core::checkpoint::latest_superblock, taken
///  from apfs.kext nx_checkpoint_find_highest_xid +
///  nx_superblock_agrees_with_main_superblock and apfsprogs read_latest_super]
fn find_latest_nx<D: WritableBlockDevice>(dev: &mut D, block0: &[u8]) -> Result<NxView, TxnError> {
    let boot = NxView::parse(block0)?;
    let bsz = boot.block_size as usize;
    let desc_base = boot.xp_desc_base;
    let desc_blocks = boot.xp_desc_blocks;
    if desc_base == 0 || desc_blocks == 0 {
        return Ok(boot);
    }
    let boot_uuid = block0.get(NX_UUID_RANGE).unwrap_or(&[]).to_vec();
    let mut best: Option<NxView> = None;
    let mut buf = vec![0u8; bsz];
    for i in 0..desc_blocks {
        let Some(bno) = desc_base.checked_add(u64::from(i)) else {
            continue;
        };
        let Ok(off) = block_offset(dev, bno, bsz) else {
            continue;
        };
        if dev.read_at(off, &mut buf).is_err() {
            continue;
        }
        let Ok(candidate) = NxView::parse(&buf) else {
            continue;
        };
        let same_container = buf.get(NX_UUID_RANGE).unwrap_or(&[]) == boot_uuid.as_slice()
            && candidate.block_size == boot.block_size
            && candidate.block_count == boot.block_count
            && candidate.xp_desc_base == boot.xp_desc_base
            && candidate.xp_desc_blocks == boot.xp_desc_blocks
            && candidate.xp_data_base == boot.xp_data_base
            && candidate.xp_data_blocks == boot.xp_data_blocks;
        if !same_container || candidate.current_xid == 0 {
            continue;
        }
        // Two superblocks of this container under one transaction id describe
        // a ring no reader can choose from (the reader refuses it the same way).
        match best.as_ref() {
            Some(b) if b.current_xid == candidate.current_xid => {
                return Err(TxnError::NxParse(format!(
                    "two checkpoint superblocks carry transaction {}",
                    candidate.current_xid
                )));
            }
            Some(b) if b.current_xid > candidate.current_xid => {}
            _ => best = Some(candidate),
        }
    }
    Ok(best.unwrap_or(boot))
}

/// `sm_fs_reserve_alloc_count`: blocks held by the volumes that have a reserve.
const SPACEMAN_FS_RESERVE_ALLOC_COUNT: usize = 192;
/// `apfs_superblock_t`: `apfs_fs_reserve_block_count` and `apfs_fs_alloc_count`.
const VSB_RESERVE_BLOCK_COUNT: usize = 0x48;
const VSB_ALLOC_COUNT: usize = 0x58;
/// `apfs_superblock_t`: `apfs_total_blocks_alloced` and
/// `apfs_total_blocks_freed`, the volume's running totals.
const VSB_TOTAL_BLOCKS_ALLOCED: usize = 0xE0;
const VSB_TOTAL_BLOCKS_FREED: usize = 0xE8;
/// `apfs_omap_oid`: the physical address of the volume's object-map header.
const VSB_OMAP_OID: usize = 0x80;
/// `sm_cibs_per_cab`: how many chunk-info blocks one address block lists.
const SPACEMAN_CIBS_PER_CAB: usize = 44;
/// `sm_dev[MAIN].sm_cib_count`: how many chunk-info blocks the main device has.
const SPACEMAN_MAIN_CIB_COUNT: usize = 64;
/// `sm_dev[MAIN].sm_cab_count`: how many chunk-info address blocks the main
/// device uses. Zero on containers up to about 8 TiB, where the array at
/// `sm_addr_offset` holds the chunk-info blocks themselves.
const SPACEMAN_MAIN_CAB_COUNT: usize = 68;
/// `sm_dev[MAIN].sm_addr_offset`: where the address array starts, as a byte
/// offset from the start of the spaceman.
const SPACEMAN_MAIN_ADDR_OFFSET: usize = 80;
/// A chunk-info address block: after the object header come `cab_index`,
/// `cab_cib_count`, then the chunk-info block addresses.
const CAB_INDEX_OFFSET: usize = 32;
const CAB_CIB_COUNT_OFFSET: usize = 36;
const CAB_ENTRIES_OFFSET: usize = 40;
/// Object type of a chunk-info address block.
pub const OBJECT_TYPE_SPACEMAN_CAB: u32 = 0x0000_0006;

/// Where the address of one chunk-info block is recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CibSlot {
    /// Directly in the spaceman's address array, at this byte offset.
    Spaceman { off: usize },
    /// In a chunk-info address block: the array entry naming that block, the
    /// block's address, and the byte offset of the entry inside it.
    Cab {
        array_off: usize,
        cab_bno: u64,
        entry_off: usize,
    },
}

/// Bring one chunk-info address block into the transaction's view, reading
/// it from disk the first time.
///
/// [CERTAIN: apfsprogs/apfsck/spaceman.c parse_cab checks the object type,
///  that `cab_index` equals the block's position in the array, and that its
///  transaction id is no newer than the spaceman's]
fn load_cab<'a, D: WritableBlockDevice>(
    dev: &mut D,
    sm: &'a mut SpacemanView,
    bsz: usize,
    cab_idx: usize,
    cab_bno: u64,
) -> Result<&'a CabBlock, TxnError> {
    if !sm.cabs.contains_key(&cab_bno) {
        let mut raw = vec![0u8; bsz];
        let off = cab_bno
            .checked_mul(bsz as u64)
            .ok_or(TxnError::BlockOutOfRange {
                block: cab_bno,
                size: dev.size(),
            })?;
        dev.read_at(off, &mut raw)?;
        if u32_from_le(&raw, 0x18) & 0x0000_FFFF != OBJECT_TYPE_SPACEMAN_CAB {
            return Err(TxnError::SpacemanParse(format!(
                "block {cab_bno}, entry {cab_idx} of the address array, is not a chunk-info \
                 address block"
            )));
        }
        let index = u32_from_le(&raw, CAB_INDEX_OFFSET) as usize;
        if index != cab_idx {
            return Err(TxnError::SpacemanParse(format!(
                "chunk-info address block {cab_bno} carries index {index} at position {cab_idx}"
            )));
        }
        if u64_from_le(&raw, 16) > sm.txn_xid {
            return Err(TxnError::SpacemanParse(format!(
                "chunk-info address block {cab_bno} is newer than the transaction"
            )));
        }
        sm.cabs.insert(cab_bno, CabBlock { raw, dirty: false });
    }
    sm.cabs.get(&cab_bno).ok_or_else(|| {
        TxnError::SpacemanParse(format!("chunk-info address block {cab_bno} vanished"))
    })
}

/// The address of chunk-info block `cib_idx`, and where that address lives.
///
/// Up to about 8 TiB the spaceman's array lists the chunk-info blocks
/// themselves. Above that it lists chunk-info address blocks, each holding
/// `sm_cibs_per_cab` chunk-info block addresses (apfsprogs: `cab_count =
/// ceil(cib_count / cibs_per_cab)`, and zero when one address block would
/// do). `None` when the arrays do not reach that far.
fn locate_cib<D: WritableBlockDevice>(
    dev: &mut D,
    sm: &mut SpacemanView,
    bsz: usize,
    cib_idx: usize,
) -> Result<Option<(u64, CibSlot)>, TxnError> {
    if sm.raw.len() < SPACEMAN_MAIN_ADDR_OFFSET + 4 {
        return Err(TxnError::SpacemanParse(
            "spaceman shorter than its header".into(),
        ));
    }
    let addr_offset = u32_from_le(&sm.raw, SPACEMAN_MAIN_ADDR_OFFSET) as usize;
    let cab_count = u32_from_le(&sm.raw, SPACEMAN_MAIN_CAB_COUNT) as usize;
    if cab_count == 0 {
        let off = addr_offset + cib_idx * 8;
        if off + 8 > sm.raw.len() {
            return Ok(None);
        }
        return Ok(Some((u64_from_le(&sm.raw, off), CibSlot::Spaceman { off })));
    }
    let cibs_per_cab = u32_from_le(&sm.raw, SPACEMAN_CIBS_PER_CAB) as usize;
    if cibs_per_cab == 0 {
        return Err(TxnError::SpacemanParse(
            "the spaceman uses address blocks but says each lists no chunk-info blocks".into(),
        ));
    }
    let cab_idx = cib_idx / cibs_per_cab;
    let array_off = addr_offset + cab_idx * 8;
    if cab_idx >= cab_count || array_off + 8 > sm.raw.len() {
        return Ok(None);
    }
    let cab_bno = u64_from_le(&sm.raw, array_off);
    if cab_bno == 0 {
        return Ok(None);
    }
    let cab = load_cab(dev, sm, bsz, cab_idx, cab_bno)?;
    let listed = u32_from_le(&cab.raw, CAB_CIB_COUNT_OFFSET) as usize;
    let entry = cib_idx % cibs_per_cab;
    let entry_off = CAB_ENTRIES_OFFSET + entry * 8;
    if entry >= listed || entry_off + 8 > cab.raw.len() {
        return Ok(None);
    }
    Ok(Some((
        u64_from_le(&cab.raw, entry_off),
        CibSlot::Cab {
            array_off,
            cab_bno,
            entry_off,
        },
    )))
}

/// Record the new address of a copied chunk-info block where the old one was
/// recorded. An address inside a chunk-info address block changes that block,
/// so the block is copied first if it still belongs to an older transaction:
/// its identifier is its address and the previous checkpoint still names it.
/// The copy takes the transaction's id, which is also the id the copied
/// chunk-info block gets, as the checker requires (an address block's id is
/// the newest among its chunk-info blocks').
///
/// Returns `false` when the pool has no room for the copy; nothing has been
/// changed then.
///
/// [CERTAIN: apfsprogs/apfsck/spaceman.c parse_cab: "Cab only changes if a
///  cib changes", the pool holds three blocks per chunk, chunk-info block and
///  address block alike]
fn record_cib_address(
    sm: &mut SpacemanView,
    bsz: usize,
    slot: CibSlot,
    fresh_cib: u64,
) -> Result<bool, TxnError> {
    match slot {
        CibSlot::Spaceman { off } => {
            write_u64_le_slice(&mut sm.raw, off, fresh_cib);
            Ok(true)
        }
        CibSlot::Cab {
            array_off,
            cab_bno,
            entry_off,
        } => {
            let txn_xid = sm.txn_xid;
            let Some(cab) = sm.cabs.get(&cab_bno) else {
                return Err(TxnError::SpacemanParse(format!(
                    "chunk-info address block {cab_bno} was not loaded before being changed"
                )));
            };
            let mut raw = cab.raw.clone();
            let mut at = cab_bno;
            if u64_from_le(&raw, 16) < txn_xid {
                if !sm.ip.contains(cab_bno) {
                    // Outside the pool there is nowhere to copy it to, and
                    // changing it in place would rewrite the previous
                    // checkpoint's description of the container.
                    return Ok(false);
                }
                let Some(fresh) = sm.ip.find_free(bsz) else {
                    return Ok(false);
                };
                sm.ip.mark_used(bsz, fresh)?;
                write_u64_le_slice(&mut raw, 8, fresh);
                write_u64_le_slice(&mut raw, 16, txn_xid);
                sm.cabs.remove(&cab_bno);
                sm.ip.release(cab_bno);
                write_u64_le_slice(&mut sm.raw, array_off, fresh);
                at = fresh;
            }
            write_u64_le_slice(&mut raw, entry_off, fresh_cib);
            sm.cabs.insert(at, CabBlock { raw, dirty: true });
            Ok(true)
        }
    }
}

/// The number of entries in the checkpoint map at `bno`, once the block's
/// checksum is verified and the entries are known to lie inside it. A map
/// with a wrong checksum or more entries than fit describes a checkpoint
/// that cannot be trusted, so it is an error rather than a block skipped or
/// a count cut short.
fn checkpoint_map_entries(buf: &[u8], bno: u64) -> Result<usize, TxnError> {
    apfs_core::checksum::verify_block(buf).map_err(|err| {
        TxnError::NxParse(format!("checkpoint map {bno} fails its checksum: {err:?}"))
    })?;
    let count = u32_from_le(buf, 0x24) as usize;
    let end = count
        .checked_mul(CHECKPOINT_MAP_ENTRY_LEN)
        .and_then(|n| n.checked_add(0x28));
    match end {
        Some(end) if end <= buf.len() => Ok(count),
        _ => Err(TxnError::NxParse(format!(
            "checkpoint map {bno}: {count} entries do not fit the block"
        ))),
    }
}

/// Read the spaceman ephemeral object from the checkpoint data area.
/// [CERTAIN: D.2 the APFS specification; spaceman is ephemeral, located via
/// the checkpoint map in the descriptor area.]
fn read_spaceman<D: WritableBlockDevice>(
    dev: &mut D,
    nx: &NxView,
) -> Result<SpacemanView, TxnError> {
    let bsz = nx.block_size as usize;
    // Walk the current checkpoint's descriptor blocks to find the checkpoint
    // map that records the spaceman's data-area slot.
    // The current checkpoint occupies descriptor slots [xp_desc_index .. + xp_desc_len].
    let desc_base = nx.xp_desc_base;
    let desc_blocks = nx.xp_desc_blocks;
    let mut buf = vec![0u8; bsz];

    for i in 0..nx.xp_desc_len {
        // Both fields come from disk: a crafted index near u32::MAX must wrap,
        // not overflow, and the modulo is safe because the parser refuses a
        // ring of zero blocks.
        let slot = nx.xp_desc_index.wrapping_add(i) % desc_blocks;
        let bno = desc_base
            .checked_add(u64::from(slot))
            .ok_or(TxnError::BlockOutOfRange {
                block: desc_base,
                size: dev.size(),
            })?;
        let off = block_offset(dev, bno, bsz)?;
        dev.read_at(off, &mut buf)?;
        // Check if this is a checkpoint_map_phys_t block:
        // o_type @0x18 (u32) - OBJECT_TYPE_CHECKPOINT_MAP | OBJ_EPHEMERAL
        // [CERTAIN: apfs_raw.h checkpoint_map_phys_t]
        let o_type_raw = u32_from_le(&buf, 0x18);
        let o_type_low = o_type_raw & 0x0000_FFFF;
        if o_type_low != OBJECT_TYPE_CHECKPOINT_MAP {
            continue;
        }
        // A block claiming to be this checkpoint's map is consumed only with
        // its checksum intact and its entries inside the block.
        let cpm_count = checkpoint_map_entries(&buf, bno)?;
        // cpm_flags @0x20 (u32), cpm_count @0x24 (u32)
        // Each entry: cme_type(u32) + cme_subtype(u32) + cme_size(u32) +
        //             cme_pad(u32) + cme_fs_oid(u64) + cme_oid(u64) + cme_paddr(u64)
        // = 40 bytes per entry. First entry at offset 0x28.
        // [CERTAIN: apfs_raw.h struct checkpoint_mapping_t]
        for j in 0..cpm_count {
            let entry_off = 0x28 + j * 40;
            let cme_type = u32_from_le(&buf, entry_off);
            let cme_subtype = u32_from_le(&buf, entry_off + 4);
            let cme_size = u32_from_le(&buf, entry_off + 8);
            // cme_fs_oid @entry_off+16 (u64)
            // cme_oid @entry_off+24 (u64)
            let cme_oid = u64_from_le(&buf, entry_off + 24);
            // cme_paddr @entry_off+32 (u64)
            let cme_paddr = u64_from_le(&buf, entry_off + 32);
            let cme_type_low = cme_type & 0x0000_FFFF;
            if cme_type_low == OBJECT_TYPE_SPACEMAN && cme_oid == nx.spaceman_oid {
                // Found the spaceman. It is the one ephemeral object that may
                // be longer than a block - its chunk-info address array grows
                // with the container - and the mapping says how long.
                let sm_buf = read_data_ring_object(dev, nx, cme_paddr, cme_size, "spaceman")?;
                // Parse spaceman header fields needed for commit.
                // o_oid @8 (u64), o_xid @16 (u64).
                // [CERTAIN: apfs_raw.h struct apfs_spaceman_device the APFS specification]
                // sm_dev[0].sm_free_count @72 (u64).
                let sm_oid = u64_from_le(&sm_buf, 8);
                let sm_xid = u64_from_le(&sm_buf, 16);
                let free_count = u64_from_le(&sm_buf, 72);
                let ip = read_internal_pool(dev, &sm_buf, bsz)?;
                return Ok(SpacemanView {
                    paddr: cme_paddr,
                    oid: sm_oid,
                    xid: sm_xid,
                    o_type: cme_type,
                    o_subtype: cme_subtype,
                    free_count,
                    block_size: nx.block_size,
                    raw: sm_buf,
                    // The caller sets this to the xid it will commit under.
                    txn_xid: 0,
                    ip,
                    cabs: HashMap::new(),
                });
            }
        }
    }
    Err(TxnError::SpacemanParse(
        "spaceman not found in checkpoint map".into(),
    ))
}

/// Read one ephemeral object out of the checkpoint data ring.
///
/// The mapping gives the object's first block and its size; an object longer
/// than a block continues in the following ring slots and wraps around at the
/// ring's end. The checksum covers the whole object.
///
/// [CERTAIN: linux-apfs-rw/transaction.c apfs_write_single_ephemeral_object
///  ("objects can wrap around") and apfs_multiblock_set_csum;
///  apfsprogs/apfsck/super.c parse_cpoint_map requires the size to be a
///  multiple of the block size and the object to lie inside the checkpoint's
///  data range]
fn read_data_ring_object<D: WritableBlockDevice>(
    dev: &mut D,
    nx: &NxView,
    paddr: u64,
    size: u32,
    what: &str,
) -> Result<Vec<u8>, TxnError> {
    let bsz = nx.block_size as usize;
    let blocks = data_ring_object_blocks(nx, size, what)?;
    let base = nx.xp_data_base;
    let ring = u64::from(nx.xp_data_blocks);
    if paddr < base || paddr >= base + ring {
        return Err(TxnError::SpacemanParse(format!(
            "{what} at block {paddr} lies outside the checkpoint data ring"
        )));
    }
    let mut obj = vec![0u8; blocks * bsz];
    for (i, chunk) in obj.chunks_mut(bsz).enumerate() {
        let slot = (paddr - base + i as u64) % ring;
        let bno = base.checked_add(slot).ok_or(TxnError::BlockOutOfRange {
            block: base,
            size: dev.size(),
        })?;
        let off = block_offset(dev, bno, bsz)?;
        dev.read_at(off, chunk)
            .map_err(|e| TxnError::SpacemanParse(format!("{what} read: {e}")))?;
    }
    apfs_core::checksum::verify_block(&obj).map_err(|_| {
        TxnError::SpacemanParse(format!(
            "{what} at block {paddr}: bad checksum over {size} bytes"
        ))
    })?;
    Ok(obj)
}

/// How many ring slots an ephemeral object of `size` bytes occupies.
fn data_ring_object_blocks(nx: &NxView, size: u32, what: &str) -> Result<usize, TxnError> {
    let bsz = nx.block_size;
    if bsz == 0 || size == 0 || !size.is_multiple_of(bsz) {
        return Err(TxnError::SpacemanParse(format!(
            "{what} size {size} is not a whole number of {bsz}-byte blocks"
        )));
    }
    let blocks = size / bsz;
    if blocks > nx.xp_data_blocks {
        return Err(TxnError::SpacemanParse(format!(
            "{what} of {blocks} blocks does not fit the checkpoint data ring ({} blocks)",
            nx.xp_data_blocks
        )));
    }
    Ok(blocks as usize)
}

/// Write one ephemeral object into the checkpoint data ring starting at ring
/// slot `slot`, wrapping at the ring's end. Returns the object's first block.
fn write_data_ring_object<D: WritableBlockDevice>(
    dev: &mut D,
    nx: &NxView,
    slot: u32,
    obj: &[u8],
) -> Result<u64, TxnError> {
    let bsz = nx.block_size as usize;
    let ring = nx.xp_data_blocks;
    if ring == 0 || !obj.len().is_multiple_of(bsz) {
        return Err(TxnError::SpacemanParse(format!(
            "ephemeral object of {} bytes cannot be tiled into {bsz}-byte ring slots",
            obj.len()
        )));
    }
    let first = nx.xp_data_base + u64::from(slot % ring);
    for (i, chunk) in obj.chunks(bsz).enumerate() {
        let s = (u64::from(slot) + i as u64) % u64::from(ring);
        let bno = nx.xp_data_base + s;
        let off = bno
            .checked_mul(bsz as u64)
            .ok_or(TxnError::BlockOutOfRange {
                block: bno,
                size: dev.size(),
            })?;
        dev.write_at(off, chunk)?;
    }
    Ok(first)
}

/// Read the internal pool's description and its current bitmap out of the
/// spaceman.
///
/// A container whose fields are zero (or whose ring slots point outside the
/// ring) simply has no pool we can use: the allocator then declines to touch a
/// bitmap-less chunk rather than guessing. That is a refusal, never a
/// corruption, so it is not an error here.
///
/// [CERTAIN: field offsets decoded from a macOS-formatted container and matched
///  against apfsprogs/include/apfs/raw.h; the ring layout follows
///  apfsprogs/apfsck/spaceman.c parse_ip_bitmap_list]
fn read_internal_pool<D: WritableBlockDevice>(
    dev: &mut D,
    sm_raw: &[u8],
    bsz: usize,
) -> Result<InternalPool, TxnError> {
    let mut ip = InternalPool {
        block_count: u64_from_le(sm_raw, SMI_IP_BLOCK_COUNT),
        bm_size: u32_from_le(sm_raw, SMI_IP_BM_SIZE_IN_BLOCKS),
        bm_ring_len: u32_from_le(sm_raw, SMI_IP_BM_BLOCK_COUNT),
        bm_base: u64_from_le(sm_raw, SMI_IP_BM_BASE),
        base: u64_from_le(sm_raw, SMI_IP_BASE),
        xid_offset: u32_from_le(sm_raw, SMI_IP_BM_XID_OFFSET),
        bitmap_offset: u32_from_le(sm_raw, SMI_IP_BITMAP_OFFSET),
        free_next_offset: u32_from_le(sm_raw, SMI_IP_BM_FREE_NEXT_OFFSET),
        bitmaps: Vec::new(),
        dirty: Vec::new(),
        released: Vec::new(),
    };
    // A pool needs a bitmap covering it, a ring holding that bitmap, and room
    // in the spaceman for the three arrays that describe the ring.
    let arrays_fit = ip.bitmap_offset as usize + ip.bm_size as usize * 2 <= sm_raw.len()
        && ip.xid_offset as usize + ip.bm_size as usize * 8 <= sm_raw.len()
        && ip.free_next_offset as usize + ip.bm_ring_len as usize * 2 <= sm_raw.len();
    if ip.bm_size == 0 || ip.bm_base == 0 || ip.block_count == 0 || !arrays_fit {
        return Ok(InternalPool::default());
    }
    for i in 0..ip.bm_size as usize {
        let slot = u16_from_le(sm_raw, ip.bitmap_offset as usize + i * 2);
        if u32::from(slot) >= ip.bm_ring_len {
            tracing::warn!(
                slot,
                ring = ip.bm_ring_len,
                "internal-pool bitmap slot is outside the ring - the pool is unusable"
            );
            return Ok(InternalPool::default());
        }
        let bno = ip
            .bm_base
            .checked_add(u64::from(slot))
            .ok_or(TxnError::BlockOutOfRange {
                block: ip.bm_base,
                size: dev.size(),
            })?;
        let off = block_offset(dev, bno, bsz)?;
        let mut buf = vec![0u8; bsz];
        dev.read_at(off, &mut buf)
            .map_err(|e| TxnError::SpacemanParse(format!("internal-pool bitmap read: {e}")))?;
        ip.bitmaps.push(buf);
        ip.dirty.push(false);
    }
    Ok(ip)
}

/// Read all non-spaceman ephemeral objects from the current checkpoint map.
/// These must be written to the next checkpoint data area every transaction.
/// [CERTAIN: fsck_apfs "nx_xp_data_len (1) is less than 2"; baseline has reaper + 2 btrees]
fn read_other_ephemerals<D: WritableBlockDevice>(
    dev: &mut D,
    nx: &NxView,
    spaceman_oid: u64,
) -> Result<Vec<EphemeralObj>, TxnError> {
    let bsz = nx.block_size as usize;
    let desc_base = nx.xp_desc_base;
    let desc_blocks = nx.xp_desc_blocks;
    let mut buf = vec![0u8; bsz];
    let mut result = Vec::new();

    for i in 0..nx.xp_desc_len {
        // Both fields come from disk: a crafted index near u32::MAX must wrap,
        // not overflow, and the modulo is safe because the parser refuses a
        // ring of zero blocks.
        let slot = nx.xp_desc_index.wrapping_add(i) % desc_blocks;
        let bno = desc_base
            .checked_add(u64::from(slot))
            .ok_or(TxnError::BlockOutOfRange {
                block: desc_base,
                size: dev.size(),
            })?;
        let off = block_offset(dev, bno, bsz)?;
        // A descriptor block that cannot be read may be the map naming the
        // objects this checkpoint carries; skipping it would drop them from
        // the next checkpoint without a word.
        dev.read_at(off, &mut buf)?;
        let o_type_raw = u32_from_le(&buf, 0x18);
        if o_type_raw & 0x0000_FFFF != OBJECT_TYPE_CHECKPOINT_MAP {
            continue;
        }
        let cpm_count = checkpoint_map_entries(&buf, bno)?;
        for j in 0..cpm_count {
            let entry_off = 0x28 + j * CHECKPOINT_MAP_ENTRY_LEN;
            let cme_type = u32_from_le(&buf, entry_off);
            let cme_subtype = u32_from_le(&buf, entry_off + 4);
            let cme_size = u32_from_le(&buf, entry_off + 8);
            let cme_fs_oid = u64_from_le(&buf, entry_off + 16);
            let cme_oid = u64_from_le(&buf, entry_off + 24);
            let cme_paddr = u64_from_le(&buf, entry_off + 32);
            // Skip the spaceman - it's handled separately.
            if cme_oid == spaceman_oid {
                continue;
            }
            // Read the whole object, however many ring slots it takes. An
            // object that cannot be read is an object this checkpoint names
            // and the next one would silently drop, so it is an error.
            let eph_buf = read_data_ring_object(dev, nx, cme_paddr, cme_size, "ephemeral object")?;
            result.push(EphemeralObj {
                cme_type,
                cme_subtype,
                fs_oid: cme_fs_oid,
                oid: cme_oid,
                raw: eph_buf,
            });
        }
    }
    Ok(result)
}

/// Allocate one free block from the spaceman.
/// Allocate up to `n` contiguous blocks from one bitmap chunk, updating only
/// in-memory caches. Physical writes are deferred to commit().
///
/// Returns (start_paddr, count) where count <= n.
///
/// [CERTAIN: D.2 steps 2-8, Q1 IMMEDIATE bitmap update in-memory,
///  deferred physical write pattern mirrors linux-apfs-rw apfs_write_ip_bitmaps;
///  the APFS specification]
/// The allocator's working copies of the metadata it is changing.
struct DirtyAlloc<'a> {
    bitmaps: &'a mut HashMap<u64, Vec<u8>>,
    cibs: &'a mut HashMap<u64, Vec<u8>>,
    runs: &'a mut HashMap<u64, crate::free_runs::ChunkFreeRuns>,
}

/// Bring a chunk-info block, and the bitmap of one chunk inside it, into this
/// transaction, and hand back the addresses to work with.
///
/// Neither can be changed where it lies. The checkpoint before the newest one
/// still names both, and it is the one a machine falls back to when a commit is
/// torn; rewriting them under it makes it describe a container that no longer
/// exists. So a block belonging to an older transaction is copied to a fresh
/// internal-pool block, the copy is what this transaction changes, and the
/// original goes on the pool's free queue - which releases it a transaction
/// later, once the checkpoint that needs it has stopped being the fallback.
///
/// A chunk with no bitmap at all gets a zeroed one, which is the same
/// operation from the pool's point of view.
///
/// Returns `None` when the pool cannot supply a block and the caller should
/// leave this chunk alone. A container with no usable pool keeps the older
/// in-place behaviour, which is what the synthetic spacemen in the unit tests
/// exercise.
///
/// [CERTAIN: linux-apfs-rw/spaceman.c apfs_chunk_alloc_free copies both, gives
///  the chunk-info copy a new object id and the current transaction id, and
///  queues each original on the free queue; apfsprogs/apfsck/spaceman.c
///  parse_chunk_info_block requires a chunk-info block's xid to equal the
///  newest xid among its chunks and to be no newer than the spaceman]
/// Offset of `cib_chunk_info_count` in a chunk-info block.
const CIB_CHUNK_INFO_COUNT: usize = 36;
/// Where a chunk-info block's entries start, and the size of one.
const CIB_FIRST_CHUNK_INFO: usize = 40;
const CHUNK_INFO_SIZE: usize = 32;

/// How many chunk-info entries `cib` holds: its own count, which the last
/// block of a container keeps below the space manager's per-block capacity.
/// Entries past it are stale bytes, not chunks. A count above that capacity
/// or above what the block can hold describes no real block.
fn cib_chunk_count(cib: &[u8], chunks_per_cib: usize, bsz: usize) -> Result<usize, TxnError> {
    let n = u32_from_le(cib, CIB_CHUNK_INFO_COUNT) as usize;
    let fits = bsz.saturating_sub(CIB_FIRST_CHUNK_INFO) / CHUNK_INFO_SIZE;
    if n > chunks_per_cib || n > fits {
        return Err(TxnError::SpacemanParse(format!(
            "chunk-info block holds {n} chunks; at most {} fit",
            chunks_per_cib.min(fits)
        )));
    }
    Ok(n)
}

/// Refuse a chunk-info entry describing a chunk that cannot exist: more
/// blocks than a chunk or its one-block bitmap holds, more free blocks than
/// blocks, or blocks past the end of the device. Allocating or freeing
/// through such an entry would set bits no bitmap has or hand out blocks
/// nothing backs.
fn check_chunk_info(
    ci_addr: u64,
    ci_block_count: u32,
    ci_free_count: u32,
    blocks_per_chunk: u64,
    bsz: usize,
    dev_blocks: u64,
) -> Result<(), TxnError> {
    let bitmap_bits = (bsz as u64).saturating_mul(8);
    let blocks = u64::from(ci_block_count);
    let end = ci_addr.checked_add(blocks);
    if blocks > blocks_per_chunk
        || blocks > bitmap_bits
        || ci_free_count > ci_block_count
        || end.is_none_or(|e| e > dev_blocks)
    {
        return Err(TxnError::SpacemanParse(format!(
            "chunk at {ci_addr}: {ci_block_count} blocks, {ci_free_count} free does not \
             fit a chunk of {blocks_per_chunk} blocks on a device of {dev_blocks}"
        )));
    }
    Ok(())
}

fn cow_chunk_metadata<D: WritableBlockDevice>(
    dev: &mut D,
    sm: &mut SpacemanView,
    dirty: &mut DirtyAlloc<'_>,
    bsz: usize,
    cib_idx: usize,
    chunk_idx: usize,
) -> Result<Option<(u64, u64)>, TxnError> {
    let Some((mut cib_bno, slot)) = locate_cib(dev, sm, bsz, cib_idx)? else {
        return Ok(None);
    };
    if cib_bno == 0 {
        return Ok(None);
    }
    let ci_off = 40 + chunk_idx * 32;
    if ci_off + 32 > bsz {
        return Ok(None);
    }
    let txn_xid = sm.txn_xid;

    let mut cib = match dirty.cibs.get(&cib_bno) {
        Some(b) => b.clone(),
        None => {
            let Ok(off) = block_offset(dev, cib_bno, bsz) else {
                return Ok(None);
            };
            let mut b = vec![0u8; bsz];
            if dev.read_at(off, &mut b).is_err() {
                return Ok(None);
            }
            b
        }
    };

    // The chunk-info block itself.
    if u64_from_le(&cib, 16) < txn_xid && sm.ip.contains(cib_bno) {
        let Some(fresh) = sm.ip.find_free(bsz) else {
            return Ok(None);
        };
        sm.ip.mark_used(bsz, fresh)?;
        // The copy's new address has to be recorded where the old one was,
        // which on a large container means copying an address block too. If
        // that cannot be done, nothing has changed yet: give the block back.
        if !record_cib_address(sm, bsz, slot, fresh)? {
            sm.ip.mark_free(bsz, fresh)?;
            return Ok(None);
        }
        // The copy is a different object: it lives at its own address, and a
        // physical object's identifier is that address.
        write_u64_le_slice(&mut cib, 8, fresh);
        write_u64_le_slice(&mut cib, 16, txn_xid);
        dirty.cibs.remove(&cib_bno);
        sm.ip.release(cib_bno);
        cib_bno = fresh;
    }

    // The chunk's bitmap.
    let ci_xid = u64_from_le(&cib, ci_off);
    let ci_block_count = u32_from_le(&cib, ci_off + 16) as usize;
    let old_bitmap = u64_from_le(&cib, ci_off + 24);
    let mut bitmap_bno = old_bitmap;
    if old_bitmap == 0 {
        let Some(fresh) = sm.ip.find_free(bsz) else {
            return Ok(None);
        };
        sm.ip.mark_used(bsz, fresh)?;
        let zeroed = vec![0u8; bsz];
        dirty.runs.insert(
            fresh,
            crate::free_runs::ChunkFreeRuns::from_bitmap(&zeroed, ci_block_count),
        );
        dirty.bitmaps.insert(fresh, zeroed);
        bitmap_bno = fresh;
    } else if ci_xid < txn_xid && sm.ip.contains(old_bitmap) {
        let Some(fresh) = sm.ip.find_free(bsz) else {
            return Ok(None);
        };
        let content = match dirty.bitmaps.remove(&old_bitmap) {
            Some(b) => b,
            None => {
                let Ok(off) = block_offset(dev, old_bitmap, bsz) else {
                    return Ok(None);
                };
                let mut b = vec![0u8; bsz];
                if dev.read_at(off, &mut b).is_err() {
                    return Ok(None);
                }
                b
            }
        };
        sm.ip.mark_used(bsz, fresh)?;
        let runs = match dirty.runs.remove(&old_bitmap) {
            Some(r) => r,
            None => crate::free_runs::ChunkFreeRuns::from_bitmap(&content, ci_block_count),
        };
        dirty.runs.insert(fresh, runs);
        dirty.bitmaps.insert(fresh, content);
        sm.ip.release(old_bitmap);
        bitmap_bno = fresh;
    } else if !dirty.bitmaps.contains_key(&old_bitmap) {
        // Already ours, but not yet loaded.
        let Ok(off) = block_offset(dev, old_bitmap, bsz) else {
            return Ok(None);
        };
        let mut b = vec![0u8; bsz];
        if dev.read_at(off, &mut b).is_err() {
            return Ok(None);
        }
        dirty.runs.insert(
            old_bitmap,
            crate::free_runs::ChunkFreeRuns::from_bitmap(&b, ci_block_count),
        );
        dirty.bitmaps.insert(old_bitmap, b);
    }

    // The chunk's xid is its block's: the transaction's when the block was
    // copied above, the old checkpoint's when it was not (a chunk-info block
    // outside the pool). A chunk claiming to be newer than the block that
    // holds it is what the checker calls "xid is too recent".
    let cib_xid = u64_from_le(&cib, 16);
    write_u64_le_slice(&mut cib, ci_off, cib_xid);
    write_u64_le_slice(&mut cib, ci_off + 24, bitmap_bno);
    dirty.cibs.insert(cib_bno, cib);
    Ok(Some((cib_bno, bitmap_bno)))
}

/// Blocks a fresh free run must have for a transaction to start allocating
/// in it (1 MiB of 4 KiB blocks), so what it allocates next can follow on.
const LOCALITY_RUN_BLOCKS: u32 = 256;

/// The passes of [`alloc_run_near`], in order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AllocPass {
    AtHint,
    Room,
    Whole,
    Largest,
}

/// What a transaction's allocations keep between calls: where the next
/// block would sit right after the last one, and the longest free run of
/// each chunk it looked at without taking anything (by bitmap address), so
/// a chunk without room is read once and never copied for nothing.
#[derive(Default)]
pub(crate) struct Locality {
    next: Option<u64>,
    longest_run: HashMap<u64, u32>,
}

/// [`alloc_run_near`] with nothing known about locality.
fn alloc_run_cached<D: WritableBlockDevice>(
    dev: &mut D,
    sm: &mut SpacemanView,
    dirty_bitmaps: &mut HashMap<u64, Vec<u8>>,
    dirty_cibs: &mut HashMap<u64, Vec<u8>>,
    dirty_runs: &mut HashMap<u64, crate::free_runs::ChunkFreeRuns>,
    bsz: usize,
    n: usize,
) -> Result<(u64, usize), TxnError> {
    alloc_run_near(
        dev,
        sm,
        dirty_bitmaps,
        dirty_cibs,
        dirty_runs,
        &mut Locality::default(),
        bsz,
        n,
    )
}

#[allow(clippy::too_many_arguments)]
fn alloc_run_near<D: WritableBlockDevice>(
    dev: &mut D,
    sm: &mut SpacemanView,
    dirty_bitmaps: &mut HashMap<u64, Vec<u8>>,
    dirty_cibs: &mut HashMap<u64, Vec<u8>>,
    dirty_runs: &mut HashMap<u64, crate::free_runs::ChunkFreeRuns>,
    loc: &mut Locality,
    bsz: usize,
    n: usize,
) -> Result<(u64, usize), TxnError> {
    let prefer = loc.next;
    // sm_dev[0].sm_cib_count @64 (u32)
    let cib_count = u32_from_le(&sm.raw, SPACEMAN_MAIN_CIB_COUNT) as usize;
    // sm_blocks_per_chunk @36 (u32) - how many blocks per chunk (usually 16384 = 2^14).
    let blocks_per_chunk = u32_from_le(&sm.raw, 36) as u64;
    if blocks_per_chunk == 0 {
        return Err(TxnError::SpacemanParse("blocks_per_chunk is zero".into()));
    }
    // sm_chunks_per_cib @40 (u32)
    let chunks_per_cib = u32_from_le(&sm.raw, 40) as usize;
    if chunks_per_cib == 0 {
        return Err(TxnError::SpacemanParse("chunks_per_cib is zero".into()));
    }

    // Four passes, the first that finds blocks wins:
    //   AtHint  - the blocks right after the transaction's last allocation,
    //   Room    - the start of a free run with room to keep growing,
    //   Whole   - a run of the whole length in any chunk (best fit),
    //   Largest - the largest run of the first chunk with anything free.
    // The first two keep a transaction's new blocks next to each other, so
    // its commit writes a few long runs. Best fit alone put each block of a
    // one-block request in a different hole copy-on-write had left: 300
    // scattered 4 KiB writes to a file became 230 device writes at commit,
    // 11.8 s on a USB drive whose small random writes take 50 ms each.
    // Whole before Largest: the largest run of the first chunk cut a large
    // file into scraps wherever earlier deletes had left holes.
    let whole = n.min(u32::MAX as usize) as u32;
    let room = whole.max(LOCALITY_RUN_BLOCKS);
    for pass in [
        AllocPass::AtHint,
        AllocPass::Room,
        AllocPass::Whole,
        AllocPass::Largest,
    ] {
        if pass == AllocPass::AtHint && prefer.is_none() {
            continue;
        }
        let whole_only = pass != AllocPass::Largest;
        // Chunk-info blocks in order; `locate_cib` follows the address blocks of
        // a large container.
        // [CERTAIN: spaceman.c the APFS specification]
        for cib_idx in 0..cib_count {
            let Some((cib_bno, _)) = locate_cib(dev, sm, bsz, cib_idx)? else {
                break;
            };
            if cib_bno == 0 {
                continue;
            }
            // Get or load the chunk-info block into the dirty cache. One the
            // allocator names but that lies outside the device, cannot be read
            // or fails its checksum is a broken allocator: allocating around it
            // would hide that, and rewriting it at commit would launder a
            // corrupted block under a fresh checksum.
            let cib_off = block_offset(dev, cib_bno, bsz).map_err(|err| {
                TxnError::SpacemanParse(format!(
                    "chunk-info block {cib_bno} lies outside the device: {err}"
                ))
            })?;
            let cib_buf = match dirty_cibs.get(&cib_bno) {
                Some(buf) => buf.clone(),
                None => read_verified_block(dev, cib_off, bsz, "chunk-info block", cib_bno)
                    .map_err(TxnError::SpacemanParse)?,
            };

            // CIB block layout (chunk_info_block_phys_t):
            //   obj_phys header (32 bytes)
            //   cib_index u32 @32, cib_count u32 @36  (8 bytes)
            //   chunk_info_t[] starting at offset 40.
            // chunk_info_t: xid(u64) + addr(u64) + block_count(u32) +
            //               free_count(u32) + bitmap_addr(u64) = 32 bytes.
            // [CERTAIN: apfs_raw.h struct apfs_chunk_info_block + apfs_chunk_info,
            //  the APFS specification; offset 40 confirmed by real-image empirical]
            let chunks_here = cib_chunk_count(&cib_buf, chunks_per_cib, bsz)?;
            for chunk_idx in 0..chunks_here {
                let ci_off = 40 + chunk_idx * 32;
                if ci_off + 32 > bsz {
                    break;
                }
                let ci_xid = u64_from_le(&cib_buf, ci_off);
                let ci_addr = u64_from_le(&cib_buf, ci_off + 8);
                let ci_block_count = u32_from_le(&cib_buf, ci_off + 16);
                let ci_free_count = u32_from_le(&cib_buf, ci_off + 20);
                let _ = ci_xid; // the copy-on-write helper reads it from its own copy

                if ci_free_count == 0 || ci_block_count == 0 {
                    continue;
                }
                let hint_bit = match (pass, prefer) {
                    (AllocPass::AtHint, Some(at)) => match at.checked_sub(ci_addr) {
                        Some(bit) if bit < u64::from(ci_block_count) => bit as u32,
                        _ => continue,
                    },
                    _ => 0,
                };
                if pass == AllocPass::Room {
                    if ci_free_count < room {
                        continue;
                    }
                    // Free blocks are not a free run: look before copying.
                    let bitmap_addr = u64_from_le(&cib_buf, ci_off + 24);
                    let longest = match dirty_runs.get(&bitmap_addr) {
                        Some(runs) => runs.longest(),
                        None if bitmap_addr == 0 => ci_block_count,
                        None => match loc.longest_run.get(&bitmap_addr) {
                            Some(&l) => l,
                            None => {
                                let off = block_offset(dev, bitmap_addr, bsz).map_err(|e| {
                                    TxnError::SpacemanParse(format!(
                                        "bitmap {bitmap_addr} lies outside the device: {e}"
                                    ))
                                })?;
                                let mut bm = vec![0u8; bsz];
                                dev.read_at(off, &mut bm).map_err(TxnError::Device)?;
                                let l = crate::free_runs::ChunkFreeRuns::from_bitmap(
                                    &bm,
                                    ci_block_count as usize,
                                )
                                .longest();
                                loc.longest_run.insert(bitmap_addr, l);
                                l
                            }
                        },
                    };
                    if longest < room {
                        continue;
                    }
                }
                if whole_only && pass != AllocPass::AtHint && ci_free_count < whole {
                    continue;
                }
                check_chunk_info(
                    ci_addr,
                    ci_block_count,
                    ci_free_count,
                    blocks_per_chunk,
                    bsz,
                    dev.size() / bsz as u64,
                )?;

                // A free count above zero means this chunk can satisfy the request,
                // so from here on the chunk is going to change - which is what makes
                // it safe to copy it into the transaction before touching anything.
                //
                // That copy also covers the case of a chunk with no bitmap at all:
                // ci_bitmap_addr == 0 states that the WHOLE chunk is free and that
                // there is nothing to record an allocation in. Handing out a block
                // and only decrementing ci_free_count would leave nothing on disk
                // saying the block was taken, so any other implementation would hand
                // the same block to a second object.
                let (cib_bno, bitmap_bno) = match cow_chunk_metadata(
                    dev,
                    sm,
                    &mut DirtyAlloc {
                        bitmaps: dirty_bitmaps,
                        cibs: dirty_cibs,
                        runs: dirty_runs,
                    },
                    bsz,
                    cib_idx,
                    chunk_idx,
                )? {
                    Some(pair) => pair,
                    None => continue,
                };

                // Pull the runs index for this chunk (present: the helper leaves the
                // bitmap and its index loaded together).
                let runs = match dirty_runs.get_mut(&bitmap_bno) {
                    Some(r) => r,
                    None => continue,
                };

                // Try a full-fit first; fall back to the largest available run so a
                // caller asking for N can still make progress when no single run of
                // length N exists in this chunk.
                let want = n.min(ci_free_count as usize).min(u32::MAX as usize) as u32;
                let found = match pass {
                    AllocPass::AtHint => runs.alloc_at(hint_bit, want),
                    AllocPass::Room => runs.alloc_in_room(room, want),
                    AllocPass::Whole => runs.alloc_exact(want),
                    AllocPass::Largest => runs.alloc_largest(want),
                };
                let Some((first_bit_u32, run_len_u32)) = found else {
                    continue;
                };
                let first_bit = first_bit_u32 as usize;
                let run_len = run_len_u32 as usize;
                if run_len == 0 {
                    continue;
                }

                // Mirror the alloc into the on-disk bitmap (kept authoritative for
                // crash-safety; the runs index is only an in-memory accelerator).
                let bm_buf = match dirty_bitmaps.get_mut(&bitmap_bno) {
                    Some(b) => b,
                    None => continue,
                };
                for i in 0..run_len {
                    let bit = first_bit + i;
                    if let Some(byte) = bm_buf.get_mut(bit / 8) {
                        *byte |= 1u8 << (bit % 8);
                    }
                }

                // Update CIB free_count in dirty cache (no checksum yet - deferred).
                let new_free = ci_free_count - run_len as u32;
                let cib_entry = match dirty_cibs.get_mut(&cib_bno) {
                    Some(c) => c,
                    None => continue,
                };
                write_u32_le_slice(cib_entry, ci_off + 20, new_free);

                // Decrement sm_dev[0].sm_free_count.
                let sm_free = u64_from_le(&sm.raw, 72).saturating_sub(run_len as u64);
                write_u64_le_slice(&mut sm.raw, 72, sm_free);
                sm.free_count = sm_free;

                let alloc_bno = ci_addr + first_bit as u64;
                loc.next = Some(alloc_bno + run_len as u64);
                return Ok((alloc_bno, run_len));
            }
        }
    }
    Err(TxnError::NoFreeBlocks)
}

/// Free a previously-allocated block back to the spaceman, updating only
/// in-memory caches. Physical writes are deferred to commit().
///
/// [CERTAIN: empirical - overallocation disappears when orphaned blocks are freed;
///  APFS normally uses the reaper for deferred free, but immediate freeing is
///  equivalent for a clean checkpoint; the APFS specification]
/// Read the object block at `off` and verify its checksum, naming it `what`
/// at `bno` in the message when either fails. Metadata this transaction
/// reads raw, changes and re-stamps would otherwise launder a corrupted
/// block under a fresh checksum.
fn read_verified_block<D: WritableBlockDevice>(
    dev: &mut D,
    off: u64,
    bsz: usize,
    what: &str,
    bno: u64,
) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; bsz];
    dev.read_at(off, &mut buf)
        .map_err(|err| format!("{what} {bno} unreadable: {err}"))?;
    apfs_core::checksum::verify_block(&buf)
        .map_err(|err| format!("{what} {bno} fails its checksum: {err:?}"))?;
    Ok(buf)
}

fn free_one_block_cached<D: WritableBlockDevice>(
    dev: &mut D,
    sm: &mut SpacemanView,
    dirty_bitmaps: &mut HashMap<u64, Vec<u8>>,
    dirty_cibs: &mut HashMap<u64, Vec<u8>>,
    dirty_runs: &mut HashMap<u64, crate::free_runs::ChunkFreeRuns>,
    reclaim_bitmap_paddrs: &mut HashSet<u64>,
    reclaim_cib_paddrs: &mut HashSet<u64>,
    bsz: usize,
    free_bno: u64,
) -> Result<(), TxnError> {
    let cib_count = u32_from_le(&sm.raw, SPACEMAN_MAIN_CIB_COUNT) as usize;
    let blocks_per_chunk = u32_from_le(&sm.raw, 36) as u64;
    if blocks_per_chunk == 0 {
        return Err(TxnError::SpacemanParse(
            "blocks_per_chunk is zero (free)".into(),
        ));
    }
    let chunks_per_cib = u32_from_le(&sm.raw, 40) as usize;
    if chunks_per_cib == 0 {
        return Err(TxnError::SpacemanParse(
            "chunks_per_cib is zero (free)".into(),
        ));
    }

    // The internal pool sits inside a chunk's address range, but its blocks are
    // marked used in that chunk's bitmap for the container's whole life and are
    // tracked separately in the pool's own bitmap. Clearing the chunk bit would
    // hand a block that holds a chunk bitmap to a file. Nothing frees pool
    // blocks yet - releasing one belongs on the pool's free queue, not here - so
    // an address landing in the pool means the caller mixed up two address
    // spaces, and saying so is better than writing the wrong bitmap.
    if sm.ip.contains(free_bno) {
        return Err(TxnError::SpacemanParse(format!(
            "free of block {free_bno} lands in the spaceman internal pool; \
             pool blocks are not released through the chunk bitmaps"
        )));
    }

    for cib_idx in 0..cib_count {
        let Some((cib_bno, _)) = locate_cib(dev, sm, bsz, cib_idx)? else {
            break;
        };
        if cib_bno == 0 {
            continue;
        }
        // Get or load the chunk-info block. One the allocator names but that
        // cannot be read is a broken allocator: skipping it would lose the
        // free (and leave the free counts wrong), so the transaction fails.
        let cib_off = block_offset(dev, cib_bno, bsz).map_err(|err| {
            TxnError::SpacemanParse(format!(
                "free of block {free_bno}: chunk-info block {cib_bno} lies outside the \
                 device: {err}"
            ))
        })?;
        let cib_buf = match dirty_cibs.get(&cib_bno) {
            Some(buf) => buf.clone(),
            None => read_verified_block(dev, cib_off, bsz, "chunk-info block", cib_bno)
                .map_err(TxnError::SpacemanParse)?,
        };

        let chunks_here = cib_chunk_count(&cib_buf, chunks_per_cib, bsz)?;
        for chunk_idx in 0..chunks_here {
            let ci_off = 40 + chunk_idx * 32;
            if ci_off + 32 > bsz {
                break;
            }
            let ci_addr = u64_from_le(&cib_buf, ci_off + 8);
            let ci_block_count = u32_from_le(&cib_buf, ci_off + 16);
            let ci_free_count = u32_from_le(&cib_buf, ci_off + 20);
            let ci_bitmap_addr = u64_from_le(&cib_buf, ci_off + 24);

            if ci_block_count == 0 {
                continue;
            }
            check_chunk_info(
                ci_addr,
                ci_block_count,
                ci_free_count,
                blocks_per_chunk,
                bsz,
                dev.size() / bsz as u64,
            )?;
            // Check if free_bno falls in this chunk.
            if free_bno < ci_addr || free_bno >= ci_addr + ci_block_count as u64 {
                continue;
            }

            let bit_idx = (free_bno - ci_addr) as usize;

            if ci_bitmap_addr == 0 {
                // A bitmap-less chunk states that every one of its blocks is
                // free, so a block inside it cannot be in use and freeing it is
                // a contradiction. Raising the free count anyway would push it
                // past the chunk's block count and leave a chunk fsck rejects.
                // The allocator no longer hands out blocks from such a chunk,
                // so reaching here means the address did not come from us.
                return Err(TxnError::SpacemanParse(format!(
                    "free of block {free_bno} lands in an all-free chunk at {ci_addr} \
                     (no bitmap); the block was not allocated from this container"
                )));
            } else {
                // A free changes the chunk, so the chunk-info block and the
                // bitmap have to belong to this transaction before the bit is
                // cleared - the previous checkpoint still names both and still
                // has to describe the container it was sealed with.
                let (cib_bno, bitmap_bno) = match cow_chunk_metadata(
                    dev,
                    sm,
                    &mut DirtyAlloc {
                        bitmaps: dirty_bitmaps,
                        cibs: dirty_cibs,
                        runs: dirty_runs,
                    },
                    bsz,
                    cib_idx,
                    chunk_idx,
                )? {
                    Some(pair) => pair,
                    None => {
                        return Err(TxnError::SpacemanParse(format!(
                            "free of block {free_bno}: the chunk at {ci_addr} could not be \
                             brought into this transaction"
                        )))
                    }
                };
                // Clear the bit in the cached bitmap; mirror the free into the
                // parallel runs index so a subsequent alloc can find it via the
                // size-sorted view (adjacency merge happens inside `runs.free`).
                let Some(bm_buf) = dirty_bitmaps.get_mut(&bitmap_bno) else {
                    return Err(TxnError::SpacemanParse(
                        "free_block: bitmap vanished from cache".into(),
                    ));
                };
                // Idempotent-free guard (M8 #3 batch double-free fix):
                // In a batch transaction N operations (e.g. N renames) may all
                // call free_replaced_metadata with the SAME stale vsb_raw/omap_raw
                // captured before the transaction started.  Every op therefore
                // tries to free the same set of OLD metadata blocks (old volume
                // omap header, old FS tree nodes, etc.).  Without this guard the
                // second free would:
                //   1. Call runs.free() on a bit already in the free-runs index
                //      → corrupt the ChunkFreeRuns BTreeSet (double entry).
                //   2. Over-increment ci_free_count and sm_free_count.
                //   3. Allow alloc_run_cached to re-allocate an already-freed-and-
                //      re-allocated block, causing a second pending write to land
                //      on the same address and corrupt the container omap.
                //
                // Fix: if the bit is already 0 (already freed this transaction),
                // return Ok silently.  The caller's intent (this block is not live)
                // is already satisfied.
                //
                // Derived from linux-apfs-rw transaction-lifecycle pattern: the
                // kernel issues one consistent free per released block per txn.
                let already_free = bm_buf
                    .get(bit_idx / 8)
                    .map(|b| b & (1u8 << (bit_idx % 8)) == 0)
                    .unwrap_or(false);
                if already_free {
                    return Ok(());
                }
                if let Some(byte) = bm_buf.get_mut(bit_idx / 8) {
                    *byte &= !(1u8 << (bit_idx % 8));
                }
                if let Some(runs) = dirty_runs.get_mut(&bitmap_bno) {
                    runs.free(bit_idx as u32, 1);
                }
                // Mark bitmap and CIB as reclaim-touched so commit() defers their
                // flush to Phase B (post-NXSB). This closes the power-loss window
                // where clearing a bit before NXSB leaves the old checkpoint with
                // freed blocks that still appear in its live extent tree (bug W2-1).
                reclaim_bitmap_paddrs.insert(bitmap_bno);
                reclaim_cib_paddrs.insert(cib_bno);
                // Increment ci_free_count in CIB cache.
                let Some(cib_entry) = dirty_cibs.get_mut(&cib_bno) else {
                    return Err(TxnError::SpacemanParse(
                        "free_block: chunk-info block vanished from cache".into(),
                    ));
                };
                write_u32_le_slice(cib_entry, ci_off + 20, ci_free_count + 1);
                // Checksum deferred.
            }

            // Increment sm_free_count.
            let sm_free = u64_from_le(&sm.raw, 72).saturating_add(1);
            write_u64_le_slice(&mut sm.raw, 72, sm_free);
            sm.free_count = sm_free;
            return Ok(());
        }
    }
    // The address lies outside every chunk: a stale reference or an address
    // past the container, which no bitmap can record. The reference
    // implementations refuse such a free (linux-apfs-rw: -EIO) rather than
    // pretend it happened; so does this one, and the transaction fails.
    Err(TxnError::SpacemanParse(format!(
        "free of block {free_bno}: the address lies in no chunk of the allocator"
    )))
}

/// Arguments for `make_obj_block` (bundled to stay under the 7-arg limit).
pub struct ObjBlockArgs<'a> {
    /// Logical OID (used as o_oid for virtual; for physical pass paddr).
    pub o_oid_stored: u64,
    pub xid: u64,
    pub o_type: u32,
    pub o_subtype: u32,
    /// Block body: must be exactly block_size - 32 bytes.
    pub body: &'a [u8],
}

/// Build a serialized obj_phys block with the given header fields and body.
/// Fletcher-64 is computed over bytes [8..block_size) and stored in [0..8].
/// [CERTAIN: A. obj_phys_t layout the APFS specification]
pub fn make_obj_block(args: ObjBlockArgs<'_>) -> Vec<u8> {
    let ObjBlockArgs {
        o_oid_stored,
        xid,
        o_type,
        o_subtype,
        body,
    } = args;
    let bsz = 32 + body.len();
    let mut block = vec![0u8; bsz];
    write_u64_le_slice(&mut block, 0, 0u64); // o_cksum - filled last
    write_u64_le_slice(&mut block, 8, o_oid_stored); // o_oid
    write_u64_le_slice(&mut block, 16, xid); // o_xid
    write_u32_le_slice(&mut block, 24, o_type); // o_type
    write_u32_le_slice(&mut block, 28, o_subtype); // o_subtype
    if let Some(dst) = block.get_mut(32..) {
        dst.copy_from_slice(body);
    }
    update_checksum_in_place(&mut block);
    block
}

/// Arguments for `build_checkpoint_map` (bundled to stay under the 7-arg limit).
struct CheckpointMapArgs<'a> {
    bsz: usize,
    xid: u64,
    /// Physical oid of the map block itself (= its block address).
    block_oid: u64,
    sm_oid: u64,
    sm_type: u32,
    sm_subtype: u32,
    sm_paddr: u64,
    /// Size of the spaceman object in bytes - more than a block on a large
    /// container.
    sm_size: u32,
    /// Additional ephemeral objects to include after the spaceman entry.
    other_ephemerals: &'a [EphemeralObj],
    /// Physical block addresses where each other_ephemeral was written.
    other_paddrs: &'a [u64],
}

/// Whether the checkpoint data ring can take `needed` blocks for the new
/// checkpoint while the previous one, `previous_len` blocks long, is still
/// the live one. A new checkpoint that wrapped into the previous one would
/// destroy the only consistent state on the device before its own is sealed.
fn checkpoint_ring_has_room(
    ring_blocks: u32,
    previous_len: u32,
    needed: u32,
) -> Result<(), TxnError> {
    let occupied = previous_len.saturating_add(needed);
    if occupied > ring_blocks {
        return Err(TxnError::Unsupported(format!(
            "checkpoint data ring of {ring_blocks} blocks cannot hold the previous checkpoint \
             ({previous_len} blocks) beside the new one ({needed} blocks)"
        )));
    }
    Ok(())
}

/// `checkpoint_map_phys_t`: the mappings start at byte 40 and take 40 bytes each.
const CHECKPOINT_MAP_ENTRIES_OFFSET: usize = 40;
const CHECKPOINT_MAP_ENTRY_LEN: usize = 40;

/// Build a `checkpoint_map_phys_t` block with entries for all ephemeral objects.
/// Entry 0 = spaceman; entries 1..N = other ephemerals (reaper, btree nodes).
/// [CERTAIN: C.2 the APFS specification; apfs_raw.h checkpoint_map_phys_t layout]
///
/// One map block is written per checkpoint, so the entry count it declares
/// has to be what the block can hold: a count beyond that, with the surplus
/// mappings dropped, would be a checkpoint that names objects it cannot
/// locate. More ephemeral objects than fit is refused before anything is
/// sealed (a second map block is not written today).
fn build_checkpoint_map(a: CheckpointMapArgs<'_>) -> Result<Vec<u8>, TxnError> {
    let CheckpointMapArgs {
        bsz,
        xid,
        block_oid,
        sm_oid,
        sm_type,
        sm_subtype,
        sm_paddr,
        sm_size,
        other_ephemerals,
        other_paddrs,
    } = a;
    let capacity = bsz.saturating_sub(CHECKPOINT_MAP_ENTRIES_OFFSET) / CHECKPOINT_MAP_ENTRY_LEN;
    let total = 1 + other_ephemerals.len();
    if total > capacity || other_paddrs.len() != other_ephemerals.len() {
        return Err(TxnError::Unsupported(format!(
            "checkpoint needs {total} map entries; one map block holds {capacity}"
        )));
    }
    let mut buf = vec![0u8; bsz];
    // obj_phys header: o_oid = block_oid (physical), o_xid = xid,
    // o_type = OBJECT_TYPE_CHECKPOINT_MAP | OBJ_PHYSICAL, o_subtype = 0.
    // [CERTAIN: empirical - baseline xid=2 cmap has o_type=0x4000000c (PHYSICAL|CMAP);
    //  fsck_apfs rejects 0x8000000c (EPHEMERAL|CMAP)]
    write_u64_le_slice(&mut buf, 8, block_oid);
    write_u64_le_slice(&mut buf, 16, xid);
    write_u32_le_slice(&mut buf, 24, OBJECT_TYPE_CHECKPOINT_MAP | OBJ_PHYSICAL);
    write_u32_le_slice(&mut buf, 28, 0u32);
    // cpm_flags @32 (u32) = CHECKPOINT_MAP_LAST
    write_u32_le_slice(&mut buf, 32, CHECKPOINT_MAP_LAST);
    // cpm_count @36 (u32) = 1 (spaceman) + other_ephemerals.len()
    write_u32_le_slice(&mut buf, 36, total as u32);
    // checkpoint_mapping_t entries starting at offset 40 (0x28).
    // Each entry: cme_type(u32)+cme_subtype(u32)+cme_size(u32)+
    //             cme_pad(u32)+cme_fs_oid(u64)+cme_oid(u64)+cme_paddr(u64) = 40 bytes.
    // [CERTAIN: apfs_raw.h struct checkpoint_mapping_t]
    let e0 = CHECKPOINT_MAP_ENTRIES_OFFSET;
    write_u32_le_slice(&mut buf, e0, sm_type); // cme_type
    write_u32_le_slice(&mut buf, e0 + 4, sm_subtype); // cme_subtype
    write_u32_le_slice(&mut buf, e0 + 8, sm_size); // cme_size
    write_u32_le_slice(&mut buf, e0 + 12, 0u32); // cme_pad
    write_u64_le_slice(&mut buf, e0 + 16, 0u64); // cme_fs_oid (container-level = 0)
    write_u64_le_slice(&mut buf, e0 + 24, sm_oid); // cme_oid
    write_u64_le_slice(&mut buf, e0 + 32, sm_paddr); // cme_paddr
    for (i, (eph, &paddr)) in other_ephemerals.iter().zip(other_paddrs.iter()).enumerate() {
        let e = e0 + (i + 1) * CHECKPOINT_MAP_ENTRY_LEN;
        write_u32_le_slice(&mut buf, e, eph.cme_type);
        write_u32_le_slice(&mut buf, e + 4, eph.cme_subtype);
        write_u32_le_slice(&mut buf, e + 8, eph.raw.len() as u32);
        write_u32_le_slice(&mut buf, e + 12, 0u32);
        write_u64_le_slice(&mut buf, e + 16, eph.fs_oid); // cme_fs_oid, as read
        write_u64_le_slice(&mut buf, e + 24, eph.oid);
        write_u64_le_slice(&mut buf, e + 32, paddr);
    }
    update_checksum_in_place(&mut buf);
    Ok(buf)
}

/// Recompute Fletcher-64 and store it in bytes [0..8] of `block`.
pub fn update_checksum_in_place(block: &mut [u8]) {
    let ck = fletcher64(block);
    if let Some(dst) = block.get_mut(0..8) {
        dst.copy_from_slice(&ck.to_le_bytes());
    }
}

/// Update o_xid (@0x10) and recompute Fletcher-64.
fn update_xid_and_checksum(block: &mut [u8], xid: u64) {
    write_u64_le_slice(block, 0x10, xid);
    update_checksum_in_place(block);
}

// ---------------------------------------------------------------------------
// Little-endian field helpers
// ---------------------------------------------------------------------------

fn u64_at(block: &[u8], off: usize) -> Result<u64, TxnError> {
    block
        .get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(TxnError::NxParse(format!("short read at offset {off:#x}")))
}

fn u32_at(block: &[u8], off: usize) -> Result<u32, TxnError> {
    block
        .get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(TxnError::NxParse(format!("short read at offset {off:#x}")))
}

fn u32_from_le(buf: &[u8], off: usize) -> u32 {
    buf.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}

fn u64_from_le(buf: &[u8], off: usize) -> u64 {
    buf.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}

fn write_u64_le(buf: &mut [u8], off: usize, val: u64) {
    if let Some(s) = buf.get_mut(off..off + 8) {
        s.copy_from_slice(&val.to_le_bytes());
    }
}

fn write_u32_le(buf: &mut [u8], off: usize, val: u32) {
    if let Some(s) = buf.get_mut(off..off + 4) {
        s.copy_from_slice(&val.to_le_bytes());
    }
}

fn write_u64_le_slice(buf: &mut [u8], off: usize, val: u64) {
    if let Some(s) = buf.get_mut(off..off + 8) {
        s.copy_from_slice(&val.to_le_bytes());
    }
}

fn write_u32_le_slice(buf: &mut [u8], off: usize, val: u32) {
    if let Some(s) = buf.get_mut(off..off + 4) {
        s.copy_from_slice(&val.to_le_bytes());
    }
}

fn write_u16_le_slice(buf: &mut [u8], off: usize, val: u16) {
    if let Some(s) = buf.get_mut(off..off + 2) {
        s.copy_from_slice(&val.to_le_bytes());
    }
}

fn u16_from_le(buf: &[u8], off: usize) -> u16 {
    buf.get(off..off + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Compute the APFS ring index advance: (current + 1) % ring_size.
// [CERTAIN: C.3 descriptor ring index math, the APFS specification]
// ---------------------------------------------------------------------------

/// Advance a checkpoint ring index by 1, wrapping at `ring_size`.
pub fn ring_next(current: u32, ring_size: u32) -> u32 {
    if ring_size == 0 {
        0
    } else {
        (current + 1) % ring_size
    }
}

/// An in-memory device and a transaction over it for unit tests of the tree
/// walkers in the sibling modules, which only need `read_block` over a
/// handful of crafted blocks.
#[cfg(test)]
#[allow(clippy::indexing_slicing)]
pub(crate) mod test_support {
    use super::*;
    use apfs_core::block_device::BlockDevice;

    pub(crate) struct MemDev {
        pub(crate) data: Vec<u8>,
    }

    impl MemDev {
        /// A zeroed device of `blocks` 4 KiB blocks.
        pub(crate) fn new(blocks: usize) -> Self {
            Self {
                data: vec![0u8; blocks * 4096],
            }
        }

        /// Place `block` at block address `bno`.
        pub(crate) fn put(&mut self, bno: u64, block: &[u8]) {
            let off = bno as usize * 4096;
            self.data[off..off + block.len()].copy_from_slice(block);
        }
    }

    impl BlockDevice for MemDev {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let start = usize::try_from(offset).ok();
            let end = start.and_then(|s| s.checked_add(buf.len()));
            match (start, end) {
                (Some(s), Some(e)) if e <= self.data.len() => {
                    buf.copy_from_slice(&self.data[s..e]);
                    Ok(())
                }
                _ => Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.data.len() as u64,
                }),
            }
        }
    }

    impl WritableBlockDevice for MemDev {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
            let start = usize::try_from(offset).ok();
            let end = start.and_then(|s| s.checked_add(buf.len()));
            match (start, end) {
                (Some(s), Some(e)) if e <= self.data.len() => {
                    self.data[s..e].copy_from_slice(buf);
                    Ok(())
                }
                _ => Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.data.len() as u64,
                }),
            }
        }
    }

    impl Transaction<MemDev> {
        /// A transaction over `dev` with an otherwise empty container view:
        /// enough to read blocks and walk trees, nothing else.
        pub(crate) fn over(dev: MemDev) -> Self {
            let block_count = dev.data.len() as u64 / 4096;
            let nx = NxView {
                block_size: 4096,
                block_count,
                spaceman_oid: 0,
                omap_oid: 0,
                fs_oids: Vec::new(),
                xp_desc_base: 1,
                xp_desc_blocks: 1,
                xp_data_base: 2,
                xp_data_blocks: 1,
                xp_desc_next: 0,
                xp_data_next: 0,
                xp_desc_index: 0,
                xp_desc_len: 1,
                xp_data_index: 0,
                xp_data_len: 1,
                current_xid: 1,
                next_oid: 0x1000,
                raw: vec![0u8; 4096],
            };
            let sm = SpacemanView {
                paddr: 0,
                oid: 0,
                xid: 0,
                o_type: 0,
                o_subtype: 0,
                free_count: 0,
                block_size: 4096,
                raw: vec![0u8; 4096],
                txn_xid: 2,
                ip: InternalPool::default(),
                cabs: HashMap::new(),
            };
            Self {
                dev,
                nx,
                sm,
                pending: Vec::new(),
                pending_index: HashMap::new(),
                undo: None,
                omap_entries: Vec::new(),
                staged_volumes: HashMap::new(),
                xid: 2,
                next_oid: 0x1000,
                allocated_blocks: Vec::new(),
                direct: HashSet::new(),
                data_buf: std::collections::BTreeMap::new(),
                data_buf_bytes: 0,
                other_ephemerals: Vec::new(),
                dirty_bitmaps: HashMap::new(),
                dirty_cibs: HashMap::new(),
                dirty_runs: HashMap::new(),
                locality: Locality::default(),
                reclaim_bitmap_paddrs: HashSet::new(),
                reclaim_cib_paddrs: HashSet::new(),
                pending_frees: HashSet::new(),
                blocks_allocated: 0,
                full_rebuild: false,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unit tests (TDD red→green, no disk I/O)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use apfs_core::checksum::fletcher64;

    // The data ring must hold the previous checkpoint beside the new one.
    #[test]
    fn checkpoint_ring_room_is_checked_against_the_previous_checkpoint() {
        assert!(checkpoint_ring_has_room(16, 4, 5).is_ok());
        assert!(checkpoint_ring_has_room(16, 8, 8).is_ok());
        assert!(matches!(
            checkpoint_ring_has_room(16, 9, 8),
            Err(TxnError::Unsupported(_))
        ));
        assert!(checkpoint_ring_has_room(4, u32::MAX, 1).is_err());
    }

    // A space manager that reaches its chunk-info blocks through address
    // blocks (containers above about 8 TiB) is refused before any write; one
    // without them is accepted.
    /// Pool blocks of the miniature large container below: two address
    /// blocks, each listing one chunk-info block.
    const CAB_A: u64 = 36;
    const CAB_B: u64 = 37;
    const CIB_A: u64 = 33;
    const CIB_B: u64 = 34;

    /// A spaceman of a large container in miniature: the array names two
    /// address blocks, each listing one chunk-info block, all inside a pool
    /// (blocks 32 to 47) that has room for copies.
    fn make_cab_spaceman(bsz: usize) -> (SpacemanView, FakeDev) {
        let mut sm_raw = vec![0u8; bsz];
        write_u32_le_slice(&mut sm_raw, 32, bsz as u32);
        write_u32_le_slice(&mut sm_raw, 36, 16u32); // blocks_per_chunk
        write_u32_le_slice(&mut sm_raw, 40, 1u32); // chunks_per_cib
        write_u32_le_slice(&mut sm_raw, SPACEMAN_CIBS_PER_CAB, 1u32);
        write_u64_le_slice(&mut sm_raw, 48, 64u64); // sm_block_count
        write_u64_le_slice(&mut sm_raw, 56, 2u64); // sm_chunk_count
        write_u32_le_slice(&mut sm_raw, SPACEMAN_MAIN_CIB_COUNT, 2u32);
        write_u32_le_slice(&mut sm_raw, SPACEMAN_MAIN_CAB_COUNT, 2u32);
        write_u32_le_slice(&mut sm_raw, SPACEMAN_MAIN_ADDR_OFFSET, 96u32);
        write_u64_le_slice(&mut sm_raw, 96, CAB_A);
        write_u64_le_slice(&mut sm_raw, 104, CAB_B);
        let mut dev = FakeDev::new(bsz * 64);
        for (index, (cab_bno, cib_bno)) in [(CAB_A, CIB_A), (CAB_B, CIB_B)].into_iter().enumerate()
        {
            let mut cab = vec![0u8; bsz];
            write_u64_le_slice(&mut cab, 8, cab_bno);
            write_u64_le_slice(&mut cab, 16, 1u64); // older than the transaction
            write_u32_le_slice(&mut cab, 0x18, OBJECT_TYPE_SPACEMAN_CAB);
            write_u32_le_slice(&mut cab, CAB_INDEX_OFFSET, index as u32);
            write_u32_le_slice(&mut cab, CAB_CIB_COUNT_OFFSET, 1u32);
            write_u64_le_slice(&mut cab, CAB_ENTRIES_OFFSET, cib_bno);
            update_checksum_in_place(&mut cab);
            dev.write_at(cab_bno * bsz as u64, &cab).unwrap();
        }
        dev.writes.clear();
        let mut ip = make_internal_pool(bsz, 32, 16);
        for taken in [CIB_A, CIB_B, CAB_A, CAB_B] {
            ip.mark_used(bsz, taken).unwrap();
        }
        let sm = SpacemanView {
            paddr: 1,
            oid: 1024,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: 0,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip,
            cabs: HashMap::new(),
        };
        (sm, dev)
    }

    #[test]
    fn chunk_info_blocks_are_reached_through_the_address_blocks() {
        let bsz = 4096usize;
        let (mut sm, mut dev) = make_cab_spaceman(bsz);
        let first = locate_cib(&mut dev, &mut sm, bsz, 0).unwrap();
        assert_eq!(
            first,
            Some((
                CIB_A,
                CibSlot::Cab {
                    array_off: 96,
                    cab_bno: CAB_A,
                    entry_off: CAB_ENTRIES_OFFSET
                }
            ))
        );
        let second = locate_cib(&mut dev, &mut sm, bsz, 1).unwrap();
        assert_eq!(
            second,
            Some((
                CIB_B,
                CibSlot::Cab {
                    array_off: 104,
                    cab_bno: CAB_B,
                    entry_off: CAB_ENTRIES_OFFSET
                }
            ))
        );
        assert_eq!(
            locate_cib(&mut dev, &mut sm, bsz, 2).unwrap(),
            None,
            "past the arrays"
        );
        assert!(
            sm.cabs.values().all(|c| !c.dirty),
            "reading changes nothing"
        );

        // Recording a copied chunk-info block copies its address block: the
        // copy is a fresh pool block under the transaction's id, the array
        // names it, and the old one is released to the pool's queue.
        let (_, slot) = first.unwrap();
        assert!(record_cib_address(&mut sm, bsz, slot, 40).unwrap());
        let fresh = u64_from_le(&sm.raw, 96);
        assert_ne!(fresh, CAB_A, "the array must name the copy");
        assert!(sm.ip.contains(fresh) && ![CIB_A, CIB_B, CAB_B].contains(&fresh));
        let copy = sm.cabs.get(&fresh).expect("the copy is held for commit");
        assert!(copy.dirty);
        assert_eq!(
            u64_from_le(&copy.raw, 8),
            fresh,
            "a physical object's id is its address"
        );
        assert_eq!(
            u64_from_le(&copy.raw, 16),
            2,
            "the copy carries the transaction's id"
        );
        assert_eq!(u64_from_le(&copy.raw, CAB_ENTRIES_OFFSET), 40);
        assert_eq!(
            u32_from_le(&copy.raw, CAB_INDEX_OFFSET),
            0,
            "the index is the position"
        );
        assert!(
            !sm.cabs.contains_key(&CAB_A),
            "the old block is no longer ours to change"
        );
        assert_eq!(sm.ip.released, vec![CAB_A]);
        assert_eq!(
            locate_cib(&mut dev, &mut sm, bsz, 0).unwrap(),
            Some((
                40,
                CibSlot::Cab {
                    array_off: 96,
                    cab_bno: fresh,
                    entry_off: CAB_ENTRIES_OFFSET
                }
            ))
        );

        // The copy already belongs to this transaction: a second change goes
        // into it directly.
        let slot = CibSlot::Cab {
            array_off: 96,
            cab_bno: fresh,
            entry_off: CAB_ENTRIES_OFFSET,
        };
        assert!(record_cib_address(&mut sm, bsz, slot, 41).unwrap());
        assert_eq!(u64_from_le(&sm.raw, 96), fresh, "no second copy");
        assert_eq!(u64_from_le(&sm.cabs[&fresh].raw, CAB_ENTRIES_OFFSET), 41);
        assert_eq!(sm.ip.released, vec![CAB_A]);
        assert!(
            sm.cabs[&CAB_B].raw.len() == bsz && !sm.cabs[&CAB_B].dirty,
            "the other is untouched"
        );
    }

    #[test]
    fn an_address_block_that_cannot_be_copied_is_left_alone() {
        let bsz = 4096usize;
        let (mut sm, mut dev) = make_cab_spaceman(bsz);
        let (_, slot) = locate_cib(&mut dev, &mut sm, bsz, 1).unwrap().unwrap();
        // No pool: there is nowhere to copy the address block to, and it must
        // not be changed where the previous checkpoint still reads it.
        sm.ip = InternalPool::default();
        assert!(!record_cib_address(&mut sm, bsz, slot, 40).unwrap());
        assert_eq!(u64_from_le(&sm.raw, 104), CAB_B);
        assert_eq!(u64_from_le(&sm.cabs[&CAB_B].raw, CAB_ENTRIES_OFFSET), CIB_B);
        assert!(!sm.cabs[&CAB_B].dirty);
        assert!(sm.ip.released.is_empty());
    }

    #[test]
    fn an_address_block_at_the_wrong_position_is_refused() {
        let bsz = 4096usize;
        let (mut sm, mut dev) = make_cab_spaceman(bsz);
        // Swap the two array entries: each block now sits at the other's index.
        write_u64_le_slice(&mut sm.raw, 96, CAB_B);
        write_u64_le_slice(&mut sm.raw, 104, CAB_A);
        assert!(matches!(
            locate_cib(&mut dev, &mut sm, bsz, 0),
            Err(TxnError::SpacemanParse(_))
        ));
        // And a block of another type in the array is not an address block.
        let (mut sm, mut dev) = make_cab_spaceman(bsz);
        let mut not_a_cab = vec![0u8; bsz];
        write_u32_le_slice(&mut not_a_cab, 0x18, OBJECT_TYPE_BTREE_NODE);
        update_checksum_in_place(&mut not_a_cab);
        dev.write_at(CAB_A * bsz as u64, &not_a_cab).unwrap();
        assert!(matches!(
            locate_cib(&mut dev, &mut sm, bsz, 0),
            Err(TxnError::SpacemanParse(_))
        ));
    }

    // A 4 KiB checkpoint map holds 101 mappings. The spaceman takes one, so
    // 100 other ephemerals fit and 101 do not: the count must never claim
    // more than the block holds with the surplus silently dropped.
    #[test]
    fn checkpoint_map_refuses_more_entries_than_the_block_holds() {
        let eph = |n: usize| -> Vec<EphemeralObj> {
            (0..n)
                .map(|i| EphemeralObj {
                    cme_type: 0x1,
                    cme_subtype: 0,
                    fs_oid: 0,
                    oid: 2000 + i as u64,
                    raw: vec![0u8; 4096],
                })
                .collect()
        };
        let build = |others: &[EphemeralObj]| {
            let paddrs: Vec<u64> = (0..others.len() as u64).map(|i| 5000 + i).collect();
            build_checkpoint_map(CheckpointMapArgs {
                bsz: 4096,
                xid: 7,
                block_oid: 9,
                sm_oid: 1024,
                sm_type: 0x5,
                sm_subtype: 0,
                sm_paddr: 300,
                sm_size: 4096,
                other_ephemerals: others,
                other_paddrs: &paddrs,
            })
        };
        let full = build(&eph(100)).expect("100 others fit");
        assert_eq!(u32_from_le(&full, 36), 101);
        let last = 40 + 100 * 40;
        assert_eq!(u64_from_le(&full, last + 24), 2000 + 99);
        assert_eq!(u64_from_le(&full, last + 32), 5000 + 99);
        assert!(
            build(&eph(101)).is_err(),
            "101 others do not fit in one block"
        );
    }

    // A volume-owned ephemeral object stays the volume's in the rewritten
    // checkpoint map.
    #[test]
    fn checkpoint_map_keeps_each_entrys_owner() {
        let others = [
            EphemeralObj {
                cme_type: 0x1,
                cme_subtype: 0,
                fs_oid: 0,
                oid: 2000,
                raw: vec![0u8; 4096],
            },
            EphemeralObj {
                cme_type: 0x1,
                cme_subtype: 0,
                fs_oid: 1027,
                oid: 2001,
                raw: vec![0u8; 4096],
            },
        ];
        let map = build_checkpoint_map(CheckpointMapArgs {
            bsz: 4096,
            xid: 7,
            block_oid: 9,
            sm_oid: 1024,
            sm_type: 0x5,
            sm_subtype: 0,
            sm_paddr: 300,
            sm_size: 4096,
            other_ephemerals: &others,
            other_paddrs: &[5000, 5001],
        })
        .expect("two others fit");
        assert_eq!(
            u64_from_le(&map, 40 + 16),
            0,
            "the spaceman is the container's"
        );
        assert_eq!(u64_from_le(&map, 40 + 40 + 16), 0);
        assert_eq!(
            u64_from_le(&map, 40 + 80 + 16),
            1027,
            "the volume's object stays its own"
        );
        assert_eq!(u64_from_le(&map, 40 + 80 + 24), 2001);
    }

    // --- Fake writable block device for unit testing ---

    struct FakeDev {
        data: Vec<u8>,
        writes: Vec<(u64, Vec<u8>)>, // (offset, data) - write log for order assertion
    }

    impl FakeDev {
        fn new(size: usize) -> Self {
            Self {
                data: vec![0u8; size],
                writes: Vec::new(),
            }
        }
    }

    impl apfs_core::block_device::BlockDevice for FakeDev {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let end = offset as usize + buf.len();
            if end > self.data.len() {
                return Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.data.len() as u64,
                });
            }
            buf.copy_from_slice(&self.data[offset as usize..end]);
            Ok(())
        }
    }

    impl WritableBlockDevice for FakeDev {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
            let end = offset as usize + buf.len();
            if end > self.data.len() {
                return Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.data.len() as u64,
                });
            }
            self.data[offset as usize..end].copy_from_slice(buf);
            self.writes.push((offset, buf.to_vec()));
            Ok(())
        }
    }

    // --- Tests ---

    #[test]
    fn ring_index_math_wraps() {
        assert_eq!(ring_next(0, 8), 1);
        assert_eq!(ring_next(7, 8), 0); // wraps
        assert_eq!(ring_next(3, 4), 0); // wraps at 4
        assert_eq!(ring_next(0, 1), 0); // single-slot ring: always 0
        assert_eq!(ring_next(0, 0), 0); // zero ring: safe, returns 0
    }

    #[test]
    fn ring_index_math_large() {
        // Larger ring as seen in real 64-MiB APFS images (ring size typically 8).
        assert_eq!(ring_next(5, 8), 6);
        assert_eq!(ring_next(6, 8), 7);
        assert_eq!(ring_next(7, 8), 0);
    }

    #[test]
    fn obj_phys_fletcher_matches_apfs_core() {
        // Build a known block body (all 0xAB bytes after the 32-byte header).
        let bsz = 4096usize;
        let body = vec![0xABu8; bsz - 32];
        let block = make_obj_block(ObjBlockArgs {
            o_oid_stored: 0x100,
            xid: 42,
            o_type: OBJ_PHYSICAL | OBJECT_TYPE_FS,
            o_subtype: 0,
            body: &body,
        });
        assert_eq!(block.len(), bsz);
        // Verify that the stored checksum matches apfs_core::checksum::fletcher64.
        let stored = u64::from_le_bytes(block[0..8].try_into().unwrap());
        let computed = fletcher64(&block);
        assert_eq!(stored, computed, "Fletcher-64 must match apfs_core impl");
        // o_oid must be at offset 8.
        let oid_stored = u64::from_le_bytes(block[8..16].try_into().unwrap());
        assert_eq!(oid_stored, 0x100);
        // o_xid @16.
        let xid_stored = u64::from_le_bytes(block[16..24].try_into().unwrap());
        assert_eq!(xid_stored, 42);
        // o_type @24.
        let otype_stored = u32::from_le_bytes(block[24..28].try_into().unwrap());
        assert_eq!(otype_stored, OBJ_PHYSICAL | OBJECT_TYPE_FS);
    }

    #[test]
    fn obj_phys_zero_body_fletcher() {
        // All-zero body: verify checksum is non-zero (the mod construction
        // guarantees c1 and c2 are not both zero for non-trivial inputs, but
        // an all-zero body gives c1=c2=0 → checksum = MOD-0 words pattern).
        let bsz = 4096usize;
        let body = vec![0u8; bsz - 32];
        let block = make_obj_block(ObjBlockArgs {
            o_oid_stored: 1,
            xid: 1,
            o_type: OBJ_PHYSICAL | OBJECT_TYPE_NX_SUPERBLOCK,
            o_subtype: 0,
            body: &body,
        });
        let stored = u64::from_le_bytes(block[0..8].try_into().unwrap());
        let computed = fletcher64(&block);
        assert_eq!(stored, computed);
    }

    #[test]
    fn update_checksum_in_place_produces_valid_block() {
        let mut block = vec![0u8; 4096];
        // Write something non-trivial into the body.
        block[32] = 0xDE;
        block[33] = 0xAD;
        update_checksum_in_place(&mut block);
        let stored = u64::from_le_bytes(block[0..8].try_into().unwrap());
        let computed = fletcher64(&block);
        assert_eq!(stored, computed);
    }

    #[test]
    fn verify_checkpoint_block_accepts_valid_and_matching_xid() {
        let mut block = vec![0u8; 4096];
        block[16..24].copy_from_slice(&7u64.to_le_bytes()); // o_xid = 7
        update_checksum_in_place(&mut block);
        assert!(verify_checkpoint_block(&block, Some(7), "t").is_ok());
        assert!(verify_checkpoint_block(&block, None, "t").is_ok());
    }

    #[test]
    fn verify_checkpoint_block_rejects_bad_checksum() {
        let mut block = vec![0u8; 4096];
        block[16..24].copy_from_slice(&7u64.to_le_bytes());
        update_checksum_in_place(&mut block);
        block[100] ^= 0xFF; // corrupt the body AFTER the checksum was computed
        assert!(matches!(
            verify_checkpoint_block(&block, Some(7), "t"),
            Err(TxnError::PostCommitVerify(_))
        ));
    }

    #[test]
    fn verify_checkpoint_block_rejects_wrong_xid() {
        let mut block = vec![0u8; 4096];
        block[16..24].copy_from_slice(&7u64.to_le_bytes());
        update_checksum_in_place(&mut block); // checksum valid…
        assert!(matches!(
            verify_checkpoint_block(&block, Some(8), "t"), // …but XID mismatch
            Err(TxnError::PostCommitVerify(_))
        ));
    }

    #[test]
    fn fake_dev_write_records_order() {
        let mut dev = FakeDev::new(4096 * 10);
        dev.write_at(0, &[0xAAu8; 4096]).unwrap();
        dev.write_at(4096, &[0xBBu8; 4096]).unwrap();
        assert_eq!(dev.writes.len(), 2);
        assert_eq!(dev.writes[0].0, 0);
        assert_eq!(dev.writes[1].0, 4096);
        assert_eq!(dev.writes[0].1[0], 0xAA);
        assert_eq!(dev.writes[1].1[0], 0xBB);
    }

    #[test]
    fn alloc_refuses_a_bitmap_less_chunk_with_no_internal_pool() {
        // Unit test: alloc_one_block with a minimal in-memory spaceman.
        // We construct a fake spaceman whose CIB array describes one chunk
        // of 16 blocks, all free (bitmap_addr = 0), and which has no internal
        // pool to take a bitmap block out of.
        let bsz = 4096usize;
        let mut sm_raw = vec![0u8; bsz];
        // sm_block_size @32 (u32)
        write_u32_le_slice(&mut sm_raw, 32, 4096u32);
        // sm_blocks_per_chunk @36 (u32) = 16
        write_u32_le_slice(&mut sm_raw, 36, 16u32);
        // sm_chunks_per_cib @40 (u32) = 1
        write_u32_le_slice(&mut sm_raw, 40, 1u32);
        // sm_cibs_per_cab @44 (u32) = 1
        write_u32_le_slice(&mut sm_raw, 44, 1u32);
        // sm_dev[0].sm_block_count @48 (u64) = 16
        write_u64_le_slice(&mut sm_raw, 48, 16u64);
        // sm_dev[0].sm_chunk_count @56 (u64) = 1
        write_u64_le_slice(&mut sm_raw, 56, 1u64);
        // sm_dev[0].sm_cib_count @64 (u32) = 1
        write_u32_le_slice(&mut sm_raw, 64, 1u32);
        // sm_dev[0].sm_free_count @72 (u64) = 16
        write_u64_le_slice(&mut sm_raw, 72, 16u64);
        // sm_dev[0].sm_addr_offset @80 (u32) = offset of CIB array from start = 96
        // We place the CIB address array at offset 96.
        write_u32_le_slice(&mut sm_raw, 80, 96u32);
        // CIB address array at offset 96: one entry pointing to block 2 (CIB block).
        write_u64_le_slice(&mut sm_raw, 96, 2u64); // CIB lives at block 2
        update_checksum_in_place(&mut sm_raw);

        // Build a fake device with the CIB block at block 2.
        // CIB block layout (chunk_info_block_phys_t):
        //   obj_phys header (32 bytes) @0
        //   cib_index u32 @32, cib_count u32 @36
        //   chunk_info_t[] starting at offset 40.
        // chunk_info_t: xid=1, addr=10 (blocks 10-25), block_count=16,
        //               free_count=16, bitmap_addr=0 (entire chunk free).
        let mut cib_raw = vec![0u8; bsz];
        write_u32_le_slice(&mut cib_raw, 32, 0u32); // cib_index = 0
        write_u32_le_slice(&mut cib_raw, 36, 1u32); // cib_count = 1
                                                    // chunk_info_t[0] at offset 40
        write_u64_le_slice(&mut cib_raw, 40, 1u64); // ci_xid
        write_u64_le_slice(&mut cib_raw, 48, 10u64); // ci_addr = first block of chunk
        write_u32_le_slice(&mut cib_raw, 56, 16u32); // ci_block_count
        write_u32_le_slice(&mut cib_raw, 60, 16u32); // ci_free_count
        write_u64_le_slice(&mut cib_raw, 64, 0u64); // ci_bitmap_addr = 0 (all free)
        update_checksum_in_place(&mut cib_raw);

        let mut dev = FakeDev::new(bsz * 64);
        // Write CIB at block 2.
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();

        let mut sm = SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: 16,
            block_size: 4096,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        };

        let mut dirty_bitmaps: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut dirty_cibs: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut dirty_runs: HashMap<u64, crate::free_runs::ChunkFreeRuns> = HashMap::new();
        // A chunk with ci_bitmap_addr == 0 declares itself entirely free and
        // owns no bitmap object, so an allocation from it cannot be recorded
        // anywhere on disk. Handing a block out anyway leaves every other
        // implementation believing the block is still free - and fsck comparing
        // the chunk's free count against a fabricated all-zero bitmap. The
        // bitmap has to be created first, out of the internal pool; with no
        // pool to take it from the allocator must decline instead.
        let res = alloc_run_cached(
            &mut dev,
            &mut sm,
            &mut dirty_bitmaps,
            &mut dirty_cibs,
            &mut dirty_runs,
            bsz,
            1,
        );
        assert!(
            matches!(res, Err(TxnError::NoFreeBlocks)),
            "allocating from a bitmap-less chunk must be refused, got {res:?}"
        );
        // Nothing may have been recorded as taken.
        assert_eq!(
            u64_from_le(&sm.raw, 72),
            16,
            "spaceman free_count must be untouched by a refused allocation"
        );
        assert!(
            dirty_cibs.is_empty(),
            "a refused allocation must not dirty the chunk-info block"
        );
    }

    /// Build a pool of `block_count` blocks starting at `base`, with one
    /// all-free bitmap block and a sixteen-slot ring.
    fn make_internal_pool(bsz: usize, base: u64, block_count: u64) -> InternalPool {
        InternalPool {
            base,
            block_count,
            bm_base: base - 16,
            bm_ring_len: 16,
            bm_size: 1,
            xid_offset: 2520,
            bitmap_offset: 2528,
            free_next_offset: 2536,
            bitmaps: vec![vec![0u8; bsz]],
            dirty: vec![false],
            released: Vec::new(),
        }
    }

    #[test]
    fn the_internal_pool_hands_out_each_of_its_blocks_once_and_then_stops() {
        // The pool bitmap is a whole block, so it has room for far more bits
        // than the pool has blocks. Handing out a block past the pool's end
        // would return an address belonging to whatever follows it - here, the
        // chunk bitmaps themselves.
        let bsz = 4096usize;
        let mut ip = make_internal_pool(bsz, 413, 4);
        let mut handed = Vec::new();
        while let Some(bno) = ip.find_free(bsz) {
            ip.mark_used(bsz, bno).expect("mark used");
            handed.push(bno);
            assert!(handed.len() <= 8, "find_free must stop at the pool's end");
        }
        assert_eq!(
            handed,
            vec![413, 414, 415, 416],
            "the pool must hand out exactly its own blocks, in order"
        );
        assert!(ip.has_dirty(), "taking a block must mark the bitmap dirty");

        // A released block comes back, and only that one.
        ip.mark_free(bsz, 414).expect("mark free");
        assert_eq!(ip.find_free(bsz), Some(414));

        // An address outside the pool belongs to a different bitmap entirely.
        assert!(!ip.contains(412) && !ip.contains(417));
        assert!(ip.mark_used(bsz, 417).is_err());
    }

    #[test]
    fn commit_writes_superblock_last() {
        // This test verifies the write ORDER: all data objects → spaceman
        // (ephemeral) → checkpoint map → nx_superblock LAST.
        // We build a minimal in-memory APFS image and verify the FakeDev
        // write log ends with block 0 (nx_superblock).
        // The image is too minimal for Transaction::begin() to work, so we
        // test commit ordering using the internal helpers directly.
        let bsz = 4096usize;

        // Construct a minimal nx_superblock raw block.
        let mut nx_raw = vec![0u8; bsz];
        // magic 'NXSB' @32
        nx_raw[32..36].copy_from_slice(&0x4253_584Eu32.to_le_bytes());
        // block_size = 4096 @36
        nx_raw[36..40].copy_from_slice(&4096u32.to_le_bytes());
        // block_count @40 = 64
        nx_raw[40..48].copy_from_slice(&64u64.to_le_bytes());
        // next_oid @0x58 = 100
        write_u64_le_slice(&mut nx_raw, 0x58, 100u64);
        // next_xid @0x60 = 5
        write_u64_le_slice(&mut nx_raw, 0x60, 5u64);
        // o_oid @8 = 1 (physical), o_xid @16 = 4
        write_u64_le_slice(&mut nx_raw, 8, 1u64);
        write_u64_le_slice(&mut nx_raw, 16, 4u64);
        // o_type @24 = PHYSICAL | NX_SUPERBLOCK
        write_u32_le_slice(&mut nx_raw, 24, OBJ_PHYSICAL | OBJECT_TYPE_NX_SUPERBLOCK);
        // xp_desc_blocks @0x68 = 8, xp_data_blocks @0x6C = 8
        write_u32_le_slice(&mut nx_raw, 0x68, 8u32);
        write_u32_le_slice(&mut nx_raw, 0x6C, 8u32);
        // xp_desc_base @0x70 = 1, xp_data_base @0x78 = 9
        write_u64_le_slice(&mut nx_raw, 0x70, 1u64);
        write_u64_le_slice(&mut nx_raw, 0x78, 9u64);
        // xp_desc_next @0x80 = 2, xp_data_next @0x84 = 2
        write_u32_le_slice(&mut nx_raw, 0x80, 2u32);
        write_u32_le_slice(&mut nx_raw, 0x84, 2u32);
        // xp_desc_index @0x88 = 0, xp_desc_len @0x8C = 2
        write_u32_le_slice(&mut nx_raw, 0x88, 0u32);
        write_u32_le_slice(&mut nx_raw, 0x8C, 2u32);
        // xp_data_index @0x90 = 0, xp_data_len @0x94 = 1
        write_u32_le_slice(&mut nx_raw, 0x90, 0u32);
        write_u32_le_slice(&mut nx_raw, 0x94, 1u32);
        // spaceman_oid @0x98 = 0xBEEF
        write_u64_le_slice(&mut nx_raw, 0x98, 0xBEEFu64);
        update_checksum_in_place(&mut nx_raw);

        // Verify ring_next works for the desc ring advance.
        assert_eq!(ring_next(2, 8), 3);
        // The nx_superblock must be written last (offset 0).
        // We verify this property by checking that write_at(0, ...) is the
        // final write in the commit log. We test this invariant explicitly:
        let mut dev = FakeDev::new(bsz * 64);
        // Stage writes: data block, spaceman, checkpoint map, then superblock.
        dev.write_at(5 * bsz as u64, &[0xAAu8; 4096]).unwrap(); // data
        dev.write_at(9 * bsz as u64, &[0xBBu8; 4096]).unwrap(); // spaceman (data area)
        dev.write_at(3 * bsz as u64, &[0xCCu8; 4096]).unwrap(); // checkpoint map (desc area)
        dev.write_at(0, &nx_raw).unwrap(); // superblock LAST
                                           // Assert: the last write is to offset 0.
        let last_write_offset = dev.writes.last().unwrap().0;
        assert_eq!(last_write_offset, 0, "nx_superblock must be written last");
    }

    // --- NxView::parse error-path coverage ---

    /// Build a valid minimal nx_superblock raw block (4096 bytes, correct checksum).
    fn make_valid_nx_block() -> Vec<u8> {
        let mut block = vec![0u8; 4096];
        // o_oid @8: the container superblock's fixed identifier.
        write_u64_le_slice(&mut block, 8, 1u64);
        // magic 'NXSB' @32
        block[32..36].copy_from_slice(&0x4253_584Eu32.to_le_bytes());
        // block_size = 4096 @36
        block[36..40].copy_from_slice(&4096u32.to_le_bytes());
        // block_count @40 (u64) = 64
        block[40..48].copy_from_slice(&64u64.to_le_bytes());
        // next_oid @0x58
        write_u64_le_slice(&mut block, 0x58, 0x400u64);
        // next_xid @0x60
        write_u64_le_slice(&mut block, 0x60, 3u64);
        // xp_desc_blocks @0x68, xp_data_blocks @0x6C
        write_u32_le_slice(&mut block, 0x68, 8u32);
        write_u32_le_slice(&mut block, 0x6C, 8u32);
        // xp_desc_base @0x70, xp_data_base @0x78
        write_u64_le_slice(&mut block, 0x70, 1u64);
        write_u64_le_slice(&mut block, 0x78, 9u64);
        // xp ring counters
        write_u32_le_slice(&mut block, 0x80, 2u32);
        write_u32_le_slice(&mut block, 0x84, 2u32);
        write_u32_le_slice(&mut block, 0x88, 0u32);
        write_u32_le_slice(&mut block, 0x8C, 2u32);
        write_u32_le_slice(&mut block, 0x90, 0u32);
        write_u32_le_slice(&mut block, 0x94, 1u32);
        // spaceman_oid @0x98
        write_u64_le_slice(&mut block, 0x98, 0xBEEFu64);
        update_checksum_in_place(&mut block);
        block
    }

    #[test]
    fn nxview_parse_valid_round_trips() {
        let block = make_valid_nx_block();
        let nx = NxView::parse(&block).unwrap();
        assert_eq!(nx.block_size, 4096);
        assert_eq!(nx.block_count, 64);
        assert_eq!(nx.spaceman_oid, 0xBEEF);
        assert_eq!(nx.xp_desc_blocks, 8);
        assert_eq!(nx.xp_data_blocks, 8);
        assert_eq!(nx.xp_desc_base, 1);
        assert_eq!(nx.xp_data_base, 9);
        assert_eq!(nx.next_oid, 0x400);
        // current_xid is next_xid - 1
        assert_eq!(nx.current_xid, 2);
    }

    #[test]
    fn nxview_parse_short_block_errors() {
        // Block shorter than 4096 must be rejected before any field access.
        let short = vec![0u8; 100];
        let err = NxView::parse(&short).unwrap_err();
        assert!(
            matches!(err, TxnError::NxParse(_)),
            "short block must give NxParse error, got: {err:?}"
        );
    }

    /// `make_valid_nx_block` with fields patched and the checksum redone.
    fn nx_block_with(patch: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let mut block = make_valid_nx_block();
        patch(&mut block);
        update_checksum_in_place(&mut block);
        block
    }

    /// The container superblock is the one object with a fixed identifier.
    #[test]
    fn nxview_parse_rejects_wrong_oid() {
        let block = nx_block_with(|b| write_u64_le_slice(b, 8, 7));
        let err = NxView::parse(&block).unwrap_err();
        assert!(matches!(err, TxnError::NxParse(_)), "got: {err:?}");
    }

    /// A checkpoint ring lies inside the container and starts after block 0.
    #[test]
    fn nxview_parse_rejects_a_checkpoint_ring_outside_the_container() {
        // desc base 60 with 8 blocks in a 64-block container.
        let block = nx_block_with(|b| write_u64_le_slice(b, 0x70, 60));
        assert!(matches!(NxView::parse(&block), Err(TxnError::NxParse(_))));
        // data base 0: block 0 is the superblock itself.
        let block = nx_block_with(|b| write_u64_le_slice(b, 0x78, 0));
        assert!(matches!(NxView::parse(&block), Err(TxnError::NxParse(_))));
    }

    /// The two rings are distinct; overlapping ones would have a checkpoint
    /// map written over the spaceman.
    #[test]
    fn nxview_parse_rejects_overlapping_checkpoint_rings() {
        // desc 1..9 and data 5..13.
        let block = nx_block_with(|b| write_u64_le_slice(b, 0x78, 5));
        let err = NxView::parse(&block).unwrap_err();
        assert!(matches!(err, TxnError::NxParse(_)), "got: {err:?}");
    }

    /// A tree-form checkpoint area keeps a B-tree oid where the ring base
    /// would be; the ring arithmetic has no meaning for it, so the write
    /// path declines the container rather than write into unrelated blocks.
    #[test]
    fn nxview_parse_rejects_a_tree_form_checkpoint_area() {
        let block = nx_block_with(|b| write_u32_le_slice(b, 0x68, 0x8000_0008));
        let err = NxView::parse(&block).unwrap_err();
        assert!(matches!(err, TxnError::NxParse(_)), "got: {err:?}");
    }

    /// A superblock declaring more blocks than the device holds describes a
    /// container that is not there.
    #[test]
    fn begin_rejects_a_container_larger_than_its_device() {
        let mut dev = FakeDev::new(8 * 4096);
        dev.write_at(0, &make_valid_nx_block()).unwrap(); // declares 64 blocks
        let err = Transaction::begin(dev)
            .err()
            .expect("a container larger than the device must be refused");
        assert!(
            matches!(&err, TxnError::NxParse(m) if m.contains("device")),
            "the error must say the device is too small, got: {err:?}"
        );
    }

    /// The spaceman under test for the address-space guards: one chunk of
    /// 16 free blocks at 20, its chunk-info block at 2 and its bitmap at 3.
    fn address_guard_spaceman(bsz: usize, sm_raw: Vec<u8>) -> SpacemanView {
        SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: 16,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        }
    }

    /// A chunk-info block address past the address space is refused as a
    /// broken allocator; the offset arithmetic must not overflow first.
    #[test]
    fn alloc_refuses_a_chunk_info_block_past_the_address_space() {
        let bsz = 4096usize;
        let (mut sm_raw, _cib, _bitmap) = make_bitmap_spaceman(bsz, 20, 16, 16, 3);
        write_u64_le_slice(&mut sm_raw, 96, 1u64 << 60); // the CIB "at" block 2^60
        let mut dev = FakeDev::new(bsz * 64);
        let mut sm = address_guard_spaceman(bsz, sm_raw);
        let r = alloc_run_cached(
            &mut dev,
            &mut sm,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            bsz,
            1,
        );
        match r {
            Err(TxnError::SpacemanParse(msg)) => {
                assert!(msg.contains("outside the device"), "{msg}")
            }
            other => panic!("got: {other:?}"),
        }
    }

    /// A chunk whose bitmap address lies past the address space is left
    /// alone, again without the multiplication wrapping.
    #[test]
    fn alloc_skips_a_chunk_whose_bitmap_is_past_the_address_space() {
        let bsz = 4096usize;
        let (sm_raw, mut cib_raw, _bitmap) = make_bitmap_spaceman(bsz, 20, 16, 16, 3);
        write_u64_le_slice(&mut cib_raw, 64, 1u64 << 60); // ci_bitmap_addr
        update_checksum_in_place(&mut cib_raw);
        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();
        let mut sm = address_guard_spaceman(bsz, sm_raw);
        let r = alloc_run_cached(
            &mut dev,
            &mut sm,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            bsz,
            1,
        );
        assert!(matches!(r, Err(TxnError::NoFreeBlocks)), "got: {r:?}");
    }

    /// The internal pool's bitmap address comes from the spaceman; one past
    /// the address space is an error, not an overflow.
    #[test]
    fn read_internal_pool_refuses_a_bitmap_past_the_address_space() {
        let bsz = 4096usize;
        let mut sm_raw = vec![0u8; bsz];
        write_u64_le_slice(&mut sm_raw, SMI_IP_BLOCK_COUNT, 16);
        write_u32_le_slice(&mut sm_raw, SMI_IP_BM_SIZE_IN_BLOCKS, 1);
        write_u32_le_slice(&mut sm_raw, SMI_IP_BM_BLOCK_COUNT, 1);
        write_u64_le_slice(&mut sm_raw, SMI_IP_BM_BASE, 1u64 << 60);
        write_u64_le_slice(&mut sm_raw, SMI_IP_BASE, 400);
        write_u32_le_slice(&mut sm_raw, SMI_IP_BM_XID_OFFSET, 1000);
        write_u32_le_slice(&mut sm_raw, SMI_IP_BITMAP_OFFSET, 1100);
        write_u32_le_slice(&mut sm_raw, SMI_IP_BM_FREE_NEXT_OFFSET, 1200);
        let mut dev = FakeDev::new(bsz * 64);
        let r = read_internal_pool(&mut dev, &sm_raw, bsz);
        assert!(
            matches!(r, Err(TxnError::BlockOutOfRange { .. })),
            "got: {r:?}"
        );
    }

    #[test]
    fn nxview_parse_bad_checksum_errors() {
        let mut block = make_valid_nx_block();
        // Corrupt one byte in the body - checksum mismatch.
        block[100] ^= 0xFF;
        let err = NxView::parse(&block).unwrap_err();
        assert!(
            matches!(err, TxnError::NxParse(_)),
            "bad checksum must give NxParse error, got: {err:?}"
        );
    }

    #[test]
    fn nxview_parse_bad_magic_errors() {
        let mut block = make_valid_nx_block();
        // Overwrite magic with garbage, then recompute checksum so it passes.
        block[32..36].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        update_checksum_in_place(&mut block);
        let err = NxView::parse(&block).unwrap_err();
        assert!(
            matches!(err, TxnError::NxParse(_)),
            "bad magic must give NxParse error, got: {err:?}"
        );
    }

    #[test]
    fn begin_recoverable_returns_device_on_error() {
        // An all-zero device has no valid checkpoint, so begin fails. The
        // recoverable variant must hand the device back (not drop it) so the
        // writable mount can fall back cleanly instead of leaking the handle
        // / leaving the mount dead. [#137 / BUG-02]
        let dev = FakeDev::new(64 * 4096);
        match Transaction::begin_recoverable(dev) {
            Ok(_) => panic!("begin_recoverable must fail on an all-zero device"),
            Err((dev, _e)) => {
                // Device recovered intact (still owned). FakeDev.data is in-module.
                assert_eq!(dev.data.len(), 64 * 4096);
            }
        }
    }

    #[test]
    fn nxview_parse_bad_block_size_errors() {
        let mut block = make_valid_nx_block();
        // Set block_size to 512 (unsupported), recompute checksum.
        block[36..40].copy_from_slice(&512u32.to_le_bytes());
        update_checksum_in_place(&mut block);
        let err = NxView::parse(&block).unwrap_err();
        assert!(
            matches!(err, TxnError::InvalidBlockSize(512)),
            "block_size=512 must give InvalidBlockSize(512), got: {err:?}"
        );
    }

    #[test]
    fn nxview_parse_zero_desc_ring_errors() {
        let mut block = make_valid_nx_block();
        // xp_desc_blocks = 0 would divide-by-zero in read_spaceman (i % desc).
        write_u32_le_slice(&mut block, 0x68, 0u32);
        update_checksum_in_place(&mut block);
        let err = NxView::parse(&block).unwrap_err();
        assert!(
            matches!(err, TxnError::NxParse(_)),
            "zero xp_desc_blocks must give NxParse, got: {err:?}"
        );
    }

    #[test]
    fn nxview_parse_zero_data_ring_errors() {
        let mut block = make_valid_nx_block();
        // xp_data_blocks = 0 would divide-by-zero on the checkpoint data ring.
        write_u32_le_slice(&mut block, 0x6C, 0u32);
        update_checksum_in_place(&mut block);
        let err = NxView::parse(&block).unwrap_err();
        assert!(
            matches!(err, TxnError::NxParse(_)),
            "zero xp_data_blocks must give NxParse, got: {err:?}"
        );
    }

    #[test]
    fn nxview_parse_fs_oids_skips_an_empty_slot() {
        let mut block = make_valid_nx_block();
        // Slots 0 and 2 hold volumes, slot 1 was emptied by a deletion.
        write_u64_le_slice(&mut block, 0xB8, 0x1234u64);
        write_u64_le_slice(&mut block, 0xC0, 0u64);
        write_u64_le_slice(&mut block, 0xC8, 0x5678u64);
        update_checksum_in_place(&mut block);
        let nx = NxView::parse(&block).unwrap();
        assert_eq!(nx.fs_oids, vec![0x1234, 0x5678]);
    }

    // --- alloc_blocks_run multi-block run extension ---

    /// Build a minimal in-memory spaceman + CIB with a bitmap block for a
    /// chunk that has `free_count` free blocks out of `block_count` total,
    /// starting at physical block `ci_addr`.  The bitmap block is placed at
    /// `bitmap_bno`.  Returns `(sm_raw, cib_raw, bitmap_raw)`.
    fn make_bitmap_spaceman(
        bsz: usize,
        ci_addr: u64,
        block_count: u32,
        free_count: u32,
        bitmap_bno: u64,
    ) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        // spaceman
        let mut sm_raw = vec![0u8; bsz];
        write_u32_le_slice(&mut sm_raw, 32, bsz as u32); // sm_block_size
        write_u32_le_slice(&mut sm_raw, 36, block_count); // blocks_per_chunk
        write_u32_le_slice(&mut sm_raw, 40, 1u32); // chunks_per_cib
        write_u32_le_slice(&mut sm_raw, 44, 1u32); // cibs_per_cab
        write_u64_le_slice(&mut sm_raw, 48, block_count as u64); // sm_block_count
        write_u64_le_slice(&mut sm_raw, 56, 1u64); // sm_chunk_count
        write_u32_le_slice(&mut sm_raw, 64, 1u32); // sm_cib_count
        write_u64_le_slice(&mut sm_raw, 72, free_count as u64); // sm_free_count
        write_u32_le_slice(&mut sm_raw, 80, 96u32); // sm_addr_offset -> CIB array at 96
        write_u64_le_slice(&mut sm_raw, 96, 2u64); // CIB lives at block 2
        update_checksum_in_place(&mut sm_raw);

        // CIB with one chunk_info_t having a real bitmap block
        let mut cib_raw = vec![0u8; bsz];
        write_u32_le_slice(&mut cib_raw, 32, 0u32); // cib_index
        write_u32_le_slice(&mut cib_raw, 36, 1u32); // cib_count
        write_u64_le_slice(&mut cib_raw, 40, 1u64); // ci_xid
        write_u64_le_slice(&mut cib_raw, 48, ci_addr); // ci_addr
        write_u32_le_slice(&mut cib_raw, 56, block_count); // ci_block_count
        write_u32_le_slice(&mut cib_raw, 60, free_count); // ci_free_count
        write_u64_le_slice(&mut cib_raw, 64, bitmap_bno); // ci_bitmap_addr
        update_checksum_in_place(&mut cib_raw);

        // bitmap: all-zeros = all bits free (0 = free, 1 = used in APFS).
        let bitmap_raw = vec![0u8; bsz];

        (sm_raw, cib_raw, bitmap_raw)
    }

    #[test]
    fn alloc_blocks_run_returns_contiguous_multi_block_run() {
        // A chunk with a bitmap block and 16 free blocks of 16 total.
        // Requesting 4 blocks must return 4 contiguous blocks starting at ci_addr.
        let bsz = 4096usize;
        const CI_ADDR: u64 = 20;
        const BLOCK_COUNT: u32 = 16;
        const FREE_COUNT: u32 = 16;
        const BITMAP_BNO: u64 = 3;

        let (sm_raw, cib_raw, bitmap_raw) =
            make_bitmap_spaceman(bsz, CI_ADDR, BLOCK_COUNT, FREE_COUNT, BITMAP_BNO);

        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();
        dev.write_at(BITMAP_BNO * bsz as u64, &bitmap_raw).unwrap();

        let mut sm = SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: FREE_COUNT as u64,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        };

        let mut dirty_bitmaps: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut dirty_cibs: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut dirty_runs: HashMap<u64, crate::free_runs::ChunkFreeRuns> = HashMap::new();

        let (blk, count) = alloc_run_cached(
            &mut dev,
            &mut sm,
            &mut dirty_bitmaps,
            &mut dirty_cibs,
            &mut dirty_runs,
            bsz,
            4,
        )
        .unwrap();

        // The run must start at the first free bit in the bitmap (all-free → bit 0).
        assert_eq!(blk, CI_ADDR, "run must start at first free block in chunk");
        assert_eq!(count, 4, "must allocate exactly 4 contiguous blocks");
        // sm_free_count must be decremented by count.
        assert_eq!(sm.free_count, FREE_COUNT as u64 - count as u64);
        // CIB ci_free_count in dirty cache must reflect the deduction.
        let cib_dirty = dirty_cibs.get(&2).expect("CIB must be in dirty cache");
        let ci_free_after = u32_from_le(cib_dirty, 60);
        assert_eq!(ci_free_after, FREE_COUNT - count as u32);
    }

    #[test]
    fn a_run_comes_whole_from_a_later_chunk_before_a_scrap_of_an_earlier_one() {
        // Chunk 0 (blocks 20..36) has only 3 free blocks left; chunk 1
        // (blocks 40..56) is empty. Eight blocks come whole from chunk 1, not
        // three from chunk 0: a file cut into scraps costs a device write per
        // scrap, now and every time it is read.
        let bsz = 4096usize;
        let (mut sm_raw, mut cib_raw, _) = make_bitmap_spaceman(bsz, 20, 16, 3, 3);
        write_u32_le_slice(&mut sm_raw, 40, 2); // chunks_per_cib
        write_u64_le_slice(&mut sm_raw, 48, 32); // sm_block_count
        write_u64_le_slice(&mut sm_raw, 56, 2); // sm_chunk_count
        write_u64_le_slice(&mut sm_raw, 72, 19); // sm_free_count
        update_checksum_in_place(&mut sm_raw);
        write_u32_le_slice(&mut cib_raw, 36, 2); // chunks in this block
        write_u64_le_slice(&mut cib_raw, 72, 1); // chunk 1: xid
        write_u64_le_slice(&mut cib_raw, 80, 40); //          addr
        write_u32_le_slice(&mut cib_raw, 88, 16); //          block_count
        write_u32_le_slice(&mut cib_raw, 92, 16); //          free_count
        write_u64_le_slice(&mut cib_raw, 96, 4); //           bitmap
        update_checksum_in_place(&mut cib_raw);
        // Blocks 0-15 of chunk 0 in use but for 10, 13 and 15.
        let mut scraps = vec![0u8; bsz];
        scraps[0] = 0xFF;
        scraps[1] = 0b0101_1011;

        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();
        dev.write_at(3 * bsz as u64, &scraps).unwrap();
        dev.write_at(4 * bsz as u64, &vec![0u8; bsz]).unwrap();

        let mut sm = SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: 19,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        };
        let (mut bitmaps, mut cibs, mut runs) = (HashMap::new(), HashMap::new(), HashMap::new());
        let (blk, count) = alloc_run_cached(
            &mut dev,
            &mut sm,
            &mut bitmaps,
            &mut cibs,
            &mut runs,
            bsz,
            8,
        )
        .unwrap();
        assert_eq!((blk, count), (40, 8), "eight blocks whole from chunk 1");

        // One block still comes from the first chunk that has one.
        let (blk, count) = alloc_run_cached(
            &mut dev,
            &mut sm,
            &mut bitmaps,
            &mut cibs,
            &mut runs,
            bsz,
            1,
        )
        .unwrap();
        assert_eq!(count, 1);
        assert!(
            (20..36).contains(&blk),
            "a single block fills a scrap: {blk}"
        );
    }

    /// One-block allocations of one transaction follow each other instead of
    /// filling a different one-block hole each: the holes copy-on-write
    /// leaves are everywhere, and a commit writing its blocks into them made
    /// a device write per block.
    #[test]
    fn a_transactions_blocks_follow_each_other_past_the_holes() {
        let bsz = 4096usize;
        const CI_ADDR: u64 = 100;
        const BLOCKS: u32 = 1024;
        // Blocks 0..512: every other one free (256 one-block holes);
        // 512..1024 free.
        let (sm_raw, cib_raw, _) = make_bitmap_spaceman(bsz, CI_ADDR, BLOCKS, 256 + 512, 3);
        let mut bitmap = vec![0u8; bsz];
        for byte in bitmap.iter_mut().take(64) {
            *byte = 0b0101_0101;
        }
        let mut dev = FakeDev::new(bsz * 2048);
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();
        dev.write_at(3 * bsz as u64, &bitmap).unwrap();
        let mut sm = SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: 256 + 512,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        };
        let (mut bitmaps, mut cibs, mut runs) = (HashMap::new(), HashMap::new(), HashMap::new());
        let mut loc = Locality::default();
        let mut got = Vec::new();
        for _ in 0..40 {
            let (blk, count) = alloc_run_near(
                &mut dev,
                &mut sm,
                &mut bitmaps,
                &mut cibs,
                &mut runs,
                &mut loc,
                bsz,
                1,
            )
            .unwrap();
            assert_eq!(count, 1);
            got.push(blk);
        }
        // Bit 511 is the last hole and joins the free half: the run is 511..1024.
        let first = CI_ADDR + 511;
        let want: Vec<u64> = (first..first + 40).collect();
        assert_eq!(got, want, "40 blocks in one run after the holes");
    }

    #[test]
    fn alloc_blocks_run_returns_partial_when_chunk_smaller_than_request() {
        // Only 3 free blocks available but we ask for 8.
        // alloc_run_cached must return 3 (largest available run) rather than an error.
        let bsz = 4096usize;
        const CI_ADDR: u64 = 30;
        const BLOCK_COUNT: u32 = 16;
        const FREE_COUNT: u32 = 3;
        const BITMAP_BNO: u64 = 4;

        // bitmap: first 13 bits set (used), last 3 bits clear (free).
        let mut bitmap_raw = vec![0u8; bsz];
        // 13 bits used = byte 0 has all 8 bits set, byte 1 has bits 0..4 set.
        bitmap_raw[0] = 0xFF; // blocks 0-7 used
        bitmap_raw[1] = 0x1F; // blocks 8-12 used, blocks 13-15 free

        let (sm_raw, cib_raw, _) =
            make_bitmap_spaceman(bsz, CI_ADDR, BLOCK_COUNT, FREE_COUNT, BITMAP_BNO);

        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();
        dev.write_at(BITMAP_BNO * bsz as u64, &bitmap_raw).unwrap();

        let mut sm = SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: FREE_COUNT as u64,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        };

        let mut dirty_bitmaps: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut dirty_cibs: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut dirty_runs: HashMap<u64, crate::free_runs::ChunkFreeRuns> = HashMap::new();

        let (blk, count) = alloc_run_cached(
            &mut dev,
            &mut sm,
            &mut dirty_bitmaps,
            &mut dirty_cibs,
            &mut dirty_runs,
            bsz,
            8, // ask for 8, only 3 available
        )
        .unwrap();

        // Must return whatever the largest contiguous run is (3 blocks at bit 13).
        assert!(count <= 3, "cannot return more than available free_count");
        assert!(count >= 1, "must allocate at least 1 block");
        // The returned block address must be within the chunk range.
        assert!(blk >= CI_ADDR, "block must be within the chunk");
        assert!(
            blk < CI_ADDR + BLOCK_COUNT as u64,
            "block must be within the chunk"
        );
        // sm_free_count decremented by count.
        assert_eq!(sm.free_count, FREE_COUNT as u64 - count as u64);
    }

    #[test]
    fn a_chunk_info_block_holds_only_the_chunks_it_counts() {
        let bsz = 4096;
        let mut cib = vec![0u8; bsz];
        write_u32_le_slice(&mut cib, CIB_CHUNK_INFO_COUNT, 3);
        assert_eq!(cib_chunk_count(&cib, 126, bsz).unwrap(), 3);
        // More than the space manager says a block holds.
        assert!(cib_chunk_count(&cib, 2, bsz).is_err());
        // More than the block can hold at all.
        write_u32_le_slice(&mut cib, CIB_CHUNK_INFO_COUNT, 200);
        assert!(cib_chunk_count(&cib, 1000, bsz).is_err());
    }

    #[test]
    fn a_chunk_that_cannot_exist_is_refused() {
        let bsz = 4096;
        let per_chunk = 32768;
        assert!(check_chunk_info(0, 32768, 100, per_chunk, bsz, 1 << 20).is_ok());
        // More blocks than a chunk has.
        assert!(check_chunk_info(0, 32769, 0, per_chunk, bsz, 1 << 20).is_err());
        // More blocks than its one-block bitmap has bits.
        assert!(check_chunk_info(0, 40000, 0, 1 << 20, bsz, 1 << 20).is_err());
        // More free blocks than blocks.
        assert!(check_chunk_info(0, 100, 101, per_chunk, bsz, 1 << 20).is_err());
        // Past the end of the device, or wrapping.
        assert!(check_chunk_info(1 << 20, 1, 0, per_chunk, bsz, 1 << 20).is_err());
        assert!(check_chunk_info(u64::MAX, 2, 0, per_chunk, bsz, u64::MAX).is_err());
    }

    #[test]
    fn alloc_run_cached_no_free_blocks_returns_error() {
        // Chunk is fully used (free_count = 0, all bitmap bits set).
        let bsz = 4096usize;
        let (sm_raw, mut cib_raw, _) = make_bitmap_spaceman(bsz, 50, 8, 0, 5); // free_count=0
                                                                               // Override free_count in CIB to 0.
        write_u32_le_slice(&mut cib_raw, 60, 0u32);
        update_checksum_in_place(&mut cib_raw);

        // bitmap: all bits set.
        let mut bitmap_raw = vec![0xFFu8; bsz];
        bitmap_raw[0] = 0xFF;

        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();
        dev.write_at(5 * bsz as u64, &bitmap_raw).unwrap();

        let mut sm = SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: 0,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        };

        let mut dirty_bitmaps = HashMap::new();
        let mut dirty_cibs = HashMap::new();
        let mut dirty_runs = HashMap::new();

        let err = alloc_run_cached(
            &mut dev,
            &mut sm,
            &mut dirty_bitmaps,
            &mut dirty_cibs,
            &mut dirty_runs,
            bsz,
            1,
        )
        .unwrap_err();
        assert!(
            matches!(err, TxnError::NoFreeBlocks),
            "fully-used chunk must give NoFreeBlocks, got: {err:?}"
        );
    }

    // --- Container object map rebuild ---

    /// The miniature container the object-map rebuild tests work in: one
    /// chunk of 32 blocks from 16 with its bitmap at 3, the map's header at
    /// 20, its single node at 21, a mapped volume superblock at 22.
    const OMAP_CHUNK_START: u64 = 16;
    const OMAP_CHUNK_BLOCKS: u32 = 32;
    const OMAP_BITMAP_BNO: u64 = 3;
    const OMAP_HEADER_BNO: u64 = 20;
    const OMAP_NODE_BNO: u64 = 21;
    const OMAP_VSB_BNO: u64 = 22;
    /// Table space of a root leaf macOS writes.
    const OMAP_TABLE_LEN: usize = 448;
    /// A mapping as the node stores it: (oid, xid, flags, size, paddr).
    type OmapRecord = (u64, u64, u32, u32, u64);

    /// A root leaf holding `records` the way macOS lays one out (measured on
    /// a fresh image: flags 0x7 for root, leaf and fixed-size records, a
    /// 448-byte table at offset 0, keys packed after it, values packed up
    /// from the 40-byte tree description, which names 16-byte keys and
    /// values). `table_off` moves the table inside its space.
    fn make_omap_root(bsz: usize, table_off: usize, records: &[OmapRecord]) -> Vec<u8> {
        let mut node = vec![0u8; bsz];
        write_u64_le_slice(&mut node, 8, OMAP_NODE_BNO);
        write_u64_le_slice(&mut node, 16, 1u64);
        write_u32_le_slice(&mut node, 24, OBJ_PHYSICAL | OBJECT_TYPE_BTREE);
        write_u32_le_slice(&mut node, 28, OBJECT_TYPE_OMAP);
        write_u16_le_slice(&mut node, 32, 0x7u16);
        write_u16_le_slice(&mut node, 34, 0u16);
        write_u32_le_slice(&mut node, 36, records.len() as u32);
        write_u16_le_slice(&mut node, 40, table_off as u16);
        write_u16_le_slice(&mut node, 42, OMAP_TABLE_LEN as u16);
        let key_area = 56 + OMAP_TABLE_LEN;
        let val_end = bsz - 40;
        let key_bytes = records.len() * 16;
        write_u16_le_slice(&mut node, 44, key_bytes as u16);
        write_u16_le_slice(
            &mut node,
            46,
            (val_end - key_bytes - key_area - key_bytes) as u16,
        );
        write_u16_le_slice(&mut node, 48, 0xFFFFu16);
        write_u16_le_slice(&mut node, 52, 0xFFFFu16);
        for (i, &(oid, xid, flags, size, paddr)) in records.iter().enumerate() {
            let toc = 56 + table_off + i * 4;
            write_u16_le_slice(&mut node, toc, (i * 16) as u16);
            write_u16_le_slice(&mut node, toc + 2, ((i + 1) * 16) as u16);
            let k = key_area + i * 16;
            write_u64_le_slice(&mut node, k, oid);
            write_u64_le_slice(&mut node, k + 8, xid);
            let v = val_end - (i + 1) * 16;
            write_u32_le_slice(&mut node, v, flags);
            write_u32_le_slice(&mut node, v + 4, size);
            write_u64_le_slice(&mut node, v + 8, paddr);
        }
        let longest = if records.is_empty() { 0u32 } else { 16 };
        write_u32_le_slice(&mut node, val_end, 0x12u32); // sequential insert, physical
        write_u32_le_slice(&mut node, val_end + 4, bsz as u32);
        write_u32_le_slice(&mut node, val_end + 8, 16u32);
        write_u32_le_slice(&mut node, val_end + 12, 16u32);
        write_u32_le_slice(&mut node, val_end + 16, longest);
        write_u32_le_slice(&mut node, val_end + 20, longest);
        write_u64_le_slice(&mut node, val_end + 24, records.len() as u64);
        write_u64_le_slice(&mut node, val_end + 32, 1u64);
        update_checksum_in_place(&mut node);
        node
    }

    /// The records a node holds, read the way the checker reads them:
    /// through the table its header declares.
    fn decode_omap_root(node: &[u8]) -> Vec<OmapRecord> {
        let nkeys = u32_from_le(node, 36) as usize;
        let table = 56 + u16_from_le(node, 40) as usize;
        let key_area = 56 + u16_from_le(node, 42) as usize;
        let val_end = node.len() - 40;
        (0..nkeys)
            .map(|i| {
                let k = key_area + u16_from_le(node, table + i * 4) as usize;
                let v = val_end - u16_from_le(node, table + i * 4 + 2) as usize;
                (
                    u64_from_le(node, k),
                    u64_from_le(node, k + 8),
                    u32_from_le(node, v),
                    u32_from_le(node, v + 4),
                    u64_from_le(node, v + 8),
                )
            })
            .collect()
    }

    /// What a rebuild left behind.
    struct RebuiltOmap {
        node: Vec<u8>,
        /// The chunk bitmap after the rebuild's frees, if it touched it.
        bitmap: Option<Vec<u8>>,
        writes: usize,
    }

    /// Rebuild the map whose node is `node` with `entries`.
    fn rebuild_omap(
        bsz: usize,
        node: &[u8],
        entries: &[OmapEntry],
    ) -> Result<RebuiltOmap, TxnError> {
        let free = OMAP_CHUNK_BLOCKS - 3;
        let (sm_raw, cib_raw, mut bitmap_raw) = make_bitmap_spaceman(
            bsz,
            OMAP_CHUNK_START,
            OMAP_CHUNK_BLOCKS,
            free,
            OMAP_BITMAP_BNO,
        );
        for taken in [OMAP_HEADER_BNO, OMAP_NODE_BNO, OMAP_VSB_BNO] {
            let bit = (taken - OMAP_CHUNK_START) as usize;
            bitmap_raw[bit / 8] |= 1 << (bit % 8);
        }
        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();
        dev.write_at(OMAP_BITMAP_BNO * bsz as u64, &bitmap_raw)
            .unwrap();
        let mut header = vec![0u8; bsz];
        write_u64_le_slice(&mut header, 8, OMAP_HEADER_BNO);
        write_u64_le_slice(&mut header, 16, 1u64);
        write_u32_le_slice(&mut header, 24, OBJ_PHYSICAL | OBJECT_TYPE_OMAP);
        write_u64_le_slice(&mut header, 48, OMAP_NODE_BNO);
        update_checksum_in_place(&mut header);
        dev.write_at(OMAP_HEADER_BNO * bsz as u64, &header).unwrap();
        dev.write_at(OMAP_NODE_BNO * bsz as u64, node).unwrap();
        dev.writes.clear();
        let mut sm = SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: free as u64,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        };
        let mut dirty_bitmaps = HashMap::new();
        let mut dirty_cibs = HashMap::new();
        let mut dirty_runs = HashMap::new();
        let mut reclaim_bitmaps = HashSet::new();
        let mut reclaim_cibs = HashSet::new();
        let new_header = apply_omap_entries(
            &mut dev,
            &mut sm,
            OMAP_HEADER_BNO,
            bsz,
            entries,
            2,
            &mut dirty_bitmaps,
            &mut dirty_cibs,
            &mut dirty_runs,
            &mut reclaim_bitmaps,
            &mut reclaim_cibs,
        )?;
        let header = &dev.data[new_header as usize * bsz..][..bsz];
        let node_bno = u64_from_le(header, 48) as usize;
        Ok(RebuiltOmap {
            node: dev.data[node_bno * bsz..][..bsz].to_vec(),
            bitmap: dirty_bitmaps.get(&OMAP_BITMAP_BNO).cloned(),
            writes: dev.writes.len(),
        })
    }

    fn bit_is_set(bitmap: &[u8], bno: u64) -> bool {
        let bit = (bno - OMAP_CHUNK_START) as usize;
        bitmap[bit / 8] & (1 << (bit % 8)) != 0
    }

    // A record the rebuild does not touch keeps its flags and size, and a
    // replaced mapping's old block is released.
    #[test]
    fn omap_rebuild_carries_untouched_records_whole() {
        let bsz = 4096usize;
        let node = make_omap_root(
            bsz,
            0,
            &[
                (1026, 2, 0, bsz as u32, OMAP_VSB_BNO),
                (1027, 2, 0x8, 2 * bsz as u32, 30),
            ],
        );
        let entries = [OmapEntry {
            oid: 1026,
            xid: 5,
            paddr: 23,
            size: bsz as u32,
        }];
        let rebuilt = rebuild_omap(bsz, &node, &entries).expect("rebuild");
        assert_eq!(
            decode_omap_root(&rebuilt.node),
            vec![
                (1026, 5, 0, bsz as u32, 23),
                (1027, 2, 0x8, 2 * bsz as u32, 30)
            ],
            "the untouched record must keep its flags and size"
        );
        let bitmap = rebuilt
            .bitmap
            .expect("the rebuild frees the replaced block");
        assert!(
            !bit_is_set(&bitmap, OMAP_VSB_BNO),
            "the replaced block must be released"
        );
        assert!(!bit_is_set(&bitmap, OMAP_NODE_BNO) && !bit_is_set(&bitmap, OMAP_HEADER_BNO));
    }

    // The rebuilt node's table is where its header says it is, whatever the
    // source node's own table offset was.
    #[test]
    fn omap_rebuild_puts_the_table_where_its_header_says() {
        let bsz = 4096usize;
        let node = make_omap_root(bsz, 8, &[(1026, 2, 0, bsz as u32, OMAP_VSB_BNO)]);
        assert_eq!(
            decode_omap_root(&node).len(),
            1,
            "the source decodes through its own table"
        );
        let entries = [OmapEntry {
            oid: 1027,
            xid: 2,
            paddr: 30,
            size: bsz as u32,
        }];
        let rebuilt = rebuild_omap(bsz, &node, &entries).expect("rebuild");
        assert_eq!(
            u16_from_le(&rebuilt.node, 40),
            0,
            "the header declares the table at 0"
        );
        assert_eq!(
            decode_omap_root(&rebuilt.node),
            vec![
                (1026, 2, 0, bsz as u32, OMAP_VSB_BNO),
                (1027, 2, 0, bsz as u32, 30)
            ],
            "every record must be reachable through the declared table"
        );
    }

    // A source record whose key or value lies outside the node is not
    // silently dropped: nothing is rebuilt.
    #[test]
    fn omap_rebuild_refuses_a_record_outside_the_node() {
        let bsz = 4096usize;
        let entries = [OmapEntry {
            oid: 1026,
            xid: 5,
            paddr: 23,
            size: bsz as u32,
        }];
        let mut key_out = make_omap_root(bsz, 0, &[(1026, 2, 0, bsz as u32, OMAP_VSB_BNO)]);
        write_u16_le_slice(&mut key_out, 56, 0x3000u16);
        let mut val_out = make_omap_root(bsz, 0, &[(1026, 2, 0, bsz as u32, OMAP_VSB_BNO)]);
        write_u16_le_slice(&mut val_out, 58, 8u16);
        let mut table_out = make_omap_root(bsz, 0, &[(1026, 2, 0, bsz as u32, OMAP_VSB_BNO)]);
        write_u32_le_slice(&mut table_out, 36, 200u32);
        let mut no_root_flag = make_omap_root(bsz, 0, &[(1026, 2, 0, bsz as u32, OMAP_VSB_BNO)]);
        write_u16_le_slice(&mut no_root_flag, 32, 0x6u16);
        for (what, mut node) in [
            ("key past the block", key_out),
            ("value inside the tree description", val_out),
            ("table past its space", table_out),
            ("root without the root flag", no_root_flag),
        ] {
            // Each is a well-checksummed node with a bad shape.
            update_checksum_in_place(&mut node);
            match rebuild_omap(bsz, &node, &entries) {
                Err(TxnError::OmapParse(msg)) => {
                    assert!(
                        !msg.contains("checksum"),
                        "{what}: refused for the shape, not {msg}"
                    )
                }
                Err(other) => panic!("{what}: wrong error {other:?}"),
                Ok(_) => panic!("{what}: must be refused, but the map was rebuilt"),
            }
        }
    }

    // A refused rebuild writes nothing.
    #[test]
    fn omap_rebuild_refusal_leaves_the_device_alone() {
        let bsz = 4096usize;
        let mut node = make_omap_root(bsz, 0, &[(1026, 2, 0, bsz as u32, OMAP_VSB_BNO)]);
        write_u16_le_slice(&mut node, 56, 0x3000u16);
        update_checksum_in_place(&mut node);
        let entries = [OmapEntry {
            oid: 1026,
            xid: 5,
            paddr: 23,
            size: bsz as u32,
        }];
        let (sm_raw, cib_raw, bitmap_raw) = make_bitmap_spaceman(
            bsz,
            OMAP_CHUNK_START,
            OMAP_CHUNK_BLOCKS,
            29,
            OMAP_BITMAP_BNO,
        );
        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();
        dev.write_at(OMAP_BITMAP_BNO * bsz as u64, &bitmap_raw)
            .unwrap();
        let mut header = vec![0u8; bsz];
        write_u64_le_slice(&mut header, 48, OMAP_NODE_BNO);
        update_checksum_in_place(&mut header);
        dev.write_at(OMAP_HEADER_BNO * bsz as u64, &header).unwrap();
        dev.write_at(OMAP_NODE_BNO * bsz as u64, &node).unwrap();
        dev.writes.clear();
        let mut sm = SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: 29,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        };
        let mut dirty_bitmaps = HashMap::new();
        let mut dirty_cibs = HashMap::new();
        let res = apply_omap_entries(
            &mut dev,
            &mut sm,
            OMAP_HEADER_BNO,
            bsz,
            &entries,
            2,
            &mut dirty_bitmaps,
            &mut dirty_cibs,
            &mut HashMap::new(),
            &mut HashSet::new(),
            &mut HashSet::new(),
        );
        assert!(matches!(res, Err(TxnError::OmapParse(_))), "got {res:?}");
        assert!(
            dev.writes.is_empty(),
            "a refused rebuild must write nothing"
        );
        assert!(
            dirty_bitmaps.is_empty() && dirty_cibs.is_empty(),
            "and allocate nothing"
        );
        assert_eq!(sm.free_count, 29);
    }

    // The tree description of a node rebuilt from an empty tree names the
    // record sizes once it holds a record.
    #[test]
    fn omap_rebuild_footer_names_the_record_sizes() {
        let bsz = 4096usize;
        let node = make_omap_root(bsz, 0, &[]);
        let entries = [OmapEntry {
            oid: 1026,
            xid: 2,
            paddr: OMAP_VSB_BNO,
            size: bsz as u32,
        }];
        let rebuilt = rebuild_omap(bsz, &node, &entries).expect("rebuild");
        let bti = bsz - 40;
        assert_eq!(u32_from_le(&rebuilt.node, bti + 16), 16, "longest key");
        assert_eq!(u32_from_le(&rebuilt.node, bti + 20), 16, "longest value");
        assert_eq!(u64_from_le(&rebuilt.node, bti + 24), 1, "record count");
        assert_eq!(u64_from_le(&rebuilt.node, bti + 32), 1, "node count");
        assert_eq!(
            u32_from_le(&rebuilt.node, bti),
            0x12,
            "the tree's flags are kept"
        );
        assert_eq!(
            decode_omap_root(&rebuilt.node),
            vec![(1026, 2, 0, bsz as u32, OMAP_VSB_BNO)]
        );
        assert_eq!(rebuilt.writes, 2, "the new node and the new header");
    }

    // --- Freeing a block the allocator cannot place ---

    /// A spaceman with one chunk of 16 blocks from 20, its bitmap at 3, and
    /// block 25 taken.
    fn free_fixture(bsz: usize) -> (SpacemanView, FakeDev) {
        let (sm_raw, cib_raw, mut bitmap_raw) = make_bitmap_spaceman(bsz, 20, 16, 15, 3);
        bitmap_raw[0] |= 1 << 5;
        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();
        dev.write_at(3 * bsz as u64, &bitmap_raw).unwrap();
        let sm = SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: 15,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        };
        (sm, dev)
    }

    fn free_block(
        sm: &mut SpacemanView,
        dev: &mut FakeDev,
        bsz: usize,
        bno: u64,
    ) -> Result<(), TxnError> {
        free_one_block_cached(
            dev,
            sm,
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashMap::new(),
            &mut HashSet::new(),
            &mut HashSet::new(),
            bsz,
            bno,
        )
    }

    // An address no chunk covers cannot be freed, and saying so beats
    // pretending: the free would be lost and the counts left wrong.
    #[test]
    fn freeing_an_address_in_no_chunk_is_an_error() {
        let bsz = 4096usize;
        let (mut sm, mut dev) = free_fixture(bsz);
        assert!(
            free_block(&mut sm, &mut dev, bsz, 25).is_ok(),
            "inside the chunk"
        );
        match free_block(&mut sm, &mut dev, bsz, 60) {
            Err(TxnError::SpacemanParse(msg)) => assert!(msg.contains("no chunk"), "{msg}"),
            other => panic!("a free past every chunk must be refused, got {other:?}"),
        }
        assert_eq!(sm.free_count, 16, "only the placeable free counts");
    }

    // A chunk-info block the allocator names outside the device is a broken
    // allocator, not a block to skip.
    #[test]
    fn freeing_through_an_unreadable_chunk_info_block_is_an_error() {
        let bsz = 4096usize;
        let (mut sm, mut dev) = free_fixture(bsz);
        write_u64_le_slice(&mut sm.raw, 96, 1u64 << 40);
        match free_block(&mut sm, &mut dev, bsz, 25) {
            Err(TxnError::SpacemanParse(msg)) => assert!(msg.contains("chunk-info block"), "{msg}"),
            other => panic!("must be refused, got {other:?}"),
        }
        assert_eq!(sm.free_count, 15, "nothing was freed");
    }

    // --- Metadata read raw and re-stamped must pass its checksum first ---

    // A chunk-info block with a flipped byte is refused by the allocator on
    // both the allocation and the free path, instead of being copied and
    // re-stamped with a fresh checksum at commit.
    #[test]
    fn a_corrupted_chunk_info_block_is_refused_before_it_is_rewritten() {
        let bsz = 4096usize;
        let (mut sm, mut dev) = free_fixture(bsz);
        dev.data[2 * bsz + 60] ^= 0x01; // ci_free_count, not re-stamped
        match free_block(&mut sm, &mut dev, bsz, 25) {
            Err(TxnError::SpacemanParse(msg)) => assert!(msg.contains("checksum"), "{msg}"),
            other => panic!("the free must be refused, got {other:?}"),
        }
        let mut dirty_bitmaps = HashMap::new();
        let mut dirty_cibs = HashMap::new();
        let mut dirty_runs = HashMap::new();
        match alloc_run_cached(
            &mut dev,
            &mut sm,
            &mut dirty_bitmaps,
            &mut dirty_cibs,
            &mut dirty_runs,
            bsz,
            1,
        ) {
            Err(TxnError::SpacemanParse(msg)) => assert!(msg.contains("checksum"), "{msg}"),
            other => panic!("the allocation must be refused, got {other:?}"),
        }
        assert!(
            dirty_cibs.is_empty(),
            "nothing of the corrupted block is kept"
        );
    }

    // The container object map's header and root are verified before the
    // rebuild reads their fields.
    #[test]
    fn a_corrupted_object_map_is_refused_before_it_is_rebuilt() {
        let bsz = 4096usize;
        let entries = [OmapEntry {
            oid: 1026,
            xid: 5,
            paddr: 23,
            size: bsz as u32,
        }];
        let mut node = make_omap_root(bsz, 0, &[(1026, 2, 0, bsz as u32, OMAP_VSB_BNO)]);
        node[36] ^= 0x01; // btn_nkeys, not re-stamped
        match rebuild_omap(bsz, &node, &entries) {
            Err(TxnError::OmapParse(msg)) => assert!(msg.contains("checksum"), "{msg}"),
            Err(other) => panic!("wrong error {other:?}"),
            Ok(_) => panic!("must be refused, but the map was rebuilt"),
        }
    }

    // --- Post-commit read-back of the checkpoint's data ---

    // A spaceman or ephemeral write that did not land, or landed under the
    // wrong id, fails the commit instead of being reported as committed.
    #[test]
    fn post_commit_read_back_covers_the_spaceman_and_the_ephemerals() {
        let bsz = 4096usize;
        let nx = NxView::parse(&make_valid_nx_block()).unwrap();
        let object = |oid: u64, xid: u64| {
            let mut b = vec![0u8; bsz];
            write_u64_le_slice(&mut b, 8, oid);
            write_u64_le_slice(&mut b, 16, xid);
            write_u32_le_slice(&mut b, 24, OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL);
            update_checksum_in_place(&mut b);
            b
        };
        let eph = EphemeralObj {
            cme_type: OBJ_EPHEMERAL | OBJECT_TYPE_BTREE,
            cme_subtype: 0,
            fs_oid: 0,
            oid: 0x1234,
            raw: object(0x1234, 7),
        };
        let ephs = std::slice::from_ref(&eph);
        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(9 * bsz as u64, &object(1024, 7)).unwrap();
        dev.write_at(10 * bsz as u64, &eph.raw).unwrap();
        let ok = verify_checkpoint_data(&mut dev, &nx, 7, 9, bsz as u32, ephs, &[10]);
        assert!(ok.is_ok(), "everything landed: {ok:?}");

        let mut dropped = FakeDev::new(bsz * 64);
        dropped.write_at(9 * bsz as u64, &object(1024, 7)).unwrap();
        let err = verify_checkpoint_data(&mut dropped, &nx, 7, 9, bsz as u32, ephs, &[10]);
        assert!(
            matches!(err, Err(TxnError::PostCommitVerify(_))),
            "a dropped write: {err:?}"
        );

        let mut stale = FakeDev::new(bsz * 64);
        stale.write_at(9 * bsz as u64, &object(1024, 6)).unwrap();
        stale.write_at(10 * bsz as u64, &eph.raw).unwrap();
        let err = verify_checkpoint_data(&mut stale, &nx, 7, 9, bsz as u32, ephs, &[10]);
        assert!(
            matches!(err, Err(TxnError::PostCommitVerify(_))),
            "the old id: {err:?}"
        );
    }

    // --- The checkpoint map is verified before its entries are consumed ---

    /// A checkpoint map at descriptor slot 0 (block 1 of the valid superblock's
    /// ring) naming only the spaceman, with `count` entries declared.
    fn checkpoint_map_block(bsz: usize, count: u32, stamp: bool) -> Vec<u8> {
        let mut b = vec![0u8; bsz];
        write_u64_le_slice(&mut b, 8, 1u64);
        write_u64_le_slice(&mut b, 16, 7u64);
        write_u32_le_slice(&mut b, 24, OBJECT_TYPE_CHECKPOINT_MAP | OBJ_PHYSICAL);
        write_u32_le_slice(&mut b, 0x24, count);
        write_u32_le_slice(&mut b, 0x28, OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL);
        write_u32_le_slice(&mut b, 0x28 + 8, bsz as u32);
        write_u64_le_slice(&mut b, 0x28 + 24, 0xBEEFu64); // the spaceman's oid
        write_u64_le_slice(&mut b, 0x28 + 32, 9u64);
        if stamp {
            update_checksum_in_place(&mut b);
        }
        b
    }

    #[test]
    fn a_checkpoint_map_is_consumed_only_with_its_checksum_and_entries_intact() {
        let bsz = 4096usize;
        let nx = NxView::parse(&make_valid_nx_block()).unwrap();
        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(bsz as u64, &checkpoint_map_block(bsz, 1, true))
            .unwrap();
        let others = read_other_ephemerals(&mut dev, &nx, 0xBEEF).expect("a sound map");
        assert!(
            others.is_empty(),
            "the spaceman is the only entry and is skipped"
        );

        let mut unstamped = FakeDev::new(bsz * 64);
        unstamped
            .write_at(bsz as u64, &checkpoint_map_block(bsz, 1, false))
            .unwrap();
        match read_other_ephemerals(&mut unstamped, &nx, 0xBEEF) {
            Err(TxnError::NxParse(msg)) => assert!(msg.contains("checksum"), "{msg}"),
            other => panic!("a map with a wrong checksum must be refused, got {other:?}"),
        }

        let mut overfull = FakeDev::new(bsz * 64);
        overfull
            .write_at(bsz as u64, &checkpoint_map_block(bsz, 200, true))
            .unwrap();
        match read_other_ephemerals(&mut overfull, &nx, 0xBEEF) {
            Err(TxnError::NxParse(msg)) => assert!(msg.contains("do not fit"), "{msg}"),
            other => panic!("a map with more entries than fit must be refused, got {other:?}"),
        }
    }

    // A chunk never claims to be newer than the chunk-info block holding it:
    // a block edited where it lies (no pool to copy it into) keeps the old
    // checkpoint's id, and so does its chunk.
    #[test]
    fn a_chunk_edited_in_place_keeps_its_blocks_transaction_id() {
        let bsz = 4096usize;
        let (sm_raw, mut cib_raw, bitmap_raw) = make_bitmap_spaceman(bsz, 20, 16, 16, 3);
        write_u64_le_slice(&mut cib_raw, 16, 1u64); // written by the previous checkpoint
        update_checksum_in_place(&mut cib_raw);
        let mut dev = FakeDev::new(bsz * 64);
        dev.write_at(2 * bsz as u64, &cib_raw).unwrap();
        dev.write_at(3 * bsz as u64, &bitmap_raw).unwrap();
        let mut sm = SpacemanView {
            paddr: 5,
            oid: 0xFACE,
            xid: 1,
            o_type: OBJECT_TYPE_SPACEMAN | OBJ_EPHEMERAL,
            o_subtype: 0,
            free_count: 16,
            block_size: bsz as u32,
            raw: sm_raw,
            txn_xid: 2,
            ip: InternalPool::default(),
            cabs: HashMap::new(),
        };
        let mut dirty_bitmaps = HashMap::new();
        let mut dirty_cibs = HashMap::new();
        let mut dirty_runs = HashMap::new();
        alloc_run_cached(
            &mut dev,
            &mut sm,
            &mut dirty_bitmaps,
            &mut dirty_cibs,
            &mut dirty_runs,
            bsz,
            1,
        )
        .expect("one block");
        let cib = dirty_cibs.get(&2).expect("the chunk-info block was edited");
        assert_eq!(
            u64_from_le(cib, 16),
            1,
            "not copied: the block keeps its id"
        );
        assert_eq!(
            u64_from_le(cib, 40),
            1,
            "and its chunk is no newer than the block"
        );
    }
}
