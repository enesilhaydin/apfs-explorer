//! Checkpoint descriptor area walk: select the newest valid container
//! superblock (highest xid) instead of trusting the block-0 bootstrap copy.
//! Spec: ctx_search(source:"the APFS specification").
use crate::block_device::BlockDevice;
use crate::container::ContainerError;
use crate::nx::NxSuperblock;

/// Walk the checkpoint descriptor ring described by `bootstrap` (block 0:
/// `xp_desc_base` .. `+ (xp_desc_blocks & 0x7FFF_FFFF)`) and return the
/// well-formed `nx_superblock` with the largest `xid` found there (valid
/// Fletcher-64, NX_MAGIC and object type - all enforced by
/// `NxSuperblock::parse`).
///
/// The ring decides. Block 0 is a copy the writer refreshes after the ring
/// slot, so after a power loss it can lag behind or - from a writer that
/// updated it first - run ahead; a checkpoint only block 0 knows about is
/// not one macOS mounts, so block 0's own xid never competes. It does
/// identify the container: a ring slot that names another container or
/// another geometry (a stale slot left by a reformat) is not a candidate.
/// Block 0 is returned only when the ring holds no valid superblock, or when
/// the descriptor area is non-contiguous (high bit of `xp_desc_blocks` set,
/// `xp_desc_base` a B-tree oid, out of scope).
///
/// [CERTAIN: apfs.kext `nx_checkpoint_find_highest_xid` takes the ring's
///  highest valid xid and `nx_superblock_agrees_with_main_superblock`
///  requires the same uuid, block size, block count and checkpoint areas as
///  block 0 while only logging a block 0 that is newer; apfsprogs
///  `read_latest_super` scans the ring alone]
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
    let mut best: Option<NxSuperblock> = None;
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
        // skipped; among the superblocks of this container the newest wins. A
        // transaction id of zero names no checkpoint, and two superblocks of
        // this container under the same id describe a ring no reader can
        // choose from.
        if let Ok(sb) = NxSuperblock::parse(&buf) {
            if !describes_same_container(&sb, bootstrap) || sb.obj.xid == 0 {
                continue;
            }
            match best.as_ref() {
                Some(b) if b.obj.xid == sb.obj.xid => {
                    return Err(ContainerError::Parse(crate::endian::ParseError::BadMagic {
                        expected: b.obj.xid as u32,
                        found: sb.obj.xid as u32,
                    }));
                }
                Some(b) if b.obj.xid > sb.obj.xid => {}
                _ => best = Some(sb),
            }
        }
    }
    Ok(best.unwrap_or_else(|| bootstrap.clone()))
}

/// Does a ring superblock belong to the container block 0 describes? The
/// identity and the geometry have to agree; the xid may differ.
fn describes_same_container(candidate: &NxSuperblock, bootstrap: &NxSuperblock) -> bool {
    candidate.uuid == bootstrap.uuid
        && candidate.block_size == bootstrap.block_size
        && candidate.block_count == bootstrap.block_count
        && candidate.xp_desc_blocks == bootstrap.xp_desc_blocks
        && candidate.xp_desc_base == bootstrap.xp_desc_base
        && candidate.xp_data_blocks == bootstrap.xp_data_blocks
        && candidate.xp_data_base == bootstrap.xp_data_base
}

/// Physical block of the ephemeral object `oid` in the checkpoint `nx`
/// belongs to. A checkpoint's descriptor-ring slots (`xp_desc_index`, then
/// `xp_desc_len` of them) hold its checkpoint maps followed by the
/// superblock itself, and every map lists ephemeral objects with the block
/// each was written to. `Ok(None)` when no map names the object, or when
/// the descriptor area is a tree (out of scope, as above).
pub fn ephemeral_paddr<D: BlockDevice>(
    dev: &mut D,
    nx: &NxSuperblock,
    oid: u64,
) -> Result<Option<u64>, ContainerError> {
    Ok(ephemeral_mapping(dev, nx, oid)?.map(|(paddr, _)| paddr))
}

/// The most blocks an ephemeral object is read as. The space manager, the
/// largest, spans a second block on a 4 TB container (its chunk-info
/// addresses outgrow the first); this leaves room far past any disk.
const EPHEMERAL_MAX_BLOCKS: u32 = 256;

/// The whole ephemeral object `oid` of the checkpoint `nx`, its checksum
/// verified over all of it. An object can span several blocks (`cme_size`);
/// in the checkpoint data area, a ring, it goes on at the area's first block
/// past its last. Reading only the first block failed the checksum of a
/// two-block space manager. `Ok(None)` as for [`ephemeral_paddr`].
pub fn ephemeral_object<D: BlockDevice>(
    dev: &mut D,
    nx: &NxSuperblock,
    oid: u64,
) -> Result<Option<Vec<u8>>, ContainerError> {
    let Some((paddr, size)) = ephemeral_mapping(dev, nx, oid)? else {
        return Ok(None);
    };
    let bsize = nx.block_size;
    let blocks = size / bsize;
    if size == 0 || size % bsize != 0 || blocks > EPHEMERAL_MAX_BLOCKS {
        return Err(ContainerError::BadEphemeralSize { oid, size });
    }
    let (area, area_len) = (nx.xp_data_base, u64::from(nx.xp_data_blocks & 0x7FFF_FFFF));
    let in_area = area > 0 && area_len > 0 && {
        let area = area as u64;
        paddr >= area && paddr - area < area_len
    };
    let bad = || ContainerError::BadEphemeralSize { oid, size };
    let mut obj = vec![0u8; size as usize];
    for (i, chunk) in obj.chunks_mut(bsize as usize).enumerate() {
        let block = if in_area {
            let area = area as u64;
            area + (paddr - area + i as u64) % area_len
        } else {
            paddr.checked_add(i as u64).ok_or_else(bad)?
        };
        let off = block.checked_mul(u64::from(bsize)).ok_or_else(bad)?;
        dev.read_at(off, chunk)?;
    }
    crate::checksum::verify_block(&obj)?;
    Ok(Some(obj))
}

/// The block and size (`cme_paddr`, `cme_size`) the checkpoint `nx` gives
/// the ephemeral object `oid`.
fn ephemeral_mapping<D: BlockDevice>(
    dev: &mut D,
    nx: &NxSuperblock,
    oid: u64,
) -> Result<Option<(u64, u32)>, ContainerError> {
    use crate::endian::{u32_le, u64_le};
    use crate::obj::{ObjPhys, OBJECT_TYPE_CHECKPOINT_MAP};
    const XP_DESC_TREE_FLAG: u32 = 0x8000_0000;
    /// `checkpoint_map_phys_t`: `cpm_count` and the first `checkpoint_mapping_t`.
    const MAP_COUNT: usize = 36;
    const MAP_ENTRIES: usize = 40;
    /// `checkpoint_mapping_t`: 40 bytes, `cme_size` at 8, `cme_oid` at 24,
    /// `cme_paddr` at 32.
    const ENTRY_LEN: usize = 40;
    const ENTRY_SIZE: usize = 8;
    const ENTRY_OID: usize = 24;
    const ENTRY_PADDR: usize = 32;

    let bsize = nx.block_size as usize;
    let ring = nx.xp_desc_blocks & 0x7FFF_FFFF;
    if bsize < 1024
        || nx.xp_desc_base <= 0
        || nx.xp_desc_blocks & XP_DESC_TREE_FLAG != 0
        || ring == 0
    {
        return Ok(None);
    }
    let base = nx.xp_desc_base as u64;
    let mut buf = vec![0u8; bsize];
    for i in 0..nx.xp_desc_len {
        let slot = nx.xp_desc_index.wrapping_add(i) % ring;
        let off = base
            .checked_add(u64::from(slot))
            .and_then(|b| b.checked_mul(bsize as u64));
        let Some(off) = off else { continue };
        dev.read_at(off, &mut buf)?;
        let is_map = ObjPhys::parse(&buf)
            .map(|o| o.object_type() == OBJECT_TYPE_CHECKPOINT_MAP)
            .unwrap_or(false);
        if !is_map {
            continue;
        }
        crate::checksum::verify_block(&buf)?;
        let count = (u32_le(&buf, MAP_COUNT)? as usize).min((bsize - MAP_ENTRIES) / ENTRY_LEN);
        for j in 0..count {
            let entry = MAP_ENTRIES + j * ENTRY_LEN;
            if u64_le(&buf, entry + ENTRY_OID)? == oid {
                let paddr = u64_le(&buf, entry + ENTRY_PADDR)?;
                return Ok(Some((paddr, u32_le(&buf, entry + ENTRY_SIZE)?)));
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::block_device::{BlockDevice, BlockError};
    use crate::checksum::fletcher64;
    use crate::nx::NX_MAGIC;
    use crate::obj::OBJECT_TYPE_NX_SUPERBLOCK;

    /// Ring of three: a checkpoint map naming two ephemerals, the superblock,
    /// then a stale slot; the superblock says the checkpoint starts at slot 0
    /// and spans two slots.
    #[test]
    fn ephemeral_paddr_reads_the_checkpoint_map() {
        let mut sb = make_sb(9, 1, 3);
        sb[136..140].copy_from_slice(&0u32.to_le_bytes());
        sb[140..144].copy_from_slice(&2u32.to_le_bytes());
        let c = fletcher64(&sb);
        sb[0..8].copy_from_slice(&c.to_le_bytes());
        let mut map = [0u8; 4096];
        map[24..28].copy_from_slice(&(0x4000_000cu32).to_le_bytes());
        map[36..40].copy_from_slice(&2u32.to_le_bytes());
        for (j, (oid, paddr)) in [(1024u64, 77u64), (1025, 78)].iter().enumerate() {
            let e = 40 + j * 40;
            map[e + 24..e + 32].copy_from_slice(&oid.to_le_bytes());
            map[e + 32..e + 40].copy_from_slice(&paddr.to_le_bytes());
        }
        let c = fletcher64(&map);
        map[0..8].copy_from_slice(&c.to_le_bytes());
        let mut img = vec![0u8; 4096 * 4];
        img[4096..8192].copy_from_slice(&map);
        img[8192..12288].copy_from_slice(&sb);
        let mut dev = MemDev(img);
        let nx = NxSuperblock::parse(&sb).unwrap();
        assert_eq!(ephemeral_paddr(&mut dev, &nx, 1025).unwrap(), Some(78));
        assert_eq!(ephemeral_paddr(&mut dev, &nx, 1024).unwrap(), Some(77));
        assert_eq!(ephemeral_paddr(&mut dev, &nx, 5).unwrap(), None);
        // A damaged map is an error, not a miss.
        let mut dev = MemDev(vec![0u8; 4096 * 4]);
        let mut bad = map;
        bad[100] ^= 1;
        dev.0[4096..8192].copy_from_slice(&bad);
        dev.0[8192..12288].copy_from_slice(&sb);
        assert!(ephemeral_paddr(&mut dev, &nx, 1024).is_err());
    }

    /// A spaceman of two blocks, as macOS writes on a 4 TB container: its
    /// checksum covers both blocks, and in the checkpoint data area it can
    /// start on the area's last block and go on at its first.
    #[test]
    fn an_ephemeral_object_larger_than_a_block_is_read_whole() {
        let mut sb = make_sb(9, 1, 3);
        sb[136..140].copy_from_slice(&0u32.to_le_bytes());
        sb[140..144].copy_from_slice(&2u32.to_le_bytes());
        let c = fletcher64(&sb);
        sb[0..8].copy_from_slice(&c.to_le_bytes());
        let mut map = [0u8; 4096];
        map[24..28].copy_from_slice(&(0x4000_000cu32).to_le_bytes());
        map[36..40].copy_from_slice(&1u32.to_le_bytes());
        // cme_size 8192, cme_oid 1024, cme_paddr 9: the data area's last block.
        map[40 + 8..40 + 12].copy_from_slice(&8192u32.to_le_bytes());
        map[40 + 24..40 + 32].copy_from_slice(&1024u64.to_le_bytes());
        map[40 + 32..40 + 40].copy_from_slice(&9u64.to_le_bytes());
        let c = fletcher64(&map);
        map[0..8].copy_from_slice(&c.to_le_bytes());
        let mut obj = vec![0u8; 8192];
        obj[8..16].copy_from_slice(&1024u64.to_le_bytes());
        obj[24..28].copy_from_slice(&0x8000_0005u32.to_le_bytes());
        obj[5000] = 0x5A; // in the second block
        let c = fletcher64(&obj);
        obj[0..8].copy_from_slice(&c.to_le_bytes());
        let mut img = vec![0u8; 4096 * 10];
        img[4096..8192].copy_from_slice(&map);
        img[8192..12288].copy_from_slice(&sb);
        img[9 * 4096..10 * 4096].copy_from_slice(&obj[..4096]);
        img[8 * 4096..9 * 4096].copy_from_slice(&obj[4096..]);
        let mut dev = MemDev(img);
        let nx = NxSuperblock::parse(&sb).unwrap();
        assert_eq!(
            ephemeral_object(&mut dev, &nx, 1024).unwrap(),
            Some(obj.clone())
        );
        assert_eq!(ephemeral_object(&mut dev, &nx, 5).unwrap(), None);
        // A damaged second block fails the object's checksum.
        dev.0[8 * 4096 + 100] ^= 1;
        assert!(ephemeral_object(&mut dev, &nx, 1024).is_err());
    }

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
        // The data area: blocks 8..10, clear of every descriptor area used here.
        b[108..112].copy_from_slice(&2u32.to_le_bytes());
        b[120..128].copy_from_slice(&8i64.to_le_bytes());
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

    // Two valid superblocks of the container under one transaction id leave
    // no checkpoint to choose; a transaction id of zero names none.
    #[test]
    fn a_ring_with_a_duplicated_xid_is_refused_and_xid_zero_is_no_candidate() {
        let boot = make_sb(5, 1, 2);
        let mut img = vec![0u8; 4096 * 3];
        img[0..4096].copy_from_slice(&boot);
        img[4096..8192].copy_from_slice(&make_sb(9, 1, 2));
        img[8192..12288].copy_from_slice(&make_sb(9, 1, 2));
        let mut dev = MemDev(img.clone());
        let bootstrap = NxSuperblock::parse(&boot).unwrap();
        assert!(
            latest_superblock(&mut dev, &bootstrap).is_err(),
            "two checkpoints under xid 9"
        );

        img[4096..8192].copy_from_slice(&make_sb(0, 1, 2));
        img[8192..12288].copy_from_slice(&make_sb(7, 1, 2));
        let mut dev = MemDev(img);
        let sel = latest_superblock(&mut dev, &bootstrap).unwrap();
        assert_eq!(sel.obj.xid, 7, "xid 0 is skipped, not chosen or refused");
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

    /// A power loss after block 0 was refreshed but before the ring slot
    /// landed (the order an older writer used) leaves block 0 ahead. macOS
    /// mounts the ring's newest checkpoint and merely logs the newer copy, so
    /// block 0's xid must not compete.
    #[test]
    fn a_block_zero_newer_than_the_ring_does_not_win() {
        let boot = make_sb(9, 1, 2);
        let mut img = vec![0u8; 4096 * 3];
        img[0..4096].copy_from_slice(&boot);
        img[4096..8192].copy_from_slice(&make_sb(5, 1, 2));
        img[8192..12288].copy_from_slice(&make_sb(7, 1, 2));
        let mut dev = MemDev(img);
        let bootstrap = NxSuperblock::parse(&boot).unwrap();
        let sel = latest_superblock(&mut dev, &bootstrap).unwrap();
        assert_eq!(sel.obj.xid, 7, "the ring decides, block 0 only bootstraps");
    }

    #[test]
    fn a_ring_without_a_valid_superblock_falls_back_to_block_zero() {
        let boot = make_sb(4, 1, 2);
        let mut img = vec![0u8; 4096 * 3];
        img[0..4096].copy_from_slice(&boot);
        let mut dev = MemDev(img);
        let bootstrap = NxSuperblock::parse(&boot).unwrap();
        let sel = latest_superblock(&mut dev, &bootstrap).unwrap();
        assert_eq!(sel.obj.xid, 4);
    }

    /// A container formatted over an older one can inherit ring slots of the
    /// old container with higher xids; they name another uuid or another
    /// geometry and must be ignored.
    #[test]
    fn a_ring_slot_of_another_container_is_ignored() {
        let boot = make_sb(5, 1, 2);
        let mut img = vec![0u8; 4096 * 3];
        img[0..4096].copy_from_slice(&boot);
        img[4096..8192].copy_from_slice(&make_sb(6, 1, 2));
        let mut foreign = make_sb(50, 1, 2);
        foreign[72..88].copy_from_slice(&[0xABu8; 16]);
        let c = fletcher64(&foreign);
        foreign[0..8].copy_from_slice(&c.to_le_bytes());
        img[8192..12288].copy_from_slice(&foreign);
        let mut dev = MemDev(img);
        let bootstrap = NxSuperblock::parse(&boot).unwrap();
        let sel = latest_superblock(&mut dev, &bootstrap).unwrap();
        assert_eq!(
            sel.obj.xid, 6,
            "another container's superblock is no candidate"
        );

        // Same uuid, other geometry (block count): also no candidate.
        let mut img = vec![0u8; 4096 * 3];
        img[0..4096].copy_from_slice(&boot);
        img[4096..8192].copy_from_slice(&make_sb(6, 1, 2));
        let mut resized = make_sb(60, 1, 2);
        resized[40..48].copy_from_slice(&128u64.to_le_bytes());
        let c = fletcher64(&resized);
        resized[0..8].copy_from_slice(&c.to_le_bytes());
        img[8192..12288].copy_from_slice(&resized);
        let mut dev = MemDev(img);
        let sel = latest_superblock(&mut dev, &bootstrap).unwrap();
        assert_eq!(
            sel.obj.xid, 6,
            "a superblock of another geometry is no candidate"
        );
    }
}
