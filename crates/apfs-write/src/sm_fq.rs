//! Spaceman free queue (sm_fq) — ephemeral B-tree node builder and decoder.
//!
//! M7b-RT Subtask D layer (locked spec in
//! `docs/superpowers/specs/2026-05-23-m7b-rt-subtask-d-smfq-layout.md`,
//! cross-checked with KB `apfs-spec:cow-block-reclaim` Q4/Q6).
//!
//! The sm_fq is a per-device deferred-free queue. When a block is replaced
//! by COW but still pinned by an active snapshot, instead of clearing the
//! bitmap immediately (which would corrupt the snapshot view) the writer
//! enqueues `{snap_xid, paddr}` into sm_fq. On `delete_snapshot`, the entries
//! with `xid <= deleted_snap_xid` are drained and their bitmap bits cleared.
//!
//! On-disk layout (FIXED_KV root+leaf, kernel-empirical):
//!  - obj_phys: o_type = `OBJ_EPHEMERAL | OBJECT_TYPE_BTREE` = `0x80000002`,
//!    o_subtype = `OBJECT_TYPE_SPACEMAN_FREE_QUEUE` = `0x09`.
//!  - btn_flags = ROOT | LEAF | FIXED_KV_SIZE = `0x07`.
//!  - table_space reserve = **576** bytes (= 144 kvoff_t entries).
//!  - key_size = 16 (`sfqk_xid u64`, `sfqk_paddr u64`), val_size = 8.
//!  - btree_info bt_flags = `0x0e` (SEQUENTIAL_INSERT | ALLOW_GHOSTS |
//!    EPHEMERAL).
//!
//! Helpers here are pure (no I/O), so they round-trip cleanly under unit
//! tests without a real device.

#![allow(clippy::indexing_slicing)]

const DATA_BASE: usize = 56;
const FOOTER: usize = 40;
const TOC_RESERVE: usize = 576;
const KEY_SIZE: usize = 16;
const VAL_SIZE: usize = 8;
/// Maximum entries that fit in a single sm_fq node given a block size.
///
/// The fixed 576-byte TOC reserve would hold 144 kvoff_t entries, but the
/// tighter constraint is the key + val region: `(bsz - FOOTER) -
/// (DATA_BASE + TOC_RESERVE)` bytes available, each entry needs
/// `KEY_SIZE + VAL_SIZE = 24` bytes. For `bsz = 4096`: 142 entries.
pub fn smfq_max_keys(bsz: usize) -> usize {
    let key_area_start = DATA_BASE + TOC_RESERVE;
    let val_area_end = bsz.saturating_sub(FOOTER);
    let avail = val_area_end.saturating_sub(key_area_start);
    avail / (KEY_SIZE + VAL_SIZE)
}

const OBJ_EPHEMERAL: u32 = 0x80000000;
const OBJECT_TYPE_BTREE: u32 = 0x02;
/// `OBJECT_TYPE_SPACEMAN_FREE_QUEUE` per APFS spec.
pub const OBJECT_TYPE_SPACEMAN_FREE_QUEUE: u32 = 0x09;
const SMFQ_O_TYPE: u32 = OBJ_EPHEMERAL | OBJECT_TYPE_BTREE; // 0x80000002

const BTNODE_ROOT: u16 = 0x01;
const BTNODE_LEAF: u16 = 0x02;
const BTNODE_FIXED_KV: u16 = 0x04;
const SMFQ_BTN_FLAGS: u16 = BTNODE_ROOT | BTNODE_LEAF | BTNODE_FIXED_KV; // 0x07

const BT_SEQUENTIAL_INSERT: u32 = 0x02;
const BT_ALLOW_GHOSTS: u32 = 0x04;
const BT_EPHEMERAL: u32 = 0x08;
const SMFQ_BT_FLAGS: u32 = BT_SEQUENTIAL_INSERT | BT_ALLOW_GHOSTS | BT_EPHEMERAL; // 0x0e

/// Errors raised by the sm_fq helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmFqError {
    /// The TOC reserve is full (>= `smfq_max_keys(bsz)` entries). Multi-node
    /// sm_fq is a follow-up; for now, callers must drain on delete_snapshot
    /// to keep the node well under this limit.
    Full,
    /// Malformed input node (size mismatch, impossible offsets, …).
    Malformed(&'static str),
}

fn wr_u16(b: &mut [u8], off: usize, v: u16) {
    b[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn wr_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn wr_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_le_bytes());
}
fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(b[off..off + 2].try_into().unwrap_or([0; 2]))
}
fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap_or([0; 4]))
}
fn rd_u64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().unwrap_or([0; 8]))
}

/// Build an empty sm_fq B-tree node block (root + leaf, FIXED_KV, 0 entries).
///
/// Checksum is NOT applied here — the caller stages the block and the
/// commit path runs `update_checksum_in_place`.
///
/// `paddr` is written as `o_oid` (sm_fq nodes are ephemeral but the
/// kernel still stores the paddr there); `xid` is the txn's xid.
pub fn build_empty_sm_fq_node(paddr: u64, xid: u64, bsz: usize) -> Vec<u8> {
    let mut buf = vec![0u8; bsz];
    // obj_phys (offsets 0..32). Checksum at 0 stays zero until staging.
    wr_u64(&mut buf, 8, paddr);
    wr_u64(&mut buf, 16, xid);
    wr_u32(&mut buf, 24, SMFQ_O_TYPE);
    wr_u32(&mut buf, 28, OBJECT_TYPE_SPACEMAN_FREE_QUEUE);
    // btree_node_phys body
    wr_u16(&mut buf, 32, SMFQ_BTN_FLAGS); // btn_flags
    wr_u16(&mut buf, 34, 0); // btn_level
    wr_u32(&mut buf, 36, 0); // btn_nkeys
    wr_u16(&mut buf, 40, 0); // btn_table_space.off
    wr_u16(&mut buf, 42, TOC_RESERVE as u16); // btn_table_space.len
    let key_area_start = DATA_BASE + TOC_RESERVE;
    let val_area_end = bsz - FOOTER;
    // For nkeys=0: free area spans the whole key+val region.
    wr_u16(&mut buf, 44, 0); // btn_free_space.off
    wr_u16(&mut buf, 46, (val_area_end - key_area_start) as u16); // .len
                                                                  // key_free_list / val_free_list = (0xFFFF, 0) — invalid sentinel.
    wr_u16(&mut buf, 48, 0xFFFF);
    wr_u16(&mut buf, 50, 0);
    wr_u16(&mut buf, 52, 0xFFFF);
    wr_u16(&mut buf, 54, 0);
    // btree_info_t footer (last 40 bytes).
    let info = val_area_end;
    wr_u32(&mut buf, info, SMFQ_BT_FLAGS);
    wr_u32(&mut buf, info + 4, bsz as u32);
    wr_u32(&mut buf, info + 8, KEY_SIZE as u32);
    wr_u32(&mut buf, info + 12, VAL_SIZE as u32);
    wr_u32(&mut buf, info + 16, KEY_SIZE as u32); // longest_key
    wr_u32(&mut buf, info + 20, VAL_SIZE as u32); // longest_val
    wr_u64(&mut buf, info + 24, 0); // key_count
    wr_u64(&mut buf, info + 32, 1); // node_count
    buf
}

/// Parse all entries from an sm_fq B-tree node.
///
/// Returns `Vec<(xid, paddr, count)>` in stored order (which is
/// `(xid, paddr)` ascending — the kernel keeps the leaf sorted).
pub fn parse_sm_fq_entries(node: &[u8], bsz: usize) -> Vec<(u64, u64, u64)> {
    if node.len() < bsz {
        return Vec::new();
    }
    let nkeys = rd_u32(node, 36) as usize;
    let toc_len = rd_u16(node, 42) as usize;
    let key_area = DATA_BASE + toc_len;
    let val_area_end = bsz - FOOTER;
    let mut out = Vec::with_capacity(nkeys);
    for i in 0..nkeys {
        let toc_off = DATA_BASE + i * 4;
        if toc_off + 4 > bsz {
            break;
        }
        let k_off = rd_u16(node, toc_off) as usize;
        let v_off = rd_u16(node, toc_off + 2) as usize;
        let k_abs = key_area + k_off;
        if k_abs + KEY_SIZE > val_area_end {
            continue;
        }
        let v_abs = val_area_end.saturating_sub(v_off);
        if v_abs + VAL_SIZE > val_area_end {
            continue;
        }
        let xid = rd_u64(node, k_abs);
        let paddr = rd_u64(node, k_abs + 8);
        let count = rd_u64(node, v_abs);
        out.push((xid, paddr, count));
    }
    out
}

/// Insert `{xid, paddr}` with `count=1` into an existing sm_fq node.
///
/// - Idempotent: if `{xid, paddr}` already exists, the node is rewritten
///   unchanged (same keys, same vals).
/// - Re-sorts the keys into `(xid asc, paddr asc)` order on every call.
/// - Returns `SmFqError::Full` if `entries.len() == smfq_max_keys(bsz)`
///   (multi-node sm_fq is a follow-up; bound expected entries via
///   `delete_snapshot` drain).
///
/// The returned buffer preserves the input `obj_phys` header verbatim
/// — the caller updates `o_xid` and recomputes the checksum at stage time.
pub fn insert_sm_fq_entry(
    node: &[u8],
    xid: u64,
    paddr: u64,
    bsz: usize,
) -> Result<Vec<u8>, SmFqError> {
    if node.len() < bsz {
        return Err(SmFqError::Malformed("node smaller than bsz"));
    }
    let mut entries = parse_sm_fq_entries(node, bsz);
    if !entries.iter().any(|&(x, p, _)| x == xid && p == paddr) {
        entries.push((xid, paddr, 1));
    }
    entries.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    if entries.len() > smfq_max_keys(bsz) {
        return Err(SmFqError::Full);
    }
    write_sm_fq_node(node, &entries, bsz)
}

/// Remove every entry with `xid <= upto_xid` from the node. Used by
/// `delete_snapshot` to drain the queue.
///
/// Returns the new node buffer and the list of `(xid, paddr)` pairs that
/// were drained (the caller frees the bitmap bit for each paddr).
#[allow(clippy::type_complexity)]
pub fn drain_sm_fq_upto(
    node: &[u8],
    upto_xid: u64,
    bsz: usize,
) -> Result<(Vec<u8>, Vec<(u64, u64)>), SmFqError> {
    if node.len() < bsz {
        return Err(SmFqError::Malformed("node smaller than bsz"));
    }
    let entries = parse_sm_fq_entries(node, bsz);
    let (drain, keep): (Vec<_>, Vec<_>) = entries.into_iter().partition(|&(x, _, _)| x <= upto_xid);
    let drained_pairs: Vec<(u64, u64)> = drain.iter().map(|&(x, p, _)| (x, p)).collect();
    let new_node = write_sm_fq_node(node, &keep, bsz)?;
    Ok((new_node, drained_pairs))
}

fn write_sm_fq_node(
    template: &[u8],
    entries: &[(u64, u64, u64)],
    bsz: usize,
) -> Result<Vec<u8>, SmFqError> {
    if entries.len() > smfq_max_keys(bsz) {
        return Err(SmFqError::Full);
    }
    let key_area_start = DATA_BASE + TOC_RESERVE;
    let val_area_end = bsz - FOOTER;
    let mut buf = template.to_vec();
    // Update key count in both the node header and the footer.
    wr_u32(&mut buf, 36, entries.len() as u32);
    let used_key = entries.len() * KEY_SIZE;
    let used_val = entries.len() * VAL_SIZE;
    wr_u16(&mut buf, 44, used_key as u16);
    wr_u16(
        &mut buf,
        46,
        (val_area_end - key_area_start - used_key - used_val) as u16,
    );
    // Clear TOC reserve and key/val regions before writing the new layout.
    for b in &mut buf[DATA_BASE..DATA_BASE + TOC_RESERVE] {
        *b = 0;
    }
    for b in &mut buf[key_area_start..val_area_end] {
        *b = 0;
    }
    for (i, &(x, p, c)) in entries.iter().enumerate() {
        wr_u16(&mut buf, DATA_BASE + i * 4, (i * KEY_SIZE) as u16);
        wr_u16(&mut buf, DATA_BASE + i * 4 + 2, ((i + 1) * VAL_SIZE) as u16);
        let k_abs = key_area_start + i * KEY_SIZE;
        wr_u64(&mut buf, k_abs, x);
        wr_u64(&mut buf, k_abs + 8, p);
        let v_abs = val_area_end - (i + 1) * VAL_SIZE;
        wr_u64(&mut buf, v_abs, c);
    }
    // btree_info_t.key_count.
    wr_u64(&mut buf, val_area_end + 24, entries.len() as u64);
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BSZ: usize = 4096;

    #[test]
    fn empty_round_trip() {
        let node = build_empty_sm_fq_node(0x42, 7, BSZ);
        assert_eq!(rd_u64(&node, 8), 0x42);
        assert_eq!(rd_u64(&node, 16), 7);
        assert_eq!(rd_u32(&node, 24), 0x8000_0002);
        assert_eq!(rd_u32(&node, 28), 0x09);
        assert_eq!(rd_u16(&node, 32), 0x07);
        assert_eq!(rd_u16(&node, 42), 576);
        // Footer fields locked from real-disk decode.
        assert_eq!(rd_u32(&node, BSZ - FOOTER), 0x0e);
        assert_eq!(rd_u32(&node, BSZ - FOOTER + 4), BSZ as u32);
        assert_eq!(rd_u32(&node, BSZ - FOOTER + 8), KEY_SIZE as u32);
        assert_eq!(rd_u32(&node, BSZ - FOOTER + 12), VAL_SIZE as u32);
        assert_eq!(rd_u64(&node, BSZ - FOOTER + 24), 0); // key_count
        assert_eq!(rd_u64(&node, BSZ - FOOTER + 32), 1); // node_count
        assert!(parse_sm_fq_entries(&node, BSZ).is_empty());
    }

    #[test]
    fn insert_one_and_parse() {
        let node = build_empty_sm_fq_node(100, 5, BSZ);
        let n1 = insert_sm_fq_entry(&node, 4, 0x827, BSZ).expect("insert");
        let entries = parse_sm_fq_entries(&n1, BSZ);
        assert_eq!(entries, vec![(4, 0x827, 1)]);
        // Header preserved: o_oid, o_type, o_subtype, btn_flags unchanged.
        assert_eq!(rd_u64(&n1, 8), 100);
        assert_eq!(rd_u32(&n1, 24), 0x8000_0002);
        assert_eq!(rd_u32(&n1, 28), 0x09);
        assert_eq!(rd_u16(&n1, 32), 0x07);
    }

    #[test]
    fn insert_two_unsorted_keeps_sorted() {
        let node = build_empty_sm_fq_node(100, 5, BSZ);
        let n1 = insert_sm_fq_entry(&node, 4, 0x827, BSZ).expect("insert 1");
        let n2 = insert_sm_fq_entry(&n1, 3, 0x800, BSZ).expect("insert 2");
        let entries = parse_sm_fq_entries(&n2, BSZ);
        assert_eq!(entries, vec![(3, 0x800, 1), (4, 0x827, 1)]);
    }

    #[test]
    fn insert_same_xid_sorts_by_paddr() {
        let node = build_empty_sm_fq_node(100, 5, BSZ);
        let n1 = insert_sm_fq_entry(&node, 4, 0x827, BSZ).expect("insert 1");
        let n2 = insert_sm_fq_entry(&n1, 4, 0x800, BSZ).expect("insert 2");
        let entries = parse_sm_fq_entries(&n2, BSZ);
        assert_eq!(entries, vec![(4, 0x800, 1), (4, 0x827, 1)]);
    }

    #[test]
    fn insert_is_idempotent() {
        let node = build_empty_sm_fq_node(100, 5, BSZ);
        let n1 = insert_sm_fq_entry(&node, 4, 0x827, BSZ).expect("insert 1");
        let n2 = insert_sm_fq_entry(&n1, 4, 0x827, BSZ).expect("duplicate");
        let entries = parse_sm_fq_entries(&n2, BSZ);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn capacity_at_smfq_max_keys() {
        let mut node = build_empty_sm_fq_node(100, 5, BSZ);
        for i in 0..smfq_max_keys(BSZ) as u64 {
            node = insert_sm_fq_entry(&node, 1, 0x1000 + i, BSZ).expect("under cap");
        }
        // One more should fail.
        assert_eq!(
            insert_sm_fq_entry(&node, 1, 0xFFFF, BSZ),
            Err(SmFqError::Full)
        );
        assert_eq!(parse_sm_fq_entries(&node, BSZ).len(), smfq_max_keys(BSZ));
    }

    #[test]
    fn drain_upto_xid() {
        let node = build_empty_sm_fq_node(100, 5, BSZ);
        let mut n = node;
        for (x, p) in [(2, 0x800u64), (4, 0x827), (4, 0x830), (6, 0x900)] {
            n = insert_sm_fq_entry(&n, x, p, BSZ).expect("insert");
        }
        let (new_n, drained) = drain_sm_fq_upto(&n, 4, BSZ).expect("drain");
        assert_eq!(drained, vec![(2, 0x800), (4, 0x827), (4, 0x830)]);
        assert_eq!(parse_sm_fq_entries(&new_n, BSZ), vec![(6, 0x900, 1)]);
    }

    #[test]
    fn drain_nothing_when_xid_below_oldest() {
        let n = build_empty_sm_fq_node(100, 5, BSZ);
        let n = insert_sm_fq_entry(&n, 10, 0xAAA, BSZ).expect("insert");
        let (n2, drained) = drain_sm_fq_upto(&n, 5, BSZ).expect("drain");
        assert!(drained.is_empty());
        assert_eq!(parse_sm_fq_entries(&n2, BSZ), vec![(10, 0xAAA, 1)]);
    }
}
