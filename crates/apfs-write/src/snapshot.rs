//! Snapshot create and delete: staged record sets on a `Transaction`.
//!
//! `create_snapshot` stages exactly the record set from `the APFS specification`
//! (Q3/Q4/Q5/Q6 locked decisions). `delete_snapshot` stages the synchronous
//! delete operations per Q6.
//!
//! Spec references (all locked, the APFS specification):
//!   F.1  Snapshot metadata tree records (SNAP_METADATA + SNAP_NAME)
//!   F.2  Frozen volume superblock copy (physical object, sblock_oid)
//!   F.3  Extent-reference tree (Q3: current OID to snapshot; new empty to live)
//!   F.4  snap_meta_ext: SKIP per Q4 (leave apfs_snap_meta_ext_oid untouched)
//!   F.5  Volume superblock field updates
//!   G.   Snapshot delete mechanics (Q6 synchronous subset only)
//!   I.   Volume superblock snapshot-relevant field offsets

use crate::txn::{
    update_checksum_in_place, Transaction, TxnError, OBJECT_TYPE_BLOCKREFTREE, OBJECT_TYPE_BTREE,
    OBJECT_TYPE_FS, OBJECT_TYPE_OMAP_SNAPSHOT, OBJECT_TYPE_SNAPMETATREE, OBJ_PHYSICAL,
};
use apfs_core::block_device::WritableBlockDevice;

// ---------------------------------------------------------------------------
// APFS key type constants used in the snapshot metadata B-tree.
// [CERTAIN: apfs_raw.h the APFS specification]
// ---------------------------------------------------------------------------

/// Confirmed (the APFS specification p.84): the j_obj_types
/// enum assigns SNAP_METADATA=1 and SNAP_NAME=11 (decimal). Earlier rounds
/// hallucinated 0xB and 0xC - those are actually SNAP_NAME and SIBLING_MAP.
/// This constant error caused fsck to identify TOC\[0\] (our SNAP_METADATA)
/// as a SNAP_NAME record and reject its key/val sizes - the root cause of
/// "error: snapshot name (id 3): invalid key length (8) / key size (8)/val
/// size (59) is invalid" that survived rounds 1-9.
pub const APFS_TYPE_SNAP_METADATA: u64 = 1;
pub const APFS_TYPE_SNAP_NAME: u64 = 11;

/// Object identifier used in the `j_key_t` header of a SNAP_NAME record.
/// Apple APFS Reference PAGE 119 (`the APFS specification`):
///     "The object identifier in the header is always ~0ULL."
/// The type field consumes bits 60-63; obj_id is bits 0-59, so the encoded
/// value of `~0ULL` masked to 60 bits is `0x0FFF_FFFF_FFFF_FFFF`.
/// [CERTAIN: Apple File System Reference PAGE 119, the APFS specification]
pub const SNAP_NAME_OBJ_ID: u64 = 0x0FFF_FFFF_FFFF_FFFF;

/// Default `bt_flags` for a freshly authored snap_meta tree root, used as a
/// last-resort fallback if reading the existing root's footer fails. The
/// correct value is volume-specific and MUST be preserved from the existing
/// snap_meta tree root in `vsb.snap_meta_tree_oid`. 0x52 = BTREE_PHYSICAL(0x10)
/// | BTREE_KV_NONALIGNED(0x40) | BTREE_SEQUENTIAL_INSERT(0x02) was observed on
/// macOS-formatted APFS volumes (APFS 1, no hashed-name flag).
/// [CERTAIN: the APFS specification p.129 flag definitions; volume-empirical observation]
pub const DEFAULT_SNAP_META_BT_FLAGS: u32 = 0x0000_0052;

/// Encode an APFS `j_key_t` obj_id_and_type field.
/// Type goes into bits 60-63 (upper 4 bits), obj_id in bits 0-59.
/// [CERTAIN: apfs_raw.h j_key_t, the APFS specification]
fn encode_jkey(obj_id: u64, key_type: u64) -> u64 {
    (key_type << 60) | (obj_id & 0x0FFF_FFFF_FFFF_FFFF)
}

// ---------------------------------------------------------------------------
// Volume superblock field offsets (the APFS specification, section I).
// [CERTAIN: apfs_raw.h the APFS specification]
// ---------------------------------------------------------------------------

const VSBI_FS_ALLOC_COUNT: usize = 0x58;
const VSBI_ROOT_TREE_OID: usize = 0x88;
const VSBI_OMAP_OID: usize = 0x80;
const VSBI_EXTENTREF_TREE_OID: usize = 0x90;
const VSBI_SNAP_META_TREE_OID: usize = 0x98;
const VSBI_NEXT_OBJ_ID: usize = 0xB0;
/// One volume object-map mapping: `(oid, xid, paddr)`.
use crate::file::OmapRec;
use crate::phys_tree;

/// One volume object map mapping, flags and size carried as stored.
type OmapTriple = OmapRec;

/// A catalog key's `obj_id_and_type` carries the type in the top byte.
const OBJ_ID_MASK: u64 = 0x00FF_FFFF_FFFF_FFFF;
/// A `j_phys_ext_val`'s `len_and_kind` carries the kind in the top nibble.
const PHYS_EXT_LEN_MASK: u64 = 0x0FFF_FFFF_FFFF_FFFF;
const VSBI_NUM_FILES: usize = 0xB8;
const VSBI_NUM_DIRECTORIES: usize = 0xC0;
const VSBI_NUM_SYMLINKS: usize = 0xC8;
const VSBI_NUM_OTHER_FSOBJECTS: usize = 0xD0;
const VSBI_NUM_SNAPSHOTS: usize = 0xD8;
const VSBI_LAST_MOD_TIME: usize = 0x100;
/// `apfs_extentref_tree_type @ 0x78` - the u32 o_type+o_subtype of the
/// extent-ref B-tree, stored independently of the OID. Kernel emits
/// 0x40000002 (OBJ_PHYSICAL | OBJECT_TYPE_BTREE) on macOS 15. Apple's
/// j_snap_metadata_val.extentref_tree_type copies this value verbatim
/// (NOT the subtype 0xF=BLOCKREFTREE).
/// [CERTAIN: empirical macOS 15.7.4 kernel-format scratch; the APFS specification p.117]
const VSBI_EXTENTREF_TREE_TYPE: usize = 0x78;

// omap_phys fields relative to block start.
// [CERTAIN: apfs_raw.h struct apfs_omap_phys, the APFS specification]
const OMAP_SNAP_COUNT_OFF: usize = 36;
const OMAP_SNAPSHOT_TREE_OID_OFF: usize = 56;
const OMAP_MOST_RECENT_SNAP_OFF: usize = 64;

// ---------------------------------------------------------------------------
// create_snapshot
// ---------------------------------------------------------------------------

/// Stage a snapshot-create onto `txn`.
///
/// Implements the full record set from the APFS specification F.1-F.5:
///
/// 1. Allocate a block for the frozen volume superblock copy (F.2).
///    Physical object: o_oid = paddr, o_type = PHYSICAL | OBJECT_TYPE_FS.
///    Copy current vsb raw; zero apfs_omap_oid, apfs_extentref_tree_oid,
///    apfs_snap_meta_tree_oid in the copy. sblock_oid = that paddr.
///
/// 2. Allocate a block for the new empty extentref tree root (Q3, F.3).
///    Physical B-tree root, type BLOCKREFTREE, zero records.
///    Live volume's apfs_extentref_tree_oid updated to new OID.
///    Snapshot metadata records the OLD apfs_extentref_tree_oid.
///
/// 3. Insert SNAP_METADATA + SNAP_NAME records into the snap meta tree (F.1).
///
/// 4. Update live volume superblock fields (F.5):
///    apfs_num_snapshots += 1, apfs_last_mod_time = now,
///    apfs_extentref_tree_oid = new_extentref_paddr (Q3).
///    Q5: leave apfs_revert_to_xid / apfs_revert_to_sblock_oid untouched (= 0).
///    Q4: leave apfs_snap_meta_ext_oid untouched.
///
/// 5. Update volume omap: om_snap_count += 1, om_most_recent_snap = snap_xid.
///
/// Returns the `snap_xid` (= txn.xid).
pub fn create_snapshot<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    snap_name: &str,
) -> Result<u64, TxnError> {
    create_snapshot_made(txn, vsb_raw, omap_raw, snap_name, None)
}

/// [`create_snapshot`] recording `created` (ns since 1970) as the
/// snapshot's creation time instead of now, when given.
fn create_snapshot_made<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    snap_name: &str,
    created: Option<u64>,
) -> Result<u64, TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, omap_raw)?;
    let (vsb_raw, omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    crate::file::ensure_volume_mutable(vsb_raw)?;
    // #139: RETAIN existing snapshots. Every entry already in the snap_meta
    // tree is read here, the new snapshot is appended below, and the node is
    // rebuilt with the full set so the on-disk tree matches the (incremented)
    // snapshot counts. They are read before anything is allocated so a name
    // that is refused costs nothing.
    let existing_snap_meta_paddr = u64_from_le(vsb_raw, VSBI_SNAP_META_TREE_OID);
    let (mut entries, old_snap_meta_nodes) = if existing_snap_meta_paddr != 0 {
        read_snap_meta(txn, existing_snap_meta_paddr)?
    } else {
        (Vec::new(), Vec::new())
    };
    // Every node of the object map's snapshot tree, freed once it is
    // rebuilt below.
    let old_snapshot_tree_root = u64_from_le(omap_raw, OMAP_SNAPSHOT_TREE_OID_OFF);
    let old_snapshot_tree_nodes = if old_snapshot_tree_root != 0 {
        omap_snapshot_tree_nodes(txn, old_snapshot_tree_root)?
    } else {
        Vec::new()
    };
    check_snapshot_name(snap_name, &entries)?;
    let bsz = txn.nx.block_size as usize;
    let snap_xid = txn.xid;
    // M7b-RT Subtask E refinement (empirical, post-Subtask-E full):
    // Two-xid snapshot model. fsck distinguishes "live" omap entries from
    // "snapshot-frozen" by xid relative to om_most_recent_snap. Concretely,
    // fsck walks the LIVE fsroot via the omap entry with the highest xid;
    // if no entry has xid > om_most_recent_snap (i.e. all entries are at
    // snap_xid or earlier), fsck calculates 0 fsroot references for live
    // extents → "extentref refcnt (1) vs fsroot ref count (0)" warning.
    //
    // Therefore: snapshot artifacts stay at snap_xid, while LIVE-side
    // artifacts (live fsroot COW, vol-tree, vomap header, live VSB, and
    // ultimately nx_superblock) are stamped at live_xid = snap_xid + 1.
    // om_most_recent_snap stays at snap_xid.
    // [empirical: see decode of /tmp/apfs-scratch-large.img post-repro test
    //  pre-two-xid: all fsroot omap entry xids <= om_most_recent_snap.]
    let live_xid = snap_xid + 1;
    let now_ns = apple_epoch_now_ns();

    // --- F.2: Frozen volume superblock copy ---
    let frozen_paddr = txn.alloc_block()?;
    let mut frozen = vsb_raw.to_vec();
    if frozen.len() < bsz {
        frozen.resize(bsz, 0);
    } else {
        frozen.truncate(bsz);
    }
    // Physical object: o_oid = paddr, o_xid = snap_xid.
    write_u64_le(&mut frozen, 8, frozen_paddr);
    write_u64_le(&mut frozen, 16, snap_xid);
    write_u32_le(&mut frozen, 24, OBJ_PHYSICAL | OBJECT_TYPE_FS);
    // CANONICAL Apple model (locked by reading linux-apfs-rw kernel
    // `apfs_create_superblock_snapshot`): frozen VSB ZEROES omap_oid,
    // extentref_tree_oid, and snap_meta_tree_oid. The snapshot's actual
    // extref/omap are resolved via the snap_metadata record + by sharing
    // with the live volume (omap=0 means "use live omap"). Earlier
    // -locked guidance (preserve those fields) was based on
    // ambiguous PDF text - the real kernel zeros them.
    write_u64_le(&mut frozen, VSBI_OMAP_OID, 0u64);
    write_u64_le(&mut frozen, VSBI_EXTENTREF_TREE_OID, 0u64);
    write_u64_le(&mut frozen, VSBI_SNAP_META_TREE_OID, 0u64);

    // --- F.3: NEW EMPTY extref tree for the LIVE volume (CANONICAL Apple
    //          model, locked by reading the linux-apfs-rw kernel source:
    //          apfs_create_new_extentref_tree -> apfs_make_empty_btree_root
    //          with type BLOCKREFTREE). The snapshot RETAINS the pre-snap
    //          extref via snap_metadata_val.extentref_tree_oid; the live
    //          volume starts with a fresh empty tree, and future writes
    //          allocate PHYS_EXT records there.
    //
    // layout for empty extref tree (empirical decode of
    // kernel-formatted scratch image):
    //   subtype = OBJECT_TYPE_BLOCKREFTREE (0x0F)
    //   bt_flags = 0x0052 (PHYSICAL | KV_NONALIGNED | SEQUENTIAL_INSERT)
    //   btn_flags = 0x03 (ROOT | LEAF), key_size = 0, val_size = 0
    let old_extentref_oid = u64_from_le(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    let new_extentref_paddr = txn.alloc_block()?;
    let new_extentref_block = build_empty_btree_root(
        new_extentref_paddr,
        snap_xid,
        OBJ_PHYSICAL | OBJECT_TYPE_BTREE,
        OBJECT_TYPE_BLOCKREFTREE,
        bsz,
        0x0000_0052, // bt_flags
        0,           // key_size = 0 (variable)
        0,           // val_size = 0 (variable)
    );
    txn.stage_raw(new_extentref_paddr, new_extentref_block);

    // --- F.1: Snap-meta B-tree node with SNAP_METADATA + SNAP_NAME records ---
    let name_bytes = {
        let mut b = snap_name.as_bytes().to_vec();
        b.push(0u8);
        b
    };
    let name_len = name_bytes.len() as u16;
    // Preserve `bt_flags` from the existing snap_meta tree root so we stay
    // bit-identical to whatever the formatting tool (newfs_apfs, on the
    // user's host system) wrote. Reading the live volume's footer makes the
    // writer version-resilient (e.g. BTREE_HASHED set or unset).
    // [CERTAIN: see DEFAULT_SNAP_META_BT_FLAGS doc + the APFS specification p.129]
    let bt_flags = if existing_snap_meta_paddr != 0 {
        read_btn_info_flags(txn, existing_snap_meta_paddr).unwrap_or(DEFAULT_SNAP_META_BT_FLAGS)
    } else {
        DEFAULT_SNAP_META_BT_FLAGS
    };
    // Preserve VSB.apfs_extentref_tree_type @ 0x78 - the kernel's authoritative
    // o_type for the volume's extent-ref tree. Apple snap_metadata_val.extentref_tree_type
    // copies this verbatim (NOT a subtype). [ LOCKED, the APFS specification p.117]
    let old_extentref_type = u32_from_le(vsb_raw, VSBI_EXTENTREF_TREE_TYPE);
    // The snapshot metadata record needs an object id of its own. Reusing 2 -
    // the root directory's inode number - hands the snapshot the identity of a
    // live object, so a checker walking object ids sees the same id claimed
    // both by an inode and by snapshot metadata. Take the volume's next free
    // id instead and advance the counter below, exactly as a file creation
    // does.
    let snap_inum = u64_from_le(vsb_raw, VSBI_NEXT_OBJ_ID);
    entries.push(SnapEntry {
        xid: snap_xid,
        extentref_tree_oid: old_extentref_oid,
        extentref_tree_type: old_extentref_type,
        sblock_oid: frozen_paddr,
        create_time: created.unwrap_or(now_ns),
        change_time: now_ns,
        inum: snap_inum,
        name: name_bytes.clone(),
    });
    entries.sort_by_key(|e| e.xid);
    let snap_xids: Vec<u64> = entries.iter().map(|e| e.xid).collect();
    let _ = name_len; // name carried inline in SnapEntry
    let (snap_node_paddr, new_snap_meta_nodes) =
        write_snap_meta(txn, &entries, snap_xid, bt_flags)?;

    // --- F.5: Updated live volume superblock (virtual object) ---
    let vsb_oid = u64_from_le(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    if new_vsb.len() < bsz {
        new_vsb.resize(bsz, 0);
    } else {
        new_vsb.truncate(bsz);
    }
    let old_num_snaps = u64_from_le(&new_vsb, VSBI_NUM_SNAPSHOTS);
    write_u64_le(&mut new_vsb, VSBI_NUM_SNAPSHOTS, old_num_snaps + 1);
    // Consume the id handed to the snapshot metadata record above so no later
    // object is created with the same one.
    write_u64_le(&mut new_vsb, VSBI_NEXT_OBJ_ID, snap_inum + 1);
    write_u64_le(&mut new_vsb, VSBI_LAST_MOD_TIME, now_ns);
    // Q3: live volume gets the new empty extentref tree.
    write_u64_le(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extentref_paddr);
    // F.1: live VSB snap_meta_tree_oid points to our new snap_meta_node block.
    // [CERTAIN: linux-apfs-rw snapshot.c sets apfs_snap_meta_tree_oid; F.1 spec]
    write_u64_le(&mut new_vsb, VSBI_SNAP_META_TREE_OID, snap_node_paddr);
    // Q5: revert fields left untouched (already 0 on normal volumes).
    // Q4: snap_meta_ext_oid left untouched.
    // NOTE: apfs_omap_oid will be patched after COW'ing the volume omap below.
    // stage_virtual is called AFTER the omap COW so the patched new_vsb is used.

    // --- Volume omap update: om_snap_count += 1, om_most_recent_snap = snap_xid ---
    //
    // The volume omap is a PHYSICAL object (o_type = OBJ_PHYSICAL | OBJECT_TYPE_OMAP).
    // We COW it to a new block to avoid invalidating the previous checkpoint:
    // the old block's o_xid must stay at xid-1 for the prior checkpoint to remain
    // valid. The new VSB's apfs_omap_oid is updated to point to the new block.
    // [CERTAIN: empirical - in-place overwrite causes fsck "invalid o_xid" on old checkpoint;
    //  the APFS specification COW for all modified physical objects]
    // fsck checks om_snap_count matches snapshots found in om_snapshot_tree.
    // #139: rebuild the om_snapshot tree with EVERY live snapshot xid (not just
    // the new one), matching om_snap_count. The old tree is freed below.
    let (new_snapshot_tree_paddr, new_snapshot_tree_nodes) =
        write_omap_snapshot_tree(txn, &snap_xids, snap_xid)?;
    // The previous om_snapshot tree (if any) is orphaned by the rebuild and
    // freed below. Leaking it caused fsck "overallocation" on the 2nd+
    // snapshot. [#139]

    // How much the rebuilt volume omap b-tree grew or shrank. It is a 1:1 COW
    // pair only while the tree fits one node; past that the rebuild allocates a
    // different number of nodes than it frees.
    // Whether the live catalog root was copied under this transaction here;
    // it is not when an earlier change in the same transaction already did.
    let mut live_fsroot_copied = false;
    let vol_tree_node_delta: i64 = {
        let mut new_omap = omap_raw.to_vec();
        if new_omap.len() < bsz {
            new_omap.resize(bsz, 0);
        } else {
            new_omap.truncate(bsz);
        }
        // M7b-RT Subtask A LOCKED (the APFS specification Q1/Q4): COW the
        // volume omap btree AND free the old paddr immediately. The locked
        // spec confirms fsck validates only the latest checkpoint, so prompt
        // free at commit is fsck-CLEAN. The dual-omap historical reachability
        // concern is resolved by the single-omap architecture: the frozen VSB
        // (below) is patched to point at the NEW (COW'd) volume omap, so the
        // OLD volume omap header + b-tree have no remaining referrer.
        let old_vol_tree_paddr = u64_from_le(&new_omap, 48);
        let old_vomap_paddr = u64_from_le(vsb_raw, VSBI_OMAP_OID);
        let mut tree_buf = vec![0u8; bsz];
        txn.read_block(old_vol_tree_paddr, &mut tree_buf)?;
        // Walk the WHOLE old omap. Decoding only the root reads an index node
        // as if it were a leaf once the tree has split, which would rebuild the
        // map as a single node and orphan every real leaf beneath it - and with
        // the object map goes the catalog. The tree is rebuilt from the mappings
        // themselves, which also keeps the new entry sorted and the node free
        // lists valid (poking the old node in place corrupted macOS-formatted
        // nodes: fsck "Object map is invalid" / "invalid btn_key_free_list").
        let (mut triples, old_vol_tree_nodes) =
            crate::file::collect_omap_tree(txn, &tree_buf, old_vol_tree_paddr, bsz)?;
        // Two-xid snapshot model: find the live fsroot (highest-xid root_oid
        // entry), COW it to a new paddr stamped at live_xid, and add the new
        // {root_oid, live_xid} -> new_fsroot mapping.
        let root_tree_oid = u64_from_le(vsb_raw, VSBI_ROOT_TREE_OID);
        let mut fsroot_paddr_at_max: u64 = 0;
        let mut have_live_xid_entry = false;
        let mut max_xid = 0u64;
        for &OmapRec {
            oid, xid, paddr, ..
        } in &triples
        {
            if oid != root_tree_oid {
                continue;
            }
            if xid == live_xid {
                have_live_xid_entry = true;
            }
            if xid >= max_xid {
                max_xid = xid;
                fsroot_paddr_at_max = paddr;
            }
        }
        if fsroot_paddr_at_max != 0 && !have_live_xid_entry {
            // The fsroot is a VIRTUAL object: o_oid stays the root-tree oid,
            // only o_xid is bumped.
            let new_fsroot_paddr = txn.alloc_block()?;
            let mut fsroot_buf = vec![0u8; bsz];
            txn.read_block(fsroot_paddr_at_max, &mut fsroot_buf)?;
            write_u64_le(&mut fsroot_buf, 16, live_xid);
            update_checksum_in_place(&mut fsroot_buf);
            txn.stage_raw(new_fsroot_paddr, fsroot_buf);
            triples.push(OmapRec::live(
                root_tree_oid,
                live_xid,
                new_fsroot_paddr,
                bsz,
            ));
            live_fsroot_copied = true;
        }
        let (new_vol_tree_paddr, new_vol_tree_nodes) =
            crate::file::build_omap_tree(txn, &tree_buf, &triples, live_xid, bsz)?;
        // Allocate a new block for the COW'd volume omap.
        let new_vomap_paddr = txn.alloc_block()?;
        // Physical object: o_oid = paddr, o_xid = LIVE_xid (two-xid model).
        // om_most_recent_snap stays at snap_xid so the snapshot boundary is
        // unchanged; the omap header itself is live-side.
        write_u64_le(&mut new_omap, 8, new_vomap_paddr); // o_oid = paddr
        write_u64_le(&mut new_omap, 16, live_xid); // o_xid (live)
        let old_snap_count = u32_from_le(&new_omap, OMAP_SNAP_COUNT_OFF);
        write_u32_le(&mut new_omap, OMAP_SNAP_COUNT_OFF, old_snap_count + 1);
        write_u64_le(&mut new_omap, OMAP_MOST_RECENT_SNAP_OFF, snap_xid);
        // snapshot-tree pointer + type fields populated.
        write_u64_le(&mut new_omap, 56, new_snapshot_tree_paddr);
        write_u32_le(&mut new_omap, 44, OBJ_PHYSICAL | OBJECT_TYPE_BTREE);
        // om_tree_oid points to the newly-COW'd volume omap btree.
        write_u64_le(&mut new_omap, 48, new_vol_tree_paddr);
        update_checksum_in_place(&mut new_omap);
        txn.stage_raw(new_vomap_paddr, new_omap);
        // Update the live VSB's apfs_omap_oid to point to the new volume omap block.
        // (new_vsb was already prepared above; patch it before stage_virtual.)
        write_u64_le(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
        // CANONICAL Apple model: frozen VSB.apfs_omap_oid is LEFT AT ZERO
        // (already zeroed at frozen-VSB prep above). fsck resolves the
        // snapshot's omap by sharing with the live VSB. The previous
        // "single-omap patch to new_vomap_paddr" approach was empirically
        // CLEAN for create+delete but mismatches the kernel; the canonical
        // 0 is required for fsck's "orphan extent" detector to recognize
        // shared-paddr extents correctly.
        // Free the old volume-omap header and every node of the old b-tree -
        // all orphaned once the header points at the rebuilt tree.
        txn.free_block(old_vomap_paddr)?;
        for p in &old_vol_tree_nodes {
            txn.free_block(*p)?;
        }
        new_vol_tree_nodes as i64 - old_vol_tree_nodes.len() as i64
    };

    // M7b-RT Subtask A: free the OLD snap-meta tree if it existed. The live
    // VSB now points at `snap_node_paddr` (a freshly-built tree with the new
    // snap records); the frozen VSB has snap_meta_tree_oid = 0 (snapshots
    // have no nested snapshots). Nothing references the old nodes anymore.
    for &p in &old_snap_meta_nodes {
        txn.free_block(p)?;
    }
    // #139: free the previous om_snapshot tree (rebuilt above for all snaps).
    for &p in &old_snapshot_tree_nodes {
        txn.free_block(p)?;
    }

    // Finalize the frozen VSB now that apfs_omap_oid has been patched.
    update_checksum_in_place(&mut frozen);
    txn.stage_raw(frozen_paddr, frozen);

    // M7b-RT Subtask E (CANONICAL model, two-xid + empty-extref):
    // fs_alloc_count semantics (apfsprogs/apfsck/super.c:1175): the field
    // must equal the volume's total block count - every alloc bumps it,
    // every free decrements. We tally the net delta mechanically here.
    //
    // The frozen VSB is the snapshot's OWN superblock; the live volume's
    // apfs_fs_alloc_count does not include it - fsck (the arbiter) counts +4, not
    // +5, of net-new live references for a first snapshot, on both our-format and
    // macOS-formatted volumes.
    // ALLOCs counted (+4): EMPTY live extref tree (old paddr retained by
    //   snap_metadata_val), new vomap header (COW pair), new vol-tree (COW
    //   pair), live fsroot COW; and every node of the rebuilt snap-meta and
    //   om_snapshot trees.
    // FREEs (-2): old vomap header, old vol-tree (always); and every node of
    //   the snap-meta and om_snapshot trees they replace.
    let mut net_delta: i64 = 4 - 2;
    if !live_fsroot_copied {
        // The catalog root already carried this transaction's id, so no copy
        // was taken here: one allocation fewer than the tally above assumes.
        net_delta -= 1;
    }
    net_delta += (new_snap_meta_nodes + new_snapshot_tree_nodes) as i64;
    net_delta -= (old_snap_meta_nodes.len() + old_snapshot_tree_nodes.len()) as i64;
    // The +6/-2 above counts the volume omap b-tree as one block replacing one;
    // add whatever the rebuild actually changed on top of that.
    net_delta += vol_tree_node_delta;
    let cur_fs_alloc = u64_from_le(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let new_fs_alloc = (cur_fs_alloc + net_delta).max(0) as u64;
    write_u64_le(&mut new_vsb, VSBI_FS_ALLOC_COUNT, new_fs_alloc);

    // Two-xid model: bump txn.xid to live_xid BEFORE stage_virtual + commit.
    // This causes:
    //   - live VSB (virtual object, header generated by stage_virtual) o_xid = live_xid
    //   - checkpoint map (generated in commit) o_xid = live_xid
    //   - spaceman + reaper (ephemerals, COW'd in commit) o_xid = live_xid
    //   - nx_superblock (commit point) o_xid = live_xid
    // All snap-side artifacts (frozen VSB, snap_meta node, extref COW,
    // snapshot_tree) were already staged with o_xid = snap_xid above.
    txn.xid = live_xid;

    // Now stage the updated live VSB (body = bytes 32..bsz, patched above).
    let vsb_body: Vec<u8> = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    let padded_body = pad_to(vsb_body, bsz - 32);
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &padded_body)?;

    Ok(snap_xid)
}

// ---------------------------------------------------------------------------
// delete_snapshot
// ---------------------------------------------------------------------------

/// Stage a snapshot-delete onto `txn`.
///
/// Implements the synchronous Q6 subset (the APFS specification):
/// (c) Decrement apfs_num_snapshots in volume superblock.
/// (d) Update omap om_snap_count -= 1, om_most_recent_snap if applicable.
///
/// (a)+(b) Removal of SNAP_METADATA and SNAP_NAME records from the snap meta
/// tree is handled structurally by the kernel on next mount when it sees that
/// no xid range refers to those records. In our minimal write path the snap
/// meta tree node staged during create_snapshot is simply not referenced by
/// the new checkpoint (the new nx_superblock's volume points to the updated
/// vsb, which has no new snap_meta_tree pointer to the old node).
///
/// (e) The frozen sblock block is freed back to the spaceman's free-queue cache
/// (reaper-deferred extent cleanup - Q6).
///
/// `snap_xid` is the transaction ID of the snapshot being deleted.
/// Parse the snap meta B-tree leaf to extract `sblock_oid` (frozen VSB paddr)
/// for the given `snap_xid`. Returns `None` if not found or parse fails.
///
/// The snap meta tree is a var-kv B-tree leaf; SNAP_METADATA val starts with
/// `extentref_tree_oid (u64)` then `sblock_oid (u64)` at offset 8.
/// [CERTAIN: snapshot.rs build_snap_meta_node val1 layout; F.1 the APFS specification]
/// Returns (sblock_oid, extentref_tree_oid) for the given snap_xid by
/// decoding the `apfs_snap_metadata_val` record from the snap_meta tree.
/// Layout: extentref_tree_oid (u64) @v_abs+0, sblock_oid (u64) @v_abs+8.
fn read_snap_metadata_oids<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    snap_meta_tree_paddr: u64,
    snap_xid: u64,
) -> Option<(u64, u64)> {
    // A tree that cannot be read has no such snapshot here; the reader the
    // caller runs next refuses it with its reason.
    let entries = read_all_snap_entries(txn, snap_meta_tree_paddr).ok()?;
    entries
        .iter()
        .find(|e| e.xid == snap_xid)
        .map(|e| (e.sblock_oid, e.extentref_tree_oid))
}

/// Backwards-compatible wrapper - returns only sblock_oid.
#[allow(dead_code)]
fn read_frozen_vsb_paddr<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    snap_meta_tree_paddr: u64,
    snap_xid: u64,
) -> Option<u64> {
    read_snap_metadata_oids(txn, snap_meta_tree_paddr, snap_xid).map(|(s, _)| s)
}

// ---------------------------------------------------------------------------
// Multi-snapshot retention (#139)
// ---------------------------------------------------------------------------
//
// A live volume may hold N snapshots. Both the per-volume snap_meta tree
// (var-kv, SNAP_METADATA + SNAP_NAME records) and the volume-omap
// om_snapshot tree (fixed-kv, one omap_snapshot_t per xid) must list ALL
// live snapshots, and the VSB/omap snapshot counters must match the record
// counts, or fsck reports "Snapshot is invalid" (the APFS spec empirical).
//
// We keep a single-node model: create reads the existing entries, appends
// the new one, and rebuilds both trees; revert rebuilds both WITHOUT the
// reverted (most-recent) snapshot. A single 4 KiB node holds ~32 snapshots;
// beyond that the builders return an error (no corruption - a future
// milestone can add node splitting). [clean-room: derived from our own
// single-entry builders + apfs_keycmp ordering, no reference code copied.]

/// A fully-decoded snapshot metadata entry - enough to rebuild BOTH the
/// snap_meta tree and the om_snapshot tree. Decoded from the SNAP_METADATA
/// records, which carry the name inline (so the SNAP_NAME records are
/// redundant for reconstruction).
#[derive(Clone)]
struct SnapEntry {
    xid: u64,
    extentref_tree_oid: u64,
    extentref_tree_type: u32,
    sblock_oid: u64,
    create_time: u64,
    change_time: u64,
    inum: u64,
    /// Snapshot name INCLUDING the trailing NUL byte (on-disk form).
    name: Vec<u8>,
}

/// Longest snapshot name in bytes, the trailing NUL not counted.
/// [linux-apfs-rw apfs_raw.h: 255; the name is a NUL-terminated string whose
/// stored length counts the NUL, apfsprogs snapshot.c "wrong name length"]
const SNAP_MAX_NAME_LEN: usize = 255;

/// Refuse a name the snapshot tree cannot carry: empty, longer than
/// [`SNAP_MAX_NAME_LEN`] bytes, holding a NUL (the stored length would then
/// disagree with the string), or already taken by another snapshot (two
/// SNAP_NAME records with one key break the tree's ordering).
fn check_snapshot_name(name: &str, existing: &[SnapEntry]) -> Result<(), TxnError> {
    if name.is_empty() {
        return Err(TxnError::InvalidArgument(
            "a snapshot name cannot be empty".to_string(),
        ));
    }
    if name.len() > SNAP_MAX_NAME_LEN {
        return Err(TxnError::InvalidArgument(format!(
            "a snapshot name is at most {SNAP_MAX_NAME_LEN} bytes; {} were given",
            name.len()
        )));
    }
    if name.as_bytes().contains(&0) {
        return Err(TxnError::InvalidArgument(
            "a snapshot name cannot contain a NUL byte".to_string(),
        ));
    }
    let taken = existing
        .iter()
        .any(|e| e.name.strip_suffix(&[0u8]).unwrap_or(&e.name) == name.as_bytes());
    if taken {
        return Err(TxnError::AlreadyExists(format!(
            "a snapshot named {name:?} already exists"
        )));
    }
    Ok(())
}

/// Parse every SNAP_METADATA record from a single-node snap_meta tree into a
/// list of [`SnapEntry`], sorted ascending by xid. Mirrors the value layout
/// written by `build_snap_meta_node*` (extentref_oid@0, sblock@8,
/// create_time@16, change_time@24, inum@32, extentref_type@40, flags@44,
/// name_len@48, name@50).
fn read_all_snap_entries<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    snap_meta_tree_paddr: u64,
) -> Result<Vec<SnapEntry>, TxnError> {
    Ok(read_snap_meta(txn, snap_meta_tree_paddr)?.0)
}

/// Every snapshot the snapshot metadata tree at `root` records, by xid,
/// and the address of each of the tree's nodes. The tree is read at any
/// depth: macOS grows it past one node on a volume with many snapshots.
fn read_snap_meta<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    root: u64,
) -> Result<(Vec<SnapEntry>, Vec<u64>), TxnError> {
    let tree = phys_tree::read(txn, root, OBJECT_TYPE_SNAPMETATREE)?;
    let mut out: Vec<SnapEntry> = Vec::new();
    for (k, v) in &tree.records {
        if k.len() < 8 {
            continue;
        }
        let hdr = u64_from_le(k, 0);
        let key_type = (hdr >> 60) & 0xF;
        let obj_id = hdr & 0x0FFF_FFFF_FFFF_FFFF;
        if key_type != APFS_TYPE_SNAP_METADATA {
            continue; // SNAP_NAME records are redundant; skip.
        }
        // The fixed prefix (50 bytes) holds the name length; the name
        // follows it.
        if v.len() < 50 {
            continue;
        }
        let name_len = u16_from_le(v, 48) as usize;
        let Some(name) = v.get(50..50 + name_len).filter(|n| !n.is_empty()) else {
            continue;
        };
        out.push(SnapEntry {
            xid: obj_id,
            extentref_tree_oid: u64_from_le(v, 0),
            extentref_tree_type: u32_from_le(v, 40),
            sblock_oid: u64_from_le(v, 8),
            create_time: u64_from_le(v, 16),
            change_time: u64_from_le(v, 24),
            inum: u64_from_le(v, 32),
            name: name.to_vec(),
        });
    }
    out.sort_by_key(|e| e.xid);
    Ok((out, tree.nodes))
}

/// One snapshot as [`list_snapshots`] reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// Transaction the snapshot froze: the id [`delete_snapshot`] and
    /// [`revert_to_snapshot`] take.
    pub xid: u64,
    /// Snapshot name, without the trailing NUL.
    pub name: String,
    /// Creation time, nanoseconds since 1970-01-01 UTC.
    pub create_time: u64,
}

/// Every snapshot of the volume, oldest first. `vsb_raw` and `omap_raw` are
/// the volume superblock and volume omap header as read from the device; a
/// copy this transaction has already staged takes their place.
pub fn list_snapshots<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
) -> Result<Vec<SnapshotInfo>, TxnError> {
    let (vsb_now, _) = txn.current_volume(vsb_raw, omap_raw)?;
    let snap_meta_paddr = u64_from_le(&vsb_now, VSBI_SNAP_META_TREE_OID);
    if snap_meta_paddr == 0 {
        return Ok(Vec::new());
    }
    let entries = read_all_snap_entries(txn, snap_meta_paddr)?;
    Ok(entries
        .into_iter()
        .map(|e| {
            let name = e.name.strip_suffix(&[0u8]).unwrap_or(&e.name);
            SnapshotInfo {
                xid: e.xid,
                name: String::from_utf8_lossy(name).into_owned(),
                create_time: e.create_time,
            }
        })
        .collect())
}

/// Encode the `apfs_snap_metadata_val` body for one entry (see
/// `build_snap_meta_node` for field provenance).
fn encode_snap_metadata_val(e: &SnapEntry) -> Vec<u8> {
    let mut v: Vec<u8> = Vec::with_capacity(50 + e.name.len());
    v.extend_from_slice(&e.extentref_tree_oid.to_le_bytes());
    v.extend_from_slice(&e.sblock_oid.to_le_bytes());
    v.extend_from_slice(&e.create_time.to_le_bytes());
    v.extend_from_slice(&e.change_time.to_le_bytes());
    v.extend_from_slice(&e.inum.to_le_bytes());
    v.extend_from_slice(&e.extentref_tree_type.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes()); // flags = 0
    v.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
    v.extend_from_slice(&e.name);
    v
}

/// The records of the snapshot metadata tree for `entries` (sorted by
/// xid), in `apfs_keycmp` order: all SNAP_METADATA (ascending by xid), then
/// all SNAP_NAME (ascending by `strcmp(name)`).
/// [clean-room: ordering per linux-apfs-rw key.c `apfs_keycmp` (id, type,
///  number, strcmp(name))]
fn snap_meta_records(entries: &[SnapEntry]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut recs: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(entries.len() * 2);
    for e in entries {
        let key = encode_jkey(e.xid, APFS_TYPE_SNAP_METADATA)
            .to_le_bytes()
            .to_vec();
        recs.push((key, encode_snap_metadata_val(e)));
    }
    // All SNAP_NAME records share the same (obj_id, type) header, so the
    // on-disk order is the name byte order.
    let mut by_name: Vec<&SnapEntry> = entries.iter().collect();
    by_name.sort_by(|a, b| a.name.cmp(&b.name));
    for e in by_name {
        let mut key = encode_jkey(SNAP_NAME_OBJ_ID, APFS_TYPE_SNAP_NAME)
            .to_le_bytes()
            .to_vec();
        key.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
        key.extend_from_slice(&e.name);
        recs.push((key, e.xid.to_le_bytes().to_vec()));
    }
    recs
}

fn snap_meta_spec(bt_flags: u32) -> phys_tree::Spec {
    phys_tree::Spec {
        subtype: OBJECT_TYPE_SNAPMETATREE,
        kv: phys_tree::Kv::Var,
        bt_flags,
    }
}

/// Write the snapshot metadata tree for `entries`, as many levels deep as
/// it takes. Returns its root and its node count.
fn write_snap_meta<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    entries: &[SnapEntry],
    xid: u64,
    bt_flags: u32,
) -> Result<(u64, usize), TxnError> {
    phys_tree::write(
        txn,
        &snap_meta_spec(bt_flags),
        snap_meta_records(entries),
        xid,
    )
}

/// The object map's snapshot tree: one `omap_snapshot_t` (flags and oid
/// left zero) per snapshot, keyed by xid.
const OMAP_SNAPSHOT_SPEC: phys_tree::Spec = phys_tree::Spec {
    subtype: OBJECT_TYPE_OMAP_SNAPSHOT,
    kv: phys_tree::Kv::Fixed { key: 8, val: 16 },
    // PHYSICAL | UINT64_KEYS
    bt_flags: 0x0000_0011,
};

/// Write the object map's snapshot tree for `xids`. Returns its root and
/// its node count.
fn write_omap_snapshot_tree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    xids: &[u64],
    xid: u64,
) -> Result<(u64, usize), TxnError> {
    let mut sorted: Vec<u64> = xids.to_vec();
    sorted.sort_unstable();
    let recs = sorted
        .into_iter()
        .map(|x| (x.to_le_bytes().to_vec(), vec![0u8; 16]))
        .collect();
    phys_tree::write(txn, &OMAP_SNAPSHOT_SPEC, recs, xid)
}

/// The address of every node of the object map's snapshot tree at `root`.
fn omap_snapshot_tree_nodes<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    root: u64,
) -> Result<Vec<u64>, TxnError> {
    Ok(phys_tree::read(txn, root, OBJECT_TYPE_OMAP_SNAPSHOT)?.nodes)
}

pub fn delete_snapshot<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    snap_xid: u64,
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, omap_raw)?;
    let (vsb_raw, omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    crate::file::ensure_volume_mutable(vsb_raw)?;
    let bsz = txn.nx.block_size as usize;
    let del_xid = txn.xid;
    let now_ns = apple_epoch_now_ns();

    // Collect the paddrs of orphaned blocks created during create_snapshot.
    // These blocks are set in the allocation bitmap but will no longer be
    // referenced by xid=del_xid's checkpoint after delete. They must be
    // freed (bitmap bit cleared, free_count incremented) so fsck does not
    // report "overallocation" for the new checkpoint.
    //
    // IMPORTANT: we collect these BEFORE building new_vsb (so we read from
    // vsb_raw, the xid=snap_xid VSB), and we free them AFTER new_vsb is
    // fully constructed and staged - ensuring the new VSB does NOT reference
    // any of the blocks we free.
    //
    // [CERTAIN: empirical - fsck "overallocation" = bitmap set but unreferenced
    //  by active extents in the latest checkpoint; immediate free is correct here
    //  because we update new_vsb to not reference these blocks]
    let snap_meta_paddr = u64_from_le(vsb_raw, VSBI_SNAP_META_TREE_OID);
    let old_vomap_paddr = u64_from_le(vsb_raw, VSBI_OMAP_OID);
    // NOTE: We do NOT free the volume omap B-tree (om_tree_oid at offset 48).
    // The new volume omap (COW'd below) inherits the same om_tree_oid; freeing
    // it would corrupt the active volume omap tree reference.
    // [CERTAIN: empirical - new_omap copies omap_raw and keeps om_tree_oid; freeing
    //  it at 0xc2 caused "no mountable filesystem" post-delete]

    // CANONICAL Apple model: snap_metadata_val records both the sblock_oid
    // (frozen VSB paddr) AND the extentref_tree_oid (snapshot's pinned extref).
    // The frozen VSB itself has apfs_extentref_tree_oid = 0; the OID lives in
    // the snap metadata record. Read both in one pass.
    // A snapshot that is not there is not deleted: going on would rewrite
    // the trees and drain the free queue up to an xid nothing was pinned at.
    let (frozen_vsb_paddr, snapshot_extref_paddr): (Option<u64>, Option<u64>) =
        if snap_meta_paddr != 0 {
            match read_snap_metadata_oids(txn, snap_meta_paddr, snap_xid) {
                Some((sb, er)) => (
                    if sb != 0 { Some(sb) } else { None },
                    if er != 0 { Some(er) } else { None },
                ),
                None => return Err(TxnError::SnapshotNotFound(snap_xid)),
            }
        } else {
            return Err(TxnError::SnapshotNotFound(snap_xid));
        };

    // #139: retain SURVIVING snapshots. Rebuild both trees WITHOUT the deleted
    // entry rather than clearing the whole tree (which only matched the old
    // single-snapshot model and left count/tree inconsistent once create began
    // retaining N snapshots).
    let (all_snaps, old_snap_meta_nodes) = if snap_meta_paddr != 0 {
        read_snap_meta(txn, snap_meta_paddr)?
    } else {
        (Vec::new(), Vec::new())
    };
    let old_snapshot_tree_paddr = u64_from_le(omap_raw, OMAP_SNAPSHOT_TREE_OID_OFF);
    let old_snapshot_tree_nodes = if old_snapshot_tree_paddr != 0 {
        omap_snapshot_tree_nodes(txn, old_snapshot_tree_paddr)?
    } else {
        Vec::new()
    };
    // Oldest snapshot the volume still holds. A free-queue entry stamped older
    // than this predates every snapshot on the volume, so its blocks were left
    // by the formatter rather than handed over by our copy-on-write path, and
    // they were never part of apfs_fs_alloc_count.
    let oldest_snap_xid = all_snaps
        .iter()
        .map(|e| e.xid)
        .min()
        .unwrap_or(snap_xid)
        .min(snap_xid);
    let mut remaining: Vec<SnapEntry> = all_snaps
        .into_iter()
        .filter(|e| e.xid != snap_xid)
        .collect();
    let remaining_count = remaining.len() as u32;
    let remaining_most_recent = remaining.iter().map(|e| e.xid).max().unwrap_or(0);

    // The dying snapshot's extentref records are the only thing covering the
    // extents its view holds, and every block of its tree has to be released,
    // not just the root: once the tree is multi-node, freeing the root alone
    // strands the rest (fsck: "overallocation detected").
    let mut src_recs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut snapshot_extref_nodes: Vec<u64> = Vec::new();
    if let Some(src_root) = snapshot_extref_paddr {
        let (recs, nodes) = crate::file::collect_extref_records(txn, src_root, bsz)?;
        src_recs = recs;
        snapshot_extref_nodes = nodes;
    }
    // When a NEWER snapshot survives, its view is the one that still needs
    // those records, so they go to its tree rather than to the live volume.
    // Done here, before the snapshot metadata is rebuilt below, so the record
    // that node writes for the survivor already names its new tree.
    let mut survivor_merge_delta: i64 = 0;
    let mut survivor_merge_freed: u64 = 0;
    let handed_to_survivor = !src_recs.is_empty() && remaining_most_recent > snap_xid;
    if handed_to_survivor {
        let target = remaining
            .iter_mut()
            .filter(|e| e.xid > snap_xid)
            .min_by_key(|e| e.xid)
            .ok_or_else(|| {
                TxnError::SpacemanParse("no newer snapshot to hand extent references to".into())
            })?;
        if target.extentref_tree_oid == 0 {
            return Err(TxnError::Unsupported(format!(
                "snapshot xid {:#x} has no extentref tree to receive the references \
                 held by xid {snap_xid:#x}",
                target.xid
            )));
        }
        let (new_root, node_delta, freed) =
            merge_extref_records_into(txn, target.extentref_tree_oid, &src_recs, del_xid, bsz)?;
        target.extentref_tree_oid = new_root;
        survivor_merge_delta = node_delta;
        survivor_merge_freed = freed;
    }
    let del_bt_flags = if snap_meta_paddr != 0 {
        read_btn_info_flags(txn, snap_meta_paddr).unwrap_or(DEFAULT_SNAP_META_BT_FLAGS)
    } else {
        DEFAULT_SNAP_META_BT_FLAGS
    };
    // The snapshot-metadata tree is always there, even with nothing in it: a
    // macOS-formatted volume that has never been snapshotted still points at a
    // real empty tree, and zero is not what "no snapshots" looks like. The
    // om_snapshot tree is the other way round - a kernel-formatted volume with
    // no snapshots has om_snapshot_tree_oid = 0.
    let (new_snap_meta_paddr, new_snap_meta_nodes) =
        write_snap_meta(txn, &remaining, del_xid, del_bt_flags)?;
    let (new_snapshot_tree_paddr, new_snapshot_tree_nodes) = if remaining.is_empty() {
        (0u64, 0usize)
    } else {
        let xids: Vec<u64> = remaining.iter().map(|e| e.xid).collect();
        write_omap_snapshot_tree(txn, &xids, del_xid)?
    };

    // --- (c) Decrement apfs_num_snapshots ---
    // VSB is a VIRTUAL object - stage_virtual allocates a new block and registers
    // it in the container omap B-tree. Only the body (bytes 32..bsz) is passed;
    // stage_virtual rebuilds the obj_phys header with correct o_oid/o_xid/o_type.
    let vsb_oid = u64_from_le(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    if new_vsb.len() < bsz {
        new_vsb.resize(bsz, 0);
    } else {
        new_vsb.truncate(bsz);
    }
    let old_num_snaps = u64_from_le(&new_vsb, VSBI_NUM_SNAPSHOTS);
    // #139: num_snapshots = surviving count (= old - 1 when the deleted entry
    // was present).
    write_u64_le(&mut new_vsb, VSBI_NUM_SNAPSHOTS, remaining_count as u64);
    write_u64_le(&mut new_vsb, VSBI_LAST_MOD_TIME, now_ns);
    // #139: point snap_meta_tree_oid at the rebuilt surviving-snapshots node
    // (0 when none remain - then the old tree block is freed below).
    write_u64_le(&mut new_vsb, VSBI_SNAP_META_TREE_OID, new_snap_meta_paddr);
    // KEEP extentref_tree_oid - a live APFS volume ALWAYS needs a valid extent
    // reference tree. The empty tree created during create_snapshot (Q3) stays
    // as the live volume's extentref root. Zeroing it (and freeing the block)
    // makes the volume unmountable ("no mountable filesystem").
    // [ LOCKED empirical: post-delete attach failed when oid was 0]
    // COW the live extentref tree to del_xid so its o_xid is consistent with
    // the new VSB (fsck enforces xid consistency on referenced trees).
    let old_extref_paddr = u64_from_le(&new_vsb, VSBI_EXTENTREF_TREE_OID);
    if old_extref_paddr != 0 {
        let new_extref_paddr = txn.alloc_block()?;
        let mut eb = vec![0u8; bsz];
        txn.read_block(old_extref_paddr, &mut eb)?;
        write_u64_le(&mut eb, 8, new_extref_paddr);
        write_u64_le(&mut eb, 16, del_xid);
        update_checksum_in_place(&mut eb);
        txn.stage_raw(new_extref_paddr, eb);
        write_u64_le(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
        // Free the OLD live extref paddr - it is now orphaned (COW pair's
        // old side). Symmetric to old_vol_tree_paddr / old_vomap_paddr.
        txn.free_block(old_extref_paddr)?;
    }

    // --- Hand the dying snapshot's extent references back to the live tree. ---
    // create_snapshot gives the live volume a NEW EMPTY extentref tree and
    // leaves the populated one pinned by the snapshot. Those records are the
    // only thing covering extents that are still live, so releasing that tree
    // without merging leaves every such extent unreferenced: fsck reports the
    // blocks as overallocated and the volume's block accounting drifts.
    let mut merged_node_delta: i64 = survivor_merge_delta;
    // Data blocks the merge released: extents the snapshot alone was keeping,
    // whose files were replaced or removed under it. They counted as reachable
    // while pinned and stop counting now.
    let mut merged_freed_blocks: u64 = survivor_merge_freed;
    if !src_recs.is_empty() && !handed_to_survivor {
        let live_root = u64_from_le(&new_vsb, VSBI_EXTENTREF_TREE_OID);
        let (new_live_root, node_delta, freed) =
            merge_extref_records_into(txn, live_root, &src_recs, del_xid, bsz)?;
        write_u64_le(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_live_root);
        merged_node_delta += node_delta;
        merged_freed_blocks += freed;
    }
    // NOTE: apfs_omap_oid will be patched after COW'ing the volume omap below.

    // --- (d) Update volume omap (PHYSICAL object, COW to new block) ---
    // [CERTAIN: COW required - in-place overwrite invalidates previous checkpoint;
    //  same as create_snapshot; the APFS specification]
    // How many blocks the object map gained or lost in the rebuild.
    let vol_tree_node_delta: i64;
    {
        let mut new_omap = omap_raw.to_vec();
        if new_omap.len() < bsz {
            new_omap.resize(bsz, 0);
        } else {
            new_omap.truncate(bsz);
        }
        // M7b-RT Subtask A (locked spec the APFS specification Q1/Q4):
        // COW the volume omap btree AND free the old paddr after staging the
        // new one. Single-omap architecture: the snapshot view also uses the
        // SAME live omap (frozen VSB's apfs_omap_oid was patched at create
        // time), so the old vol-tree block has no remaining referrer once
        // the new omap header is staged.
        let old_vol_tree_paddr = u64_from_le(&new_omap, 48);
        let mut tree_buf = vec![0u8; bsz];
        txn.read_block(old_vol_tree_paddr, &mut tree_buf)?;
        let new_vol_tree_paddr = {
            // The map holds a mapping per version of each object. A view - the
            // volume, or a snapshot - resolves an oid to its newest version not
            // past the view's xid, so a version is reachable only while some
            // view falls between it and the next one. With the snapshot gone
            // the map is rebuilt without the versions no view resolves any
            // more and their blocks released: an older version no remaining
            // snapshot falls on, and an oid's newest version that the volume's
            // catalog no longer names and no remaining snapshot reaches.
            // fsck: "Remove orphan/invalid omap mapping objects?" with the last
            // snapshot; apfsck: "oid-xid combination is never used" with any.
            let (triples, old_nodes) =
                crate::file::collect_omap_tree(txn, &tree_buf, old_vol_tree_paddr, bsz)?;
            let mut versions: std::collections::HashMap<u64, Vec<u64>> =
                std::collections::HashMap::new();
            for &OmapRec { oid, xid, .. } in &triples {
                versions.entry(oid).or_default().push(xid);
            }
            for xids in versions.values_mut() {
                xids.sort_unstable();
            }
            // Which catalog nodes the volume still names. A mapping for an oid
            // absent from this set was kept for a snapshot's view of the
            // catalog and describes a node the live tree replaced.
            let live_root_oid = u64_from_le(&new_vsb, VSBI_ROOT_TREE_OID);
            let mut reachable = std::collections::HashSet::new();
            crate::file::collect_fstree_oids(
                txn,
                &tree_buf,
                live_root_oid,
                bsz,
                &mut reachable,
                0,
            )?;
            let remaining_xids: Vec<u64> = remaining.iter().map(|e| e.xid).collect();
            // A remaining snapshot resolves the version at `xid` when it lies at
            // or past it and before the next version, if there is one.
            let resolved_by_remaining = |xid: u64, next: Option<u64>| {
                remaining_xids
                    .iter()
                    .any(|&r| r >= xid && next.is_none_or(|n| r < n))
            };
            let (kept, superseded): (Vec<OmapTriple>, Vec<OmapTriple>) =
                triples.into_iter().partition(|&OmapRec { oid, xid, .. }| {
                    let next = versions
                        .get(&oid)
                        .and_then(|xids| xids.iter().find(|&&x| x > xid).copied());
                    if resolved_by_remaining(xid, next) {
                        return true;
                    }
                    let Some(_) = next else {
                        // The newest version: the volume's, if its catalog names
                        // the object. Only a mapping the deleted snapshot itself
                        // pinned may be dropped for being unreachable; anything
                        // newer going missing from the catalog would be a
                        // different fault, and guessing at it here would take
                        // the volume with it.
                        return reachable.contains(&oid) || xid > snap_xid;
                    };
                    false
                });
            let (root, new_nodes) =
                crate::file::build_omap_tree(txn, &tree_buf, &kept, del_xid, bsz)?;
            let mut released = 0i64;
            for r in &superseded {
                if r.paddr != 0 && !kept.iter().any(|k| k.paddr == r.paddr) {
                    txn.free_block(r.paddr)?;
                    released += 1;
                }
            }
            vol_tree_node_delta = new_nodes as i64 - old_nodes.len() as i64 - released;
            for p in &old_nodes {
                txn.free_block(*p)?;
            }
            root
        };
        let new_vomap_paddr = txn.alloc_block()?;
        write_u64_le(&mut new_omap, 8, new_vomap_paddr); // o_oid = paddr
        write_u64_le(&mut new_omap, 16, del_xid); // o_xid
                                                  // #139: snapshot counters + tree pointer reflect the SURVIVING set.
        write_u32_le(&mut new_omap, OMAP_SNAP_COUNT_OFF, remaining_count);
        write_u64_le(
            &mut new_omap,
            OMAP_MOST_RECENT_SNAP_OFF,
            remaining_most_recent,
        );
        // Point at the rebuilt om_snapshot tree (0 when none remain - a
        // kernel-formatted 0-snapshot volume omap has om_snapshot_tree_oid = 0).
        write_u64_le(
            &mut new_omap,
            OMAP_SNAPSHOT_TREE_OID_OFF,
            new_snapshot_tree_paddr,
        );
        // om_tree_oid points to the newly-COW'd volume omap btree.
        write_u64_le(&mut new_omap, 48, new_vol_tree_paddr);
        update_checksum_in_place(&mut new_omap);
        txn.stage_raw(new_vomap_paddr, new_omap);
        // Update the live VSB's apfs_omap_oid to point to the new volume omap block.
        write_u64_le(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    }

    // M7b-RT Subtask A: fs_alloc_count net delta for snapshot deletion.
    // 1:1 COW pairs (net 0): old vol-tree ↔ new vol-tree, old vomap ↔ new vomap,
    //                        old live extref ↔ new live extref.
    // True frees only: frozen VSB, every node of the snap-meta and
    //   om_snapshot trees, snapshot's pinned extentref tree (EXT0).
    let mut net_delta: i64 = 0;
    // The frozen VSB is the snapshot's OWN superblock and create_snapshot
    // deliberately keeps it out of the live volume's fs_alloc_count, so
    // releasing it here must not decrement that count either.
    net_delta -= (old_snap_meta_nodes.len() + old_snapshot_tree_nodes.len()) as i64;
    // The whole pinned extentref tree leaves the volume, so every one of its
    // nodes counts against fs_alloc_count - a root-only decrement leaves the
    // field high by the number of stranded nodes.
    net_delta -= snapshot_extref_nodes.len() as i64;
    // #139: the rebuilt snap_meta and om_snapshot trees (the snap_meta tree
    // stays, empty, when no snapshot does).
    net_delta += (new_snap_meta_nodes + new_snapshot_tree_nodes) as i64;
    // M9 #7: safe sm_fq drain - drain only when this is the LAST snapshot.
    //
    // Each sm_fq entry is stamped with "newest snap xid at COW time", so
    // entries with xid <= deleted_snap_xid were created while the deleted
    // snap was the newest one. With ONLY the deleted snap to consider,
    // those blocks have no surviving referrer and are safe to free.
    //
    // When other snapshots survive after the delete, some of those entries
    // may still be referenced by older or newer frozen extref trees (the
    // out-of-order-delete safety hole). Detecting which blocks are still
    // referenced requires reading every surviving snapshot's frozen extref
    // tree - expensive and not required for the dominant Windows
    // single-snapshot workflow. We conservatively SKIP the drain in that
    // case; the entries accumulate in sm_fq until a future delete drains
    // them down to empty. Apple's kernel reaper takes the same conservative
    // path on a multi-snap workflow.
    //
    // [Apple cross-check: apfs_snap_vnop_remove → move_snapshot_to_purgatory
    //  → apfs_cleanup_purgatory_continuation handles per-block survivor
    //  check; ours defers to the final-snap-delete path.]
    //
    // The drain runs BEFORE the superblock is staged: the blocks it reclaims
    // leave the volume, so fs_alloc_count has to account for them. Draining
    // afterwards and discarding the count left the field permanently high.
    let _ = old_num_snaps;
    if remaining_count == 0 {
        // Reclaiming a queued block is the moment it stops being reachable, so
        // this is where it leaves fs_alloc_count. Only the entries this volume
        // enqueued count: the formatter's stale run was never part of the
        // figure, and subtracting it too left the field low by its length.
        let drained = txn.drain_sm_fq_main(snap_xid, oldest_snap_xid)?;
        net_delta -= drained.owned_blocks as i64;
    }

    // Nodes the extent-reference merge added to (or removed from) the live tree.
    net_delta += merged_node_delta;
    net_delta -= merged_freed_blocks as i64;
    // Whatever the object-map rebuild changed, on top of the 1:1 COW pair the
    // tally above assumes.
    net_delta += vol_tree_node_delta;

    let cur_fs_alloc = u64_from_le(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let new_fs_alloc = (cur_fs_alloc + net_delta).max(0) as u64;
    write_u64_le(&mut new_vsb, VSBI_FS_ALLOC_COUNT, new_fs_alloc);

    // Stage the updated live VSB (body = bytes 32..bsz, patched above).
    // new_vsb no longer references snap_meta_paddr, extref_paddr, or old_vomap_paddr.
    let vsb_body: Vec<u8> = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    let padded_body = pad_to(vsb_body, bsz - 32);
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &padded_body)?;

    // --- Free orphaned blocks from create_snapshot. ---
    // These are freed AFTER stage_virtual so that new_vsb (now staged) does
    // not reference any of them. Freeing clears bitmap bits and increments
    // free_count so fsck sees a consistent allocation state.
    // [CERTAIN: empirical - see above; blocks freed must not be referenced by
    //  any live structure reachable from the new checkpoint's NX superblock]
    if let Some(frozen_paddr) = frozen_vsb_paddr {
        txn.free_block(frozen_paddr)?;
    }
    for &p in &old_snap_meta_nodes {
        txn.free_block(p)?;
    }
    // NOTE: do NOT free the LIVE extref (vsb_raw.apfs_extentref_tree_oid).
    // The previous "extref_paddr" variable freed the live extref under the
    // misassumption that the snapshot pinned it; in the canonical Apple model
    // the snapshot's pinned extref is decoded from snap_metadata_val (above)
    // and freed separately below as `snapshot_extref_paddr`. The live extref
    // continues to be referenced by the new (post-delete) VSB.
    // Free old volume omap header block (old_vomap_paddr).
    // The new VSB points to new_vomap_paddr (COW'd above), so old_vomap_paddr
    // is genuinely orphaned and must be freed.
    // [CERTAIN: empirical - old omap header no longer reachable from new checkpoint]
    if old_vomap_paddr != 0 {
        txn.free_block(old_vomap_paddr)?;
    }
    // Free the previous om_snapshot tree - the post-delete volume omap points
    // at the rebuilt one, or at none (om_snapshot_tree_oid set to 0 above).
    for &p in &old_snapshot_tree_nodes {
        txn.free_block(p)?;
    }
    // M7b-RT Subtask A: free the snapshot's pinned extentref tree (EXT0).
    // The snapshot's frozen VSB owned it; with the frozen VSB now freed,
    // EXT0 has no remaining referrer. Its records were merged into the live
    // tree above, which was rebuilt into freshly allocated nodes, so no block
    // listed here is reachable from the post-delete volume.
    for p in &snapshot_extref_nodes {
        txn.free_block(*p)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// revert_to_snapshot
// ---------------------------------------------------------------------------

/// Fold a snapshot's extent-reference record into the live one for the same
/// physical extent.
///
/// A KIND_NEW record states an absolute reference count; a KIND_UPDATE record
/// states a signed delta against the count its snapshot holds. Merging the two
/// therefore means applying the live delta to the snapshot's count, or summing
/// two counts when both are absolute. The block count and owner are taken from
/// whichever record is authoritative (the KIND_NEW one).
fn merge_phys_ext_records(snap_val: &[u8], live_val: &[u8]) -> Vec<u8> {
    use crate::file::{
        build_phys_ext_update_val, build_phys_ext_val, phys_ext_kind, phys_ext_refcnt,
        phys_ext_update_delta, APFS_KIND_UPDATE,
    };
    let block_count = u64_from_le(snap_val, 0) & 0x0FFF_FFFF_FFFF_FFFF;
    let snap_is_update = phys_ext_kind(snap_val) == APFS_KIND_UPDATE;
    let live_is_update = phys_ext_kind(live_val) == APFS_KIND_UPDATE;
    match (snap_is_update, live_is_update) {
        // Both absolute: the extent is referenced by both sides.
        (false, false) => {
            let owner = u64_from_le(snap_val, 8);
            let total = phys_ext_refcnt(snap_val).saturating_add(phys_ext_refcnt(live_val));
            build_phys_ext_val(block_count, owner, total)
        }
        // The snapshot owns the count; the live side only recorded a delta.
        (false, true) => {
            let owner = u64_from_le(snap_val, 8);
            let base = phys_ext_refcnt(snap_val) as i64 + phys_ext_update_delta(live_val) as i64;
            build_phys_ext_val(block_count, owner, base.max(0) as u32)
        }
        // The live side owns the count; the snapshot only recorded a delta.
        (true, false) => {
            let owner = u64_from_le(live_val, 8);
            let base = phys_ext_refcnt(live_val) as i64 + phys_ext_update_delta(snap_val) as i64;
            let block_count = u64_from_le(live_val, 0) & 0x0FFF_FFFF_FFFF_FFFF;
            build_phys_ext_val(block_count, owner, base.max(0) as u32)
        }
        // Neither owns a count: the deltas add up and stay a delta.
        (true, true) => build_phys_ext_update_val(
            block_count,
            phys_ext_update_delta(snap_val).saturating_add(phys_ext_update_delta(live_val)),
        ),
    }
}

/// Fold `src_recs` into the extentref tree at `dst_root`, rebuild it, and
/// release the nodes the old tree occupied.
///
/// Returns the new root, how many blocks the tree gained or lost, and how many
/// data blocks were released. A record the destination already carries is
/// combined rather than replaced: an UPDATE on either side is a signed delta
/// against the other's count, so applying it yields the count that survives.
/// A count that reaches zero is an extent nothing references any more - the
/// file was replaced or removed while the snapshot pinned its blocks - so the
/// record goes and the blocks are released, instead of a record claiming zero
/// references over blocks that stay allocated.
///
/// The two sides need not cut their extents at the same blocks: a write into
/// the middle of a file under a snapshot leaves the live tree a one-block
/// UPDATE inside the snapshot's longer record. Records are therefore split
/// wherever either side starts or ends one, and each piece is combined on
/// its own.
fn merge_extref_records_into<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    dst_root: u64,
    src_recs: &[(Vec<u8>, Vec<u8>)],
    new_xid: u64,
    bsz: usize,
) -> Result<(u64, i64, u64), TxnError> {
    use crate::file::{
        build_phys_ext_key, phys_ext_kind, phys_ext_refcnt, phys_ext_update_delta,
        phys_ext_with_blocks, APFS_KIND_UPDATE,
    };
    use std::collections::{BTreeMap, BTreeSet};
    const LEN_MASK: u64 = 0x0FFF_FFFF_FFFF_FFFF;
    // start block -> (block count, key, value)
    type ByStart = BTreeMap<u64, (u64, Vec<u8>, Vec<u8>)>;
    let by_start = |recs: &[(Vec<u8>, Vec<u8>)]| -> ByStart {
        recs.iter()
            .map(|(k, v)| {
                let start = u64_from_le(k, 0) & LEN_MASK;
                (start, (u64_from_le(v, 0) & LEN_MASK, k.clone(), v.clone()))
            })
            .collect()
    };
    let covering = |m: &ByStart, at: u64| -> Option<(u64, Vec<u8>)> {
        m.range(..=at)
            .next_back()
            .filter(|(s, (n, _, _))| s.saturating_add(*n) > at)
            .map(|(s, (_, _, v))| (*s, v.clone()))
    };
    let (dst_recs, _) = crate::file::collect_extref_records(txn, dst_root, bsz)?;
    let src = by_start(src_recs);
    let dst = by_start(&dst_recs);

    let mut cuts: BTreeSet<u64> = BTreeSet::new();
    for (s, (n, _, _)) in src.iter().chain(dst.iter()) {
        cuts.insert(*s);
        cuts.insert(s.saturating_add(*n));
    }
    let cuts: Vec<u64> = cuts.into_iter().collect();
    // (start, blocks, value, the src and dst records it lies in). Neighbouring
    // pieces of the same pair of records join back into one.
    type Piece = (u64, u64, Vec<u8>, (Option<u64>, Option<u64>));
    let mut pieces: Vec<Piece> = Vec::new();
    for w in cuts.windows(2) {
        let &[a, b] = w else { continue };
        let (val, from) = match (covering(&src, a), covering(&dst, a)) {
            (None, None) => continue,
            (Some((s, sv)), None) => (sv, (Some(s), None)),
            (None, Some((d, dv))) => (dv, (None, Some(d))),
            (Some((s, sv)), Some((d, dv))) => {
                (merge_phys_ext_records(&sv, &dv), (Some(s), Some(d)))
            }
        };
        match pieces.last_mut() {
            Some((ps, pn, pv, pfrom))
                if *pfrom == from
                    && ps.saturating_add(*pn) == a
                    && phys_ext_with_blocks(pv, 0) == phys_ext_with_blocks(&val, 0) =>
            {
                *pn += b - a;
            }
            _ => pieces.push((a, b - a, val, from)),
        }
    }

    let mut merged: BTreeMap<u64, Vec<u8>> = BTreeMap::new();
    let mut freed_blocks: u64 = 0;
    for (start, count, val, _) in pieces {
        let val = phys_ext_with_blocks(&val, count);
        if phys_ext_kind(&val) == APFS_KIND_UPDATE {
            // A delta the two sides cancel out records nothing.
            if phys_ext_update_delta(&val) != 0 {
                merged.insert(start, val);
            }
        } else if phys_ext_refcnt(&val) == 0 {
            if count > txn.nx.block_count {
                return Err(TxnError::SpacemanParse(format!(
                    "extent at {start} claims {count} blocks, more than the container"
                )));
            }
            for i in 0..count {
                txn.free_block(start.saturating_add(i))?;
            }
            freed_blocks += count;
        } else {
            merged.insert(start, val);
        }
    }

    let mut upserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut removes: Vec<Vec<u8>> = Vec::new();
    for (start, (_, key, val)) in &dst {
        match merged.get(start) {
            None => removes.push(key.clone()),
            Some(now) if now != val => upserts.push((key.clone(), now.clone())),
            Some(_) => {}
        }
    }
    for (start, val) in &merged {
        if !dst.contains_key(start) {
            upserts.push((build_phys_ext_key(*start), val.clone()));
        }
    }
    let (new_root, old_padrs, node_delta) = crate::file::rewrite_extref_tree(
        txn,
        dst_root,
        new_xid,
        Vec::new(),
        &removes,
        &upserts,
        bsz,
    )?;
    for p in old_padrs {
        txn.free_block(p)?;
    }
    Ok((new_root, node_delta, freed_blocks))
}

/// Roll the live volume back to snapshot `snap_xid`, the way macOS does:
/// the snapshots taken after it are deleted (newest first), the volume is
/// put back to the state the snapshot froze, and the snapshot itself stays.
/// Everything happens in this transaction, so the volume is either
/// reverted or untouched.
///
/// The latest snapshot is reverted to by turning its frozen state (catalog,
/// object-map entries, extent-reference tree) into the live volume, then
/// taking it again under its name and creation time: that is the state the
/// volume was in right after it was taken. Unlike on macOS, the snapshot
/// then names this transaction rather than the one it first froze.
///
/// ## Error conditions
/// - `SnapshotNotFound(snap_xid)` - no snapshot with that xid.
///
/// `vsb_raw` and `omap_raw` are the current live VSB and omap blocks.
pub fn revert_to_snapshot<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    snap_xid: u64,
) -> Result<(), TxnError> {
    // Reverting to a snapshot older than the latest discards the ones taken
    // after it, as macOS does (apfs.kext `omap_revert_to_snapshot` deletes
    // the object map's newer snapshots and the reaper their records). They
    // are deleted here, newest first, in this same transaction, so the
    // volume is either reverted or untouched.
    let all = list_snapshots(txn, vsb_raw, omap_raw)?;
    let Some(target) = all.iter().find(|s| s.xid == snap_xid).cloned() else {
        return Err(TxnError::SnapshotNotFound(snap_xid));
    };
    let mut newer: Vec<u64> = all
        .iter()
        .map(|s| s.xid)
        .filter(|&x| x > snap_xid)
        .collect();
    newer.sort_unstable_by(|a, b| b.cmp(a));
    for x in newer {
        delete_snapshot(txn, vsb_raw, omap_raw, x)?;
    }
    revert_to_latest_snapshot(txn, vsb_raw, omap_raw, snap_xid)?;
    // macOS keeps the snapshot it reverted to. Reverting here turns the
    // snapshot's frozen state into the live volume, which leaves the volume
    // exactly as it was when the snapshot was taken, so taking it again
    // now, under its name and creation time, gives the state macOS leaves.
    // The one difference: the snapshot names this transaction, not the one
    // it first froze.
    create_snapshot_made(
        txn,
        vsb_raw,
        omap_raw,
        &target.name,
        Some(target.create_time),
    )?;
    Ok(())
}

/// [`revert_to_snapshot`] for the most recent snapshot.
fn revert_to_latest_snapshot<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    snap_xid: u64,
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, omap_raw)?;
    let (vsb_raw, omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    crate::file::ensure_volume_mutable(vsb_raw)?;
    let bsz = txn.nx.block_size as usize;
    let revert_xid = txn.xid;
    let now_ns = apple_epoch_now_ns();
    // What the transaction took and released before this revert: the
    // operations that did it (the deletes of newer snapshots) already wrote
    // their share into apfs_fs_alloc_count.
    let allocated_before = txn.blocks_allocated();
    let freed_before = txn.blocks_freed();

    // --- Validate: snap_meta tree must exist and contain snap_xid ---
    let snap_meta_paddr = u64_from_le(vsb_raw, VSBI_SNAP_META_TREE_OID);
    if snap_meta_paddr == 0 {
        return Err(TxnError::SnapshotNotFound(snap_xid));
    }
    let (frozen_vsb_paddr, snap_extref_oid) =
        match read_snap_metadata_oids(txn, snap_meta_paddr, snap_xid) {
            Some((sb, er)) => (sb, er),
            None => return Err(TxnError::SnapshotNotFound(snap_xid)),
        };

    // --- Mid-chain guard: snap_xid must equal om_most_recent_snap ---
    // om_most_recent_snap is the XID of the newest snapshot. Reverting to
    // anything older would require freeing intermediate snapshot metadata -
    // deferred to a later milestone.
    let most_recent_snap = u64_from_le(omap_raw, OMAP_MOST_RECENT_SNAP_OFF);
    if snap_xid != most_recent_snap {
        return Err(TxnError::InvalidArgument(format!(
            "revert_to_snapshot: snap_xid {snap_xid:#x} is not the most-recent \
             snapshot {most_recent_snap:#x}; mid-chain revert is not supported"
        )));
    }

    // #139: rebuild the snapshot trees WITHOUT the reverted (most-recent)
    // snapshot, instead of clearing them outright. Any EARLIER snapshots must
    // survive the revert (R2). When the reverted snapshot was the only one,
    // `remaining` is empty and the trees are cleared (R1).
    let (all_entries, old_snap_meta_nodes) = read_snap_meta(txn, snap_meta_paddr)?;
    let old_snapshot_tree_root = u64_from_le(omap_raw, OMAP_SNAPSHOT_TREE_OID_OFF);
    let old_snapshot_tree_nodes = if old_snapshot_tree_root != 0 {
        omap_snapshot_tree_nodes(txn, old_snapshot_tree_root)?
    } else {
        Vec::new()
    };
    let remaining: Vec<SnapEntry> = all_entries
        .into_iter()
        .filter(|e| e.xid != snap_xid)
        .collect();
    let remaining_count = remaining.len() as u32;
    let remaining_most_recent = remaining.iter().map(|e| e.xid).max().unwrap_or(0);
    let bt_flags = read_btn_info_flags(txn, snap_meta_paddr).unwrap_or(DEFAULT_SNAP_META_BT_FLAGS);
    // As in delete_snapshot: the snapshot-metadata tree stays, empty, because
    // that is the shape a volume with no snapshots has on a macOS-formatted
    // disk. The om_snapshot tree goes to zero, which is that shape too.
    let (new_snap_meta_paddr, _) = write_snap_meta(txn, &remaining, revert_xid, bt_flags)?;
    let new_snapshot_tree_paddr = if remaining.is_empty() {
        0u64
    } else {
        let xids: Vec<u64> = remaining.iter().map(|e| e.xid).collect();
        write_omap_snapshot_tree(txn, &xids, revert_xid)?.0
    };

    // --- Read frozen VSB to extract snapshot's tree OIDs ---
    // The frozen VSB has apfs_omap_oid=0, apfs_extentref_tree_oid=0,
    // apfs_snap_meta_tree_oid=0 (canonical model). The snapshot's actual
    // extentref OID is in the snap_metadata_val record (= snap_extref_oid).
    // The snapshot's omap is shared with the live volume (omap_oid=0 in
    // frozen VSB means "use the live omap"). The snapshot's fsroot OID is
    // in the frozen VSB at VSBI_ROOT_TREE_OID (0x88), and its omap entry
    // with xid=snap_xid points to the frozen fsroot paddr.
    //
    // For revert we do NOT need to restore apfs_omap_oid - the live omap
    // already covers the snapshot's fsroot via its existing omap entries at
    // snap_xid. We do need to point the live extentref tree at the snapshot's
    // pinned extref tree (snap_extref_oid), which records extents as of snap_xid.
    //
    // [CERTAIN: canonical model from create_snapshot; frozen VSB zeroes those fields]
    let mut frozen_buf = vec![0u8; bsz];
    if frozen_vsb_paddr != 0 {
        txn.read_block(frozen_vsb_paddr, &mut frozen_buf)?;
    }
    // The snapshot's fsroot OID - same virtual OID as live (VSBI_ROOT_TREE_OID).
    // After revert the live omap will be COW'd to drop the live_xid entry and
    // leave the snap_xid entry as the highest xid for root_tree_oid.
    // (Conservative: we leave the omap entries intact and just bump the
    // omap-header's most_recent_snap to 0, relying on the snap_xid entry
    // being the highest xid visible for root_tree_oid after the live_xid
    // entry is removed via COW.)

    // --- Build updated live VSB ---
    let vsb_oid = u64_from_le(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    if new_vsb.len() < bsz {
        new_vsb.resize(bsz, 0);
    } else {
        new_vsb.truncate(bsz);
    }

    // The reverted state IS the snapshot's state, so the object counts are the
    // ones the snapshot froze - carrying the live ones forward left them
    // counting files the revert just removed ("apfs_num_files is not valid").
    // num_snapshots is deliberately not among them: it describes the volume
    // after the delete, and is written from the surviving set below.
    if frozen_vsb_paddr != 0 {
        for off in [
            VSBI_NUM_FILES,
            VSBI_NUM_DIRECTORIES,
            VSBI_NUM_SYMLINKS,
            VSBI_NUM_OTHER_FSOBJECTS,
        ] {
            write_u64_le(&mut new_vsb, off, u64_from_le(&frozen_buf, off));
        }
    }

    // Restore extentref tree to the snapshot's pinned extref.
    // COW it to revert_xid so its o_xid is consistent with the new checkpoint.
    let old_live_extref_paddr = u64_from_le(&new_vsb, VSBI_EXTENTREF_TREE_OID);
    if snap_extref_oid != 0 {
        let new_extref_paddr = txn.alloc_block()?;
        let mut eb = vec![0u8; bsz];
        txn.read_block(snap_extref_oid, &mut eb)?;
        write_u64_le(&mut eb, 8, new_extref_paddr);
        write_u64_le(&mut eb, 16, revert_xid);
        update_checksum_in_place(&mut eb);
        txn.stage_raw(new_extref_paddr, eb);
        write_u64_le(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
        // Free the old live extentref tree (orphaned) - every node of it, not
        // just the root, which past a single node would strand the rest.
        // The new live root is a root-only copy of the snapshot's root, so it
        // still points at the snapshot tree's children by physical address;
        // those blocks stay referenced and must be kept out of the free list.
        if old_live_extref_paddr != 0 && old_live_extref_paddr != snap_extref_oid {
            let (snap_recs, snap_nodes) =
                crate::file::collect_extref_records(txn, snap_extref_oid, bsz)?;
            let (live_recs, live_nodes) =
                crate::file::collect_extref_records(txn, old_live_extref_paddr, bsz)?;
            // An extent the live tree records and the snapshot's does not came
            // into being after the snapshot was taken. Nothing in the reverted
            // state can reach it, so leaving it allocated leaks it for good.
            // Only absolute records are acted on: a delta record describes an
            // extent that already existed, which the snapshot still holds.
            for (k, v) in &live_recs {
                if crate::file::phys_ext_kind(v) != crate::file::APFS_KIND_NEW {
                    continue;
                }
                if snap_recs.iter().any(|(sk, _)| sk == k) {
                    continue;
                }
                let paddr = u64_from_le(k, 0) & OBJ_ID_MASK;
                let blocks = u64_from_le(v, 0) & PHYS_EXT_LEN_MASK;
                for i in 0..blocks {
                    txn.free_block(paddr.saturating_add(i))?;
                }
            }
            for p in live_nodes {
                if !snap_nodes.contains(&p) {
                    txn.free_block(p)?;
                }
            }
        }
        // #139: the reverted snapshot's pinned extref tree was just COW'd to
        // become the live extref; the original block is now orphaned (the
        // reverted snapshot is removed and no surviving snapshot references it).
        // Free it to avoid fsck "overallocation".
        txn.free_block(snap_extref_oid)?;
    }

    // #139: point snap_meta_tree_oid at the rebuilt surviving-snapshots node
    // (0 when none remain).
    write_u64_le(&mut new_vsb, VSBI_SNAP_META_TREE_OID, new_snap_meta_paddr);
    // num_snapshots = count of surviving snapshots (was unconditionally 0).
    write_u64_le(&mut new_vsb, VSBI_NUM_SNAPSHOTS, remaining_count as u64);
    write_u64_le(&mut new_vsb, VSBI_LAST_MOD_TIME, now_ns);

    // --- COW the volume omap ---
    // We need to:
    //   1. Remove the live_xid entry for root_tree_oid from the vol-omap btree
    //      so the snap_xid entry becomes the highest visible xid.
    //   2. Reset om_most_recent_snap = 0 (no snapshots after revert).
    //   3. Reset om_snap_count = 0.
    //   4. Reset om_snapshot_tree_oid = 0 (no snapshot tree).
    {
        let mut new_omap = omap_raw.to_vec();
        if new_omap.len() < bsz {
            new_omap.resize(bsz, 0);
        } else {
            new_omap.truncate(bsz);
        }
        let old_vol_tree_paddr = u64_from_le(&new_omap, 48);
        let old_vomap_paddr = u64_from_le(vsb_raw, VSBI_OMAP_OID);
        let mut tree_buf = vec![0u8; bsz];
        txn.read_block(old_vol_tree_paddr, &mut tree_buf)?;

        // COW-7: drop the live_xid mapping for root_tree_oid from the volume
        // omap so the snap_xid entry becomes the highest visible xid for it.
        // Zeroing ov_paddr instead does not work: Omap::resolve takes the
        // highest xid <= the query and skips only OMAP_VAL_DELETED, never a
        // paddr of 0, so resolving root_tree_oid returned block 0 (the NX
        // superblock) as the fsroot and the volume would not mount.
        //
        // The tree is walked whole and rebuilt from its mappings. Decoding only
        // the root reads an index node as a leaf once the omap has split, which
        // would replace the entire map with one node and orphan every leaf.
        // [linux-apfs-rw btree.c apfs_btree_remove + node.c nkeys/space recompute;
        //  docs/refs/di-tier-verification-2026-05.md]
        let (triples, old_vol_tree_nodes) =
            crate::file::collect_omap_tree(txn, &tree_buf, old_vol_tree_paddr, bsz)?;
        // Every mapping stamped after the snapshot describes a version of an
        // object that the reverted state does not contain - the fsroot copy
        // create_snapshot made, and each catalog node a later write copied
        // aside. Keeping them left the map pointing at objects nothing reaches
        // ("Remove orphan/invalid omap mapping objects?"); dropping only the
        // fsroot one was enough on a volume small enough that it was the only
        // one. Their blocks lose their last referrer with the mapping, so they
        // are released unless a surviving mapping names the same address.
        let (dropped, kept): (Vec<OmapTriple>, Vec<OmapTriple>) =
            triples.into_iter().partition(|r| r.xid > snap_xid);
        let (new_vol_tree_paddr, _new_vol_tree_nodes) =
            crate::file::build_omap_tree(txn, &tree_buf, &kept, revert_xid, bsz)?;
        for r in &dropped {
            if r.paddr != 0 && !kept.iter().any(|k| k.paddr == r.paddr) {
                txn.free_block(r.paddr)?;
            }
        }

        let new_vomap_paddr = txn.alloc_block()?;
        write_u64_le(&mut new_omap, 8, new_vomap_paddr);
        write_u64_le(&mut new_omap, 16, revert_xid);
        // #139: snapshot counters reflect the SURVIVING snapshots (was 0).
        write_u32_le(&mut new_omap, OMAP_SNAP_COUNT_OFF, remaining_count);
        write_u64_le(
            &mut new_omap,
            OMAP_MOST_RECENT_SNAP_OFF,
            remaining_most_recent,
        );
        // Point at the rebuilt om_snapshot tree (0 when no snapshots remain).
        write_u64_le(
            &mut new_omap,
            OMAP_SNAPSHOT_TREE_OID_OFF,
            new_snapshot_tree_paddr,
        );
        write_u64_le(&mut new_omap, 48, new_vol_tree_paddr);
        update_checksum_in_place(&mut new_omap);
        txn.stage_raw(new_vomap_paddr, new_omap);
        write_u64_le(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);

        // Free orphaned physical blocks.
        for p in &old_vol_tree_nodes {
            txn.free_block(*p)?;
        }
        txn.free_block(old_vomap_paddr)?;
        for &p in &old_snapshot_tree_nodes {
            txn.free_block(p)?;
        }
    }

    // Free the snap_meta tree - the new VSB no longer references it.
    // Released before the superblock is staged so the tally below sees it; a
    // freed block stays invisible to the allocator until commit either way, so
    // moving the call earlier changes nothing else.
    for &p in &old_snap_meta_nodes {
        txn.free_block(p)?;
    }

    // --- fs_alloc_count ---
    // Ask the transaction what it did rather than tallying by hand: fsck
    // rebuilds this field by walking what the volume reaches, so what matters
    // is exactly the blocks this revert took and released. The field used to be
    // left alone entirely ("fsck can correct it"), which every revert reported
    // as "apfs_fs_alloc_count is not valid". The volume superblock about to be
    // staged is not counted - fsck excludes it by object type - and it is
    // allocated after this point, so it stays out of the figure on its own.
    // Only this revert's share counts: a revert to an older snapshot deletes
    // the newer ones first, in the same transaction, and adding their share
    // again left the field short by what they released.
    let net_delta = (txn.blocks_allocated() - allocated_before) as i64
        - (txn.blocks_freed() - freed_before) as i64;
    let cur_fs_alloc = u64_from_le(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    write_u64_le(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (cur_fs_alloc + net_delta).max(0) as u64,
    );

    // Stage the updated live VSB.
    let vsb_body: Vec<u8> = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    let padded_body = pad_to(vsb_body, bsz - 32);
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &padded_body)?;

    // The frozen VSB is the snapshot's own superblock, which create_snapshot
    // deliberately kept out of the live volume's fs_alloc_count. Releasing it
    // after the tally keeps it out on the way back too; counting it left the
    // field one block low.
    if frozen_vsb_paddr != 0 {
        txn.free_block(frozen_vsb_paddr)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Pad or truncate `v` to exactly `target_len` bytes.
fn pad_to(mut v: Vec<u8>, target_len: usize) -> Vec<u8> {
    if v.len() < target_len {
        v.resize(target_len, 0);
    } else {
        v.truncate(target_len);
    }
    v
}

/// Read `btn_info.bt_flags` (the first u32 of the 40-byte btree_info footer)
/// from an existing physical-addressed btree root block.
///
/// The snap_meta tree (per-volume B-tree, addressed by `vsb.snap_meta_tree_oid`)
/// is a PHYSICAL B-tree. Its root node always carries a `btree_info_t` footer
/// at `bsz - 40`; `bt_flags` is the first u32 of that footer.
///
/// Returns `None` on I/O failure or impossibly small block size. Callers
/// should fall back to a sane default in that case (we never panic).
///
/// [CERTAIN: btree.rs BTREE_INFO_SIZE=40; the APFS specification p.129 btree_info layout;
///  the APFS specification F.1]
fn read_btn_info_flags<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    paddr: u64,
) -> Option<u32> {
    let bsz = txn.nx.block_size as usize;
    if bsz < 40 {
        return None;
    }
    let mut buf = vec![0u8; bsz];
    txn.read_block(paddr, &mut buf).ok()?;
    Some(u32_from_le(&buf, bsz - 40))
}

/// Build a minimal empty APFS B-tree root block (FIXED_KV, LEAF, ROOT, 0 keys).
/// Now populates the 40-byte btree_info_t footer with caller-supplied
/// bt_flags + key_size + val_size - fsck rejects all-zero footers with
/// "invalid btn_btree.bt_fixed.bt_flags (0x0)" ( finding).
/// [CERTAIN: btree.rs BtreeNode layout; the APFS specification p.126 btree_info_t;
///  empirical fsck on macOS 15.7.4]
#[allow(clippy::too_many_arguments)]
fn build_empty_btree_root(
    oid: u64,
    xid: u64,
    o_type: u32,
    o_subtype: u32,
    bsz: usize,
    bt_flags: u32,
    key_size: u32,
    val_size: u32,
) -> Vec<u8> {
    let mut buf = vec![0u8; bsz];
    write_u64_le(&mut buf, 8, oid);
    write_u64_le(&mut buf, 16, xid);
    write_u32_le(&mut buf, 24, o_type);
    write_u32_le(&mut buf, 28, o_subtype);
    // btn_flags @32: ROOT|LEAF, plus FIXED_KV_SIZE only when caller declares
    // fixed key/val sizes. Variable-kv trees (key_size == 0) must NOT set
    // FIXED_KV (the APFS spec empirical: kernel BLOCKREFTREE root uses 0x03).
    let btn_flags: u16 = if key_size > 0 { 0x0007 } else { 0x0003 };
    write_u16_le(&mut buf, 32, btn_flags);
    // btn_nkeys @36 (u32) = 0 (empty tree)
    // Pre-reserve table_space - kernel uses 448B for fixed-kv omap btree
    // (empirical macOS 15.7.4). For variable-kv, conservative 64B.
    let toc_reserve: usize = if key_size > 0 { 448 } else { 64 };
    write_u16_le(&mut buf, 40, 0u16); // table_space.off = 0
    write_u16_le(&mut buf, 42, toc_reserve as u16); // table_space.len reserved
    write_u16_le(&mut buf, 44, 0u16); // free_space.off = 0 (no keys yet)
    let free_len = bsz.saturating_sub(56 + toc_reserve + 40);
    write_u16_le(&mut buf, 46, free_len as u16); // free_space.len
    write_u16_le(&mut buf, 48, 0xFFFFu16);
    write_u16_le(&mut buf, 50, 0u16);
    write_u16_le(&mut buf, 52, 0xFFFFu16);
    write_u16_le(&mut buf, 54, 0u16);
    // btree_info_t at end of root nodes (the APFS specification p.126, 40 bytes).
    let bti = bsz.saturating_sub(40);
    write_u32_le(&mut buf, bti, bt_flags); // bt_flags
    write_u32_le(&mut buf, bti + 4, bsz as u32); // bt_node_size
    write_u32_le(&mut buf, bti + 8, key_size); // bt_key_size
    write_u32_le(&mut buf, bti + 12, val_size); // bt_val_size
                                                // Confirmed: for FIXED_KV trees (key_size > 0), longest_key/val MUST
                                                // equal the fixed sizes (the APFS specification , the APFS specification p.88-89 BLOCKREFTREE).
                                                // For variable-kv (key_size = 0), leave longest_* at 0.
    if key_size > 0 {
        write_u32_le(&mut buf, bti + 16, key_size); // bt_longest_key
        write_u32_le(&mut buf, bti + 20, val_size); // bt_longest_val
    }
    // bt_key_count @bti+24 = 0
    write_u64_le(&mut buf, bti + 32, 1u64); // bt_node_count = 1
    update_checksum_in_place(&mut buf);
    buf
}

/// Current time as nanoseconds since 1970-01-01 00:00:00 UTC (UNIX epoch).
///
/// the APFS specification p.118 (j_snap_metadata_val.create_time): "represented as
/// the number of nanoseconds since January 1, 1970 at 0:00 UTC, disregarding
/// leap seconds." Despite the function name, APFS snapshot timestamps use
/// the UNIX epoch, NOT the Mac OS Apple epoch (2001-01-01).
/// [CERTAIN: the APFS specification p.118;]
fn apple_epoch_now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

// ---------------------------------------------------------------------------
// Field helpers
// ---------------------------------------------------------------------------

fn u64_from_le(buf: &[u8], off: usize) -> u64 {
    buf.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}

fn u32_from_le(buf: &[u8], off: usize) -> u32 {
    buf.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}

fn u16_from_le(buf: &[u8], off: usize) -> u16 {
    buf.get(off..off + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
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

fn write_u16_le(buf: &mut [u8], off: usize, val: u16) {
    if let Some(s) = buf.get_mut(off..off + 2) {
        s.copy_from_slice(&val.to_le_bytes());
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::txn::OBJECT_TYPE_BLOCKREFTREE;

    // Test shim: these layout tests exercise the node a single snapshot
    // makes, a root leaf of the snapshot metadata tree.
    struct SnapNodeArgs<'a> {
        paddr: u64,
        xid: u64,
        extentref_tree_oid: u64,
        extentref_tree_type: u32,
        sblock_oid: u64,
        now_ns: u64,
        name_len: u16,
        name_bytes: &'a [u8],
        bsz: usize,
        bt_flags: u32,
    }
    fn build_snap_meta_node(a: SnapNodeArgs<'_>) -> Result<Vec<u8>, TxnError> {
        let _ = a.name_len; // derived from name bytes in the multi builder
        let e = SnapEntry {
            xid: a.xid,
            extentref_tree_oid: a.extentref_tree_oid,
            extentref_tree_type: a.extentref_tree_type,
            sblock_oid: a.sblock_oid,
            create_time: a.now_ns,
            change_time: a.now_ns,
            inum: 2,
            name: a.name_bytes.to_vec(),
        };
        phys_tree::root_leaf(
            &snap_meta_spec(a.bt_flags),
            snap_meta_records(&[e]),
            a.paddr,
            a.xid,
            a.bsz,
        )
    }

    fn named(name: &[u8]) -> SnapEntry {
        let mut stored = name.to_vec();
        stored.push(0);
        SnapEntry {
            xid: 5,
            extentref_tree_oid: 0,
            extentref_tree_type: 0,
            sblock_oid: 0,
            create_time: 0,
            change_time: 0,
            inum: 20,
            name: stored,
        }
    }

    #[test]
    fn a_snapshot_name_must_fit_the_tree() {
        let existing = [named(b"daily")];
        assert!(check_snapshot_name("weekly", &existing).is_ok());
        assert!(check_snapshot_name(&"s".repeat(SNAP_MAX_NAME_LEN), &existing).is_ok());
        assert!(matches!(
            check_snapshot_name("", &existing),
            Err(TxnError::InvalidArgument(_))
        ));
        assert!(matches!(
            check_snapshot_name(&"s".repeat(SNAP_MAX_NAME_LEN + 1), &existing),
            Err(TxnError::InvalidArgument(_))
        ));
        assert!(matches!(
            check_snapshot_name("a\0b", &existing),
            Err(TxnError::InvalidArgument(_))
        ));
        assert!(matches!(
            check_snapshot_name("daily", &existing),
            Err(TxnError::AlreadyExists(_))
        ));
        // A prefix of a taken name is another name.
        assert!(check_snapshot_name("dail", &existing).is_ok());
    }

    #[test]
    fn encode_jkey_snap_metadata() {
        let k = encode_jkey(42, APFS_TYPE_SNAP_METADATA);
        // the APFS specification p.84: SNAP_METADATA = 1 ( fix).
        assert_eq!((k >> 60) & 0xF, 1);
        assert_eq!(k & 0x0FFF_FFFF_FFFF_FFFF, 42);
    }

    #[test]
    fn encode_jkey_snap_name() {
        let k = encode_jkey(99, APFS_TYPE_SNAP_NAME);
        // the APFS specification p.84: SNAP_NAME = 11 (0xB) ( fix).
        assert_eq!((k >> 60) & 0xF, 0xB);
        assert_eq!(k & 0x0FFF_FFFF_FFFF_FFFF, 99);
    }

    #[test]
    fn encode_jkey_snap_metadata_and_name_differ() {
        let k_meta = encode_jkey(5, APFS_TYPE_SNAP_METADATA);
        let k_name = encode_jkey(5, APFS_TYPE_SNAP_NAME);
        assert_ne!(k_meta, k_name);
    }

    #[test]
    fn encode_jkey_id_zero() {
        let k = encode_jkey(0, APFS_TYPE_SNAP_METADATA);
        assert_eq!((k >> 60) & 0xF, 1);
        assert_eq!(k & 0x0FFF_FFFF_FFFF_FFFF, 0);
    }

    #[test]
    fn empty_btree_root_fletcher_valid() {
        let bsz = 4096;
        let block = build_empty_btree_root(
            100,
            5,
            OBJ_PHYSICAL | OBJECT_TYPE_BTREE,
            OBJECT_TYPE_BLOCKREFTREE,
            bsz,
            0x0000_0052,
            0,
            0,
        );
        assert_eq!(block.len(), bsz);
        let stored = u64::from_le_bytes(block[0..8].try_into().unwrap());
        let computed = apfs_core::checksum::fletcher64(&block);
        assert_eq!(stored, computed);
        // nkeys @36 = 0.
        let nkeys = u32::from_le_bytes(block[36..40].try_into().unwrap());
        assert_eq!(nkeys, 0);
        // Variable-kv (key_size == 0) → btn_flags = ROOT | LEAF (0x03), NO FIXED_KV.
        let flags = u16::from_le_bytes(block[32..34].try_into().unwrap());
        assert_eq!(flags & 0x0007, 0x0003);
        // table_space.off = 0, table_space.len = 64 (variable-kv reserve).
        assert_eq!(u16::from_le_bytes(block[40..42].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(block[42..44].try_into().unwrap()), 64);
        assert_eq!(u16::from_le_bytes(block[44..46].try_into().unwrap()), 0);
        assert!(u16::from_le_bytes(block[46..48].try_into().unwrap()) > 0);
        assert_eq!(
            u16::from_le_bytes(block[48..50].try_into().unwrap()),
            0xFFFF
        );
        assert_eq!(u16::from_le_bytes(block[50..52].try_into().unwrap()), 0);
        assert_eq!(
            u16::from_le_bytes(block[52..54].try_into().unwrap()),
            0xFFFF
        );
        assert_eq!(u16::from_le_bytes(block[54..56].try_into().unwrap()), 0);
    }

    #[test]
    fn snap_meta_node_two_records_valid_checksum() {
        let bsz = 4096;
        let name_bytes = b"test-snap\0";
        let node = build_snap_meta_node(SnapNodeArgs {
            paddr: 200,
            xid: 7,
            extentref_tree_oid: 0xAABB,
            extentref_tree_type: 0x40000002,
            sblock_oid: 0xCCDD,
            now_ns: 12345678,
            name_len: name_bytes.len() as u16,
            name_bytes,
            bsz,
            bt_flags: DEFAULT_SNAP_META_BT_FLAGS,
        })
        .unwrap();
        assert_eq!(node.len(), bsz);
        let stored = u64::from_le_bytes(node[0..8].try_into().unwrap());
        let computed = apfs_core::checksum::fletcher64(&node);
        assert_eq!(stored, computed);
        let nkeys = u32::from_le_bytes(node[36..40].try_into().unwrap());
        assert_eq!(nkeys, 2);
        let flags = u16::from_le_bytes(node[32..34].try_into().unwrap());
        assert!(flags & 0x0002 != 0, "must be a leaf");
        assert_eq!(u16::from_le_bytes(node[40..42].try_into().unwrap()), 0);
        // table_space.len = 64 (variable-kv reserve; empirically fsck-CLEAN).
        assert_eq!(u16::from_le_bytes(node[42..44].try_into().unwrap()), 64);
        assert!(u16::from_le_bytes(node[44..46].try_into().unwrap()) > 0);
        assert!(u16::from_le_bytes(node[46..48].try_into().unwrap()) > 0);
        assert_eq!(u16::from_le_bytes(node[48..50].try_into().unwrap()), 0xFFFF);
        assert_eq!(u16::from_le_bytes(node[50..52].try_into().unwrap()), 0);
        assert_eq!(u16::from_le_bytes(node[52..54].try_into().unwrap()), 0xFFFF);
        assert_eq!(u16::from_le_bytes(node[54..56].try_into().unwrap()), 0);
    }

    #[test]
    fn delete_snapshot_decrements_num_snapshots() {
        let mut vsb = vec![0u8; 4096];
        vsb[32..36].copy_from_slice(&0x4253_5041u32.to_le_bytes());
        write_u64_le(&mut vsb, VSBI_NUM_SNAPSHOTS, 2u64);
        let new_num = u64_from_le(&vsb, VSBI_NUM_SNAPSHOTS).saturating_sub(1);
        write_u64_le(&mut vsb, VSBI_NUM_SNAPSHOTS, new_num);
        assert_eq!(u64_from_le(&vsb, VSBI_NUM_SNAPSHOTS), 1);
    }

    /// Q5 assertion: revert fields must be left untouched (= 0) during
    /// normal snapshot create.
    #[test]
    fn q5_revert_fields_untouched() {
        let vsb = vec![0u8; 4096];
        // apfs_revert_to_xid @0xA0 and apfs_revert_to_sblock_oid @0xA8
        // must remain 0 after a normal snapshot create.
        assert_eq!(u64_from_le(&vsb, 0xA0), 0, "Q5: revert_to_xid must be 0");
        assert_eq!(
            u64_from_le(&vsb, 0xA8),
            0,
            "Q5: revert_to_sblock_oid must be 0"
        );
    }

    /// Q4 assertion: snap_meta_ext_oid at 0x3E8 is not written by create_snapshot.
    #[test]
    fn q4_snap_meta_ext_oid_not_written() {
        let vsb = vec![0u8; 4096];
        assert_eq!(
            u64_from_le(&vsb, 0x3E8),
            0,
            "Q4: snap_meta_ext_oid is 0; create_snapshot must not touch it"
        );
    }

    /// Q3 assertion: snapshot metadata val records OLD extentref_tree_oid.
    #[test]
    fn q3_snapshot_metadata_records_old_extentref_oid() {
        let bsz = 4096;
        let mut vsb = vec![0u8; bsz];
        let old_oid: u64 = 0xDEAD_BEEF_1234_5678;
        write_u64_le(&mut vsb, VSBI_EXTENTREF_TREE_OID, old_oid);

        let old_extentref_oid = u64_from_le(&vsb, VSBI_EXTENTREF_TREE_OID);
        assert_eq!(old_extentref_oid, old_oid);

        // Simulate what create_snapshot does: build snap_meta val1 with old_oid.
        let name_bytes = b"snap\0";
        let node = build_snap_meta_node(SnapNodeArgs {
            paddr: 300,
            xid: 8,
            extentref_tree_oid: old_extentref_oid,
            extentref_tree_type: 0x40000002,
            sblock_oid: 0xBEEF,
            now_ns: 0,
            name_len: name_bytes.len() as u16,
            name_bytes,
            bsz,
            bt_flags: DEFAULT_SNAP_META_BT_FLAGS,
        })
        .unwrap();
        // val1 starts in the value area near end of block. The extentref_tree_oid
        // is the first u64 field of apfs_snap_metadata_val. We verify the
        // node contains old_oid bytes somewhere in the block.
        let old_oid_bytes = old_oid.to_le_bytes();
        let found = node.windows(8).any(|w| w == old_oid_bytes);
        assert!(
            found,
            "Q3: snap meta node must contain old extentref_tree_oid"
        );
    }

    // -----------------------------------------------------------------------
    // SNAP_NAME key layout - Apple File System Reference PAGE 119 verbatim.
    //
    //   struct j_snap_name_key {
    //       j_key_t  hdr;        // 8 bytes
    //       uint16_t name_len;   // 2 bytes
    //       uint8_t  name[0];    // variable
    //   } __attribute__((packed));
    //
    //   "The object identifier in the header is always ~0ULL."
    //
    // These three tests are the RED regression guards for the bug surfaced by
    // fsck_apfs on the populated real USB on 2026-05-20 ("snapshot name
    // (id 145): invalid key length (8)", "Snapshot metadata tree is invalid").
    // [CERTAIN: Apple PAGE 119 the APFS specification]
    // -----------------------------------------------------------------------

    /// Locate the (absolute offset, length) of the SNAP_NAME key (record 2)
    /// inside the snap_meta tree node `buf` via the var-kv TOC.
    fn locate_key2_in_node(buf: &[u8]) -> (usize, usize) {
        const DATA_BASE: usize = 56;
        const TOC_ENTRY_SIZE: usize = 8;
        // btn_table_space.len at offset 42 (high u16 of u32 at @40).
        let toc_len_raw = u32::from_le_bytes(buf[40..44].try_into().unwrap());
        let toc_len = (toc_len_raw >> 16) as usize;
        let key_area_start = DATA_BASE + toc_len;
        let toc1 = DATA_BASE + TOC_ENTRY_SIZE;
        let entry = u32::from_le_bytes(buf[toc1..toc1 + 4].try_into().unwrap());
        let k2_off = (entry & 0xFFFF) as usize;
        let k2_len = ((entry >> 16) & 0xFFFF) as usize;
        (key_area_start + k2_off, k2_len)
    }

    #[test]
    fn snap_name_key_includes_name_len_field_before_name_bytes() {
        let bsz = 4096;
        let name_bytes = b"test\0";
        let node = build_snap_meta_node(SnapNodeArgs {
            paddr: 0x100,
            xid: 0x91,
            extentref_tree_oid: 0xAA,
            extentref_tree_type: 0x40000002,
            sblock_oid: 0xBB,
            now_ns: 0xC0,
            name_len: name_bytes.len() as u16,
            name_bytes,
            bsz,
            bt_flags: DEFAULT_SNAP_META_BT_FLAGS,
        })
        .unwrap();

        let (k2_abs, k2_len) = locate_key2_in_node(&node);
        // Per Apple PAGE 119: key = j_key_t(8) + name_len:u16(2) + name(N).
        assert_eq!(
            k2_len,
            8 + 2 + name_bytes.len(),
            "SNAP_NAME key length must equal sizeof(j_key_t)+sizeof(uint16_t)+name_len; \
             prior bug omitted the name_len field"
        );
        let name_len_le = u16::from_le_bytes(node[k2_abs + 8..k2_abs + 10].try_into().unwrap());
        assert_eq!(
            name_len_le as usize,
            name_bytes.len(),
            "SNAP_NAME key bytes [8..10] must be the uint16_t name_len LE"
        );
        assert_eq!(
            &node[k2_abs + 10..k2_abs + 10 + name_bytes.len()],
            name_bytes,
            "SNAP_NAME key name bytes (null-terminated) must follow name_len"
        );
    }

    #[test]
    fn snap_name_key_obj_id_is_all_ones() {
        let bsz = 4096;
        let name_bytes = b"x\0";
        let node = build_snap_meta_node(SnapNodeArgs {
            paddr: 0x100,
            xid: 0x91,
            extentref_tree_oid: 0xAA,
            extentref_tree_type: 0x40000002,
            sblock_oid: 0xBB,
            now_ns: 0,
            name_len: name_bytes.len() as u16,
            name_bytes,
            bsz,
            bt_flags: DEFAULT_SNAP_META_BT_FLAGS,
        })
        .unwrap();

        let (k2_abs, _) = locate_key2_in_node(&node);
        let obj_id_and_type = u64::from_le_bytes(node[k2_abs..k2_abs + 8].try_into().unwrap());
        let obj_id = obj_id_and_type & 0x0FFF_FFFF_FFFF_FFFF;
        let key_type = (obj_id_and_type >> 60) & 0xF;
        assert_eq!(
            obj_id, SNAP_NAME_OBJ_ID,
            "SNAP_NAME j_key_t.obj_id must be ~0ULL (60-bit all-ones); \
             prior bug used the snapshot xid"
        );
        assert_eq!(
            key_type, APFS_TYPE_SNAP_NAME,
            "SNAP_NAME j_key_t type bits must be APFS_TYPE_SNAP_NAME (0xC)"
        );
    }

    #[test]
    fn snap_meta_node_uses_provided_bt_flags_in_footer() {
        // bt_flags must be threaded from the caller (read from the existing
        // snap_meta tree root, not hardcoded). Different APFS versions can
        // have different flags (e.g. BTREE_HASHED 0x80 on APFS 2 hashed-name
        // volumes). The writer must be version-resilient across every disk.
        let bsz = 4096;
        let name_bytes = b"snap\0";
        let chosen_flags: u32 = 0x0000_00D2; // 0x52 | 0x80 (HASHED)
        let node = build_snap_meta_node(SnapNodeArgs {
            paddr: 1,
            xid: 1,
            extentref_tree_oid: 0,
            extentref_tree_type: 0x40000002,
            sblock_oid: 0,
            now_ns: 0,
            name_len: name_bytes.len() as u16,
            name_bytes,
            bsz,
            bt_flags: chosen_flags,
        })
        .unwrap();
        let bti = bsz - 40;
        let bt_flags = u32::from_le_bytes(node[bti..bti + 4].try_into().unwrap());
        assert_eq!(
            bt_flags, chosen_flags,
            "bt_flags must be preserved from the caller; hardcoding breaks \
             on volumes with different btree_info flags (e.g. BTREE_HASHED)"
        );
    }

    #[test]
    fn apple_epoch_now_ns_is_reasonable() {
        let ns = apple_epoch_now_ns();
        // : this returns UNIX-epoch ns (APFS timestamps are UNIX-epoch,
        // not Apple-epoch). Valid range ~2023-2035 in UNIX epoch seconds.
        let min_ns: u64 = 1_600_000_000 * 1_000_000_000;
        let max_ns: u64 = 2_100_000_000 * 1_000_000_000;
        assert!(
            ns >= min_ns && ns <= max_ns,
            "apple_epoch_now_ns = {ns} outside expected range"
        );
    }
}
