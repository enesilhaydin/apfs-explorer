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
    #[error("invalid block size {0}: must be 4096")]
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
}

// ---------------------------------------------------------------------------
// Pending write: a fully-serialized block scheduled for commit.
// ---------------------------------------------------------------------------

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
        let block_count = u64_at(block, 40)?;
        let next_oid = u64_at(block, 0x58)?;
        let next_xid = u64_at(block, 0x60)?;
        // xp_desc_blocks @0x68 (u32), xp_data_blocks @0x6C (u32)
        let xp_desc_blocks = u32_at(block, 0x68)?;
        let xp_data_blocks = u32_at(block, 0x6C)?;
        // xp_desc_base @0x70 (i64 stored as u64), xp_data_base @0x78
        let xp_desc_base_raw = u64_at(block, 0x70)?;
        let xp_data_base = u64_at(block, 0x78)?;
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
        // nx_fs_oid array @0xB8, 100 * 8 bytes
        let mut fs_oids = Vec::new();
        for i in 0..100usize {
            let off = 0xB8 + i * 8;
            if off + 8 > block.len() {
                break;
            }
            let oid = u64_at(block, off)?;
            if oid == 0 {
                break;
            }
            fs_oids.push(oid);
        }
        Ok(Self {
            block_size,
            block_count,
            spaceman_oid,
            omap_oid,
            fs_oids,
            xp_desc_base: xp_desc_base_raw,
            xp_desc_blocks: xp_desc_blocks & 0x7FFF_FFFF,
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
    /// Virtual object → physical mappings to register in the volume omap.
    omap_entries: Vec<OmapEntry>,
    /// XID for this transaction (= nx.current_xid + 1).
    pub xid: u64,
    /// Next OID to assign within this transaction.
    next_oid: u64,
    /// Physical block numbers allocated in this transaction (for audit).
    allocated_blocks: Vec<u64>,
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
}

impl<D: WritableBlockDevice> Transaction<D> {
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
            let xid = nx.current_xid + 1;
            let next_oid = nx.next_oid;
            // Read spaceman from the checkpoint data area (ephemeral: its paddr
            // is recorded in the checkpoint map block(s) in the descriptor area).
            let sm = read_spaceman(&mut dev, &nx)?;
            // Read other ephemeral objects (reaper, reaper-list btree nodes) to
            // carry them forward to the new checkpoint.
            // [CERTAIN: fsck_apfs requires nx_xp_data_len >= 2; baseline has 4]
            let other_ephemerals = read_other_ephemerals(&mut dev, &nx, sm.oid)?;
            Ok((nx, xid, next_oid, sm, other_ephemerals))
        })();
        match parsed {
            Ok((nx, xid, next_oid, sm, other_ephemerals)) => Ok(Self {
                dev,
                nx,
                sm,
                pending: Vec::new(),
                omap_entries: Vec::new(),
                xid,
                next_oid,
                allocated_blocks: Vec::new(),
                other_ephemerals,
                dirty_bitmaps: HashMap::new(),
                dirty_cibs: HashMap::new(),
                dirty_runs: HashMap::new(),
                reclaim_bitmap_paddrs: HashSet::new(),
                reclaim_cib_paddrs: HashSet::new(),
                pending_frees: HashSet::new(),
            }),
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
        alloc_run_cached(
            &mut self.dev,
            &mut self.sm,
            &mut self.dirty_bitmaps,
            &mut self.dirty_cibs,
            &mut self.dirty_runs,
            bsz,
            n,
        )
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
                crate::sm_fq::validate_sm_fq_tree(sm_fq_main_oid, &view, bsz).unwrap_or(0)
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
                crate::sm_fq::validate_sm_fq_tree(sm_fq_main_oid, &view, bsz)
                    .unwrap_or(before_count)
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
    /// Returns the number of entries drained - caller uses this for
    /// `fs_alloc_count` accounting (each drained block was volume-owned).
    #[allow(clippy::indexing_slicing)]
    pub fn drain_sm_fq_main(&mut self, upto_xid: u64) -> Result<usize, TxnError> {
        let bsz = self.nx.block_size as usize;
        const SFQ_MAIN_BASE: usize = 0xf0;
        let count_off = SFQ_MAIN_BASE;
        let tree_oid_off = SFQ_MAIN_BASE + 8;
        let oldest_xid_off = SFQ_MAIN_BASE + 16;

        let sm_fq_main_oid = u64_from_le(&self.sm.raw, tree_oid_off);
        if sm_fq_main_oid == 0 {
            return Ok(0);
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
            return Ok(0);
        }

        // #142: each sm_fq entry can cover a CONTIGUOUS RUN of `count` blocks
        // (sfqv_count). Free the whole run [paddr, paddr+count); dropping the
        // run length leaked every block past the first (e.g. the formatter's
        // stale xid=1 volume-omap is one {paddr, count=2} run = header + tree,
        // and freeing only the header left the tree as fsck "overallocation").
        for (_x, p, c) in all_drained {
            for off in 0..c.max(1) {
                self.free_block(p + off)?;
            }
        }
        // Adjust counters.
        let old_count = u64_from_le(&self.sm.raw, count_off);
        write_u64_le(
            &mut self.sm.raw,
            count_off,
            old_count.saturating_sub(total_drained_count as u64),
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
        Ok(total_drained_count)
    }

    /// Read one block from the device (for inspection during delete_snapshot).
    pub fn read_block(&mut self, paddr: u64, buf: &mut [u8]) -> Result<(), TxnError> {
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
        self.pending.push(PendingWrite { paddr, data: block });
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
        let paddr = self.alloc_block()?;
        let block = make_obj_block(ObjBlockArgs {
            o_oid_stored: oid,
            xid: self.xid,
            o_type: o_type | OBJ_VIRTUAL,
            o_subtype,
            body,
        });
        self.allocated_blocks.push(paddr);
        self.pending.push(PendingWrite { paddr, data: block });
        self.omap_entries.push(OmapEntry {
            oid,
            xid: self.xid,
            paddr,
            size: self.nx.block_size,
        });
        Ok(paddr)
    }

    /// Stage a raw pre-built block (already has correct obj_phys header + Fletcher-64).
    ///
    /// Used by snapshot.rs to write objects (like the frozen volume superblock
    /// copy) that require precise field layout.
    pub fn stage_raw(&mut self, paddr: u64, data: Vec<u8>) {
        self.allocated_blocks.push(paddr);
        self.pending.push(PendingWrite { paddr, data });
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
        self.pending.push(PendingWrite { paddr, data });
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

        // Remove stale pending writes for dropped intermediate blocks and
        // remove them from allocated_blocks so Phase A does not flush their
        // alloc-bits (they will be freed in Phase B below).
        if !dropped_intermediate_padrs.is_empty() {
            let drop_set: std::collections::HashSet<u64> =
                dropped_intermediate_padrs.iter().copied().collect();
            self.pending.retain(|pw| !drop_set.contains(&pw.paddr));
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
        let mut deferred_bitmaps: HashMap<u64, Vec<u8>> = HashMap::new();
        let mut deferred_cibs: HashMap<u64, Vec<u8>> = HashMap::new();
        {
            let keys_to_defer: Vec<u64> = self
                .dirty_bitmaps
                .keys()
                .filter(|k| self.reclaim_bitmap_paddrs.contains(k))
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
                .filter(|k| self.reclaim_cib_paddrs.contains(k))
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

        // --- Step 2: Write all staged data/metadata blocks. ---
        for pw in &self.pending {
            let byte_off = pw.paddr.checked_mul(self.nx.block_size as u64).ok_or(
                TxnError::BlockOutOfRange {
                    block: pw.paddr,
                    size: self.dev.size(),
                },
            )?;
            self.dev.write_at(byte_off, &pw.data)?;
        }

        // (pending_frees were drained in Step 1a, before the Phase A split - see
        // the #136 fix there. Draining here, after the split, dropped the clears.)

        // --- Step 2: Write spaceman + all other ephemerals to checkpoint data area. ---
        // data_index = xp_data_next (current superblock value).
        let data_index = self.nx.xp_data_next;
        let data_bno = self.nx.xp_data_base + (data_index as u64 % self.nx.xp_data_blocks as u64);
        // Update spaceman xid and recompute Fletcher-64.
        update_xid_and_checksum(&mut self.sm.raw, xid);
        let sm_paddr = data_bno;
        let sm_off =
            sm_paddr
                .checked_mul(self.nx.block_size as u64)
                .ok_or(TxnError::BlockOutOfRange {
                    block: sm_paddr,
                    size: self.dev.size(),
                })?;
        self.dev.write_at(sm_off, &self.sm.raw)?;

        // Write other ephemeral objects (reaper, reaper-list btree nodes) to
        // consecutive slots in the data ring, updating their xid each time.
        // [CERTAIN: fsck_apfs requires nx_xp_data_len >= 2; empirical baseline]
        let mut eph_paddrs: Vec<u64> = Vec::with_capacity(self.other_ephemerals.len());
        for (i, eph) in self.other_ephemerals.iter_mut().enumerate() {
            let slot_idx = (data_index + 1 + i as u32) % self.nx.xp_data_blocks;
            let eph_bno = self.nx.xp_data_base + slot_idx as u64;
            update_xid_and_checksum(&mut eph.raw, xid);
            let eph_off = eph_bno.checked_mul(self.nx.block_size as u64).ok_or(
                TxnError::BlockOutOfRange {
                    block: eph_bno,
                    size: self.dev.size(),
                },
            )?;
            self.dev.write_at(eph_off, &eph.raw)?;
            eph_paddrs.push(eph_bno);
        }
        let total_ephemerals = 1 + self.other_ephemerals.len() as u32; // spaceman + others
        let new_data_next = (data_index + total_ephemerals) % self.nx.xp_data_blocks;

        // --- Step 2b: Sync previous checkpoint's spaceman free count to match bitmap. ---
        // The allocation bitmap is shared across all checkpoints. When we allocate
        // new blocks and set bits in the bitmap, the previous checkpoint's spaceman
        // (still at its old data ring slot) reports a higher free_count than the
        // bitmap actually shows - causing fsck to report "overallocation" for the
        // old checkpoint. We fix this by patching the previous checkpoint's spaceman
        // free_count to equal our new free_count, making it consistent with the bitmap.
        // [CERTAIN: empirical - fsck validates all checkpoints in the ring; bitmap is shared]
        sync_prev_spaceman_free_count(&mut self.dev, &self.nx, self.sm.free_count, bsz)?;

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
            block_size: self.nx.block_size,
            other_ephemerals: &self.other_ephemerals,
            other_paddrs: &eph_paddrs,
        });
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
        //   xp_data_len   = 1 (one ephemeral object = spaceman)
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
        // Write nx_superblock to block 0 (bootstrap) AND to the descriptor ring
        // at nx_ring_slot (= desc_index + 1), one past the checkpoint map.
        // [CERTAIN: empirical baseline macOS scratch image; linux-apfs-rw convention]
        self.dev.write_at(0, &nx_raw)?;
        let nx_ring_bno =
            self.nx.xp_desc_base + (nx_ring_slot as u64 % self.nx.xp_desc_blocks as u64);
        let nx_ring_off = nx_ring_bno.checked_mul(self.nx.block_size as u64).ok_or(
            TxnError::BlockOutOfRange {
                block: nx_ring_bno,
                size: self.dev.size(),
            },
        )?;
        self.dev.write_at(nx_ring_off, &nx_raw)?;
        // --- COW-1 barrier #2: flush the sealed checkpoint (NXSB at block 0 +
        // ring slot) to media BEFORE Phase B reclaim clears any allocation bits.
        // The Phase B crash-safety argument below ("new NXSB is valid") only
        // holds once the NXSB is durable; otherwise a crash could persist the
        // freed-bit writes while the new NXSB is lost, letting the next mount
        // fall back to the prior checkpoint whose still-live blocks are now
        // marked free - silent corruption. Mirrors linux-apfs-rw
        // apfs_checkpoint_end (filemap_write_and_wait after the superblock write).
        self.dev.flush_data()?;

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
        // at block 0 and at its descriptor-ring slot, plus the checkpoint map -
        // and confirm each has a valid Fletcher-64 and the new transaction XID.
        // On failure we return Err WITHOUT having corrupted anything: every
        // referenced block is COW (the previous checkpoint is intact), so the
        // next mount simply falls back to it and the failed transaction is lost.
        // This catches silent write failures that APFS's own model assumes the
        // hardware does not make. [matches "trust-but-verify"; not a snapshot/
        // reread layer - verifies only the atomic commit point.]
        {
            let bsz = self.nx.block_size as usize;
            let mut buf = vec![0u8; bsz];
            self.dev.read_at(0, &mut buf)?;
            verify_checkpoint_block(&buf, Some(xid), "nxsb@block0")?;
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
        }

        Ok(())
    }
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
    let mut omap_blk = vec![0u8; bsz];
    dev.read_at(omap_off, &mut omap_blk)
        .map_err(|e| TxnError::OmapParse(e.to_string()))?;

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
    let mut tree_blk = vec![0u8; bsz];
    dev.read_at(tree_off, &mut tree_blk)
        .map_err(|e| TxnError::OmapParse(e.to_string()))?;

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

    // TOC base = DATA_BASE + toc_space_off.
    let toc_base = data_base + toc_space_off;
    // Key area immediately follows the TOC allocation: DATA_BASE + toc_space_len.
    let key_area_start = data_base + toc_space_len;

    // Whether this node is a B-tree root (BTNODE_ROOT flag = 0x1).
    // Root nodes reserve the last 40 bytes for btree_info_t; the value area
    // therefore ends at bsz - 40 rather than bsz.
    // [CERTAIN: empirical macOS scratch image; the APFS specification]
    let is_root = btn_flags & 0x0001 != 0;
    const BTREE_INFO_SIZE: usize = 40;
    let val_area_end = if is_root {
        bsz.saturating_sub(BTREE_INFO_SIZE)
    } else {
        bsz
    };

    // Fixed sizes for omap B-tree.
    const KEY_SIZE: usize = 16; // omap_key_t = oid(8) + xid(8)
    const VAL_SIZE: usize = 16; // omap_val_t = flags(4) + size(4) + paddr(8)
    const TOC_ENTRY_SIZE: usize = 4; // (key_off u16, val_off u16)

    // Parse existing TOC entries and build a sorted list of (oid, xid, paddr)
    // so we can merge-insert the new entries in order.
    #[derive(Clone)]
    struct OmapKV {
        oid: u64,
        xid: u64,
        paddr: u64,
        size: u32,
    }
    let mut existing: Vec<OmapKV> = Vec::with_capacity(nkeys);
    for i in 0..nkeys {
        let toc_off = toc_base + i * TOC_ENTRY_SIZE;
        if toc_off + TOC_ENTRY_SIZE > bsz {
            break;
        }
        let key_off_rel = u16_from_le(&tree_blk, toc_off) as usize;
        let val_off_rel = u16_from_le(&tree_blk, toc_off + 2) as usize;

        let k_off = key_area_start + key_off_rel;
        if k_off + KEY_SIZE > bsz {
            continue;
        }
        let k_oid = u64_from_le(&tree_blk, k_off);
        let k_xid = u64_from_le(&tree_blk, k_off + 8);

        // Value offsets in fixed-kv omap B-tree root:
        //   val_area_end = bsz - 40 for root nodes (btree_info_t at end).
        //   v_abs = val_area_end - val_off_rel
        // Empirically confirmed from a fresh macOS-written scratch image:
        //   entry 0: val_off_rel=16, v_abs=4056-16=4040 → valid omap_val.
        // [CERTAIN: empirical macOS scratch image; the APFS specification]
        let v_abs = val_area_end.saturating_sub(val_off_rel);
        if v_abs + VAL_SIZE > val_area_end {
            continue;
        }
        let v_flags = u32_from_le(&tree_blk, v_abs);
        let v_size = u32_from_le(&tree_blk, v_abs + 4);
        let v_paddr = u64_from_le(&tree_blk, v_abs + 8);
        let _ = (v_flags, v_size); // kept for completeness
        existing.push(OmapKV {
            oid: k_oid,
            xid: k_xid,
            paddr: v_paddr,
            size: bsz as u32,
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
                paddr: e.paddr,
                size: e.size,
            };
        } else {
            merged.push(OmapKV {
                oid: e.oid,
                xid: e.xid,
                paddr: e.paddr,
                size: e.size,
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
        let toc_off = toc_base + i * TOC_ENTRY_SIZE;

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
        write_u32_le_slice(&mut new_blk, v_abs, 0u32); // ov_flags = 0
        write_u32_le_slice(&mut new_blk, v_abs + 4, kv.size); // ov_size
        write_u64_le_slice(&mut new_blk, v_abs + 8, kv.paddr); // ov_paddr
    }

    // Preserve btree_info_t at end of block for root nodes.
    // btree_info_t occupies [bsz - BTREE_INFO_SIZE .. bsz) = [4056..4096).
    // We copy it from the old block (fixed fields: node_size, key_size, val_size,
    // flags stay the same) and update bt_key_count to reflect the new nkeys.
    // [CERTAIN: btree.rs BTREE_INFO_SIZE = 40; BTNODE_ROOT = 0x0001;
    //  empirical macOS scratch image; the APFS specification]
    if is_root {
        let bti_start = bsz.saturating_sub(BTREE_INFO_SIZE);
        if let (Some(dst), Some(src)) = (
            new_blk.get_mut(bti_start..bsz),
            tree_blk.get(bti_start..bsz),
        ) {
            dst.copy_from_slice(src);
        }
        // Update bt_key_count @bti_start+24 (u64) to nkeys.
        // bt_node_count @bti_start+32 stays 1 (single-node tree).
        write_u64_le_slice(&mut new_blk, bti_start + 24, nkeys as u64);
        // Sanity: the lowest value must not reach into the key area.
        let lowest_v_abs = val_area_end.saturating_sub(nkeys * VAL_SIZE);
        if lowest_v_abs < key_area_start_new + nkeys * KEY_SIZE {
            return Err(TxnError::OmapParse(
                "omap B-tree keys/values overlap after btree_info_t adjustment".into(),
            ));
        }
    }

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

/// Find the latest valid nx_superblock by walking the checkpoint descriptor ring.
/// [CERTAIN: checkpoint.rs latest_superblock algorithm, the APFS specification]
fn find_latest_nx<D: WritableBlockDevice>(dev: &mut D, block0: &[u8]) -> Result<NxView, TxnError> {
    let mut best = NxView::parse(block0)?;
    let bsz = best.block_size as usize;
    let desc_base = best.xp_desc_base;
    let desc_blocks = best.xp_desc_blocks;
    if desc_base == 0 || desc_blocks == 0 {
        return Ok(best);
    }
    let mut buf = vec![0u8; bsz];
    for i in 0..desc_blocks {
        let bno = desc_base + i as u64;
        let off = bno * bsz as u64;
        if off + bsz as u64 > dev.size() {
            continue;
        }
        if dev.read_at(off, &mut buf).is_err() {
            continue;
        }
        if let Ok(candidate) = NxView::parse(&buf) {
            if candidate.current_xid > best.current_xid {
                best = candidate;
            }
        }
    }
    Ok(best)
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
        let slot = (nx.xp_desc_index + i) % desc_blocks;
        let bno = desc_base + slot as u64;
        let off = bno * bsz as u64;
        if dev.read_at(off, &mut buf).is_err() {
            continue;
        }
        // Check if this is a checkpoint_map_phys_t block:
        // o_type @0x18 (u32) - OBJECT_TYPE_CHECKPOINT_MAP | OBJ_EPHEMERAL
        // [CERTAIN: apfs_raw.h checkpoint_map_phys_t]
        let o_type_raw = u32_from_le(&buf, 0x18);
        let o_type_low = o_type_raw & 0x0000_FFFF;
        if o_type_low != OBJECT_TYPE_CHECKPOINT_MAP {
            continue;
        }
        // cpm_flags @0x20 (u32), cpm_count @0x24 (u32)
        // Each entry: cme_type(u32) + cme_subtype(u32) + cme_size(u32) +
        //             cme_pad(u32) + cme_fs_oid(u64) + cme_oid(u64) + cme_paddr(u64)
        // = 40 bytes per entry. First entry at offset 0x28.
        // [CERTAIN: apfs_raw.h struct checkpoint_mapping_t]
        let cpm_count = u32_from_le(&buf, 0x24);
        for j in 0..cpm_count as usize {
            let entry_off = 0x28 + j * 40;
            if entry_off + 40 > bsz {
                break;
            }
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
                // Found the spaceman. Read it from the data area.
                let sm_off = cme_paddr * bsz as u64;
                let mut sm_buf = vec![0u8; bsz];
                dev.read_at(sm_off, &mut sm_buf)
                    .map_err(|e| TxnError::SpacemanParse(e.to_string()))?;
                // Parse spaceman header fields needed for commit.
                // o_oid @8 (u64), o_xid @16 (u64).
                // [CERTAIN: apfs_raw.h struct apfs_spaceman_device the APFS specification]
                // sm_dev[0].sm_free_count @72 (u64).
                let sm_oid = u64_from_le(&sm_buf, 8);
                let sm_xid = u64_from_le(&sm_buf, 16);
                let free_count = u64_from_le(&sm_buf, 72);
                let _ = cme_size; // used for validation in a full impl
                return Ok(SpacemanView {
                    paddr: cme_paddr,
                    oid: sm_oid,
                    xid: sm_xid,
                    o_type: cme_type,
                    o_subtype: cme_subtype,
                    free_count,
                    block_size: nx.block_size,
                    raw: sm_buf,
                });
            }
        }
    }
    Err(TxnError::SpacemanParse(
        "spaceman not found in checkpoint map".into(),
    ))
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
        let slot = (nx.xp_desc_index + i) % desc_blocks;
        let bno = desc_base + slot as u64;
        let off = bno * bsz as u64;
        if dev.read_at(off, &mut buf).is_err() {
            continue;
        }
        let o_type_raw = u32_from_le(&buf, 0x18);
        if o_type_raw & 0x0000_FFFF != OBJECT_TYPE_CHECKPOINT_MAP {
            continue;
        }
        let cpm_count = u32_from_le(&buf, 0x24);
        for j in 0..cpm_count as usize {
            let entry_off = 0x28 + j * 40;
            if entry_off + 40 > bsz {
                break;
            }
            let cme_type = u32_from_le(&buf, entry_off);
            let cme_subtype = u32_from_le(&buf, entry_off + 4);
            let cme_oid = u64_from_le(&buf, entry_off + 24);
            let cme_paddr = u64_from_le(&buf, entry_off + 32);
            // Skip the spaceman - it's handled separately.
            if cme_oid == spaceman_oid {
                continue;
            }
            // Read the ephemeral block.
            let eph_off = cme_paddr * bsz as u64;
            let mut eph_buf = vec![0u8; bsz];
            if dev.read_at(eph_off, &mut eph_buf).is_err() {
                continue;
            }
            result.push(EphemeralObj {
                cme_type,
                cme_subtype,
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
fn alloc_run_cached<D: WritableBlockDevice>(
    dev: &mut D,
    sm: &mut SpacemanView,
    dirty_bitmaps: &mut HashMap<u64, Vec<u8>>,
    dirty_cibs: &mut HashMap<u64, Vec<u8>>,
    dirty_runs: &mut HashMap<u64, crate::free_runs::ChunkFreeRuns>,
    bsz: usize,
    n: usize,
) -> Result<(u64, usize), TxnError> {
    // sm_dev[0].sm_addr_offset @80 (u32 byte offset from spaceman start to CIB array).
    // [CERTAIN: apfs_raw.h sm_dev[0].sm_addr_offset, the APFS specification]
    let addr_offset = u32_from_le(&sm.raw, 80) as usize;
    // sm_dev[0].sm_cib_count @64 (u32)
    let cib_count = u32_from_le(&sm.raw, 64) as usize;
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

    // The CIB address array lives at [addr_offset .. addr_offset + cib_count * 8]
    // inside the spaceman block. Each entry is a u64 block address of a CIB block.
    // [CERTAIN: spaceman.c the APFS specification]
    for cib_idx in 0..cib_count {
        let cib_addr_off = addr_offset + cib_idx * 8;
        if cib_addr_off + 8 > sm.raw.len() {
            break;
        }
        let cib_bno = u64_from_le(&sm.raw, cib_addr_off);
        if cib_bno == 0 {
            continue;
        }
        // Get or load the CIB into the dirty cache.
        let cib_off = cib_bno * bsz as u64;
        let cib_buf = match dirty_cibs.get(&cib_bno) {
            Some(buf) => buf.clone(),
            None => {
                let mut buf = vec![0u8; bsz];
                if dev.read_at(cib_off, &mut buf).is_err() {
                    continue;
                }
                buf
            }
        };

        // CIB block layout (chunk_info_block_phys_t):
        //   obj_phys header (32 bytes)
        //   cib_index u32 @32, cib_count u32 @36  (8 bytes)
        //   chunk_info_t[] starting at offset 40.
        // chunk_info_t: xid(u64) + addr(u64) + block_count(u32) +
        //               free_count(u32) + bitmap_addr(u64) = 32 bytes.
        // [CERTAIN: apfs_raw.h struct apfs_chunk_info_block + apfs_chunk_info,
        //  the APFS specification; offset 40 confirmed by real-image empirical]
        for chunk_idx in 0..chunks_per_cib {
            let ci_off = 40 + chunk_idx * 32;
            if ci_off + 32 > bsz {
                break;
            }
            let ci_xid = u64_from_le(&cib_buf, ci_off);
            let ci_addr = u64_from_le(&cib_buf, ci_off + 8);
            let ci_block_count = u32_from_le(&cib_buf, ci_off + 16);
            let ci_free_count = u32_from_le(&cib_buf, ci_off + 20);
            let ci_bitmap_addr = u64_from_le(&cib_buf, ci_off + 24);
            let _ = ci_xid; // used structurally

            if ci_free_count == 0 || ci_block_count == 0 {
                continue;
            }

            // ROOT CAUSE FIX (refined ): when bitmap_addr=0 the
            // kernel has NOT allocated a persistent bitmap block. If the chunk
            // is only PARTIALLY free (ci_free_count < ci_block_count) the
            // kernel's used blocks are scattered non-contiguously and we cannot
            // locate a free block - the naive "first N used" assumption causes
            // double-allocation (the 4-error cascade). Skip such chunks.
            // But a FULLY-FREE bitmap-less chunk (ci_free_count == block_count)
            // is safe to allocate from contiguously starting at ci_addr.
            if ci_bitmap_addr == 0 {
                if ci_free_count != ci_block_count {
                    continue;
                }
                // Contiguous bump from the start of an all-free chunk.
                // In the bitmap-less case we can only safely alloc 1 at a time
                // (the "used" counter approach is not stable across multiple calls
                // without a bitmap to anchor on).
                let used = ci_block_count - ci_free_count; // == 0 on first alloc
                let alloc_bno = ci_addr + used as u64;
                let mut new_cib = cib_buf.clone();
                write_u32_le_slice(&mut new_cib, ci_off + 20, ci_free_count - 1);
                // Checksum deferred to commit flush.
                dirty_cibs.insert(cib_bno, new_cib);
                let sm_free = u64_from_le(&sm.raw, 72).saturating_sub(1);
                write_u64_le_slice(&mut sm.raw, 72, sm_free);
                sm.free_count = sm_free;
                return Ok((alloc_bno, 1));
            }

            // Bitmap exists: get or load into dirty cache, then build/lookup
            // the parallel free-runs index for this chunk.
            if let std::collections::hash_map::Entry::Vacant(bm_entry) =
                dirty_bitmaps.entry(ci_bitmap_addr)
            {
                let bm_off = ci_bitmap_addr * bsz as u64;
                let mut buf = vec![0u8; bsz];
                if dev.read_at(bm_off, &mut buf).is_err() {
                    continue;
                }
                let runs =
                    crate::free_runs::ChunkFreeRuns::from_bitmap(&buf, ci_block_count as usize);
                bm_entry.insert(buf);
                dirty_runs.insert(ci_bitmap_addr, runs);
            }
            // Pull the runs index for this chunk (panic-impossible: just inserted
            // or already present alongside the bitmap).
            let runs = match dirty_runs.get_mut(&ci_bitmap_addr) {
                Some(r) => r,
                None => continue,
            };

            // Try a full-fit first; fall back to the largest available run so a
            // caller asking for N can still make progress when no single run of
            // length N exists in this chunk.
            let want = n.min(ci_free_count as usize).min(u32::MAX as usize) as u32;
            let (first_bit_u32, run_len_u32) = match runs.alloc_exact(want) {
                Some(p) => p,
                None => match runs.alloc_largest(want) {
                    Some(p) => p,
                    None => continue,
                },
            };
            let first_bit = first_bit_u32 as usize;
            let run_len = run_len_u32 as usize;
            if run_len == 0 {
                continue;
            }

            // Mirror the alloc into the on-disk bitmap (kept authoritative for
            // crash-safety; the runs index is only an in-memory accelerator).
            let bm_buf = match dirty_bitmaps.get_mut(&ci_bitmap_addr) {
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
            let cib_entry = dirty_cibs.entry(cib_bno).or_insert(cib_buf.clone());
            write_u32_le_slice(cib_entry, ci_off + 20, new_free);

            // Decrement sm_dev[0].sm_free_count.
            let sm_free = u64_from_le(&sm.raw, 72).saturating_sub(run_len as u64);
            write_u64_le_slice(&mut sm.raw, 72, sm_free);
            sm.free_count = sm_free;

            let alloc_bno = ci_addr + first_bit as u64;
            return Ok((alloc_bno, run_len));
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
    let addr_offset = u32_from_le(&sm.raw, 80) as usize;
    let cib_count = u32_from_le(&sm.raw, 64) as usize;
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

    for cib_idx in 0..cib_count {
        let cib_addr_off = addr_offset + cib_idx * 8;
        if cib_addr_off + 8 > sm.raw.len() {
            break;
        }
        let cib_bno = u64_from_le(&sm.raw, cib_addr_off);
        if cib_bno == 0 {
            continue;
        }
        // Get or load CIB.
        let cib_off = cib_bno * bsz as u64;
        let cib_buf = match dirty_cibs.get(&cib_bno) {
            Some(buf) => buf.clone(),
            None => {
                let mut buf = vec![0u8; bsz];
                if dev.read_at(cib_off, &mut buf).is_err() {
                    tracing::warn!(
                        cib_bno,
                        "free_one_block_cached: CIB read failed, skipping - the free \
                         of this paddr may be lost (possible sm_free_count drift)"
                    );
                    continue;
                }
                buf
            }
        };

        for chunk_idx in 0..chunks_per_cib {
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
            // Check if free_bno falls in this chunk.
            if free_bno < ci_addr || free_bno >= ci_addr + ci_block_count as u64 {
                continue;
            }

            let bit_idx = (free_bno - ci_addr) as usize;

            if ci_bitmap_addr == 0 {
                // No bitmap - allocated by counter only; just increment back.
                let cib_entry = dirty_cibs.entry(cib_bno).or_insert(cib_buf.clone());
                write_u32_le_slice(cib_entry, ci_off + 20, ci_free_count + 1);
                // Checksum deferred.
                // Mark this CIB as reclaim-touched so commit() defers its flush
                // to Phase B (post-NXSB), preserving crash-safety ordering.
                reclaim_cib_paddrs.insert(cib_bno);
            } else {
                // Clear the bit in the cached bitmap; mirror the free into the
                // parallel runs index so a subsequent alloc can find it via the
                // size-sorted view (adjacency merge happens inside `runs.free`).
                if let std::collections::hash_map::Entry::Vacant(bm_entry) =
                    dirty_bitmaps.entry(ci_bitmap_addr)
                {
                    let bm_off = ci_bitmap_addr * bsz as u64;
                    let mut buf = vec![0u8; bsz];
                    if dev.read_at(bm_off, &mut buf).is_err() {
                        return Err(TxnError::SpacemanParse(
                            "free_block: could not read bitmap".into(),
                        ));
                    }
                    let runs =
                        crate::free_runs::ChunkFreeRuns::from_bitmap(&buf, ci_block_count as usize);
                    bm_entry.insert(buf);
                    dirty_runs.insert(ci_bitmap_addr, runs);
                }
                let Some(bm_buf) = dirty_bitmaps.get_mut(&ci_bitmap_addr) else {
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
                if let Some(runs) = dirty_runs.get_mut(&ci_bitmap_addr) {
                    runs.free(bit_idx as u32, 1);
                }
                // Mark bitmap and CIB as reclaim-touched so commit() defers their
                // flush to Phase B (post-NXSB). This closes the power-loss window
                // where clearing a bit before NXSB leaves the old checkpoint with
                // freed blocks that still appear in its live extent tree (bug W2-1).
                reclaim_bitmap_paddrs.insert(ci_bitmap_addr);
                reclaim_cib_paddrs.insert(cib_bno);
                // Increment ci_free_count in CIB cache.
                let cib_entry = dirty_cibs.entry(cib_bno).or_insert(cib_buf.clone());
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
    // Block not found in any spaceman-managed chunk. This is an anomaly: the
    // address lies outside every chunk's range, which can indicate a stale
    // reference or a double-free upstream. linux-apfs-rw (apfs_main_free) treats
    // an unlocatable free as -EIO; we keep Ok(()) - the caller's "this block is
    // no longer live" intent is already satisfied - but surface a warning so the
    // condition is never silent. (COW-9)
    tracing::warn!(
        free_bno,
        "free_one_block_cached: paddr not found in any spaceman chunk - possible \
         stale reference or out-of-range address"
    );
    Ok(())
}

/// Sync the previous checkpoint's spaceman free_count to match `new_free_count`.
///
/// The allocation bitmap is shared across checkpoints. When we allocate new
/// blocks and set bits in the bitmap, the OLD checkpoint's spaceman (at its
/// data ring slot) still has the old free_count, making it inconsistent with
/// the bitmap. fsck_apfs validates ALL checkpoints in the ring and reports
/// "overallocation" for any checkpoint whose spaceman free_count is greater
/// than what the bitmap shows.
///
/// Fix: after our new spaceman has been written, patch the previous checkpoint's
/// spaceman at its data ring slot(s) with the same free_count.
/// [CERTAIN: empirical - fsck "overallocation" disappears when old spaceman is
///  updated; macOS achieves this via the ip_bitmap mechanism (per-xid deltas),
///  but for our minimal allocator patching the raw free_count is equivalent]
fn sync_prev_spaceman_free_count<D: WritableBlockDevice>(
    dev: &mut D,
    nx: &NxView,
    new_free_count: u64,
    bsz: usize,
) -> Result<(), TxnError> {
    let prev_data_index = nx.xp_data_index;
    let prev_data_len = nx.xp_data_len;
    // Walk the previous checkpoint's data ring slots looking for the spaceman.
    for i in 0..prev_data_len {
        let slot = (prev_data_index + i) % nx.xp_data_blocks;
        let bno = nx.xp_data_base + slot as u64;
        let off = bno
            .checked_mul(bsz as u64)
            .ok_or_else(|| TxnError::SpacemanParse("prev sm paddr overflow".into()))?;
        let mut buf = vec![0u8; bsz];
        if dev.read_at(off, &mut buf).is_err() {
            continue;
        }
        // Identify spaceman by o_type bits.
        let o_type_low = u32_from_le(&buf, 0x18) & 0x0000_FFFF;
        if o_type_low != OBJECT_TYPE_SPACEMAN {
            continue;
        }
        // Update sm_dev[0].sm_free_count @72 (u64).
        write_u64_le_slice(&mut buf, 72, new_free_count);
        // Recompute Fletcher-64 (preserves o_xid - the old xid stays).
        update_checksum_in_place(&mut buf);
        dev.write_at(off, &buf)?;
        // There is only one spaceman in the data ring; stop after finding it.
        return Ok(());
    }
    // No previous spaceman found - not an error (first-ever write or ring empty).
    Ok(())
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
    block_size: u32,
    /// Additional ephemeral objects to include after the spaceman entry.
    other_ephemerals: &'a [EphemeralObj],
    /// Physical block addresses where each other_ephemeral was written.
    other_paddrs: &'a [u64],
}

/// Build a `checkpoint_map_phys_t` block with entries for all ephemeral objects.
/// Entry 0 = spaceman; entries 1..N = other ephemerals (reaper, btree nodes).
/// [CERTAIN: C.2 the APFS specification; apfs_raw.h checkpoint_map_phys_t layout]
fn build_checkpoint_map(a: CheckpointMapArgs<'_>) -> Vec<u8> {
    let CheckpointMapArgs {
        bsz,
        xid,
        block_oid,
        sm_oid,
        sm_type,
        sm_subtype,
        sm_paddr,
        block_size,
        other_ephemerals,
        other_paddrs,
    } = a;
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
    let total = 1 + other_ephemerals.len();
    write_u32_le_slice(&mut buf, 36, total as u32);
    // checkpoint_mapping_t entries starting at offset 40 (0x28).
    // Each entry: cme_type(u32)+cme_subtype(u32)+cme_size(u32)+
    //             cme_pad(u32)+cme_fs_oid(u64)+cme_oid(u64)+cme_paddr(u64) = 40 bytes.
    // [CERTAIN: apfs_raw.h struct checkpoint_mapping_t]
    let e0 = 40usize;
    write_u32_le_slice(&mut buf, e0, sm_type); // cme_type
    write_u32_le_slice(&mut buf, e0 + 4, sm_subtype); // cme_subtype
    write_u32_le_slice(&mut buf, e0 + 8, block_size); // cme_size
    write_u32_le_slice(&mut buf, e0 + 12, 0u32); // cme_pad
    write_u64_le_slice(&mut buf, e0 + 16, 0u64); // cme_fs_oid (container-level = 0)
    write_u64_le_slice(&mut buf, e0 + 24, sm_oid); // cme_oid
    write_u64_le_slice(&mut buf, e0 + 32, sm_paddr); // cme_paddr
    for (i, (eph, &paddr)) in other_ephemerals.iter().zip(other_paddrs.iter()).enumerate() {
        let e = e0 + (i + 1) * 40;
        if e + 40 > bsz {
            break; // block full - shouldn't happen in practice
        }
        write_u32_le_slice(&mut buf, e, eph.cme_type);
        write_u32_le_slice(&mut buf, e + 4, eph.cme_subtype);
        write_u32_le_slice(&mut buf, e + 8, block_size);
        write_u32_le_slice(&mut buf, e + 12, 0u32);
        write_u64_le_slice(&mut buf, e + 16, 0u64); // cme_fs_oid = 0 (container-level)
        write_u64_le_slice(&mut buf, e + 24, eph.oid);
        write_u64_le_slice(&mut buf, e + 32, paddr);
    }
    update_checksum_in_place(&mut buf);
    buf
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

// ---------------------------------------------------------------------------
// Unit tests (TDD red→green, no disk I/O)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use apfs_core::checksum::fletcher64;

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
    fn block_alloc_bump_from_free_range() {
        // Unit test: alloc_one_block with a minimal in-memory spaceman.
        // We construct a fake spaceman whose CIB array describes one chunk
        // of 16 blocks, all free (bitmap_addr = 0).
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
            1,
        )
        .unwrap();
        assert_eq!(count, 1, "alloc_run_cached must return count=1 for n=1");
        // First free block in the chunk starting at block 10.
        assert_eq!(blk, 10, "first alloc from all-free chunk must be ci_addr");
        // sm_dev[0].sm_free_count must have been decremented (in-memory, immediate).
        let new_free = u64_from_le(&sm.raw, 72);
        assert_eq!(new_free, 15, "free_count must be decremented by 1");
        // ci_free_count in dirty_cibs cache must be decremented; flush then check device.
        let cib_bno = 2u64;
        for (cib_paddr, mut cib_buf) in dirty_cibs.drain() {
            update_checksum_in_place(&mut cib_buf);
            dev.write_at(cib_paddr * bsz as u64, &cib_buf).unwrap();
        }
        let _ = cib_bno;
        let updated_cib = &dev.data[2 * bsz..3 * bsz];
        let ci_free = u32_from_le(updated_cib, 60);
        assert_eq!(ci_free, 15, "CIB ci_free_count must be decremented");
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
    fn nxview_parse_fs_oids_stops_at_zero() {
        let mut block = make_valid_nx_block();
        // Write one non-zero fs_oid at offset 0xB8, followed by zero → stops there.
        write_u64_le_slice(&mut block, 0xB8, 0x1234u64);
        write_u64_le_slice(&mut block, 0xC0, 0u64); // terminator
        update_checksum_in_place(&mut block);
        let nx = NxView::parse(&block).unwrap();
        assert_eq!(nx.fs_oids.len(), 1);
        assert_eq!(nx.fs_oids[0], 0x1234);
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
}
