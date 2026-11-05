//! The snapshots a volume records in its snapshot metadata tree.
//!
//! The tree is a physical B-tree with variable-size records: each snapshot
//! has a `SNAP_METADATA` record keyed by its transaction id, holding the
//! address of the volume superblock frozen when it was taken and its name,
//! and a `SNAP_NAME` record keyed by the name. An index record's value is
//! its child's block address. macOS grows the tree past one node on a
//! volume with many snapshots, so it is read at any depth.
//! [APFS specification: j_snap_metadata_key_t / j_snap_metadata_val_t;
//!  linux-apfs-rw snapshot.c apfs_switch_to_snapshot]

use crate::block_device::BlockDevice;
use crate::btree::BtreeNode;
use crate::container::ContainerError;
use crate::endian::{u16_le, u32_le, u64_le, ParseError};
use crate::jkey::{JKey, APFS_TYPE_SNAP_METADATA};
use crate::obj::{OBJECT_TYPE_BTREE, OBJECT_TYPE_BTREE_NODE};
use std::collections::HashSet;

/// `o_subtype` of every node of the snapshot metadata tree.
pub const OBJECT_TYPE_SNAPMETATREE: u32 = 0x0000_0010;
/// `j_snap_metadata_val_t.flags`: the snapshot is dataless; its content is
/// not on this volume.
pub const SNAP_META_PENDING_DATALESS: u32 = 0x0000_0001;
/// Deeper than any snapshot tree is.
const MAX_DEPTH: u16 = 8;
/// More nodes than any snapshot tree has: a cycle, or garbage.
const MAX_NODES: usize = 1 << 16;
/// `j_snap_metadata_val_t` up to its name: extent-reference tree oid and
/// type, superblock oid, times, inode number, flags and name length.
const SNAP_VAL_FIXED: usize = 50;

/// One snapshot of a volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotMeta {
    /// The transaction the snapshot froze.
    pub xid: u64,
    /// Its name, without the trailing NUL.
    pub name: String,
    /// Physical address of the volume superblock frozen with it.
    pub sblock_oid: u64,
    /// Creation time, nanoseconds since 1970-01-01 UTC.
    pub create_time: u64,
    /// `SNAP_META_*` flags.
    pub flags: u32,
}

fn bad(field: &'static str, value: u64) -> ContainerError {
    ContainerError::Parse(ParseError::BadField { field, value })
}

/// Every snapshot in the snapshot metadata tree rooted at physical `root`,
/// oldest first. A root of 0 is a volume without the tree: no snapshots.
pub fn list<D: BlockDevice>(
    dev: &mut D,
    root: u64,
    block_size: u32,
) -> Result<Vec<SnapshotMeta>, ContainerError> {
    let mut out: Vec<SnapshotMeta> = Vec::new();
    if root == 0 {
        return Ok(out);
    }
    let mut seen: HashSet<u64> = HashSet::new();
    // (address, level its parent expects, whether it is the root)
    let mut stack: Vec<(u64, Option<u16>, bool)> = vec![(root, None, true)];
    while let Some((paddr, want_level, is_root)) = stack.pop() {
        if !seen.insert(paddr) || seen.len() > MAX_NODES {
            return Err(bad("snapshot tree node reached twice", paddr));
        }
        let mut buf = vec![0u8; block_size as usize];
        let at = paddr
            .checked_mul(u64::from(block_size))
            .ok_or_else(|| bad("snapshot tree node address", paddr))?;
        dev.read_at(at, &mut buf)?;
        let node = BtreeNode::parse(&buf)?;
        let want_type = if is_root {
            OBJECT_TYPE_BTREE
        } else {
            OBJECT_TYPE_BTREE_NODE
        };
        if node.obj.object_type() != want_type || node.obj.subtype != OBJECT_TYPE_SNAPMETATREE {
            return Err(bad("snapshot tree node type", u64::from(node.obj.subtype)));
        }
        if node.is_root() != is_root
            || node.is_leaf() != (node.level == 0)
            || node.level > MAX_DEPTH
            || want_level.is_some_and(|l| l != node.level)
            || node.is_fixed_kv()
        {
            return Err(bad("snapshot tree node flags", u64::from(node.flags)));
        }
        let mut children: Vec<u64> = Vec::new();
        for i in 0..node.nkeys {
            let (k, v) = node.var_kv(i)?;
            if !node.is_leaf() {
                children.push(u64_le(v, 0)?);
                continue;
            }
            let key = JKey::parse(k)?;
            if key.obj_type != APFS_TYPE_SNAP_METADATA {
                continue; // the SNAP_NAME records repeat the names
            }
            if v.len() < SNAP_VAL_FIXED {
                return Err(bad("snapshot metadata length", v.len() as u64));
            }
            let name_len = usize::from(u16_le(v, 48)?);
            let raw = v
                .get(SNAP_VAL_FIXED..SNAP_VAL_FIXED + name_len)
                .ok_or_else(|| bad("snapshot name length", name_len as u64))?;
            let raw = raw.strip_suffix(&[0u8]).unwrap_or(raw);
            out.push(SnapshotMeta {
                xid: key.obj_id,
                name: String::from_utf8_lossy(raw).into_owned(),
                sblock_oid: u64_le(v, 8)?,
                create_time: u64_le(v, 16)?,
                flags: u32_le(v, 44)?,
            });
        }
        let below = node.level.checked_sub(1);
        // Pushed last, read first: reverse to keep key order.
        for c in children.into_iter().rev() {
            stack.push((c, below, false));
        }
    }
    out.sort_by_key(|s| s.xid);
    Ok(out)
}
