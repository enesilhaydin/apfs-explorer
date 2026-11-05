//! Small physical B-trees read and written whole: the snapshot metadata
//! tree and the object map's snapshot tree.
//!
//! Both change only when a snapshot is taken, deleted or reverted to, and
//! hold a few records per snapshot, so a change rewrites the tree: every
//! record is packed into new leaves, index levels are built over them until
//! one node holds the rest, and the old nodes are freed. Nodes of a physical
//! tree are named by their block address: an index record's value is its
//! child's address. macOS reads and grows these trees as any other B-tree,
//! so a volume with many snapshots has them several levels deep.
//!
//! Layout follows the APFS specification (`btree_node_phys_t`,
//! `btree_info_t`) and the checks `fsck_apfs` and apfsck make: a tree with
//! fixed-size records preallocates its whole table of contents in every
//! node, sized as if the root had no footer; the footer counts every record
//! and node, the longest key over index nodes too and the longest value over
//! leaves only; a node other than the root is never empty.

use crate::txn::{
    update_checksum_in_place, Transaction, TxnError, OBJECT_TYPE_BTREE, OBJECT_TYPE_BTREE_NODE,
    OBJ_PHYSICAL,
};
use apfs_core::block_device::WritableBlockDevice;
use std::collections::HashSet;

/// `btree_node_phys_t` header: object header, then flags, level, count and
/// the table, free-space and free-list locations.
const NODE_HEADER: usize = 56;
/// `btree_info_t` at the end of a root node.
const INFO_SIZE: usize = 40;
const BTNODE_ROOT: u16 = 0x0001;
const BTNODE_LEAF: u16 = 0x0002;
const BTNODE_FIXED_KV_SIZE: u16 = 0x0004;
/// Table entries of a tree with variable-size records (`kvloc_t`).
const KVLOC: usize = 8;
/// Table entries of a tree with fixed-size records (`kvoff_t`).
const KVOFF: usize = 4;
/// A variable-size table grows in steps of this many entries; macOS sizes
/// tables this way and counts on it when it rebalances a node.
const VAR_TOC_STEP: usize = 8;
/// An index record's value: the child's block address.
const CHILD_ADDR: usize = 8;
/// Deeper than any tree of a few thousand records can be.
const MAX_DEPTH: u16 = 8;
/// More nodes than a tree of these records can have: a cycle, or garbage.
const MAX_NODES: usize = 1 << 16;
/// "No entry" in a free list.
const NLOC_NONE: u16 = 0xFFFF;

/// A record: its key and its value.
pub(crate) type Record = (Vec<u8>, Vec<u8>);

/// How a tree's records are sized.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Kv {
    /// Each record has its own key and value length (`kvloc_t` table).
    Var,
    /// Every leaf key and value has these sizes (`kvoff_t` table); an index
    /// record has the same key and a child address for its value.
    Fixed { key: usize, val: usize },
}

/// What a tree is, as its nodes say it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Spec {
    /// `o_subtype` of every node.
    pub subtype: u32,
    pub kv: Kv,
    /// `bt_flags` of the footer.
    pub bt_flags: u32,
}

/// A tree read back: its leaf records in key order, and the address of
/// every node.
pub(crate) struct Tree {
    pub records: Vec<Record>,
    pub nodes: Vec<u64>,
}

fn rd16(b: &[u8], off: usize) -> Result<u16, TxnError> {
    b.get(off..off + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| bad(format!("read past the node at {off}")))
}

fn rd32(b: &[u8], off: usize) -> Result<u32, TxnError> {
    b.get(off..off + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| bad(format!("read past the node at {off}")))
}

fn rd64(b: &[u8], off: usize) -> Result<u64, TxnError> {
    b.get(off..off + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| bad(format!("read past the node at {off}")))
}

fn put(b: &mut [u8], off: usize, bytes: &[u8]) -> Result<(), TxnError> {
    b.get_mut(off..off + bytes.len())
        .ok_or_else(|| bad(format!("write past the node at {off}")))?
        .copy_from_slice(bytes);
    Ok(())
}

fn bad(why: String) -> TxnError {
    TxnError::InvalidArgument(format!("physical b-tree: {why}"))
}

/// Read the tree whose root is at `root`, checking each node is one of
/// this tree's (`subtype`), sound and in place.
pub(crate) fn read<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    root: u64,
    subtype: u32,
) -> Result<Tree, TxnError> {
    let bsz = txn.nx.block_size as usize;
    let mut tree = Tree {
        records: Vec::new(),
        nodes: Vec::new(),
    };
    let mut seen: HashSet<u64> = HashSet::new();
    // Leaf sizes of a fixed-size tree, from the root's footer.
    let mut fixed: Option<(usize, usize)> = None;
    // (address, level the parent expects, whether it is the root)
    let mut stack: Vec<(u64, Option<u16>, bool)> = vec![(root, None, true)];
    while let Some((paddr, want_level, is_root)) = stack.pop() {
        if !seen.insert(paddr) || seen.len() > MAX_NODES {
            return Err(bad(format!("node {paddr:#x} reached twice")));
        }
        let mut buf = vec![0u8; bsz];
        txn.read_block(paddr, &mut buf)?;
        apfs_core::checksum::verify_block(&buf)
            .map_err(|e| bad(format!("node {paddr:#x}: {e}")))?;
        if rd32(&buf, 28)? != subtype {
            return Err(bad(format!("node {paddr:#x} belongs to another tree")));
        }
        let flags = rd16(&buf, 32)?;
        let level = rd16(&buf, 34)?;
        let nkeys = rd32(&buf, 36)? as usize;
        let leaf = flags & BTNODE_LEAF != 0;
        if (flags & BTNODE_ROOT != 0) != is_root
            || leaf != (level == 0)
            || level > MAX_DEPTH
            || want_level.is_some_and(|l| l != level)
        {
            return Err(bad(format!("node {paddr:#x} is out of place")));
        }
        let is_fixed = flags & BTNODE_FIXED_KV_SIZE != 0;
        if is_root && is_fixed {
            let info = bsz - INFO_SIZE;
            fixed = Some((
                rd32(&buf, info + 8)? as usize,
                rd32(&buf, info + 12)? as usize,
            ));
        }
        let toc = NODE_HEADER + rd16(&buf, 40)? as usize;
        let key_base = NODE_HEADER + rd16(&buf, 40)? as usize + rd16(&buf, 42)? as usize;
        let val_end = if is_root { bsz - INFO_SIZE } else { bsz };
        let entry = if is_fixed { KVOFF } else { KVLOC };
        if toc + nkeys * entry > key_base || key_base > val_end {
            return Err(bad(format!("node {paddr:#x} has a bad table")));
        }
        tree.nodes.push(paddr);
        let mut children: Vec<u64> = Vec::new();
        for i in 0..nkeys {
            let t = toc + i * entry;
            let (k_off, k_len, v_off, v_len) = if is_fixed {
                let (k, v) = fixed.ok_or_else(|| bad("no footer".into()))?;
                let v = if leaf { v } else { CHILD_ADDR };
                (rd16(&buf, t)? as usize, k, rd16(&buf, t + 2)? as usize, v)
            } else {
                (
                    rd16(&buf, t)? as usize,
                    rd16(&buf, t + 2)? as usize,
                    rd16(&buf, t + 4)? as usize,
                    rd16(&buf, t + 6)? as usize,
                )
            };
            let k = buf
                .get(key_base + k_off..key_base + k_off + k_len)
                .ok_or_else(|| bad(format!("key {i} of node {paddr:#x} out of bounds")))?;
            let v = val_end
                .checked_sub(v_off)
                .and_then(|s| buf.get(s..s + v_len))
                .ok_or_else(|| bad(format!("value {i} of node {paddr:#x} out of bounds")))?;
            if leaf {
                tree.records.push((k.to_vec(), v.to_vec()));
            } else {
                children.push(rd64(v, 0)?);
            }
        }
        if !leaf {
            // Last pushed is read first: push in reverse to keep key order.
            for c in children.into_iter().rev() {
                stack.push((c, Some(level - 1), false));
            }
        }
    }
    Ok(tree)
}

/// Bytes of the table of contents for `n` records.
fn toc_len(kv: Kv, leaf: bool, n: usize, bsz: usize) -> usize {
    match kv {
        Kv::Var => n.max(1).div_ceil(VAR_TOC_STEP) * VAR_TOC_STEP * KVLOC,
        Kv::Fixed { key, val } => {
            let val = if leaf { val } else { CHILD_ADDR };
            (bsz - NODE_HEADER) / (key + val + KVOFF) * KVOFF
        }
    }
}

/// Whether `recs` fit one node.
fn fits(kv: Kv, leaf: bool, root: bool, recs: &[Record], bsz: usize) -> bool {
    let end = if root { bsz - INFO_SIZE } else { bsz };
    let toc = toc_len(kv, leaf, recs.len(), bsz);
    let entry = match kv {
        Kv::Var => KVLOC,
        Kv::Fixed { .. } => KVOFF,
    };
    let data: usize = recs.iter().map(|(k, v)| k.len() + v.len()).sum();
    recs.len() * entry <= toc && NODE_HEADER + toc + data <= end
}

/// Split `recs` into runs that each fill a node other than the root.
fn pack(kv: Kv, leaf: bool, recs: Vec<Record>, bsz: usize) -> Result<Vec<Vec<Record>>, TxnError> {
    let mut out: Vec<Vec<Record>> = Vec::new();
    let mut cur: Vec<Record> = Vec::new();
    for r in recs {
        cur.push(r);
        if !fits(kv, leaf, false, &cur, bsz) {
            let last = cur.pop().ok_or_else(|| bad("empty run".into()))?;
            if cur.is_empty() {
                return Err(bad("a record larger than a node".into()));
            }
            out.push(std::mem::take(&mut cur));
            cur.push(last);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

/// The footer a root carries.
struct Info {
    key_count: u64,
    node_count: u64,
    longest_key: usize,
    longest_val: usize,
}

/// One node at `paddr` holding `recs` (in key order) at `level`, with
/// `info` when it is the root.
fn build_node(
    spec: &Spec,
    recs: &[Record],
    level: u16,
    info: Option<&Info>,
    paddr: u64,
    xid: u64,
    bsz: usize,
) -> Result<Vec<u8>, TxnError> {
    let leaf = level == 0;
    let root = info.is_some();
    let toc = toc_len(spec.kv, leaf, recs.len(), bsz);
    let key_base = NODE_HEADER + toc;
    let val_end = if root { bsz - INFO_SIZE } else { bsz };
    let mut buf = vec![0u8; bsz];
    put(&mut buf, 8, &paddr.to_le_bytes())?;
    put(&mut buf, 16, &xid.to_le_bytes())?;
    let o_type = if root {
        OBJECT_TYPE_BTREE
    } else {
        OBJECT_TYPE_BTREE_NODE
    };
    put(&mut buf, 24, &(OBJ_PHYSICAL | o_type).to_le_bytes())?;
    put(&mut buf, 28, &spec.subtype.to_le_bytes())?;
    let mut flags = 0u16;
    if root {
        flags |= BTNODE_ROOT;
    }
    if leaf {
        flags |= BTNODE_LEAF;
    }
    if matches!(spec.kv, Kv::Fixed { .. }) {
        flags |= BTNODE_FIXED_KV_SIZE;
    }
    put(&mut buf, 32, &flags.to_le_bytes())?;
    put(&mut buf, 34, &level.to_le_bytes())?;
    put(&mut buf, 36, &(recs.len() as u32).to_le_bytes())?;
    put(&mut buf, 40, &0u16.to_le_bytes())?;
    put(&mut buf, 42, &(toc as u16).to_le_bytes())?;
    let (mut k_cur, mut v_cur) = (0usize, 0usize);
    for (i, (k, v)) in recs.iter().enumerate() {
        v_cur += v.len();
        match spec.kv {
            Kv::Var => {
                let t = NODE_HEADER + i * KVLOC;
                put(&mut buf, t, &(k_cur as u16).to_le_bytes())?;
                put(&mut buf, t + 2, &(k.len() as u16).to_le_bytes())?;
                put(&mut buf, t + 4, &(v_cur as u16).to_le_bytes())?;
                put(&mut buf, t + 6, &(v.len() as u16).to_le_bytes())?;
            }
            Kv::Fixed { .. } => {
                let t = NODE_HEADER + i * KVOFF;
                put(&mut buf, t, &(k_cur as u16).to_le_bytes())?;
                put(&mut buf, t + 2, &(v_cur as u16).to_le_bytes())?;
            }
        }
        put(&mut buf, key_base + k_cur, k)?;
        let v_at = val_end
            .checked_sub(v_cur)
            .ok_or_else(|| bad("values overflow the node".into()))?;
        put(&mut buf, v_at, v)?;
        k_cur += k.len();
    }
    let free_len = (val_end - v_cur)
        .checked_sub(key_base + k_cur)
        .ok_or_else(|| bad("records overflow the node".into()))?;
    put(&mut buf, 44, &(k_cur as u16).to_le_bytes())?;
    put(&mut buf, 46, &(free_len as u16).to_le_bytes())?;
    put(&mut buf, 48, &NLOC_NONE.to_le_bytes())?;
    put(&mut buf, 52, &NLOC_NONE.to_le_bytes())?;
    if let Some(info) = info {
        let (key_size, val_size) = match spec.kv {
            Kv::Var => (0usize, 0usize),
            Kv::Fixed { key, val } => (key, val),
        };
        let at = bsz - INFO_SIZE;
        put(&mut buf, at, &spec.bt_flags.to_le_bytes())?;
        put(&mut buf, at + 4, &(bsz as u32).to_le_bytes())?;
        put(&mut buf, at + 8, &(key_size as u32).to_le_bytes())?;
        put(&mut buf, at + 12, &(val_size as u32).to_le_bytes())?;
        put(&mut buf, at + 16, &(info.longest_key as u32).to_le_bytes())?;
        put(&mut buf, at + 20, &(info.longest_val as u32).to_le_bytes())?;
        put(&mut buf, at + 24, &info.key_count.to_le_bytes())?;
        put(&mut buf, at + 32, &info.node_count.to_le_bytes())?;
    }
    update_checksum_in_place(&mut buf);
    Ok(buf)
}

/// A tree of one node: `records` (in key order) in a root leaf at `paddr`.
#[cfg(test)]
pub(crate) fn root_leaf(
    spec: &Spec,
    records: Vec<Record>,
    paddr: u64,
    xid: u64,
    bsz: usize,
) -> Result<Vec<u8>, TxnError> {
    let (longest_key, longest_val) = match spec.kv {
        Kv::Fixed { key, val } => (key, val),
        Kv::Var => (
            records.iter().map(|(k, _)| k.len()).max().unwrap_or(0),
            records.iter().map(|(_, v)| v.len()).max().unwrap_or(0),
        ),
    };
    let info = Info {
        key_count: records.len() as u64,
        node_count: 1,
        longest_key,
        longest_val,
    };
    build_node(spec, &records, 0, Some(&info), paddr, xid, bsz)
}

/// Write `records` (in key order) as a new tree stamped with `xid`.
/// Returns the root's address and how many nodes the tree has.
pub(crate) fn write<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    spec: &Spec,
    records: Vec<Record>,
    xid: u64,
) -> Result<(u64, usize), TxnError> {
    let bsz = txn.nx.block_size as usize;
    let key_count = records.len() as u64;
    let (mut longest_key, longest_val) = match spec.kv {
        // The footer of a fixed-size tree names the sizes, records or not.
        Kv::Fixed { key, val } => (key, val),
        Kv::Var => (
            records.iter().map(|(k, _)| k.len()).max().unwrap_or(0),
            records.iter().map(|(_, v)| v.len()).max().unwrap_or(0),
        ),
    };
    let mut level: u16 = 0;
    let mut current = records;
    let mut nodes = 0usize;
    // Until one node holds what is left, write a level of nodes and go up
    // with an index record (first key, address) for each.
    while !fits(spec.kv, level == 0, true, &current, bsz) {
        if level >= MAX_DEPTH {
            return Err(bad("tree too deep".into()));
        }
        let runs = pack(spec.kv, level == 0, current, bsz)?;
        let mut up: Vec<Record> = Vec::with_capacity(runs.len());
        for run in runs {
            let first = run
                .first()
                .map(|(k, _)| k.clone())
                .ok_or_else(|| bad("empty node".into()))?;
            let paddr = txn.alloc_block()?;
            let node = build_node(spec, &run, level, None, paddr, xid, bsz)?;
            txn.stage_raw(paddr, node);
            nodes += 1;
            longest_key = longest_key.max(first.len());
            up.push((first, paddr.to_le_bytes().to_vec()));
        }
        current = up;
        level += 1;
    }
    let root = txn.alloc_block()?;
    nodes += 1;
    let info = Info {
        key_count,
        node_count: nodes as u64,
        longest_key,
        longest_val,
    };
    let node = build_node(spec, &current, level, Some(&info), root, xid, bsz)?;
    txn.stage_raw(root, node);
    Ok((root, nodes))
}
