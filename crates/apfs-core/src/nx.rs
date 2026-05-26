//! `nx_superblock_t` — APFS container superblock (physical block 0).
//! Field offsets: ctx_search(source:"apfs-spec:m1-confirmed").
use crate::checksum::verify_block;
use crate::endian::{i64_le, u32_le, u64_le, ParseError};
use crate::obj::{ObjPhys, OBJECT_TYPE_NX_SUPERBLOCK};

pub const NX_MAGIC: u32 = 0x4253_584e; // 'NXSB' little-endian
pub const NX_MAX_FILE_SYSTEMS: usize = 100;

#[derive(Debug, Clone)]
pub struct NxSuperblock {
    pub obj: ObjPhys,
    pub block_size: u32,
    pub block_count: u64,
    pub xp_desc_blocks: u32,
    pub xp_desc_base: i64,
    pub next_xid: u64,
    pub spaceman_oid: u64,
    pub omap_oid: u64,
    pub reaper_oid: u64,
    pub max_file_systems: u32,
    pub fs_oids: Vec<u64>,
    /// Container UUID — `nx_uuid` at offset 0x30 (48).
    /// Confirmed: linux-apfs-rw apfs_raw.h /*48*/ + apfsprogs raw.h /*48*/.
    pub uuid: [u8; 16],
}

impl NxSuperblock {
    pub fn parse(block: &[u8]) -> Result<Self, ParseError> {
        verify_block(block)?;
        let obj = ObjPhys::parse(block)?;
        let magic = u32_le(block, 32)?;
        if magic != NX_MAGIC {
            return Err(ParseError::BadMagic {
                expected: NX_MAGIC,
                found: magic,
            });
        }
        if obj.object_type() != OBJECT_TYPE_NX_SUPERBLOCK {
            return Err(ParseError::BadMagic {
                expected: OBJECT_TYPE_NX_SUPERBLOCK,
                found: obj.object_type(),
            });
        }
        let block_size = u32_le(block, 36)?;
        let block_count = u64_le(block, 40)?;
        let xp_desc_blocks = u32_le(block, 104)?;
        let xp_desc_base = i64_le(block, 112)?;
        let next_xid = u64_le(block, 96)?;
        let spaceman_oid = u64_le(block, 152)?;
        let omap_oid = u64_le(block, 160)?;
        let reaper_oid = u64_le(block, 168)?;
        let max_file_systems = u32_le(block, 180)?;
        // nx_uuid at offset 0x30 = 48.
        // Cross-check: linux-apfs-rw apfs_raw.h /*48*/ + apfsprogs raw.h /*48*/.
        let uuid: [u8; 16] = block
            .get(48..64)
            .and_then(|s| s.try_into().ok())
            .ok_or(ParseError::Short { at: 48, need: 16, len: block.len() })?;
        let n = (max_file_systems as usize).min(NX_MAX_FILE_SYSTEMS);
        let mut fs_oids = Vec::with_capacity(n);
        for i in 0..n {
            let oid = u64_le(block, 184 + i * 8)?;
            if oid != 0 {
                fs_oids.push(oid);
            }
        }
        Ok(Self {
            obj,
            block_size,
            block_count,
            xp_desc_blocks,
            xp_desc_base,
            next_xid,
            spaceman_oid,
            omap_oid,
            reaper_oid,
            max_file_systems,
            fs_oids,
            uuid,
        })
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_real_container_superblock() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing — run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let data = std::fs::read(img).unwrap();
        let sb = NxSuperblock::parse(&data[0..4096]).expect("real nx_superblock");
        assert_eq!(sb.obj.object_type(), OBJECT_TYPE_NX_SUPERBLOCK);
        assert_eq!(sb.block_size, 4096, "block_size {}", sb.block_size);
        assert!(sb.block_count > 0);
        assert!(sb.omap_oid != 0, "container omap oid must be set");
        assert!(!sb.fs_oids.is_empty(), "at least one volume fs_oid");
        assert!(
            sb.xp_desc_blocks > 0,
            "xp_desc_blocks {}",
            sb.xp_desc_blocks
        );
        assert!(sb.xp_desc_base > 0, "xp_desc_base {}", sb.xp_desc_base);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut b = vec![0u8; 4096];
        let c = crate::checksum::fletcher64(&b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
        assert!(matches!(
            NxSuperblock::parse(&b),
            Err(ParseError::BadMagic { .. })
        ));
    }
}
