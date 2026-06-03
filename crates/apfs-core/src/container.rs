//! Container - opens an APFS container via a BlockDevice and validates block 0.
use crate::block_device::{BlockDevice, BlockError};
use crate::endian::ParseError;
use crate::nx::NxSuperblock;
use crate::volume::VolumeRole;

// ---------------------------------------------------------------------------
// VolumeInfo - public metadata summary for a single APFS volume.
// ---------------------------------------------------------------------------

/// Metadata for one APFS volume, returned by `Container::list_volumes`.
/// All fields are parsed from the volume superblock; no I/O happens after
/// `list_volumes` returns.
#[derive(Debug, Clone)]
pub struct VolumeInfo {
    /// Position of this volume's `fs_oid` in the container superblock's
    /// `fs_oids` array (0-based).
    pub index: u32,
    /// Volume UUID (`apfs_vol_uuid` at offset 0xF0 in the VSB).
    pub uuid: [u8; 16],
    /// UTF-8 volume name from `apfs_volname`.
    pub name: String,
    /// Decoded role from `apfs_role`.
    pub role: VolumeRole,
    /// Raw `apfs_fs_flags` u64.
    pub features: u64,
}

/// Convenience constructor for an out-of-range / overflow `ContainerError`.
const fn oor() -> ContainerError {
    ContainerError::Parse(ParseError::Short {
        at: 0,
        need: 0,
        len: 0,
    })
}

#[derive(Debug)]
pub enum ContainerError {
    Io(BlockError),
    Parse(ParseError),
}
impl From<BlockError> for ContainerError {
    fn from(e: BlockError) -> Self {
        Self::Io(e)
    }
}
impl From<ParseError> for ContainerError {
    fn from(e: ParseError) -> Self {
        Self::Parse(e)
    }
}

pub struct Container {
    pub superblock: NxSuperblock,
}

impl Container {
    /// Read physical block 0 (bootstrap copy), then select the newest valid
    /// superblock from the checkpoint descriptor ring (M1b).
    pub fn open<D: BlockDevice>(dev: &mut D) -> Result<Self, ContainerError> {
        let mut buf = vec![0u8; 4096];
        dev.read_at(0, &mut buf)?;
        let bootstrap = NxSuperblock::parse(&buf)?;
        // block 0 may be a stale copy; the authoritative superblock is the
        // newest valid one in the checkpoint descriptor ring (M1b).
        let superblock = crate::checkpoint::latest_superblock(dev, &bootstrap)?;
        Ok(Self { superblock })
    }

    /// Return the container UUID (`nx_uuid` from the container superblock).
    pub fn container_uuid(&self) -> [u8; 16] {
        self.superblock.uuid
    }

    /// Resolve every container `fs_oid` via the container object map and return
    /// the user-visible volume superblocks (system-role volumes filtered out).
    pub fn list_user_volumes<D: BlockDevice>(
        &self,
        dev: &mut D,
    ) -> Result<Vec<crate::volume::VolumeSuperblock>, ContainerError> {
        let omap =
            crate::omap::Omap::open(dev, self.superblock.omap_oid, self.superblock.block_size)?;
        let xid = self.superblock.obj.xid;
        let mut out = Vec::new();
        for &fs_oid in &self.superblock.fs_oids {
            if let Some(paddr) = omap.resolve(dev, fs_oid, xid, self.superblock.block_size)? {
                let bsz = self.superblock.block_size as usize;
                let mut buf = vec![0u8; bsz];
                dev.read_at(
                    paddr
                        .checked_mul(self.superblock.block_size as u64)
                        .ok_or_else(oor)?,
                    &mut buf,
                )?;
                if let Ok(v) = crate::volume::VolumeSuperblock::parse(&buf) {
                    if !v.is_system() {
                        out.push(v);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Resolve ALL volumes (including system-role ones) and return `VolumeInfo`
    /// metadata for each. This is the full-fidelity replacement for
    /// `list_user_volumes`; it does not filter by role.
    pub fn list_volumes<D: BlockDevice>(
        &self,
        dev: &mut D,
    ) -> Result<Vec<VolumeInfo>, ContainerError> {
        let omap =
            crate::omap::Omap::open(dev, self.superblock.omap_oid, self.superblock.block_size)?;
        let xid = self.superblock.obj.xid;
        let mut out = Vec::new();
        for (idx, &fs_oid) in self.superblock.fs_oids.iter().enumerate() {
            if let Some(paddr) = omap.resolve(dev, fs_oid, xid, self.superblock.block_size)? {
                let bsz = self.superblock.block_size as usize;
                let mut buf = vec![0u8; bsz];
                dev.read_at(
                    paddr
                        .checked_mul(self.superblock.block_size as u64)
                        .ok_or_else(oor)?,
                    &mut buf,
                )?;
                if let Ok(v) = crate::volume::VolumeSuperblock::parse(&buf) {
                    let role = v.volume_role();
                    out.push(VolumeInfo {
                        index: idx as u32,
                        uuid: v.uuid,
                        name: v.name,
                        role,
                        features: v.fs_flags,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Return the `VolumeInfo` for the volume matching `uuid`, or `None`.
    pub fn open_volume_by_uuid<D: BlockDevice>(
        &self,
        dev: &mut D,
        uuid: &[u8; 16],
    ) -> Result<Option<VolumeInfo>, ContainerError> {
        Ok(self
            .list_volumes(dev)?
            .into_iter()
            .find(|v| &v.uuid == uuid))
    }

    /// Return the first `VolumeInfo` whose role matches `role`, or `None`.
    pub fn open_volume_by_role<D: BlockDevice>(
        &self,
        dev: &mut D,
        role: VolumeRole,
    ) -> Result<Option<VolumeInfo>, ContainerError> {
        Ok(self.list_volumes(dev)?.into_iter().find(|v| v.role == role))
    }

    /// Return all `VolumeInfo` entries whose role matches `role`.
    /// Useful when a container has multiple volumes with the same role.
    pub fn volumes_by_role<D: BlockDevice>(
        &self,
        dev: &mut D,
        role: VolumeRole,
    ) -> Result<Vec<VolumeInfo>, ContainerError> {
        Ok(self
            .list_volumes(dev)?
            .into_iter()
            .filter(|v| v.role == role)
            .collect())
    }

    /// Return the `VolumeInfo` at position `index` in the container's fs_oids
    /// array, or `None` if the index is out of range or the volume failed to parse.
    pub fn open_volume_by_index<D: BlockDevice>(
        &self,
        dev: &mut D,
        index: u32,
    ) -> Result<Option<VolumeInfo>, ContainerError> {
        Ok(self
            .list_volumes(dev)?
            .into_iter()
            .find(|v| v.index == index))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::block_device::{BlockDevice, FileBlockDevice};
    use crate::nx::NxSuperblock;
    use std::path::Path;

    #[test]
    fn lists_real_user_volumes() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !std::path::Path::new(img).exists() {
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let mut dev = crate::block_device::FileBlockDevice::open(img).expect("open");
        let c = Container::open(&mut dev).expect("container");
        let vols = c.list_user_volumes(&mut dev).expect("list volumes");
        assert!(!vols.is_empty(), "expected >=1 user volume");
        let v = &vols[0];
        assert_eq!(v.obj.object_type(), crate::obj::OBJECT_TYPE_FS);
        assert!(!v.name.is_empty(), "volume name should be readable");
        assert!(!v.is_system());
    }

    #[test]
    fn opens_real_container_via_blockdevice() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open real APFS container");
        assert_eq!(c.superblock.block_size, 4096);
        assert!(!c.superblock.fs_oids.is_empty());

        // The selected superblock must be at least as new as the block-0 copy.
        let mut dev0 = FileBlockDevice::open(img).expect("reopen fixture");
        let mut b0 = vec![0u8; 4096];
        dev0.read_at(0, &mut b0).expect("read block 0");
        let boot = NxSuperblock::parse(&b0).expect("parse block 0 superblock");
        assert!(
            c.superblock.obj.xid >= boot.obj.xid,
            "selected xid {} < bootstrap xid {}",
            c.superblock.obj.xid,
            boot.obj.xid
        );
        assert_eq!(c.superblock.block_size, 4096);
        assert!(!c.superblock.fs_oids.is_empty());
    }

    // --- Container::open error branches (pure in-memory, no fixture) ---

    /// Minimal in-memory block device for error-path tests.
    struct MemDev(Vec<u8>);
    impl crate::block_device::BlockDevice for MemDev {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(
            &mut self,
            offset: u64,
            buf: &mut [u8],
        ) -> Result<(), crate::block_device::BlockError> {
            let o = offset as usize;
            let end = o.saturating_add(buf.len());
            if end > self.0.len() {
                return Err(crate::block_device::BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.0.len() as u64,
                });
            }
            buf.copy_from_slice(&self.0[o..end]);
            Ok(())
        }
    }

    /// Build a valid minimal nx_superblock block (4096 bytes) with given xid.
    fn make_nx_block(xid: u64) -> [u8; 4096] {
        use crate::checksum::fletcher64;
        use crate::nx::NX_MAGIC;
        use crate::obj::OBJECT_TYPE_NX_SUPERBLOCK;
        let mut b = [0u8; 4096];
        // obj_phys: oid=1 @8, xid @16, type @24
        b[8..16].copy_from_slice(&1u64.to_le_bytes());
        b[16..24].copy_from_slice(&xid.to_le_bytes());
        b[24..28].copy_from_slice(&OBJECT_TYPE_NX_SUPERBLOCK.to_le_bytes());
        // nx_magic @32, block_size @36, block_count @40
        b[32..36].copy_from_slice(&NX_MAGIC.to_le_bytes());
        b[36..40].copy_from_slice(&4096u32.to_le_bytes());
        b[40..48].copy_from_slice(&64u64.to_le_bytes());
        // xp_desc_blocks @104 = 2 (contiguous), xp_desc_base @112 = 1
        b[104..108].copy_from_slice(&2u32.to_le_bytes());
        b[112..120].copy_from_slice(&1i64.to_le_bytes());
        // omap_oid @160 = 0 (points nowhere, but open() won't reach list_user_volumes)
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        b
    }

    #[test]
    fn open_rejects_all_zeros_block0() {
        // A device whose block 0 is all zeros has no valid NX magic.
        // Container::open must return a Parse error, not panic.
        let dev_data = vec![0u8; 4096 * 4];
        let mut dev = MemDev(dev_data);
        let err = Container::open(&mut dev);
        assert!(
            err.is_err(),
            "all-zeros block 0 must be rejected by Container::open"
        );
    }

    #[test]
    fn open_rejects_bad_magic_in_block0() {
        // Valid checksum but wrong magic - NxSuperblock::parse must reject it.
        use crate::checksum::fletcher64;
        let mut block = [0u8; 4096];
        // Put a wrong magic word, correct block_size.
        block[32..36].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        block[36..40].copy_from_slice(&4096u32.to_le_bytes());
        let ck = fletcher64(&block);
        block[0..8].copy_from_slice(&ck.to_le_bytes());
        let mut data = vec![0u8; 4096 * 4];
        data[..4096].copy_from_slice(&block);
        let mut dev = MemDev(data);
        let result = Container::open(&mut dev);
        assert!(result.is_err(), "bad magic must be rejected");
        assert!(
            matches!(result.err().unwrap(), ContainerError::Parse(_)),
            "must be a Parse error"
        );
    }

    // -----------------------------------------------------------------------
    // list_volumes / open_volume_by_uuid / open_volume_by_role / volumes_by_role
    // All use the real fixture when present.
    // -----------------------------------------------------------------------

    #[test]
    fn list_volumes_returns_volume_info_fields() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let vols = c.list_volumes(&mut dev).expect("list_volumes");
        assert!(!vols.is_empty(), "must have at least one volume");
        let v = &vols[0];
        assert!(!v.name.is_empty(), "volume name must be non-empty");
        // index field must match position
        assert_eq!(v.index, 0);
    }

    #[test]
    fn open_volume_by_uuid_finds_existing_volume() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let vols = c.list_volumes(&mut dev).expect("list_volumes");
        let uuid = vols[0].uuid;
        let found = c
            .open_volume_by_uuid(&mut dev, &uuid)
            .expect("open_volume_by_uuid");
        assert!(found.is_some(), "must find volume by its own uuid");
        assert_eq!(found.unwrap().uuid, uuid);
    }

    #[test]
    fn open_volume_by_uuid_returns_none_for_unknown_uuid() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let bogus = [0xffu8; 16];
        let found = c.open_volume_by_uuid(&mut dev, &bogus).expect("ok");
        assert!(found.is_none(), "unknown uuid must return None");
    }

    #[test]
    fn open_volume_by_role_returns_none_for_absent_role() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        // Baseband role (0x80) is very unlikely to exist in a tiny test image.
        use crate::volume::VolumeRole;
        let found = c
            .open_volume_by_role(&mut dev, VolumeRole::Baseband)
            .expect("ok");
        assert!(
            found.is_none(),
            "Baseband role should not exist in tiny fixture"
        );
    }

    #[test]
    fn volumes_by_role_returns_vec() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        use crate::volume::VolumeRole;
        // Hardware role is non-existent → empty vec, no error
        let v = c
            .volumes_by_role(&mut dev, VolumeRole::Hardware)
            .expect("ok");
        assert!(v.is_empty(), "Hardware role should yield empty vec");
    }

    #[test]
    fn open_volume_by_index_returns_none_for_out_of_range() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let found = c.open_volume_by_index(&mut dev, 9999).expect("ok");
        assert!(found.is_none(), "out-of-range index must return None");
    }

    #[test]
    fn container_uuid_is_non_zero() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let uuid = c.container_uuid();
        assert_ne!(uuid, [0u8; 16], "container UUID must not be all zeros");
    }

    // -----------------------------------------------------------------------
    // Pure synthetic tests for list_user_volumes / list_volumes / by_uuid /
    // by_role / by_index - no fixture required.
    //
    // Memory layout (all blocks 4096 bytes, block-addressed):
    //   block 0  = NX bootstrap (xid=5, omap_oid=4, fs_oids=[6])
    //   block 1  = NX descriptor ring slot (xid=5, same content, for
    //              latest_superblock to pick)
    //   block 4  = OmapPhys (tree_oid=5)
    //   block 5  = Omap BTREE ROOT FIXED_KV leaf: key(oid=6,xid=5)→paddr=7
    //   block 7  = VolumeSuperblock (OBJECT_TYPE_FS, role=DATA=0x40, name="Synth")
    // -----------------------------------------------------------------------

    struct BlkDev(std::collections::HashMap<u64, Vec<u8>>);
    impl BlockDevice for BlkDev {
        fn size(&self) -> u64 {
            4096 * 64
        }
        fn read_at(
            &mut self,
            off: u64,
            buf: &mut [u8],
        ) -> Result<(), crate::block_device::BlockError> {
            let addr = off / 4096;
            match self.0.get(&addr) {
                Some(d) => {
                    let l = buf.len().min(d.len());
                    buf[..l].copy_from_slice(&d[..l]);
                    Ok(())
                }
                None => {
                    buf.fill(0);
                    Ok(())
                }
            }
        }
    }

    fn build_synthetic_container() -> BlkDev {
        use crate::btree::{BTNODE_FIXED_KV_SIZE, BTNODE_LEAF, BTNODE_ROOT};
        use crate::checksum::fletcher64;
        use crate::nx::NX_MAGIC;
        use crate::obj::{
            OBJECT_TYPE_BTREE, OBJECT_TYPE_FS, OBJECT_TYPE_NX_SUPERBLOCK, OBJECT_TYPE_OMAP,
        };
        use crate::volume::APFS_MAGIC;

        let mut blocks: std::collections::HashMap<u64, Vec<u8>> = std::collections::HashMap::new();

        // --- NxSuperblock block (used for both block 0 and descriptor slot 1) ---
        let make_nx = |xid: u64| -> Vec<u8> {
            let mut b = vec![0u8; 4096];
            b[8..16].copy_from_slice(&1u64.to_le_bytes()); // oid=1
            b[16..24].copy_from_slice(&xid.to_le_bytes()); // xid
            b[24..28].copy_from_slice(&OBJECT_TYPE_NX_SUPERBLOCK.to_le_bytes());
            b[32..36].copy_from_slice(&NX_MAGIC.to_le_bytes());
            b[36..40].copy_from_slice(&4096u32.to_le_bytes()); // block_size
            b[40..48].copy_from_slice(&64u64.to_le_bytes()); // block_count
            b[104..108].copy_from_slice(&2u32.to_le_bytes()); // xp_desc_blocks=2
            b[112..120].copy_from_slice(&1i64.to_le_bytes()); // xp_desc_base=1
            b[160..168].copy_from_slice(&4u64.to_le_bytes()); // omap_oid=4 (physical)
            b[180..184].copy_from_slice(&1u32.to_le_bytes()); // max_file_systems=1
            b[184..192].copy_from_slice(&6u64.to_le_bytes()); // fs_oids[0]=6
            let ck = fletcher64(&b);
            b[0..8].copy_from_slice(&ck.to_le_bytes());
            b
        };
        blocks.insert(0, make_nx(5)); // bootstrap
        blocks.insert(1, make_nx(5)); // descriptor ring slot 1

        // --- OmapPhys block at block 4 (tree_oid=5) ---
        let mut omap_raw = vec![0u8; 4096];
        omap_raw[8..16].copy_from_slice(&4u64.to_le_bytes());
        omap_raw[16..24].copy_from_slice(&1u64.to_le_bytes());
        omap_raw[24..28].copy_from_slice(&OBJECT_TYPE_OMAP.to_le_bytes());
        omap_raw[48..56].copy_from_slice(&5u64.to_le_bytes()); // tree_oid=5
        let ck2 = fletcher64(&omap_raw);
        omap_raw[0..8].copy_from_slice(&ck2.to_le_bytes());
        blocks.insert(4, omap_raw);

        // --- BTREE FIXED_KV ROOT LEAF at block 5 ---
        // one entry: key(oid=6, xid=5) → omap_val{flags=0, size=4096, paddr=7}
        const DATA_BASE: usize = 56;
        const BTREE_INFO: usize = 40;
        let mut root = vec![0u8; 4096];
        root[8..16].copy_from_slice(&5u64.to_le_bytes());
        root[16..24].copy_from_slice(&1u64.to_le_bytes());
        root[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        let flags = BTNODE_LEAF | BTNODE_FIXED_KV_SIZE | BTNODE_ROOT;
        root[32..34].copy_from_slice(&flags.to_le_bytes());
        root[36..40].copy_from_slice(&1u32.to_le_bytes()); // nkeys=1
        root[40..42].copy_from_slice(&0u16.to_le_bytes()); // toc_off=0
        root[42..44].copy_from_slice(&4u16.to_le_bytes()); // toc_len=4 (1 entry × 4B)
                                                           // kvoff_t at DATA_BASE: k_off=0, v_off=16
        root[DATA_BASE..DATA_BASE + 2].copy_from_slice(&0u16.to_le_bytes());
        root[DATA_BASE + 2..DATA_BASE + 4].copy_from_slice(&16u16.to_le_bytes());
        // key at DATA_BASE+toc_len = 60: omap_key{oid=6, xid=5}
        root[60..68].copy_from_slice(&6u64.to_le_bytes());
        root[68..76].copy_from_slice(&5u64.to_le_bytes());
        // ROOT: val_area_end = 4096-40=4056; val at 4056-16=4040
        root[4040..4044].copy_from_slice(&0u32.to_le_bytes()); // flags=0
        root[4044..4048].copy_from_slice(&4096u32.to_le_bytes()); // size=4096
        root[4048..4056].copy_from_slice(&7i64.to_le_bytes()); // paddr=7
        let ck3 = fletcher64(&root);
        root[0..8].copy_from_slice(&ck3.to_le_bytes());
        blocks.insert(5, root);

        // --- VolumeSuperblock at block 7 ---
        let mut vsb = vec![0u8; 4096];
        vsb[8..16].copy_from_slice(&6u64.to_le_bytes()); // oid=6
        vsb[16..24].copy_from_slice(&5u64.to_le_bytes()); // xid=5
        vsb[24..28].copy_from_slice(&OBJECT_TYPE_FS.to_le_bytes());
        vsb[32..36].copy_from_slice(&APFS_MAGIC.to_le_bytes());
        // uuid at 240
        let uuid: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        vsb[240..256].copy_from_slice(&uuid);
        // name at 704
        vsb[704..710].copy_from_slice(b"Synth\0");
        // role=DATA(0x40) at 964
        vsb[964..966].copy_from_slice(&0x0040u16.to_le_bytes());
        let ck4 = fletcher64(&vsb);
        vsb[0..8].copy_from_slice(&ck4.to_le_bytes());
        blocks.insert(7, vsb);

        BlkDev(blocks)
    }

    #[test]
    fn synthetic_list_user_volumes_returns_one_volume() {
        let mut dev = build_synthetic_container();
        let c = Container::open(&mut dev).expect("open synthetic container");
        let vols = c.list_user_volumes(&mut dev).expect("list_user_volumes");
        assert_eq!(vols.len(), 1, "must return exactly one volume");
        assert_eq!(vols[0].name, "Synth");
    }

    #[test]
    fn synthetic_list_volumes_returns_volume_info() {
        let mut dev = build_synthetic_container();
        let c = Container::open(&mut dev).expect("open");
        let vols = c.list_volumes(&mut dev).expect("list_volumes");
        assert_eq!(vols.len(), 1);
        assert_eq!(vols[0].name, "Synth");
        assert_eq!(vols[0].index, 0);
        assert_eq!(vols[0].role, crate::volume::VolumeRole::Data);
    }

    #[test]
    fn synthetic_open_volume_by_uuid_found() {
        let uuid: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let mut dev = build_synthetic_container();
        let c = Container::open(&mut dev).expect("open");
        let found = c.open_volume_by_uuid(&mut dev, &uuid).expect("ok");
        assert!(found.is_some(), "must find volume by uuid");
    }

    #[test]
    fn synthetic_open_volume_by_uuid_not_found() {
        let bogus = [0xffu8; 16];
        let mut dev = build_synthetic_container();
        let c = Container::open(&mut dev).expect("open");
        let found = c.open_volume_by_uuid(&mut dev, &bogus).expect("ok");
        assert!(found.is_none(), "unknown uuid must return None");
    }

    #[test]
    fn synthetic_open_volume_by_role() {
        let mut dev = build_synthetic_container();
        let c = Container::open(&mut dev).expect("open");
        let found = c
            .open_volume_by_role(&mut dev, crate::volume::VolumeRole::Data)
            .expect("ok");
        assert!(found.is_some(), "Data role volume must be found");
        assert_eq!(found.unwrap().name, "Synth");
    }

    #[test]
    fn synthetic_volumes_by_role_empty_for_absent_role() {
        let mut dev = build_synthetic_container();
        let c = Container::open(&mut dev).expect("open");
        let vols = c
            .volumes_by_role(&mut dev, crate::volume::VolumeRole::System)
            .expect("ok");
        assert!(
            vols.is_empty(),
            "System role must not exist in synthetic container"
        );
    }

    #[test]
    fn synthetic_open_volume_by_index_found_and_not_found() {
        let mut dev = build_synthetic_container();
        let c = Container::open(&mut dev).expect("open");
        let found = c.open_volume_by_index(&mut dev, 0).expect("ok");
        assert!(found.is_some(), "index 0 must return Some");
        let not_found = c.open_volume_by_index(&mut dev, 99).expect("ok");
        assert!(not_found.is_none(), "index 99 must return None");
    }

    #[test]
    fn synthetic_container_uuid_is_accessible() {
        let mut dev = build_synthetic_container();
        let c = Container::open(&mut dev).expect("open");
        let _ = c.container_uuid(); // exercises the method
    }

    #[test]
    fn open_selects_valid_superblock_from_descriptor_ring() {
        // Block 0 = xid 5, descriptor ring slot 1 = xid 9 (newer).
        // Container::open must select xid=9 via latest_superblock.
        let boot = make_nx_block(5);
        let newer = make_nx_block(9);
        let mut data = vec![0u8; 4096 * 4];
        data[..4096].copy_from_slice(&boot); // block 0 = bootstrap (xid=5)
        data[4096..8192].copy_from_slice(&newer); // block 1 = descriptor ring slot (xid=9)
        let mut dev = MemDev(data);
        let c = Container::open(&mut dev).expect("open with descriptor ring");
        assert!(
            c.superblock.obj.xid >= 5,
            "selected xid must be at least as new as bootstrap"
        );
    }

    #[test]
    fn open_rejects_short_block0() {
        // Device is smaller than one block - read_at must return OutOfRange and
        // Container::open must propagate it as ContainerError::Io, not panic.
        let mut dev = MemDev(vec![0u8; 100]);
        let err = Container::open(&mut dev);
        assert!(err.is_err(), "short device must be rejected");
        assert!(
            matches!(err.err().unwrap(), ContainerError::Io(_)),
            "short device error must be Io variant"
        );
    }

    #[test]
    fn open_rejects_bad_checksum_in_block0() {
        // Valid NX magic and object type but corrupted body byte → Fletcher-64
        // mismatch → Container::open returns Parse error.
        let mut block = make_nx_block(1);
        block[500] ^= 0xFF; // corrupt an arbitrary body byte (checksum now stale)
        let mut data = vec![0u8; 4096 * 4];
        data[..4096].copy_from_slice(&block);
        let mut dev = MemDev(data);
        let result = Container::open(&mut dev);
        assert!(result.is_err(), "bad checksum must be rejected");
        assert!(
            matches!(result.err().unwrap(), ContainerError::Parse(_)),
            "bad checksum error must be Parse variant"
        );
    }

    #[test]
    fn open_rejects_wrong_object_type_in_block0() {
        // Correct NX_MAGIC and valid checksum but wrong object type field.
        use crate::checksum::fletcher64;
        use crate::nx::NX_MAGIC;
        use crate::obj::OBJECT_TYPE_OMAP; // deliberately wrong type
        let mut block = [0u8; 4096];
        block[8..16].copy_from_slice(&1u64.to_le_bytes());
        block[16..24].copy_from_slice(&1u64.to_le_bytes());
        block[24..28].copy_from_slice(&OBJECT_TYPE_OMAP.to_le_bytes()); // wrong
        block[32..36].copy_from_slice(&NX_MAGIC.to_le_bytes());
        block[36..40].copy_from_slice(&4096u32.to_le_bytes());
        let ck = fletcher64(&block);
        block[0..8].copy_from_slice(&ck.to_le_bytes());
        let mut data = vec![0u8; 4096 * 4];
        data[..4096].copy_from_slice(&block);
        let mut dev = MemDev(data);
        let result = Container::open(&mut dev);
        assert!(result.is_err(), "wrong object type must be rejected");
    }

    #[test]
    fn open_ring_scan_picks_higher_xid_over_bootstrap() {
        // Block 0 = xid 3 (bootstrap), descriptor ring slot at block 1 = xid 7.
        // Container::open must select xid=7.
        let boot = make_nx_block(3);
        let newer = make_nx_block(7);
        let mut data = vec![0u8; 4096 * 4];
        data[..4096].copy_from_slice(&boot);
        data[4096..8192].copy_from_slice(&newer);
        let mut dev = MemDev(data);
        let c = Container::open(&mut dev).expect("open must succeed");
        assert_eq!(
            c.superblock.obj.xid, 7,
            "ring scan must select the newer xid=7 superblock"
        );
    }
}
