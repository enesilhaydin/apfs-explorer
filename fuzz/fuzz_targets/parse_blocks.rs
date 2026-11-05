//! Every parser of an on-disk block or record, on arbitrary bytes: each must
//! return an error for input it cannot use, never panic or read out of
//! bounds.
#![no_main]

use apfs_core::btree::BtreeNode;
use apfs_core::decmpfs::DecmpfsHeader;
use apfs_core::inode::{parse_unknown_xfields, Inode, JDstream};
use apfs_core::jkey::JKey;
use apfs_core::nx::NxSuperblock;
use apfs_core::obj::ObjPhys;
use apfs_core::omap::OmapPhys;
use apfs_core::volume::VolumeSuperblock;
use apfs_core::xattr::{xattr_key_name, xattr_val, XattrDstream};
use libfuzzer_sys::fuzz_target;

/// Walk every record of a node, both ways a node can lay them out.
fn walk(node: &BtreeNode) {
    for i in 0..node.nkeys.min(4096) {
        let _ = node.var_kv(i);
        let _ = node.fixed_kv(i, 16, 16);
        let _ = node.fixed_kv(i, 8, 16);
        let _ = node.fixed_kv(i, 16, 8);
    }
}

fuzz_target!(|data: &[u8]| {
    let _ = ObjPhys::parse(data);
    let _ = NxSuperblock::parse(data);
    let _ = VolumeSuperblock::parse(data);
    let _ = OmapPhys::parse(data);
    if let Ok(n) = BtreeNode::parse(data) {
        walk(&n);
    }
    if let Ok(n) = BtreeNode::parse_headerless(data) {
        walk(&n);
    }
    let _ = Inode::parse(data);
    let _ = JDstream::parse(data);
    let _ = parse_unknown_xfields(data);
    let _ = JKey::parse(data);
    let _ = XattrDstream::parse(data);
    let _ = xattr_key_name(data);
    let _ = xattr_val(data);
    let _ = DecmpfsHeader::parse(data);
});
