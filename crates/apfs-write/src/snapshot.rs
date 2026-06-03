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
const VSBI_OMAP_OID: usize = 0x80;
const VSBI_EXTENTREF_TREE_OID: usize = 0x90;
const VSBI_SNAP_META_TREE_OID: usize = 0x98;
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
    let existing_snap_meta_paddr = u64_from_le(vsb_raw, VSBI_SNAP_META_TREE_OID);
    let bt_flags = if existing_snap_meta_paddr != 0 {
        read_btn_info_flags(txn, existing_snap_meta_paddr).unwrap_or(DEFAULT_SNAP_META_BT_FLAGS)
    } else {
        DEFAULT_SNAP_META_BT_FLAGS
    };
    let snap_node_paddr = txn.alloc_block()?;
    // Preserve VSB.apfs_extentref_tree_type @ 0x78 - the kernel's authoritative
    // o_type for the volume's extent-ref tree. Apple snap_metadata_val.extentref_tree_type
    // copies this verbatim (NOT a subtype). [ LOCKED, the APFS specification p.117]
    let old_extentref_type = u32_from_le(vsb_raw, VSBI_EXTENTREF_TREE_TYPE);
    // #139: RETAIN existing snapshots. Read every entry already in the
    // snap_meta tree, append this new snapshot, and rebuild the node with the
    // full set so the on-disk tree matches the (incremented) snapshot counts.
    // Previously this rebuilt a single-entry node, silently dropping every
    // earlier snapshot and triggering fsck "Snapshot is invalid".
    let mut entries: Vec<SnapEntry> = if existing_snap_meta_paddr != 0 {
        read_all_snap_entries(txn, existing_snap_meta_paddr)?
    } else {
        Vec::new()
    };
    entries.push(SnapEntry {
        xid: snap_xid,
        extentref_tree_oid: old_extentref_oid,
        extentref_tree_type: old_extentref_type,
        sblock_oid: frozen_paddr,
        create_time: now_ns,
        change_time: now_ns,
        inum: 2, // ROOT_DIR_INO_NUM ( empirical)
        name: name_bytes.clone(),
    });
    entries.sort_by_key(|e| e.xid);
    let snap_xids: Vec<u64> = entries.iter().map(|e| e.xid).collect();
    let _ = name_len; // name carried inline in SnapEntry
    let snap_node = build_snap_meta_node_multi(&entries, snap_node_paddr, snap_xid, bsz, bt_flags)?;
    txn.stage_raw(snap_node_paddr, snap_node);

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
    // fsck checks om_snap_count matches snapshots found
    // in om_snapshot_tree. Build a populated single-entry snapshot tree
    // with one omap_snapshot_t record keyed by snap_xid.
    let new_snapshot_tree_paddr = txn.alloc_block()?;
    // #139: rebuild the om_snapshot tree with EVERY live snapshot xid (not just
    // the new one), matching om_snap_count. The old tree is freed below.
    let snapshot_tree_block =
        build_omap_snapshot_tree(&snap_xids, new_snapshot_tree_paddr, snap_xid, bsz)?;
    txn.stage_raw(new_snapshot_tree_paddr, snapshot_tree_block);
    // Free the previous om_snapshot tree (if any) - rebuilt fresh above, so the
    // old block is orphaned. Leaking it caused fsck "overallocation" on the
    // 2nd+ snapshot. [#139]
    let old_snapshot_tree_paddr = u64_from_le(omap_raw, OMAP_SNAPSHOT_TREE_OID_OFF);

    {
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
        let new_vol_tree_paddr = txn.alloc_block()?;
        let mut tree_buf = vec![0u8; bsz];
        txn.read_block(old_vol_tree_paddr, &mut tree_buf)?;
        write_u64_le(&mut tree_buf, 8, new_vol_tree_paddr);
        // Live-side: vol-tree is stamped at live_xid (two-xid model).
        write_u64_le(&mut tree_buf, 16, live_xid);
        // M7b-RT Subtask E continuation (TWO-XID empirical refinement):
        // (1) find the current fsroot paddr (highest-xid root_oid entry),
        // (2) COW the fsroot to a new paddr stamped with o_xid=live_xid,
        // (3) APPEND `{root_oid, live_xid} -> new_fsroot_paddr` to the
        //     volume omap b-tree's TOC + key/val arrays.
        // The old fsroot paddr stays referenced via the existing omap
        // entry at its original xid (snapshot's frozen view at snap_xid
        // resolves to that older entry; fsck walks live via xid > snap).
        let root_tree_oid = u64_from_le(vsb_raw, 0x88); // VSBI_ROOT_TREE_OID
        let nkeys = u32_from_le(&tree_buf, 36) as usize;
        let toc_len = {
            let arr: [u8; 2] = tree_buf
                .get(42..44)
                .ok_or_else(|| TxnError::SpacemanParse("vol omap toc_len oob".into()))?
                .try_into()
                .map_err(|_| TxnError::SpacemanParse("vol omap toc_len cast".into()))?;
            u16::from_le_bytes(arr) as usize
        };
        let key_area_start = 56 + toc_len;
        let val_area_end = bsz - 40;
        let mut fsroot_paddr_at_max: u64 = 0;
        let mut have_live_xid_entry = false;
        let mut max_xid = 0u64;
        for i in 0..nkeys {
            let toc_off = 56 + i * 4;
            let k_off_a: [u8; 2] = tree_buf
                .get(toc_off..toc_off + 2)
                .ok_or_else(|| TxnError::SpacemanParse("vol omap TOC k_off oob".into()))?
                .try_into()
                .map_err(|_| TxnError::SpacemanParse("vol omap TOC k_off cast".into()))?;
            let v_off_a: [u8; 2] = tree_buf
                .get(toc_off + 2..toc_off + 4)
                .ok_or_else(|| TxnError::SpacemanParse("vol omap TOC v_off oob".into()))?
                .try_into()
                .map_err(|_| TxnError::SpacemanParse("vol omap TOC v_off cast".into()))?;
            let k_off = u16::from_le_bytes(k_off_a) as usize;
            let v_off = u16::from_le_bytes(v_off_a) as usize;
            let k_abs = key_area_start + k_off;
            let v_abs = val_area_end - v_off;
            let oid = u64_from_le(&tree_buf, k_abs);
            let xid = u64_from_le(&tree_buf, k_abs + 8);
            if oid == root_tree_oid {
                if xid == live_xid {
                    have_live_xid_entry = true;
                }
                if xid >= max_xid {
                    max_xid = xid;
                    fsroot_paddr_at_max = u64_from_le(&tree_buf, v_abs + 8);
                }
            }
        }
        if fsroot_paddr_at_max != 0 && !have_live_xid_entry {
            // (2) COW the fsroot to a new paddr stamped at LIVE_xid (two-xid).
            // fsroot is a VIRTUAL object - `o_oid` is its virtual oid
            // (= apfs_root_tree_oid, e.g. 0x404), NOT its paddr. Only
            // `o_xid` is bumped to live_xid; o_oid stays unchanged.
            let new_fsroot_paddr = txn.alloc_block()?;
            let mut fsroot_buf = vec![0u8; bsz];
            txn.read_block(fsroot_paddr_at_max, &mut fsroot_buf)?;
            write_u64_le(&mut fsroot_buf, 16, live_xid);
            update_checksum_in_place(&mut fsroot_buf);
            txn.stage_raw(new_fsroot_paddr, fsroot_buf);
            // (3) APPEND `{root_oid, live_xid} -> new_fsroot_paddr` to tree_buf.
            // fsck identifies this as the "live" fsroot via xid > om_most_recent_snap.
            let new_idx = nkeys;
            let toc_off = 56 + new_idx * 4;
            let k_off_new = new_idx * 16;
            let v_off_new = (new_idx + 1) * 16;
            write_u16_le(&mut tree_buf, toc_off, k_off_new as u16);
            write_u16_le(&mut tree_buf, toc_off + 2, v_off_new as u16);
            let k_abs = key_area_start + k_off_new;
            write_u64_le(&mut tree_buf, k_abs, root_tree_oid);
            write_u64_le(&mut tree_buf, k_abs + 8, live_xid);
            let v_abs = val_area_end - v_off_new;
            write_u32_le(&mut tree_buf, v_abs, 0); // ov_flags
            write_u32_le(&mut tree_buf, v_abs + 4, bsz as u32); // ov_size
            write_u64_le(&mut tree_buf, v_abs + 8, new_fsroot_paddr);
            let new_nkeys = (nkeys + 1) as u32;
            write_u32_le(&mut tree_buf, 36, new_nkeys);
            let used_key = new_nkeys as usize * 16;
            let used_val = new_nkeys as usize * 16;
            write_u16_le(&mut tree_buf, 44, used_key as u16);
            let free_len = val_area_end - key_area_start - used_key - used_val;
            write_u16_le(&mut tree_buf, 46, free_len as u16);
            write_u64_le(&mut tree_buf, val_area_end + 24, new_nkeys as u64);
        }
        update_checksum_in_place(&mut tree_buf);
        txn.stage_raw(new_vol_tree_paddr, tree_buf);
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
        // Free old volume-omap header + b-tree node - orphaned after the patch.
        txn.free_block(old_vomap_paddr)?;
        txn.free_block(old_vol_tree_paddr)?;
    }

    // M7b-RT Subtask A: free the OLD snap-meta tree root if it existed. The
    // live VSB now points at `snap_node_paddr` (a freshly-built node with the
    // new snap records); the frozen VSB has snap_meta_tree_oid = 0 (snapshots
    // have no nested snapshots). Nothing references the old root anymore.
    if existing_snap_meta_paddr != 0 {
        txn.free_block(existing_snap_meta_paddr)?;
    }
    // #139: free the previous om_snapshot tree (rebuilt above for all snaps).
    if old_snapshot_tree_paddr != 0 {
        txn.free_block(old_snapshot_tree_paddr)?;
    }

    // Finalize the frozen VSB now that apfs_omap_oid has been patched.
    update_checksum_in_place(&mut frozen);
    txn.stage_raw(frozen_paddr, frozen);

    // M7b-RT Subtask E (CANONICAL model, two-xid + empty-extref):
    // fs_alloc_count semantics (apfsprogs/apfsck/super.c:1175): the field
    // must equal the volume's total block count - every alloc bumps it,
    // every free decrements. We tally the net delta mechanically here.
    //
    // ALLOCs (+7): frozen VSB, EMPTY live extref tree (old paddr retained
    //   by snap_metadata_val), snap-meta node, om_snapshot_tree, new vomap
    //   header (COW pair), new vol-tree (COW pair), live fsroot COW.
    // FREEs (-2 or -3): old vomap header, old vol-tree (always); old
    //   snap-meta tree root (replaced when it existed).
    // Net: +5 (no existing snap_meta), +4 (existing).
    let mut net_delta: i64 = 7 - 2; // +5
    if existing_snap_meta_paddr != 0 {
        net_delta -= 1;
    }
    // #139: also freed the previous om_snapshot tree when one existed.
    if old_snapshot_tree_paddr != 0 {
        net_delta -= 1;
    }
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
    let bsz = txn.nx.block_size as usize;
    let mut buf = vec![0u8; bsz];
    txn.read_block(snap_meta_tree_paddr, &mut buf).ok()?;

    let toc_len = (u32_from_le(&buf, 40) >> 16) as usize;
    let nkeys = u32_from_le(&buf, 36) as usize;
    let toc_base: usize = 56;
    let key_area_start = 56 + toc_len;
    let val_area_end = bsz.saturating_sub(40);

    for i in 0..nkeys {
        let toc_off = toc_base + i * 8;
        if toc_off + 8 > bsz {
            break;
        }
        let key_off_rel = (u32_from_le(&buf, toc_off) & 0xFFFF) as usize;
        let val_off_rel = (u32_from_le(&buf, toc_off + 4) & 0xFFFF) as usize;

        let k_off = key_area_start + key_off_rel;
        if k_off + 8 > bsz {
            continue;
        }
        let obj_id_and_type = u64_from_le(&buf, k_off);
        let key_type = (obj_id_and_type >> 60) & 0xF;
        let obj_id = obj_id_and_type & 0x0FFF_FFFF_FFFF_FFFF;

        if key_type == APFS_TYPE_SNAP_METADATA && obj_id == snap_xid {
            let v_abs = val_area_end.saturating_sub(val_off_rel);
            if v_abs + 16 > bsz {
                continue;
            }
            let extentref_tree_oid = u64_from_le(&buf, v_abs);
            let sblock_oid = u64_from_le(&buf, v_abs + 8);
            return Some((sblock_oid, extentref_tree_oid));
        }
    }
    None
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

/// Parse every SNAP_METADATA record from a single-node snap_meta tree into a
/// list of [`SnapEntry`], sorted ascending by xid. Mirrors the value layout
/// written by `build_snap_meta_node*` (extentref_oid@0, sblock@8,
/// create_time@16, change_time@24, inum@32, extentref_type@40, flags@44,
/// name_len@48, name@50).
fn read_all_snap_entries<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    snap_meta_tree_paddr: u64,
) -> Result<Vec<SnapEntry>, TxnError> {
    let bsz = txn.nx.block_size as usize;
    let mut buf = vec![0u8; bsz];
    txn.read_block(snap_meta_tree_paddr, &mut buf)?;

    // btn_level @34 must be 0 - this engine only writes single-node snap_meta
    // trees. A multi-level tree would be mis-parsed as a flat leaf.
    if u16_from_le(&buf, 34) != 0 {
        return Err(TxnError::InvalidArgument(
            "snap_meta tree is multi-level; not supported".into(),
        ));
    }

    let toc_len = (u32_from_le(&buf, 40) >> 16) as usize;
    let nkeys = u32_from_le(&buf, 36) as usize;
    let toc_base: usize = 56;
    let key_area_start = 56 + toc_len;
    let val_area_end = bsz.saturating_sub(40);

    let mut out: Vec<SnapEntry> = Vec::new();
    for i in 0..nkeys {
        let toc_off = toc_base + i * 8;
        if toc_off + 8 > bsz {
            break;
        }
        let key_off_rel = (u32_from_le(&buf, toc_off) & 0xFFFF) as usize;
        let val_off_rel = (u32_from_le(&buf, toc_off + 4) & 0xFFFF) as usize;
        let k_off = key_area_start + key_off_rel;
        if k_off + 8 > bsz {
            continue;
        }
        let hdr = u64_from_le(&buf, k_off);
        let key_type = (hdr >> 60) & 0xF;
        let obj_id = hdr & 0x0FFF_FFFF_FFFF_FFFF;
        if key_type != APFS_TYPE_SNAP_METADATA {
            continue; // SNAP_NAME records are redundant; skip.
        }
        let v = val_area_end.saturating_sub(val_off_rel);
        // fixed prefix (50 bytes) must be present before reading name_len.
        if v + 50 > bsz {
            continue;
        }
        let extentref_tree_oid = u64_from_le(&buf, v);
        let sblock_oid = u64_from_le(&buf, v + 8);
        let create_time = u64_from_le(&buf, v + 16);
        let change_time = u64_from_le(&buf, v + 24);
        let inum = u64_from_le(&buf, v + 32);
        let extentref_tree_type = u32_from_le(&buf, v + 40);
        let name_len = u16_from_le(&buf, v + 48) as usize;
        let name_start = v + 50;
        if name_start + name_len > bsz || name_len == 0 {
            continue;
        }
        let name = buf
            .get(name_start..name_start + name_len)
            .unwrap_or(&[])
            .to_vec();
        out.push(SnapEntry {
            xid: obj_id,
            extentref_tree_oid,
            extentref_tree_type,
            sblock_oid,
            create_time,
            change_time,
            inum,
            name,
        });
    }
    out.sort_by_key(|e| e.xid);
    Ok(out)
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

/// Build a single-node snap_meta tree (root+leaf) holding every entry in
/// `entries`. Records are emitted in `apfs_keycmp` order: all SNAP_METADATA
/// (ascending by xid), then all SNAP_NAME (ascending by `strcmp(name)`).
/// Returns `InvalidArgument` if the records do not fit one node.
/// [clean-room: generalized from `build_snap_meta_node`; ordering per
///  linux-apfs-rw key.c `apfs_keycmp` (id, type, number, strcmp(name))]
fn build_snap_meta_node_multi(
    entries: &[SnapEntry],
    paddr: u64,
    xid: u64,
    bsz: usize,
    bt_flags: u32,
) -> Result<Vec<u8>, TxnError> {
    const BTREE_INFO_SIZE: usize = 40;
    const DATA_BASE: usize = 56;
    let val_area_end = bsz.saturating_sub(BTREE_INFO_SIZE);

    // Build (key, val) records in final TOC order.
    let mut recs: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(entries.len() * 2);

    // SNAP_METADATA records, ascending by xid (entries already sorted).
    for e in entries {
        let key = encode_jkey(e.xid, APFS_TYPE_SNAP_METADATA)
            .to_le_bytes()
            .to_vec();
        recs.push((key, encode_snap_metadata_val(e)));
    }

    // SNAP_NAME records, ascending by strcmp(name). All share the same
    // (obj_id, type) header, so the on-disk order is the name byte order.
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

    let nkeys = recs.len();
    // Pre-reserve table space: kernel keeps a minimum of 64 bytes (8 kvloc
    // slots) even for a 2-record node ( empirical); grow to fit nkeys.
    let toc_len = core::cmp::max(64usize, nkeys * 8);
    let key_area_start = DATA_BASE + toc_len;

    // Total key / value bytes (keys grow forward, values backward from end).
    let key_bytes: usize = recs.iter().map(|(k, _)| k.len()).sum();
    let value_bytes: usize = recs.iter().map(|(_, v)| v.len()).sum();
    let value_start = val_area_end
        .checked_sub(value_bytes)
        .ok_or_else(|| TxnError::InvalidArgument("snap_meta: values overflow node".into()))?;
    let key_end = key_area_start + key_bytes;
    if key_end > value_start {
        return Err(TxnError::InvalidArgument(format!(
            "snap_meta: {nkeys} records ({key_bytes}B keys + {value_bytes}B vals) \
             exceed one {bsz}B node; node splitting not supported"
        )));
    }

    let mut buf = vec![0u8; bsz];
    write_u64_le(&mut buf, 8, paddr);
    write_u64_le(&mut buf, 16, xid);
    write_u32_le(&mut buf, 24, OBJ_PHYSICAL | OBJECT_TYPE_BTREE);
    write_u32_le(&mut buf, 28, OBJECT_TYPE_SNAPMETATREE);
    write_u16_le(&mut buf, 32, 0x0003u16); // ROOT | LEAF (var-kv)
    write_u32_le(&mut buf, 36, nkeys as u32);
    // btn_table_space @40: off=0, len=toc_len.
    write_u16_le(&mut buf, 40, 0u16);
    write_u16_le(&mut buf, 42, toc_len as u16);
    // btn_free_space @44: off (rel key_area) = key_bytes, len = gap.
    write_u16_le(&mut buf, 44, key_bytes as u16);
    write_u16_le(&mut buf, 46, (value_start - key_end) as u16);
    // Free-list sentinels.
    write_u16_le(&mut buf, 48, 0xFFFFu16);
    write_u16_le(&mut buf, 50, 0u16);
    write_u16_le(&mut buf, 52, 0xFFFFu16);
    write_u16_le(&mut buf, 54, 0u16);

    let mut longest_key = 0usize;
    let mut longest_val = 0usize;
    let mut k_cursor = 0usize; // offset relative to key_area_start
    let mut v_cursor = 0usize; // cumulative bytes from val_area_end
    for (i, (k, v)) in recs.iter().enumerate() {
        v_cursor += v.len(); // TOC[i] value ends at val_area_end - v_cursor
                             // TOC kvloc_t {k_off, k_len, v_off_from_end, v_len}.
        let toc = DATA_BASE + i * 8;
        write_u16_le(&mut buf, toc, k_cursor as u16);
        write_u16_le(&mut buf, toc + 2, k.len() as u16);
        write_u16_le(&mut buf, toc + 4, v_cursor as u16);
        write_u16_le(&mut buf, toc + 6, v.len() as u16);
        // Key forward.
        let k_abs = key_area_start + k_cursor;
        buf.get_mut(k_abs..k_abs + k.len())
            .ok_or_else(|| TxnError::InvalidArgument("snap_meta key oob".into()))?
            .copy_from_slice(k);
        // Value backward.
        let v_abs = val_area_end - v_cursor;
        buf.get_mut(v_abs..v_abs + v.len())
            .ok_or_else(|| TxnError::InvalidArgument("snap_meta val oob".into()))?
            .copy_from_slice(v);
        k_cursor += k.len();
        longest_key = longest_key.max(k.len());
        longest_val = longest_val.max(v.len());
    }

    // btree_info_t footer.
    let bti = val_area_end;
    write_u32_le(&mut buf, bti, bt_flags);
    write_u32_le(&mut buf, bti + 4, bsz as u32);
    write_u32_le(&mut buf, bti + 8, 0u32); // key_size = 0 (var-kv)
    write_u32_le(&mut buf, bti + 12, 0u32); // val_size = 0 (var-kv)
    write_u32_le(&mut buf, bti + 16, longest_key as u32);
    write_u32_le(&mut buf, bti + 20, longest_val as u32);
    write_u64_le(&mut buf, bti + 24, nkeys as u64);
    write_u64_le(&mut buf, bti + 32, 1u64);
    update_checksum_in_place(&mut buf);
    Ok(buf)
}

/// Build a single-node om_snapshot tree (fixed-kv) holding one
/// `omap_snapshot_t` per xid in `xids`, sorted ascending. Generalizes
/// `build_omap_snapshot_tree_single`.
fn build_omap_snapshot_tree(
    xids: &[u64],
    paddr: u64,
    xid: u64,
    bsz: usize,
) -> Result<Vec<u8>, TxnError> {
    const KEY_SIZE: usize = 8;
    const VAL_SIZE: usize = 16;
    const DATA_BASE: usize = 56;
    const BTREE_INFO_SIZE: usize = 40;
    let val_area_end = bsz - BTREE_INFO_SIZE;

    let mut sorted: Vec<u64> = xids.to_vec();
    sorted.sort_unstable();
    let nkeys = sorted.len();

    // Pre-reserve toc per  finding (576 fits 144 entries). Grow if
    // somehow more are present.
    let toc_alloc: usize = core::cmp::max(576usize, nkeys * 4);
    let key_area_start = DATA_BASE + toc_alloc;
    let keys_end = key_area_start + nkeys * KEY_SIZE;
    let values_start = val_area_end - nkeys * VAL_SIZE;
    if keys_end > values_start {
        return Err(TxnError::InvalidArgument(format!(
            "om_snapshot: {nkeys} entries exceed one {bsz}B node"
        )));
    }

    let mut buf = vec![0u8; bsz];
    write_u64_le(&mut buf, 8, paddr);
    write_u64_le(&mut buf, 16, xid);
    write_u32_le(&mut buf, 24, OBJ_PHYSICAL | OBJECT_TYPE_BTREE);
    write_u32_le(&mut buf, 28, OBJECT_TYPE_OMAP_SNAPSHOT);
    write_u16_le(&mut buf, 32, 0x0007u16); // ROOT | LEAF | FIXED_KV_SIZE
    write_u32_le(&mut buf, 36, nkeys as u32);
    write_u16_le(&mut buf, 40, 0u16); // table_space.off
    write_u16_le(&mut buf, 42, toc_alloc as u16); // table_space.len
    write_u16_le(&mut buf, 44, (nkeys * KEY_SIZE) as u16); // free_space.off
    write_u16_le(&mut buf, 46, (values_start - keys_end) as u16); // free_space.len
    write_u16_le(&mut buf, 48, 0xFFFFu16);
    write_u16_le(&mut buf, 50, 0u16);
    write_u16_le(&mut buf, 52, 0xFFFFu16);
    write_u16_le(&mut buf, 54, 0u16);

    for (i, &x) in sorted.iter().enumerate() {
        // TOC kvoff_t {key_off (rel key_area), val_off (rel from end)}.
        let toc = DATA_BASE + i * 4;
        write_u16_le(&mut buf, toc, (i * KEY_SIZE) as u16);
        write_u16_le(&mut buf, toc + 2, ((i + 1) * VAL_SIZE) as u16);
        // Key: xid_t.
        write_u64_le(&mut buf, key_area_start + i * KEY_SIZE, x);
        // Value: omap_snapshot_t {oms_flags=0, oms_pad=0, oms_oid=0}.
        // (left zero)
    }

    let bti = val_area_end;
    write_u32_le(&mut buf, bti, 0x0000_0011u32); // PHYSICAL | UINT64_KEYS
    write_u32_le(&mut buf, bti + 4, bsz as u32);
    write_u32_le(&mut buf, bti + 8, KEY_SIZE as u32);
    write_u32_le(&mut buf, bti + 12, VAL_SIZE as u32);
    write_u32_le(&mut buf, bti + 16, KEY_SIZE as u32);
    write_u32_le(&mut buf, bti + 20, VAL_SIZE as u32);
    write_u64_le(&mut buf, bti + 24, nkeys as u64);
    write_u64_le(&mut buf, bti + 32, 1u64);
    update_checksum_in_place(&mut buf);
    Ok(buf)
}

pub fn delete_snapshot<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    snap_xid: u64,
) -> Result<(), TxnError> {
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
    let (frozen_vsb_paddr, snapshot_extref_paddr): (Option<u64>, Option<u64>) =
        if snap_meta_paddr != 0 {
            match read_snap_metadata_oids(txn, snap_meta_paddr, snap_xid) {
                Some((sb, er)) => (
                    if sb != 0 { Some(sb) } else { None },
                    if er != 0 { Some(er) } else { None },
                ),
                None => (None, None),
            }
        } else {
            (None, None)
        };

    // #139: retain SURVIVING snapshots. Rebuild both trees WITHOUT the deleted
    // entry rather than clearing the whole tree (which only matched the old
    // single-snapshot model and left count/tree inconsistent once create began
    // retaining N snapshots).
    let remaining: Vec<SnapEntry> = if snap_meta_paddr != 0 {
        read_all_snap_entries(txn, snap_meta_paddr)?
            .into_iter()
            .filter(|e| e.xid != snap_xid)
            .collect()
    } else {
        Vec::new()
    };
    let remaining_count = remaining.len() as u32;
    let remaining_most_recent = remaining.iter().map(|e| e.xid).max().unwrap_or(0);
    let del_bt_flags = if snap_meta_paddr != 0 {
        read_btn_info_flags(txn, snap_meta_paddr).unwrap_or(DEFAULT_SNAP_META_BT_FLAGS)
    } else {
        DEFAULT_SNAP_META_BT_FLAGS
    };
    let (new_snap_meta_paddr, new_snapshot_tree_paddr) = if remaining.is_empty() {
        (0u64, 0u64)
    } else {
        let sm_paddr = txn.alloc_block()?;
        let sm = build_snap_meta_node_multi(&remaining, sm_paddr, del_xid, bsz, del_bt_flags)?;
        txn.stage_raw(sm_paddr, sm);
        let st_paddr = txn.alloc_block()?;
        let xids: Vec<u64> = remaining.iter().map(|e| e.xid).collect();
        let st = build_omap_snapshot_tree(&xids, st_paddr, del_xid, bsz)?;
        txn.stage_raw(st_paddr, st);
        (sm_paddr, st_paddr)
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
    // NOTE: apfs_omap_oid will be patched after COW'ing the volume omap below.

    // --- (d) Update volume omap (PHYSICAL object, COW to new block) ---
    // [CERTAIN: COW required - in-place overwrite invalidates previous checkpoint;
    //  same as create_snapshot; the APFS specification]
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
        let new_vol_tree_paddr = txn.alloc_block()?;
        let mut tree_buf = vec![0u8; bsz];
        txn.read_block(old_vol_tree_paddr, &mut tree_buf)?;
        write_u64_le(&mut tree_buf, 8, new_vol_tree_paddr);
        write_u64_le(&mut tree_buf, 16, del_xid);
        update_checksum_in_place(&mut tree_buf);
        txn.stage_raw(new_vol_tree_paddr, tree_buf);
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
        // M7b-RT Subtask A: free the OLD vol-tree block now that new_omap is
        // staged (its om_tree_oid = new_vol_tree_paddr, NOT the old).
        txn.free_block(old_vol_tree_paddr)?;
    }

    // M7b-RT Subtask A: fs_alloc_count net delta for snapshot deletion.
    // 1:1 COW pairs (net 0): old vol-tree ↔ new vol-tree, old vomap ↔ new vomap,
    //                        old live extref ↔ new live extref.
    // True frees only (each -1 if present): frozen VSB, snap-meta node,
    //   om_snapshot_tree, snapshot's pinned extentref tree (EXT0).
    let mut net_delta: i64 = 0;
    let old_snapshot_tree_paddr = u64_from_le(omap_raw, OMAP_SNAPSHOT_TREE_OID_OFF);
    if frozen_vsb_paddr.is_some() {
        net_delta -= 1;
    }
    if snap_meta_paddr != 0 {
        net_delta -= 1;
    }
    if old_snapshot_tree_paddr != 0 {
        net_delta -= 1;
    }
    if snapshot_extref_paddr.is_some() {
        net_delta -= 1;
    }
    // #139: when snapshots survive we ALSO allocate rebuilt snap_meta +
    // om_snapshot trees (COW pairs with the old ones freed above).
    if new_snap_meta_paddr != 0 {
        net_delta += 1;
    }
    if new_snapshot_tree_paddr != 0 {
        net_delta += 1;
    }
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
    if snap_meta_paddr != 0 {
        txn.free_block(snap_meta_paddr)?;
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
    // Free the populated om_snapshot_tree from create_snapshot - the post-delete
    // volume omap no longer references it (om_snapshot_tree_oid set to 0 above).
    if old_snapshot_tree_paddr != 0 {
        txn.free_block(old_snapshot_tree_paddr)?;
    }
    // M7b-RT Subtask A: free the snapshot's pinned extentref tree (EXT0).
    // The snapshot's frozen VSB owned it; with the frozen VSB now freed,
    // EXT0 has no remaining referrer.
    if let Some(p) = snapshot_extref_paddr {
        txn.free_block(p)?;
    }

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
    let _ = old_num_snaps;
    if remaining_count == 0 {
        let _ = txn.drain_sm_fq_main(snap_xid)?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// revert_to_snapshot
// ---------------------------------------------------------------------------

/// Roll the live volume back to a previously created snapshot.
///
/// After this call the live volume tree, omap and extentref tree all point at
/// the snapshot's frozen copies. The snapshot meta record itself is removed
/// (the reverted state IS the new live state). All blocks allocated after
/// `snap_xid` are conservatively NOT freed here - they accumulate as
/// unreachable space that a future fsck pass can reclaim. This is the same
/// conservative approach taken for multi-snapshot delete above, and carries
/// no corruption risk (over-allocation, not under-allocation).
///
/// ## Limitations (conservative reclaim)
/// Blocks allocated between `snap_xid` and the current XID are leaked until
/// fsck reclaims them. This is safe: the on-disk state is self-consistent and
/// mountable; only the free-block count is temporarily pessimistic.
///
/// ## Mid-chain restriction
/// Only the **most-recent** snapshot may be reverted. Reverting to an older
/// snapshot in a multi-snapshot chain is rejected with `SnapshotNotFound`
/// because the intermediate snapshot's om_snapshot_tree record (which would
/// need to survive) references freed extref trees. Supporting mid-chain
/// revert requires a full extent-survivor scan; deferred to a later milestone.
///
/// ## Error conditions
/// - `SnapshotNotFound(snap_xid)` - no SNAP_METADATA record for that XID.
/// - `InvalidArgument` - `snap_xid` is not the most-recent snapshot (mid-chain).
///
/// `vsb_raw` and `omap_raw` are the current live VSB and omap blocks.
pub fn revert_to_snapshot<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    snap_xid: u64,
) -> Result<(), TxnError> {
    let bsz = txn.nx.block_size as usize;
    let revert_xid = txn.xid;
    let now_ns = apple_epoch_now_ns();

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
    let all_entries = read_all_snap_entries(txn, snap_meta_paddr)?;
    let remaining: Vec<SnapEntry> = all_entries
        .into_iter()
        .filter(|e| e.xid != snap_xid)
        .collect();
    let remaining_count = remaining.len() as u32;
    let remaining_most_recent = remaining.iter().map(|e| e.xid).max().unwrap_or(0);
    let bt_flags = read_btn_info_flags(txn, snap_meta_paddr).unwrap_or(DEFAULT_SNAP_META_BT_FLAGS);
    let (new_snap_meta_paddr, new_snapshot_tree_paddr) = if remaining.is_empty() {
        (0u64, 0u64)
    } else {
        let sm_paddr = txn.alloc_block()?;
        let sm = build_snap_meta_node_multi(&remaining, sm_paddr, revert_xid, bsz, bt_flags)?;
        txn.stage_raw(sm_paddr, sm);
        let st_paddr = txn.alloc_block()?;
        let xids: Vec<u64> = remaining.iter().map(|e| e.xid).collect();
        let st = build_omap_snapshot_tree(&xids, st_paddr, revert_xid, bsz)?;
        txn.stage_raw(st_paddr, st);
        (sm_paddr, st_paddr)
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
    let root_tree_oid = u64_from_le(vsb_raw, 0x88); // VSBI_ROOT_TREE_OID

    // --- Build updated live VSB ---
    let vsb_oid = u64_from_le(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    if new_vsb.len() < bsz {
        new_vsb.resize(bsz, 0);
    } else {
        new_vsb.truncate(bsz);
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
        // Free the old live extentref tree (orphaned).
        if old_live_extref_paddr != 0 && old_live_extref_paddr != snap_extref_oid {
            txn.free_block(old_live_extref_paddr)?;
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
        let new_vol_tree_paddr = txn.alloc_block()?;
        let mut tree_buf = vec![0u8; bsz];
        txn.read_block(old_vol_tree_paddr, &mut tree_buf)?;

        // COW-7: REMOVE the live_xid mapping for root_tree_oid from the volume
        // omap b-tree (fixed-kv: key {oid,xid} 16B, val {flags,size,paddr} 16B)
        // so the snap_xid entry becomes the highest visible xid for
        // root_tree_oid. The previous approach zeroed ov_paddr, but
        // Omap::resolve picks the highest xid <= query and skips ONLY
        // OMAP_VAL_DELETED - never a paddr == 0 - so resolving root_tree_oid
        // returned block 0 (the NX superblock) as the fsroot, making the volume
        // unmountable after a revert. Reuse the canonical fixed-kv node builder
        // so the rebuilt node matches the layout fsck expects (no hand-rolled
        // TOC/key/val surgery).
        // [linux-apfs-rw btree.c apfs_btree_remove + node.c nkeys/space recompute;
        //  docs/refs/di-tier-verification-2026-05.md]
        //
        // Defensive: rebuild_omap_node_without (like the rest of revert) assumes
        // a single-node omap (root = leaf). A multi-level omap (btn_level != 0)
        // would be mis-parsed as a flat leaf and silently corrupted, so refuse
        // rather than risk the volume. Volumes this engine writes keep a
        // single-node omap; a macOS volume with many snapshots could exceed it.
        // Mirrors the incremental omap path's level guard.
        if u16_from_le(&tree_buf, 34) != 0 {
            return Err(TxnError::InvalidArgument(
                "revert_to_snapshot: multi-level volume omap not supported".into(),
            ));
        }
        let live_xid = revert_xid.saturating_sub(1);
        // #139: locate the physical block backing the {root_tree_oid, live_xid}
        // omap entry BEFORE it is dropped from the rebuilt node. That block is
        // the post-snapshot live fsroot COW created by create_snapshot; once the
        // entry is removed it has no referrer and must be freed, or fsck reports
        // "overallocation". (fixed-kv omap node: key{oid,xid} 16B forward,
        // val{flags,size,paddr} 16B backward from bsz-40.)
        let orphan_fsroot_paddr: u64 = {
            let nkeys = u32_from_le(&tree_buf, 36) as usize;
            let toc_len = u16_from_le(&tree_buf, 42) as usize;
            let key_area_start = 56 + toc_len;
            let val_area_end = bsz - 40;
            let mut found = 0u64;
            for i in 0..nkeys {
                let toc_off = 56 + i * 4;
                let k_off = u16_from_le(&tree_buf, toc_off) as usize;
                let v_off = u16_from_le(&tree_buf, toc_off + 2) as usize;
                let k_abs = key_area_start + k_off;
                if k_abs + 16 > bsz {
                    continue;
                }
                let oid = u64_from_le(&tree_buf, k_abs);
                let xid = u64_from_le(&tree_buf, k_abs + 8);
                if oid == root_tree_oid && xid == live_xid {
                    let v_abs = val_area_end - v_off;
                    found = u64_from_le(&tree_buf, v_abs + 8);
                    break;
                }
            }
            found
        };
        let new_tree = crate::file::rebuild_omap_node_without(
            &tree_buf,
            root_tree_oid,
            live_xid,
            new_vol_tree_paddr,
            revert_xid,
            bsz,
        );
        txn.stage_raw(new_vol_tree_paddr, new_tree);
        // Free the orphaned post-snapshot live fsroot block (if found). The
        // reverted snapshot's frozen fsroot (at snap_xid) remains referenced by
        // its surviving omap entry, so only the dropped live_xid block is freed.
        if orphan_fsroot_paddr != 0 {
            txn.free_block(orphan_fsroot_paddr)?;
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
        let old_snapshot_tree_paddr = u64_from_le(omap_raw, OMAP_SNAPSHOT_TREE_OID_OFF);
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
        txn.free_block(old_vol_tree_paddr)?;
        txn.free_block(old_vomap_paddr)?;
        if old_snapshot_tree_paddr != 0 {
            txn.free_block(old_snapshot_tree_paddr)?;
        }
    }

    // --- fs_alloc_count delta ---
    // COW pairs (net 0): old vol-tree, old vomap, old live extref.
    // True frees: frozen VSB, snap_meta node, snapshot tree (counted above).
    // True allocs: new extref COW (+1 if snap_extref_oid != 0).
    // Net: conservative 0 (COW pairs cancel; we don't free frozen VSB here
    // to avoid corrupting the snapshot's historical record - conservative).
    // We leave fs_alloc_count unchanged; fsck can correct it.
    // [Deliberate: conservative lean-toward-overcount; no corruption risk]

    // Stage the updated live VSB.
    let vsb_body: Vec<u8> = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    let padded_body = pad_to(vsb_body, bsz - 32);
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &padded_body)?;

    // Free the snap_meta tree block (no longer referenced by new VSB).
    if snap_meta_paddr != 0 {
        txn.free_block(snap_meta_paddr)?;
    }
    // Free the frozen VSB (snapshot is now live; frozen copy is orphaned).
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

    // Test shim: the single-entry snap_meta node builder was generalized to
    // `build_snap_meta_node_multi` (#139). These layout tests still exercise the
    // single-snapshot node, so wrap the multi builder with the old signature.
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
        build_snap_meta_node_multi(&[e], a.paddr, a.xid, a.bsz, a.bt_flags)
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
