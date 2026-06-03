//! omap_phys_t (object map) - parse + locate its B-tree root.
//! Field offsets CONFIRMED (3 sources): ctx_search(source:"the APFS specification").
use crate::block_device::BlockDevice;
use crate::btree::BtreeNode;
use crate::checksum::verify_block;
use crate::container::ContainerError;
use crate::endian::{u32_le, u64_le};
use crate::obj::{ObjPhys, OBJECT_TYPE_BTREE, OBJECT_TYPE_OMAP};

/// omap_val_t flag: mapping is deleted (tombstone).
pub const OMAP_VAL_DELETED: u32 = 0x0000_0001;

// OBJECT_TYPE_OMAP (0x0000_000b) is imported from obj.rs - already defined there from M1a.

#[derive(Debug, Clone)]
pub struct OmapPhys {
    pub obj: ObjPhys,
    pub flags: u32,
    pub snap_count: u32,
    pub tree_type: u32,
    pub tree_oid: u64, // offset 48 (CONFIRMED: the APFS specification/Apple p44 + + impls)
}

impl OmapPhys {
    pub fn parse(block: &[u8]) -> Result<Self, ContainerError> {
        verify_block(block)?;
        let obj = ObjPhys::parse(block)?;
        if obj.object_type() != OBJECT_TYPE_OMAP {
            return Err(ContainerError::Parse(crate::endian::ParseError::BadMagic {
                expected: OBJECT_TYPE_OMAP,
                found: obj.object_type(),
            }));
        }
        Ok(Self {
            obj,
            flags: u32_le(block, 32)?,
            snap_count: u32_le(block, 36)?,
            tree_type: u32_le(block, 40)?,
            tree_oid: u64_le(block, 48)?,
        })
    }
}

/// The container object map plus its (validated) B-tree root node.
pub struct Omap {
    pub phys: OmapPhys,
    pub root: BtreeNode,
}

impl Omap {
    /// `omap_paddr` = the container superblock's omap_oid (a PHYSICAL oid =
    /// block address). Reads the omap, then its tree root (also physical for
    /// the container omap), validating both.
    pub fn open<D: BlockDevice>(
        dev: &mut D,
        omap_paddr: u64,
        block_size: u32,
    ) -> Result<Self, ContainerError> {
        let bsz = block_size as usize;
        let mut buf = vec![0u8; bsz];
        dev.read_at(
            omap_paddr.checked_mul(block_size as u64).ok_or_else(oor)?,
            &mut buf,
        )?;
        let phys = OmapPhys::parse(&buf)?;
        let mut rbuf = vec![0u8; bsz];
        dev.read_at(
            phys.tree_oid
                .checked_mul(block_size as u64)
                .ok_or_else(oor)?,
            &mut rbuf,
        )?;
        let root = BtreeNode::parse(&rbuf)?;
        if root.obj.object_type() != OBJECT_TYPE_BTREE {
            return Err(ContainerError::Parse(crate::endian::ParseError::BadMagic {
                expected: OBJECT_TYPE_BTREE,
                found: root.obj.object_type(),
            }));
        }
        Ok(Self { phys, root })
    }

    /// Resolve a virtual object id + transaction id to a physical block address
    /// by descending the (physical) object-map B-tree. Returns `None` if the
    /// mapping is absent or the entry is a deleted tombstone.
    ///
    /// Key layout (FIXED_KV, confirmed triple-source):
    ///   omap_key { u64 ok_oid@0, u64 ok_xid@8 }  - 16 bytes
    /// Leaf value layout:
    ///   omap_val { u32 ov_flags@0, u32 ov_size@4, i64 ov_paddr@8 } - 16 bytes
    /// Non-leaf value:
    ///   oid_t (8 bytes) - physical child block address (BTREE_PHYSICAL).
    pub fn resolve<D: BlockDevice>(
        &self,
        dev: &mut D,
        oid: u64,
        xid: u64,
        block_size: u32,
    ) -> Result<Option<u64>, ContainerError> {
        let bsz = block_size as usize;
        let mut node = self.root.clone();

        // Bounded descent: APFS omap trees are shallow (≤4 levels in practice).
        // Cap at 32 to handle corrupt data gracefully - no panic, just None.
        for _ in 0..32 {
            let is_leaf = node.is_leaf();
            // FIXED_KV: key=omap_key(16B). Non-leaf val=oid_t(8B); leaf val=omap_val(16B).
            let vsz: usize = if is_leaf { 16 } else { 8 };

            // Scan all entries to find the best match.
            // Index node: largest (ok_oid, ok_xid) <= (oid, xid).
            // Leaf node:  ok_oid == oid, largest ok_xid <= xid.
            let mut best: Option<(u64, u64, u32)> = None; // (k_oid, k_xid, idx)

            for i in 0..node.nkeys {
                let (k, _) = node.fixed_kv(i, 16, vsz)?;
                let k_oid =
                    u64::from_le_bytes(k.get(0..8).ok_or_else(oor)?.try_into().map_err(|_| oor())?);
                let k_xid = u64::from_le_bytes(
                    k.get(8..16)
                        .ok_or_else(oor)?
                        .try_into()
                        .map_err(|_| oor())?,
                );

                if is_leaf {
                    // Leaf: match only entries with ok_oid == oid and ok_xid <= xid;
                    // keep the one with the largest ok_xid seen so far.
                    if k_oid == oid && k_xid <= xid {
                        match best {
                            Some((_, bx, _)) if bx >= k_xid => {}
                            _ => best = Some((k_oid, k_xid, i)),
                        }
                    }
                } else {
                    // Index: pick largest (ok_oid, ok_xid) <= (oid, xid).
                    if k_oid < oid || (k_oid == oid && k_xid <= xid) {
                        let better = match best {
                            None => true,
                            Some((bo, bx, _)) => k_oid > bo || (k_oid == bo && k_xid > bx),
                        };
                        if better {
                            best = Some((k_oid, k_xid, i));
                        }
                    }
                }
            }

            let Some((_, _, idx)) = best else {
                return Ok(None);
            };

            let (_, v) = node.fixed_kv(idx, 16, vsz)?;

            if is_leaf {
                // omap_val: u32 ov_flags@0, u32 ov_size@4, i64 ov_paddr@8
                let flags =
                    u32::from_le_bytes(v.get(0..4).ok_or_else(oor)?.try_into().map_err(|_| oor())?);
                if flags & OMAP_VAL_DELETED != 0 {
                    return Ok(None);
                }
                let paddr = i64::from_le_bytes(
                    v.get(8..16)
                        .ok_or_else(oor)?
                        .try_into()
                        .map_err(|_| oor())?,
                );
                return Ok(Some(paddr as u64));
            }

            // Non-leaf: child is a physical block address (BTREE_PHYSICAL).
            let child =
                u64::from_le_bytes(v.get(0..8).ok_or_else(oor)?.try_into().map_err(|_| oor())?);
            let mut buf = vec![0u8; bsz];
            dev.read_at(
                child.checked_mul(block_size as u64).ok_or_else(oor)?,
                &mut buf,
            )?;
            node = BtreeNode::parse(&buf)?;
        }

        // Exceeded descent bound - corrupt tree, return None gracefully.
        Ok(None)
    }
}

fn oor() -> ContainerError {
    ContainerError::Parse(crate::endian::ParseError::Short {
        at: 0,
        need: 0,
        len: 0,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod parse_tests {
    use super::*;
    use crate::btree::{BTNODE_FIXED_KV_SIZE, BTNODE_LEAF, BTNODE_ROOT};
    use crate::checksum::fletcher64;
    use crate::obj::{OBJECT_TYPE_BTREE, OBJECT_TYPE_NX_SUPERBLOCK, OBJECT_TYPE_OMAP};

    // Minimal in-memory block device (same pattern as other test modules).
    struct MemDev {
        blocks: std::collections::HashMap<u64, Vec<u8>>,
        block_size: usize,
    }
    impl MemDev {
        fn new(bsz: usize) -> Self {
            Self {
                blocks: std::collections::HashMap::new(),
                block_size: bsz,
            }
        }
        fn insert(&mut self, addr: u64, data: Vec<u8>) {
            self.blocks.insert(addr, data);
        }
    }
    impl crate::block_device::BlockDevice for MemDev {
        fn size(&self) -> u64 {
            (self.blocks.len() * self.block_size) as u64
        }
        fn read_at(
            &mut self,
            offset: u64,
            buf: &mut [u8],
        ) -> Result<(), crate::block_device::BlockError> {
            let bsz = self.block_size as u64;
            let addr = offset / bsz;
            match self.blocks.get(&addr) {
                Some(d) => {
                    let l = buf.len().min(d.len());
                    buf[..l].copy_from_slice(&d[..l]);
                    Ok(())
                }
                None => Err(crate::block_device::BlockError::Io(std::io::Error::other(
                    format!("block {addr} missing"),
                ))),
            }
        }
    }

    fn make_omap_block(tree_oid: u64) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[8..16].copy_from_slice(&1u64.to_le_bytes()); // oid
        b[16..24].copy_from_slice(&1u64.to_le_bytes()); // xid
        b[24..28].copy_from_slice(&OBJECT_TYPE_OMAP.to_le_bytes()); // o_type
                                                                    // flags=0, snap_count=0, tree_type=0 already zero
        b[48..56].copy_from_slice(&tree_oid.to_le_bytes()); // tree_oid at off 48
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        b
    }

    fn make_btree_leaf_empty() -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[8..16].copy_from_slice(&2u64.to_le_bytes()); // oid
        b[16..24].copy_from_slice(&1u64.to_le_bytes()); // xid
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        let flags = BTNODE_LEAF | BTNODE_FIXED_KV_SIZE | BTNODE_ROOT;
        b[32..34].copy_from_slice(&flags.to_le_bytes());
        // nkeys=0
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        b
    }

    #[test]
    fn omap_phys_parse_wrong_type_rejected() {
        // Build a block with OBJECT_TYPE_NX_SUPERBLOCK instead of OBJECT_TYPE_OMAP.
        let mut b = vec![0u8; 4096];
        b[24..28].copy_from_slice(&OBJECT_TYPE_NX_SUPERBLOCK.to_le_bytes());
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        let err = OmapPhys::parse(&b);
        assert!(err.is_err(), "wrong object type must be rejected");
    }

    #[test]
    fn omap_phys_parse_bad_checksum_rejected() {
        let mut b = make_omap_block(10);
        b[100] ^= 0xff; // corrupt a byte → bad checksum
        let err = OmapPhys::parse(&b);
        assert!(err.is_err(), "bad checksum must be rejected");
    }

    #[test]
    fn omap_open_bad_root_type_rejected() {
        // omap block points to tree_oid=2, but block 2 has wrong object type.
        let omap_blk = make_omap_block(2);
        // Build a block at addr 2 with OBJECT_TYPE_OMAP instead of OBJECT_TYPE_BTREE.
        let mut bad_root = vec![0u8; 4096];
        bad_root[8..16].copy_from_slice(&2u64.to_le_bytes());
        bad_root[24..28].copy_from_slice(&OBJECT_TYPE_OMAP.to_le_bytes());
        let ck = fletcher64(&bad_root);
        bad_root[0..8].copy_from_slice(&ck.to_le_bytes());

        let mut dev = MemDev::new(4096);
        dev.insert(1, omap_blk); // omap at block 1
        dev.insert(2, bad_root); // bad root at block 2

        let err = Omap::open(&mut dev, 1, 4096);
        assert!(
            err.is_err(),
            "root with wrong object type must be rejected by Omap::open"
        );
    }

    #[test]
    fn omap_open_io_error_propagated() {
        // omap block at addr 1 exists but tree_oid=99 which is not in MemDev.
        let omap_blk = make_omap_block(99); // tree_oid=99
        let mut dev = MemDev::new(4096);
        dev.insert(1, omap_blk);
        // block 99 is absent → read_at fails → Omap::open must return Err
        let err = Omap::open(&mut dev, 1, 4096);
        assert!(
            err.is_err(),
            "missing tree root block must propagate IO error"
        );
    }

    #[test]
    fn resolve_omap_val_deleted_returns_none() {
        // Build a leaf node with one entry whose omap_val has OMAP_VAL_DELETED set.
        use crate::btree::BTNODE_ROOT;
        const BSZ: usize = 4096;
        const DATA_BASE: usize = 56;

        let oid_target: u64 = 42;
        let xid_target: u64 = 5;

        // omap_key: oid=42, xid=5
        let mut key = [0u8; 16];
        key[0..8].copy_from_slice(&oid_target.to_le_bytes());
        key[8..16].copy_from_slice(&xid_target.to_le_bytes());

        // omap_val with OMAP_VAL_DELETED flag set
        let mut val = [0u8; 16];
        val[0..4].copy_from_slice(&OMAP_VAL_DELETED.to_le_bytes()); // flags = deleted
        val[4..8].copy_from_slice(&4096u32.to_le_bytes());
        val[8..16].copy_from_slice(&100i64.to_le_bytes()); // paddr=100 (shouldn't be returned)

        let mut b = [0u8; BSZ];
        b[8..16].copy_from_slice(&1u64.to_le_bytes());
        b[16..24].copy_from_slice(&1u64.to_le_bytes());
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        let flags = BTNODE_LEAF | BTNODE_FIXED_KV_SIZE | BTNODE_ROOT;
        b[32..34].copy_from_slice(&flags.to_le_bytes());
        b[36..40].copy_from_slice(&1u32.to_le_bytes()); // nkeys=1
                                                        // toc at DATA_BASE: kvoff_t { k_off=0, v_off=16 }
        b[DATA_BASE..DATA_BASE + 2].copy_from_slice(&0u16.to_le_bytes());
        b[DATA_BASE + 2..DATA_BASE + 4].copy_from_slice(&16u16.to_le_bytes());
        // key at DATA_BASE + toc_len(4) = 60
        let key_start = DATA_BASE + 4;
        b[key_start..key_start + 16].copy_from_slice(&key);
        // val at BSZ - BTREE_INFO(40) - v_off(16) = 4040
        let val_end = BSZ - 40; // root node: val_area_end = BSZ - BTREE_INFO_SIZE
        let val_start = val_end - 16;
        b[val_start..val_start + 16].copy_from_slice(&val);
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());

        let root = crate::btree::BtreeNode::parse(&b).expect("parse leaf");
        let omap = Omap {
            phys: OmapPhys {
                obj: ObjPhys {
                    cksum: 0,
                    oid: 0,
                    xid: 0,
                    o_type: OBJECT_TYPE_OMAP,
                    subtype: 0,
                },
                flags: 0,
                snap_count: 0,
                tree_type: 0,
                tree_oid: 1,
            },
            root,
        };
        // MemDev is not needed (single-level leaf), but resolve signature requires D.
        let mut dev = MemDev::new(4096);
        let result = omap
            .resolve(&mut dev, oid_target, xid_target, 4096)
            .expect("resolve must not error");
        assert_eq!(result, None, "deleted omap_val must return None");
    }

    #[test]
    fn resolve_no_match_returns_none() {
        // Empty leaf node - resolving any oid must return None gracefully.
        let leaf_blk = make_btree_leaf_empty();
        let root = crate::btree::BtreeNode::parse(&leaf_blk).expect("parse empty leaf");
        let omap = Omap {
            phys: OmapPhys {
                obj: ObjPhys {
                    cksum: 0,
                    oid: 0,
                    xid: 0,
                    o_type: OBJECT_TYPE_OMAP,
                    subtype: 0,
                },
                flags: 0,
                snap_count: 0,
                tree_type: 0,
                tree_oid: 1,
            },
            root,
        };
        let mut dev = MemDev::new(4096);
        let result = omap.resolve(&mut dev, 999, 9, 4096).expect("no error");
        assert_eq!(result, None, "empty omap must return None for any oid");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::block_device::FileBlockDevice;
    use crate::container::Container;
    use std::path::Path;

    #[test]
    fn opens_real_container_omap_and_btree_root() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open image");
        let c = Container::open(&mut dev).expect("open real APFS container");
        let sb = c.superblock;
        let block_size = sb.block_size;
        let omap_oid = sb.omap_oid;
        assert!(omap_oid != 0, "container omap_oid must be non-zero");

        let omap = Omap::open(&mut dev, omap_oid, block_size)
            .expect("Omap::open must succeed on real image");

        assert_eq!(
            omap.phys.obj.object_type(),
            OBJECT_TYPE_OMAP,
            "omap block object_type must be OMAP (0xb)"
        );
        assert!(omap.phys.tree_oid != 0, "om_tree_oid must be non-zero");
        assert_eq!(
            omap.root.obj.object_type(),
            OBJECT_TYPE_BTREE,
            "tree root object_type must be BTREE (0x2)"
        );
        assert!(
            omap.root.is_root(),
            "tree root node must have BTNODE_ROOT flag"
        );

        eprintln!(
            "omap_oid={} tree_oid={} omap_obj_type=0x{:x} root_obj_type=0x{:x} root_flags=0x{:x}",
            omap_oid,
            omap.phys.tree_oid,
            omap.phys.obj.object_type(),
            omap.root.obj.object_type(),
            omap.root.flags,
        );
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]
mod resolve_tests {
    use super::*;
    use crate::block_device::{BlockDevice, BlockError, FileBlockDevice};
    use crate::btree::{BTNODE_FIXED_KV_SIZE, BTNODE_LEAF, BTNODE_ROOT};
    use crate::checksum::fletcher64;
    use crate::container::Container;
    use crate::obj::{OBJECT_TYPE_BTREE, OBJECT_TYPE_FS};
    use std::path::Path;

    // -----------------------------------------------------------------------
    // (a) Real-image test: resolve fs_oids[0] → OBJECT_TYPE_FS block
    // -----------------------------------------------------------------------
    #[test]
    fn real_image_resolve_fs_oid_to_volume_superblock() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open image");
        let c = Container::open(&mut dev).expect("open container");
        let sb = &c.superblock;
        let block_size = sb.block_size;
        assert!(!sb.fs_oids.is_empty(), "need at least one fs_oid");

        let omap = Omap::open(&mut dev, sb.omap_oid, block_size).expect("open omap");

        let fs_oid = sb.fs_oids[0];
        let xid = sb.obj.xid;

        let paddr = omap
            .resolve(&mut dev, fs_oid, xid, block_size)
            .expect("resolve must not error")
            .expect("resolve must return Some(paddr) for valid fs_oid");

        eprintln!("fs_oid={fs_oid} xid={xid} → paddr={paddr}");

        // Read that block and verify it is an APFS volume superblock (0x0d).
        let mut buf = vec![0u8; block_size as usize];
        dev.read_at(paddr * block_size as u64, &mut buf)
            .expect("read resolved block");
        let obj = ObjPhys::parse(&buf).expect("parse ObjPhys of resolved block");
        let object_type = obj.object_type();
        eprintln!("resolved block object_type = 0x{object_type:x}");
        assert_eq!(
            object_type, OBJECT_TYPE_FS,
            "resolved block must be OBJECT_TYPE_FS (0x0d), got 0x{object_type:x}"
        );
    }

    // -----------------------------------------------------------------------
    // Minimal in-memory block device for synthetic tests
    // -----------------------------------------------------------------------
    struct MemDev {
        blocks: std::collections::HashMap<u64, Vec<u8>>,
        block_size: usize,
    }

    impl MemDev {
        fn new(block_size: usize) -> Self {
            Self {
                blocks: std::collections::HashMap::new(),
                block_size,
            }
        }
        fn insert(&mut self, block_addr: u64, data: Vec<u8>) {
            self.blocks.insert(block_addr, data);
        }
    }

    impl BlockDevice for MemDev {
        fn size(&self) -> u64 {
            (self.blocks.len() * self.block_size) as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let bsz = self.block_size as u64;
            if !offset.is_multiple_of(bsz) {
                return Err(BlockError::Io(std::io::Error::other("unaligned read")));
            }
            let block_addr = offset / bsz;
            match self.blocks.get(&block_addr) {
                Some(data) => {
                    let len = buf.len().min(data.len());
                    buf[..len].copy_from_slice(&data[..len]);
                    if len < buf.len() {
                        buf[len..].fill(0);
                    }
                    Ok(())
                }
                None => Err(BlockError::Io(std::io::Error::other(format!(
                    "block {block_addr} not in MemDev"
                )))),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Block builders (real Fletcher-64)
    //
    // Layout for a FIXED_KV node with N entries:
    //   [0..8]   checksum (fletcher64, filled last)
    //   [8..16]  oid (u64 LE)
    //   [16..24] xid (u64 LE) - we set 1
    //   [24..28] o_type (u32 LE) = OBJECT_TYPE_BTREE
    //   [28..32] subtype = 0
    //   [32..34] btn_flags (u16 LE)
    //   [34..36] btn_level (u16 LE)
    //   [36..40] btn_nkeys (u32 LE)
    //   [40..42] toc_off (u16 LE) = 0   (toc starts at DATA_BASE=56)
    //   [42..44] toc_len (u16 LE) = N*4
    //   [44..56] (reserved / padding - zero)
    //   DATA_BASE=56: toc entries: N × kvoff_t { k_off:u16, v_off:u16 }
    //   key area starts at DATA_BASE + toc_off + toc_len = 56 + 0 + N*4
    //   value area:
    //     non-root: ends at block_size (4096)
    //     root:     ends at block_size - BTREE_INFO_SIZE (40) = 4056
    //   v_off is measured backward from val_area_end:
    //     value at [val_area_end - v_off .. val_area_end - v_off + vsz]
    // -----------------------------------------------------------------------
    const BSZ: usize = 4096;
    const DATA_BASE: usize = 56;
    const BTREE_INFO_SIZE: usize = 40;

    /// Write a u64 LE at byte offset `off` in `b`.
    fn w64(b: &mut [u8], off: usize, v: u64) {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    /// Write a u32 LE at byte offset `off` in `b`.
    fn w32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    /// Write a u16 LE at byte offset `off` in `b`.
    fn w16(b: &mut [u8], off: usize, v: u16) {
        b[off..off + 2].copy_from_slice(&v.to_le_bytes());
    }

    /// Build an omap_key = { ok_oid:u64, ok_xid:u64 } → 16 bytes.
    fn omap_key_bytes(ok_oid: u64, ok_xid: u64) -> [u8; 16] {
        let mut k = [0u8; 16];
        k[0..8].copy_from_slice(&ok_oid.to_le_bytes());
        k[8..16].copy_from_slice(&ok_xid.to_le_bytes());
        k
    }

    /// Build an omap_val = { ov_flags:u32=0, ov_size:u32=4096, ov_paddr:i64 } → 16 bytes.
    fn omap_val_bytes(ov_paddr: u64) -> [u8; 16] {
        let mut v = [0u8; 16];
        // ov_flags = 0 (not deleted)
        v[0..4].copy_from_slice(&0u32.to_le_bytes());
        // ov_size = 4096
        v[4..8].copy_from_slice(&4096u32.to_le_bytes());
        // ov_paddr = ov_paddr cast to i64
        v[8..16].copy_from_slice(&(ov_paddr as i64).to_le_bytes());
        v
    }

    /// Build a FIXED_KV LEAF node (level=0, flags=LEAF|FIXED_KV, NOT root)
    /// with the given (key16, val16) pairs.
    fn make_leaf_node(oid: u64, entries: &[([u8; 16], [u8; 16])]) -> [u8; BSZ] {
        let n = entries.len();
        let mut b = [0u8; BSZ];
        // obj header
        w64(&mut b, 8, oid); // oid
        w64(&mut b, 16, 1u64); // xid
        w32(&mut b, 24, OBJECT_TYPE_BTREE); // o_type
                                            // btn header
        w16(&mut b, 32, BTNODE_LEAF | BTNODE_FIXED_KV_SIZE); // flags: LEAF | FIXED_KV
        w16(&mut b, 34, 0u16); // level = 0
        w32(&mut b, 36, n as u32); // nkeys
        w16(&mut b, 40, 0u16); // toc_off = 0
        w16(&mut b, 42, (n * 4) as u16); // toc_len = n * sizeof(kvoff_t)

        // key area starts at: DATA_BASE + toc_off(0) + toc_len(n*4) = 56 + n*4
        let key_area_start = DATA_BASE + n * 4; // toc_off=0 so toc at DATA_BASE
                                                // val area end: leaf (not root) → BSZ = 4096
        let val_area_end = BSZ;

        for (i, (key, val)) in entries.iter().enumerate() {
            let k_off = i * 16; // key_i starts at key_area_start + i*16
                                // Values packed from val_area_end downward, entry 0 nearest end.
                                // v_off is bytes-back from val_area_end to the START of the value.
                                // entry 0: val at [val_area_end-16 .. val_area_end], v_off = (i+1)*16
            let v_off = (i + 1) * 16;

            // Write kvoff_t at DATA_BASE + i*4
            let toc_entry = DATA_BASE + i * 4;
            w16(&mut b, toc_entry, k_off as u16);
            w16(&mut b, toc_entry + 2, v_off as u16);

            // Write key
            let kstart = key_area_start + k_off;
            b[kstart..kstart + 16].copy_from_slice(key);

            // Write val: val_area_end - v_off
            let vstart = val_area_end - v_off;
            b[vstart..vstart + 16].copy_from_slice(val);
        }

        // Checksum
        let c = fletcher64(&b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
        b
    }

    /// Build a FIXED_KV INDEX (non-leaf) ROOT node (level=1, flags=ROOT|FIXED_KV)
    /// with the given (key16, child_block_addr:u64) pairs.
    /// Non-leaf values are oid_t = 8 bytes (child physical block address).
    fn make_index_root(oid: u64, entries: &[([u8; 16], u64)]) -> [u8; BSZ] {
        let n = entries.len();
        let mut b = [0u8; BSZ];
        // obj header
        w64(&mut b, 8, oid);
        w64(&mut b, 16, 1u64); // xid
        w32(&mut b, 24, OBJECT_TYPE_BTREE);
        // btn header - ROOT | FIXED_KV (NOT LEAF, level=1)
        w16(&mut b, 32, BTNODE_ROOT | BTNODE_FIXED_KV_SIZE);
        w16(&mut b, 34, 1u16); // level = 1
        w32(&mut b, 36, n as u32); // nkeys
        w16(&mut b, 40, 0u16); // toc_off = 0
        w16(&mut b, 42, (n * 4) as u16); // toc_len

        // key area starts at: DATA_BASE + toc_len = 56 + n*4
        let key_area_start = DATA_BASE + n * 4;
        // val area end: ROOT → BSZ - BTREE_INFO_SIZE = 4056
        let val_area_end = BSZ - BTREE_INFO_SIZE;

        for (i, (key, child_addr)) in entries.iter().enumerate() {
            let k_off = i * 16;
            // Non-leaf vals are 8 bytes.
            let v_off = (i + 1) * 8;

            let toc_entry = DATA_BASE + i * 4;
            w16(&mut b, toc_entry, k_off as u16);
            w16(&mut b, toc_entry + 2, v_off as u16);

            let kstart = key_area_start + k_off;
            b[kstart..kstart + 16].copy_from_slice(key);

            let vstart = val_area_end - v_off;
            b[vstart..vstart + 8].copy_from_slice(&child_addr.to_le_bytes());
        }

        // Checksum
        let c = fletcher64(&b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
        b
    }

    // -----------------------------------------------------------------------
    // (b) Synthetic 2-level tree test
    // -----------------------------------------------------------------------
    #[test]
    fn synthetic_two_level_descent() {
        // Block layout:
        //   block 1 = index root (level=1, ROOT|FIXED_KV)
        //   block 2 = leaf A (oid=10, paddr=0xAAAA)
        //   block 3 = leaf B (oid=20, paddr=0xBBBB)
        //
        // Index root entries:
        //   key(oid=10, xid=1) → child block 2
        //   key(oid=20, xid=1) → child block 3
        //
        // Leaf A (block 2): key(oid=10, xid=5) → omap_val{flags=0, size=4096, paddr=0xAAAA}
        // Leaf B (block 3): key(oid=20, xid=5) → omap_val{flags=0, size=4096, paddr=0xBBBB}

        let block_size: u32 = 4096;

        let root_block = make_index_root(
            1, // oid=1 for root node
            &[
                (omap_key_bytes(10, 1), 2), // key(oid=10,xid=1) → block 2
                (omap_key_bytes(20, 1), 3), // key(oid=20,xid=1) → block 3
            ],
        );
        let leaf_a = make_leaf_node(
            2, // oid=2
            &[(omap_key_bytes(10, 5), omap_val_bytes(0xAAAA))],
        );
        let leaf_b = make_leaf_node(
            3, // oid=3
            &[(omap_key_bytes(20, 5), omap_val_bytes(0xBBBB))],
        );

        // Verify our hand-built blocks parse correctly before we start.
        let root_node = BtreeNode::parse(&root_block).expect("root parses");
        assert!(root_node.is_root(), "root must have ROOT flag");
        assert!(!root_node.is_leaf(), "root must NOT be leaf");
        assert_eq!(root_node.level, 1);
        assert_eq!(root_node.nkeys, 2);

        BtreeNode::parse(&leaf_a).expect("leaf A parses");
        BtreeNode::parse(&leaf_b).expect("leaf B parses");

        // Build MemDev
        let mut dev = MemDev::new(block_size as usize);
        dev.insert(1, root_block.to_vec()); // root at block 1
        dev.insert(2, leaf_a.to_vec()); // leaf A at block 2
        dev.insert(3, leaf_b.to_vec()); // leaf B at block 3

        // Build a minimal OmapPhys (fields not used by resolve, only root matters)
        let omap = Omap {
            phys: OmapPhys {
                obj: ObjPhys {
                    cksum: 0,
                    oid: 0,
                    xid: 0,
                    o_type: OBJECT_TYPE_OMAP,
                    subtype: 0,
                },
                flags: 0,
                snap_count: 0,
                tree_type: 0,
                tree_oid: 1,
            },
            root: root_node,
        };

        // resolve(oid=20, xid=9) → index picks key(20,1)<=( 20,9), descend block 3,
        // leaf picks key(20,5)<=(20,9) → paddr=0xBBBB
        let r20 = omap
            .resolve(&mut dev, 20, 9, block_size)
            .expect("resolve(20,9) must not error");
        assert_eq!(
            r20,
            Some(0xBBBB),
            "resolve(20,9) must be Some(0xBBBB), got {r20:?}"
        );

        // resolve(oid=10, xid=9) → index picks key(10,1)<=(10,9), descend block 2,
        // leaf picks key(10,5)<=(10,9) → paddr=0xAAAA
        let r10 = omap
            .resolve(&mut dev, 10, 9, block_size)
            .expect("resolve(10,9) must not error");
        assert_eq!(
            r10,
            Some(0xAAAA),
            "resolve(10,9) must be Some(0xAAAA), got {r10:?}"
        );

        // resolve(oid=99, xid=9) → no key with oid=99 in any leaf → None
        let r99 = omap
            .resolve(&mut dev, 99, 9, block_size)
            .expect("resolve(99,9) must not error");
        assert_eq!(r99, None, "resolve(99,9) must be None, got {r99:?}");
    }

    // -----------------------------------------------------------------------
    // xid boundary: key present but ok_xid > query_xid → must return None.
    // -----------------------------------------------------------------------
    #[test]
    fn resolve_returns_none_when_only_key_has_higher_xid() {
        // Single-leaf tree: key(oid=5, xid=10). Query xid=3 < 10 → no match → None.
        // Confirms the k_xid <= xid guard in the leaf branch.
        const BSZ: usize = 4096;
        let block_size: u32 = 4096;

        let leaf = make_leaf_node(1, &[(omap_key_bytes(5, 10), omap_val_bytes(0xCAFE))]);
        let root_node = BtreeNode::parse(&leaf).expect("leaf parses");

        let omap = Omap {
            phys: OmapPhys {
                obj: ObjPhys {
                    cksum: 0,
                    oid: 0,
                    xid: 0,
                    o_type: OBJECT_TYPE_OMAP,
                    subtype: 0,
                },
                flags: 0,
                snap_count: 0,
                tree_type: 0,
                tree_oid: 1,
            },
            root: root_node,
        };

        let mut dev = MemDev::new(BSZ);
        // No extra blocks needed - root is a leaf.

        let result = omap
            .resolve(&mut dev, 5, 3, block_size)
            .expect("must not error");
        assert_eq!(result, None, "xid=3 < key xid=10 must return None");

        // Sanity: querying with xid=10 must find it.
        let found = omap
            .resolve(&mut dev, 5, 10, block_size)
            .expect("must not error");
        assert_eq!(found, Some(0xCAFE), "xid=10 must resolve to 0xCAFE");
    }

    // -----------------------------------------------------------------------
    // Depth-limit guard: a chain of 32+ index nodes returns None gracefully.
    // -----------------------------------------------------------------------
    #[test]
    fn resolve_depth_limit_returns_none_gracefully() {
        // Build a chain of 33 single-entry index nodes pointing to each other.
        // Node i (block i+1) points to block i+2. The last node (block 34)
        // points to block 35 which is absent → read returns zeros → parse fails
        // inside the loop OR the 32-iteration cap fires first. Either way the
        // result must be Ok(None), not a panic.
        const BSZ: usize = 4096;
        let block_size: u32 = 4096;

        let mut dev = MemDev::new(BSZ);

        // Build 33 index nodes: block b points to block b+1.
        // We only need 33 hops to exceed the 32-iteration cap.
        for b in 1u64..=33 {
            let child_addr = b + 1;
            let node_raw = make_index_root(b, &[(omap_key_bytes(42, 1), child_addr)]);
            dev.insert(b, node_raw.to_vec());
        }
        // Block 34 intentionally absent → MemDev returns Io error.

        // Root is block 1.
        let root_raw = make_index_root(1, &[(omap_key_bytes(42, 1), 2)]);
        dev.insert(1, root_raw.to_vec());
        let root_node = BtreeNode::parse(&root_raw).expect("root parses");

        let omap = Omap {
            phys: OmapPhys {
                obj: ObjPhys {
                    cksum: 0,
                    oid: 0,
                    xid: 0,
                    o_type: OBJECT_TYPE_OMAP,
                    subtype: 0,
                },
                flags: 0,
                snap_count: 0,
                tree_type: 0,
                tree_oid: 1,
            },
            root: root_node,
        };

        // Must not panic - either returns Ok(None) from cap or Err from IO.
        let result = omap.resolve(&mut dev, 42, 99, block_size);
        // Both Ok(None) and Err are acceptable - no panic is the requirement.
        match result {
            Ok(None) => {} // depth cap fired gracefully
            Ok(Some(_)) => panic!("must not resolve through an unbounded chain"),
            Err(_) => {} // IO error after 32 descents - also acceptable
        }
    }

    // -----------------------------------------------------------------------
    // Multi-version snapshot: two leaf entries for the same oid, different xids.
    // resolve must pick the one with the largest xid <= query.
    // -----------------------------------------------------------------------
    #[test]
    fn resolve_picks_latest_xid_not_exceeding_query() {
        // Leaf has two entries: key(oid=7, xid=2)→paddr=0x100, key(oid=7, xid=8)→paddr=0x200.
        // Query(oid=7, xid=5) → only xid=2 qualifies (2<=5, 8>5) → paddr=0x100.
        // Query(oid=7, xid=10) → both qualify; largest xid=8 wins → paddr=0x200.
        const BSZ: usize = 4096;
        let block_size: u32 = 4096;

        let leaf = make_leaf_node(
            1,
            &[
                (omap_key_bytes(7, 2), omap_val_bytes(0x100)),
                (omap_key_bytes(7, 8), omap_val_bytes(0x200)),
            ],
        );
        let root_node = BtreeNode::parse(&leaf).expect("leaf parses");

        let omap = Omap {
            phys: OmapPhys {
                obj: ObjPhys {
                    cksum: 0,
                    oid: 0,
                    xid: 0,
                    o_type: OBJECT_TYPE_OMAP,
                    subtype: 0,
                },
                flags: 0,
                snap_count: 0,
                tree_type: 0,
                tree_oid: 1,
            },
            root: root_node,
        };

        let mut dev = MemDev::new(BSZ);

        let r5 = omap.resolve(&mut dev, 7, 5, block_size).expect("no error");
        assert_eq!(r5, Some(0x100), "xid=5: only xid=2 entry qualifies → 0x100");

        let r10 = omap.resolve(&mut dev, 7, 10, block_size).expect("no error");
        assert_eq!(
            r10,
            Some(0x200),
            "xid=10: xid=8 entry is newest qualifying → 0x200"
        );
    }
}
