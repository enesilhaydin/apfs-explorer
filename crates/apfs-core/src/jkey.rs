//! j_key_t - the 8-byte header at the start of every file-system (catalog)
//! record key. Constants/layout: ctx_search(source:"the APFS specification").
use crate::endian::{u64_le, ParseError};

pub const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
pub const OBJ_TYPE_MASK: u64 = 0xf000_0000_0000_0000;
pub const OBJ_TYPE_SHIFT: u64 = 60;

// j_obj_types - the record type stored in the j_key_t type bits.
pub const APFS_TYPE_SNAP_METADATA: u8 = 1;
pub const APFS_TYPE_EXTENT: u8 = 2;
pub const APFS_TYPE_INODE: u8 = 3;
pub const APFS_TYPE_XATTR: u8 = 4;
pub const APFS_TYPE_SIBLING_LINK: u8 = 5;
pub const APFS_TYPE_DSTREAM_ID: u8 = 6;
pub const APFS_TYPE_CRYPTO_STATE: u8 = 7;
pub const APFS_TYPE_FILE_EXTENT: u8 = 8;
pub const APFS_TYPE_DIR_REC: u8 = 9;
pub const APFS_TYPE_DIR_STATS: u8 = 10;
pub const APFS_TYPE_SNAP_NAME: u8 = 11;
pub const APFS_TYPE_SIBLING_MAP: u8 = 12;
pub const APFS_TYPE_FILE_INFO: u8 = 13;

// Reserved inode numbers (Apple "Inode Numbers", p96).
pub const ROOT_DIR_INO_NUM: u64 = 2;
pub const PRIV_DIR_INO_NUM: u64 = 3;

/// Parsed j_key_t header: object identifier and record type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JKey {
    pub obj_id: u64,
    pub obj_type: u8,
}

impl JKey {
    /// Parse the leading 8-byte `obj_id_and_type` field.
    pub fn parse(key: &[u8]) -> Result<Self, ParseError> {
        let v = u64_le(key, 0)?;
        Ok(Self {
            obj_id: v & OBJ_ID_MASK,
            obj_type: ((v & OBJ_TYPE_MASK) >> OBJ_TYPE_SHIFT) as u8,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn splits_obj_id_and_type() {
        // type = INODE (3) in high 4 bits, id = 42 in low 60 bits.
        let v: u64 = (3u64 << 60) | 42u64;
        let jk = JKey::parse(&v.to_le_bytes()).unwrap();
        assert_eq!(jk.obj_id, 42);
        assert_eq!(jk.obj_type, APFS_TYPE_INODE);
    }

    #[test]
    fn short_buffer_errors_not_panics() {
        assert!(JKey::parse(&[0u8; 4]).is_err());
    }
}
