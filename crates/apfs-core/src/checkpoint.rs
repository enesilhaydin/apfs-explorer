//! Checkpoint descriptor area walk: select the newest valid container
//! superblock (highest xid) instead of trusting the block-0 bootstrap copy.
//! Spec: ctx_search(source:"the APFS specification").
use crate::block_device::BlockDevice;
use crate::container::ContainerError;
use crate::nx::NxSuperblock;

/// Walk the checkpoint descriptor ring described by `bootstrap`
/// (`xp_desc_base` .. `+ (xp_desc_blocks & 0x7FFF_FFFF)`) and return the
/// `nx_superblock` with the largest `xid` that is well-formed (valid
/// Fletcher-64, NX_MAGIC and object type - all enforced by
/// `NxSuperblock::parse`). If the descriptor area is non-contiguous
/// (high bit of `xp_desc_blocks` set => `xp_desc_base` is a B-tree oid, out
/// of M1b scope) or nothing valid is found, fall back to `bootstrap`.
pub fn latest_superblock<D: BlockDevice>(
    dev: &mut D,
    bootstrap: &NxSuperblock,
) -> Result<NxSuperblock, ContainerError> {
    const XP_DESC_TREE_FLAG: u32 = 0x8000_0000;
    let bsize = bootstrap.block_size as usize;
    if bsize < 1024
        || bootstrap.xp_desc_base <= 0
        || bootstrap.xp_desc_blocks & XP_DESC_TREE_FLAG != 0
    {
        return Ok(bootstrap.clone());
    }
    let count = bootstrap.xp_desc_blocks & 0x7FFF_FFFF;
    if count == 0 {
        return Ok(bootstrap.clone());
    }
    let base = bootstrap.xp_desc_base as u64;
    let mut best = bootstrap.clone();
    let mut buf = vec![0u8; bsize];
    for i in 0..u64::from(count) {
        let off = base
            .checked_add(i)
            .and_then(|b| b.checked_mul(bsize as u64));
        let Some(off) = off else { continue };
        if dev.read_at(off, &mut buf).is_err() {
            continue;
        }
        // Non-superblock ring blocks (checkpoint maps) fail this parse and are
        // skipped; only valid superblocks with a larger xid win.
        if let Ok(sb) = NxSuperblock::parse(&buf) {
            if sb.obj.xid > best.obj.xid {
                best = sb;
            }
        }
    }
    Ok(best)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::block_device::{BlockDevice, BlockError};
    use crate::checksum::fletcher64;
    use crate::nx::NX_MAGIC;
    use crate::obj::OBJECT_TYPE_NX_SUPERBLOCK;

    struct MemDev(Vec<u8>);
    impl BlockDevice for MemDev {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let o = offset as usize;
            let end = o + buf.len();
            if end > self.0.len() {
                return Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.0.len() as u64,
                });
            }
            buf.copy_from_slice(&self.0[o..end]);
            Ok(())
        }
    }

    fn make_sb(xid: u64, xp_base: i64, xp_blocks: u32) -> [u8; 4096] {
        let mut b = [0u8; 4096];
        b[8..16].copy_from_slice(&1u64.to_le_bytes());
        b[16..24].copy_from_slice(&xid.to_le_bytes());
        b[24..28].copy_from_slice(&OBJECT_TYPE_NX_SUPERBLOCK.to_le_bytes());
        b[32..36].copy_from_slice(&NX_MAGIC.to_le_bytes());
        b[36..40].copy_from_slice(&4096u32.to_le_bytes());
        b[40..48].copy_from_slice(&64u64.to_le_bytes());
        b[104..108].copy_from_slice(&xp_blocks.to_le_bytes());
        b[112..120].copy_from_slice(&xp_base.to_le_bytes());
        b[180..184].copy_from_slice(&1u32.to_le_bytes());
        b[184..192].copy_from_slice(&1026u64.to_le_bytes());
        let c = fletcher64(&b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
        b
    }

    #[test]
    fn picks_highest_valid_xid_from_ring() {
        let boot = make_sb(5, 1, 2);
        let mut img = vec![0u8; 4096 * 3];
        img[0..4096].copy_from_slice(&boot);
        img[4096..8192].copy_from_slice(&make_sb(5, 1, 2));
        img[8192..12288].copy_from_slice(&make_sb(9, 1, 2));
        let mut dev = MemDev(img);
        let bootstrap = NxSuperblock::parse(&boot).unwrap();
        let sel = latest_superblock(&mut dev, &bootstrap).unwrap();
        assert_eq!(sel.obj.xid, 9);
    }

    #[test]
    fn skips_corrupt_higher_xid_keeps_valid() {
        let boot = make_sb(7, 1, 2);
        let mut img = vec![0u8; 4096 * 3];
        img[0..4096].copy_from_slice(&boot);
        img[4096..8192].copy_from_slice(&make_sb(7, 1, 2));
        let mut corrupt = make_sb(99, 1, 2);
        corrupt[200] ^= 0xFF;
        img[8192..12288].copy_from_slice(&corrupt);
        let mut dev = MemDev(img);
        let bootstrap = NxSuperblock::parse(&boot).unwrap();
        let sel = latest_superblock(&mut dev, &bootstrap).unwrap();
        assert_eq!(sel.obj.xid, 7, "corrupt xid 99 must be rejected");
    }

    #[test]
    fn non_contiguous_falls_back_to_bootstrap() {
        // high bit of xp_desc_blocks set => B-tree, out of scope -> bootstrap.
        let boot = make_sb(3, 1, 0x8000_0002);
        let mut dev = MemDev(boot.to_vec());
        let bootstrap = NxSuperblock::parse(&boot).unwrap();
        let sel = latest_superblock(&mut dev, &bootstrap).unwrap();
        assert_eq!(sel.obj.xid, 3);
    }
}
