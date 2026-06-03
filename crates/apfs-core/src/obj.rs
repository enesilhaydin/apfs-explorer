//! `obj_phys_t` - the 32-byte header prefixing paged APFS objects.
//! Offsets/constants: ctx_search(source:"the APFS specification").
use crate::endian::{u32_le, u64_le, ParseError};

pub const OBJ_PHYS_SIZE: usize = 32;
pub const OBJECT_TYPE_MASK: u32 = 0x0000_ffff;
pub const OBJECT_TYPE_FLAGS_MASK: u32 = 0xffff_0000;
pub const OBJ_STORAGETYPE_MASK: u32 = 0xc000_0000;
pub const OBJECT_TYPE_NX_SUPERBLOCK: u32 = 0x0000_0001;
pub const OBJECT_TYPE_BTREE: u32 = 0x0000_0002;
pub const OBJECT_TYPE_BTREE_NODE: u32 = 0x0000_0003;
pub const OBJECT_TYPE_OMAP: u32 = 0x0000_000b;
pub const OBJECT_TYPE_FS: u32 = 0x0000_000d; // apfs_superblock_t (volume superblock)

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjPhys {
    pub cksum: u64,
    pub oid: u64,
    pub xid: u64,
    pub o_type: u32,
    pub subtype: u32,
}

impl ObjPhys {
    /// Parse the 32-byte header from the start of `block`.
    pub fn parse(block: &[u8]) -> Result<Self, ParseError> {
        Ok(Self {
            cksum: u64_le(block, 0)?,
            oid: u64_le(block, 8)?,
            xid: u64_le(block, 16)?,
            o_type: u32_le(block, 24)?,
            subtype: u32_le(block, 28)?,
        })
    }
    /// Object type without storage-class/flag bits.
    pub const fn object_type(&self) -> u32 {
        self.o_type & OBJECT_TYPE_MASK
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn parses_header_and_masks_type() {
        let mut b = [0u8; 32];
        b[0..8].copy_from_slice(&0xAABB_u64.to_le_bytes());
        b[8..16].copy_from_slice(&1u64.to_le_bytes());
        b[16..24].copy_from_slice(&7u64.to_le_bytes());
        b[24..28].copy_from_slice(&0x4000_0001u32.to_le_bytes());
        b[28..32].copy_from_slice(&0u32.to_le_bytes());
        let o = ObjPhys::parse(&b).unwrap();
        assert_eq!(o.oid, 1);
        assert_eq!(o.xid, 7);
        assert_eq!(o.o_type, 0x4000_0001);
        assert_eq!(o.object_type(), OBJECT_TYPE_NX_SUPERBLOCK);
    }

    #[test]
    fn short_block_errors() {
        assert!(ObjPhys::parse(&[0u8; 16]).is_err());
    }
}
