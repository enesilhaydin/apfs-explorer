//! `nx_superblock_t` - APFS container superblock (physical block 0).
//! Field offsets: ctx_search(source:"the APFS specification").
use crate::checksum::verify_block;
use crate::endian::{i64_le, u32_le, u64_le, ParseError};
use crate::obj::{ObjPhys, OBJECT_TYPE_NX_SUPERBLOCK};

pub const NX_MAGIC: u32 = 0x4253_584e; // 'NXSB' little-endian
pub const NX_MAX_FILE_SYSTEMS: usize = 100;
/// The container superblock is the one object whose identifier is fixed.
pub const NX_SUPERBLOCK_OID: u64 = 1;
/// High bit of `nx_xp_desc_blocks` / `nx_xp_data_blocks`: the area is a
/// B-tree rather than a contiguous run of blocks, and the base field holds
/// the tree's oid instead of a block address.
pub const XP_AREA_TREE_FLAG: u32 = 0x8000_0000;

/// `nx_incompatible_features` bit: the container spans two devices (Fusion).
///
/// Tier-2 (rotational) blocks are tagged with the high bit of the block
/// address, so a reader that ignores the split resolves those addresses
/// against the wrong device. Fusion is a declared non-goal: detect it in order
/// to refuse the container instead of mis-addressing it.
pub const NX_INCOMPAT_FUSION: u64 = 0x0000_0100;

#[derive(Debug, Clone)]
pub struct NxSuperblock {
    pub obj: ObjPhys,
    pub block_size: u32,
    pub block_count: u64,
    pub xp_desc_blocks: u32,
    pub xp_desc_base: i64,
    /// The checkpoint data area: `nx_xp_data_blocks` (offset 108) and
    /// `nx_xp_data_base` (offset 120), where the ephemeral objects live.
    pub xp_data_blocks: u32,
    pub xp_data_base: i64,
    /// First descriptor-ring slot of this checkpoint (`nx_xp_desc_index`,
    /// offset 136) and how many slots it spans (`nx_xp_desc_len`, offset
    /// 140): its checkpoint maps, then this superblock as the last slot.
    pub xp_desc_index: u32,
    pub xp_desc_len: u32,
    pub next_xid: u64,
    pub spaceman_oid: u64,
    pub omap_oid: u64,
    pub reaper_oid: u64,
    pub max_file_systems: u32,
    /// `nx_incompatible_features` at offset 64. Carries the Fusion bit and the
    /// container version; a bit we do not understand means the on-disk format
    /// is not the one this code implements.
    pub incompatible_features: u64,
    /// The volume oids of `nx_fs_oid`, empty slots left out.
    pub fs_oids: Vec<u64>,
    /// Every occupied slot of `nx_fs_oid` as `(slot, oid)`. Deleting a volume
    /// leaves its slot empty and the volumes after it where they were
    /// (measured on macOS: three volumes, the middle one deleted, the third
    /// stays in slot 2 and diskutil keeps naming it `s3`), so a volume's
    /// index is its slot, not its position among the occupied ones.
    pub fs_slots: Vec<(u32, u64)>,
    /// Container UUID - `nx_uuid` at offset 0x48 (72).
    /// Confirmed: linux-apfs-rw apfs_raw.h /*48*/ (a hex offset = 0x48 = 72) +
    /// apfsprogs raw.h + a real newfs_apfs container (diskutil-reported UUID).
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
        // Reject an implausible block size before it is used downstream as a
        // buffer length or a block-address multiplier: a crafted value would
        // drive multi-GiB allocations (vec![0u8; block_size]) and overflow
        // paddr * block_size. macOS only ever writes 4096 and the write path
        // enforces the same invariant. A wider but still-safe rule would be
        // (512..=65536).contains(&block_size) && block_size.is_power_of_two().
        if block_size != 4096 {
            return Err(ParseError::BadBlockSize(block_size));
        }
        let block_count = u64_le(block, 40)?;
        let xp_desc_blocks = u32_le(block, 104)?;
        let xp_desc_base = i64_le(block, 112)?;
        let xp_data_blocks = u32_le(block, 108)?;
        let xp_data_base = i64_le(block, 120)?;
        let xp_desc_index = u32_le(block, 136)?;
        let xp_desc_len = u32_le(block, 140)?;
        let next_xid = u64_le(block, 96)?;
        let spaceman_oid = u64_le(block, 152)?;
        let omap_oid = u64_le(block, 160)?;
        let reaper_oid = u64_le(block, 168)?;
        let max_file_systems = u32_le(block, 180)?;
        let incompatible_features = u64_le(block, 64)?;
        // nx_uuid at offset 0x48 = 72, after nx_features@48,
        // nx_readonly_compatible_features@56, nx_incompatible_features@64. The
        // previous offset 48 misread the linux-apfs-rw /*48*/ HEX annotation as
        // decimal and read nx_features (all-zero on a plain container). Confirmed
        // on a real newfs_apfs container: bytes 72..88 equal the UUID
        // `diskutil info` reports; bytes 48..64 are all zero.
        let uuid: [u8; 16] =
            block
                .get(72..88)
                .and_then(|s| s.try_into().ok())
                .ok_or(ParseError::Short {
                    at: 72,
                    need: 16,
                    len: block.len(),
                })?;
        if obj.oid != NX_SUPERBLOCK_OID {
            return Err(ParseError::BadField {
                field: "nx_o_oid",
                value: obj.oid,
            });
        }
        // The container's byte size bounds every block address; a count that
        // does not even fit the arithmetic describes nothing.
        if block_count.checked_mul(u64::from(block_size)).is_none() {
            return Err(ParseError::BadField {
                field: "nx_block_count",
                value: block_count,
            });
        }
        let desc = checkpoint_area("nx_xp_desc_base", xp_desc_base, xp_desc_blocks, block_count)?;
        let data = checkpoint_area("nx_xp_data_base", xp_data_base, xp_data_blocks, block_count)?;
        // The descriptor ring and the data ring are distinct; overlapping
        // ones would have a checkpoint map overwrite the space manager.
        if let (Some((desc_start, desc_end)), Some((data_start, data_end))) = (desc, data) {
            if desc_start < data_end && data_start < desc_end {
                return Err(ParseError::BadField {
                    field: "nx_xp_data_base",
                    value: data_start,
                });
            }
        }
        // The array of volume ids has room for 100 entries; a larger count is
        // not clamped to it but refused, since the array it describes does
        // not exist.
        if max_file_systems as usize > NX_MAX_FILE_SYSTEMS {
            return Err(ParseError::BadField {
                field: "nx_max_file_systems",
                value: u64::from(max_file_systems),
            });
        }
        let n = max_file_systems as usize;
        let mut fs_oids = Vec::with_capacity(n);
        let mut fs_slots = Vec::with_capacity(n);
        for (slot, i) in (0u32..).zip(0..n) {
            let oid = u64_le(block, 184 + i * 8)?;
            if oid != 0 {
                fs_oids.push(oid);
                fs_slots.push((slot, oid));
            }
        }
        Ok(Self {
            obj,
            block_size,
            block_count,
            xp_desc_blocks,
            xp_desc_base,
            xp_data_blocks,
            xp_data_base,
            xp_desc_index,
            xp_desc_len,
            next_xid,
            spaceman_oid,
            omap_oid,
            reaper_oid,
            max_file_systems,
            incompatible_features,
            fs_oids,
            fs_slots,
            uuid,
        })
    }

    /// True when the container spans two devices (Fusion). Its tier-2 block
    /// addresses are tagged, so resolving them without the split reads the
    /// wrong device.
    pub fn is_fusion(&self) -> bool {
        self.incompatible_features & NX_INCOMPAT_FUSION != 0
    }
}

/// The blocks `[start, end)` of a contiguous checkpoint area, checked to lie
/// inside the container. Block 0 is the superblock itself, so an area starts
/// after it, and an empty area leaves the checkpoint nowhere to live. A
/// tree-form area (high bit set) keeps a B-tree oid in the base field rather
/// than a block address: it is not a range, and the checkpoint walker
/// declines it (apfsprogs refuses that form outright).
fn checkpoint_area(
    field: &'static str,
    base: i64,
    blocks: u32,
    block_count: u64,
) -> Result<Option<(u64, u64)>, ParseError> {
    if blocks & XP_AREA_TREE_FLAG != 0 {
        return Ok(None);
    }
    let bad = || ParseError::BadField {
        field,
        value: base as u64,
    };
    if base <= 0 || blocks == 0 {
        return Err(bad());
    }
    let start = base as u64;
    let end = start.checked_add(u64::from(blocks)).ok_or_else(bad)?;
    if end > block_count {
        return Err(bad());
    }
    Ok(Some((start, end)))
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
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture`");
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
        // nx_uuid must be the real container UUID (offset 72), not the all-zero
        // nx_features field at the previously-misread offset 48.
        assert_ne!(sb.uuid, [0u8; 16], "container nx_uuid must be non-zero");
    }

    /// Build a minimally valid NXSB with the given nx_incompatible_features:
    /// oid 1, 64 blocks, the checkpoint descriptor area at blocks 1..3 and
    /// the data area at blocks 3..7.
    fn make_nxsb(incompat: u64) -> Vec<u8> {
        let mut b = vec![0u8; 4096];
        b[8..16].copy_from_slice(&NX_SUPERBLOCK_OID.to_le_bytes());
        b[24..28].copy_from_slice(&OBJECT_TYPE_NX_SUPERBLOCK.to_le_bytes());
        b[32..36].copy_from_slice(&NX_MAGIC.to_le_bytes());
        b[36..40].copy_from_slice(&4096u32.to_le_bytes());
        b[40..48].copy_from_slice(&64u64.to_le_bytes()); // nx_block_count
        b[64..72].copy_from_slice(&incompat.to_le_bytes()); // nx_incompatible_features
        b[104..108].copy_from_slice(&2u32.to_le_bytes()); // nx_xp_desc_blocks
        b[108..112].copy_from_slice(&4u32.to_le_bytes()); // nx_xp_data_blocks
        b[112..120].copy_from_slice(&1i64.to_le_bytes()); // nx_xp_desc_base
        b[120..128].copy_from_slice(&3i64.to_le_bytes()); // nx_xp_data_base
        reseal(&mut b);
        b
    }

    fn reseal(b: &mut [u8]) {
        let c = crate::checksum::fletcher64(b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
    }

    /// `make_nxsb(0)` with one field patched and the checksum redone.
    fn nxsb_with(patch: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let mut b = make_nxsb(0);
        patch(&mut b);
        reseal(&mut b);
        b
    }

    /// The container superblock is the one object with a fixed identifier;
    /// a block that carries another one is not a container superblock.
    #[test]
    fn rejects_wrong_superblock_oid() {
        let b = nxsb_with(|b| b[8..16].copy_from_slice(&2u64.to_le_bytes()));
        assert!(
            matches!(
                NxSuperblock::parse(&b),
                Err(ParseError::BadField {
                    field: "nx_o_oid",
                    value: 2
                })
            ),
            "oid 2 must be refused"
        );
    }

    /// A checkpoint area has to lie inside the container's blocks; one that
    /// runs past `nx_block_count` would have every slot read past the end.
    #[test]
    fn rejects_checkpoint_area_past_the_container() {
        let b = nxsb_with(|b| {
            b[112..120].copy_from_slice(&60i64.to_le_bytes()); // desc base 60
            b[104..108].copy_from_slice(&8u32.to_le_bytes()); // 60 + 8 = 68 > 64 blocks
        });
        assert!(
            matches!(
                NxSuperblock::parse(&b),
                Err(ParseError::BadField {
                    field: "nx_xp_desc_base",
                    ..
                })
            ),
            "a descriptor area past the container must be refused"
        );
        let b = nxsb_with(|b| b[120..128].copy_from_slice(&i64::MAX.to_le_bytes()));
        assert!(
            NxSuperblock::parse(&b).is_err(),
            "a data area whose end overflows must be refused"
        );
    }

    /// The descriptor and data areas are distinct rings; overlapping ones
    /// would let a checkpoint map overwrite the spaceman.
    #[test]
    fn rejects_overlapping_checkpoint_areas() {
        let b = nxsb_with(|b| b[120..128].copy_from_slice(&2i64.to_le_bytes())); // data 2..6 over desc 1..3
        assert!(
            matches!(
                NxSuperblock::parse(&b),
                Err(ParseError::BadField {
                    field: "nx_xp_data_base",
                    ..
                })
            ),
            "overlapping checkpoint areas must be refused"
        );
    }

    /// Block 0 holds this superblock, so no area starts there, and an empty
    /// area leaves the checkpoint nowhere to live.
    #[test]
    fn rejects_empty_or_zero_based_checkpoint_area() {
        let b = nxsb_with(|b| b[108..112].copy_from_slice(&0u32.to_le_bytes()));
        assert!(
            NxSuperblock::parse(&b).is_err(),
            "an empty data area must be refused"
        );
        let b = nxsb_with(|b| b[112..120].copy_from_slice(&0i64.to_le_bytes()));
        assert!(
            NxSuperblock::parse(&b).is_err(),
            "a descriptor area starting at block 0 must be refused"
        );
    }

    /// The format allows at most 100 volumes; a larger count is not clamped
    /// but refused, since the array it describes does not exist.
    #[test]
    fn rejects_more_file_systems_than_the_format_allows() {
        let b = nxsb_with(|b| b[180..184].copy_from_slice(&101u32.to_le_bytes()));
        assert!(
            matches!(
                NxSuperblock::parse(&b),
                Err(ParseError::BadField {
                    field: "nx_max_file_systems",
                    value: 101
                })
            ),
            "101 file systems must be refused"
        );
        let b = nxsb_with(|b| b[180..184].copy_from_slice(&100u32.to_le_bytes()));
        assert_eq!(
            NxSuperblock::parse(&b)
                .expect("100 is allowed")
                .max_file_systems,
            100
        );
    }

    /// A tree-form area (high bit set, base is a B-tree oid) is not a block
    /// range and is left to the checkpoint walker, which declines it.
    #[test]
    fn tree_form_checkpoint_area_still_parses() {
        let b = nxsb_with(|b| b[104..108].copy_from_slice(&0x8000_0002u32.to_le_bytes()));
        assert!(NxSuperblock::parse(&b).is_ok());
    }

    #[test]
    fn fusion_container_is_detected() {
        // A Fusion container splits its address space across two devices and
        // tags tier-2 blocks with the high address bit. We do not implement
        // that addressing, so the container must be recognisable in order to
        // be refused rather than mis-addressed.
        let plain = NxSuperblock::parse(&make_nxsb(0)).expect("parse plain");
        assert_eq!(plain.incompatible_features, 0);
        assert!(!plain.is_fusion());

        let fusion = NxSuperblock::parse(&make_nxsb(NX_INCOMPAT_FUSION)).expect("parse fusion");
        assert!(
            fusion.is_fusion(),
            "NX_INCOMPAT_FUSION must be reported as a Fusion container"
        );

        // The version bits sit in the same field and must not be confused with it.
        let versioned = NxSuperblock::parse(&make_nxsb(0x2)).expect("parse versioned");
        assert!(
            !versioned.is_fusion(),
            "unrelated nx_incompatible_features bits must not imply Fusion"
        );
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

    #[test]
    fn rejects_bad_block_size() {
        // Valid checksum + magic + object type, but a bogus 1 MiB block size:
        // parse must reject it before any block_size-derived allocation.
        let mut b = vec![0u8; 4096];
        b[24..28].copy_from_slice(&OBJECT_TYPE_NX_SUPERBLOCK.to_le_bytes()); // obj.o_type
        b[32..36].copy_from_slice(&NX_MAGIC.to_le_bytes());
        b[36..40].copy_from_slice(&0x0010_0000u32.to_le_bytes()); // 1 MiB, != 4096
        let c = crate::checksum::fletcher64(&b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
        assert!(matches!(
            NxSuperblock::parse(&b),
            Err(ParseError::BadBlockSize(0x0010_0000))
        ));
    }
}
