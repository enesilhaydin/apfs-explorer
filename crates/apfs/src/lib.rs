#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! apfs - safe high-level filesystem API over `apfs-core`. `FsView` is the
//! OS-agnostic layer a mount host (winfsp-host / FUSE) delegates to.
use std::collections::HashMap;

pub mod case_fold;

use apfs_core::block_device::BlockDevice;
use apfs_core::catalog::{Catalog, DirEntry, InodeStat};
use apfs_core::container::{Container, ContainerError};
use apfs_core::jkey::ROOT_DIR_INO_NUM;
use apfs_core::VolumeRole;

/// Which volume to mount inside a multi-volume APFS container.
///
/// A macOS-formatted disk's container holds several volumes (e.g. a boot
/// volume group has Preboot, Recovery, VM, the sealed System volume, and the
/// user **Data** volume). The user's files live on the Data volume; the
/// boot-helper volumes (Preboot/Recovery/VM) have intentionally empty user
/// catalogs. Selecting the wrong one is exactly why the root looked empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VolumeSelector {
    /// Pick the user-data volume automatically: prefer the `Data` role, else
    /// the first non-system volume (skips Preboot/Recovery/VM/Installer/Baseband).
    #[default]
    Default,
    /// 0-based index into the container's `fs_oids` array (raw, unfiltered).
    Index(usize),
    /// First volume whose raw `apfs_role` bitfield equals this value.
    Role(u16),
    /// Volume whose `apfs_vol_uuid` equals this UUID.
    Uuid([u8; 16]),
}

/// Errors surfaced by the high-level filesystem view.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("apfs-core: {0:?}")]
    Container(ContainerError),
    #[error("block device: {0}")]
    Block(apfs_core::block_device::BlockError),
    #[error("no user volume in container")]
    NoVolume,
}

impl From<ContainerError> for FsError {
    fn from(e: ContainerError) -> Self {
        FsError::Container(e)
    }
}

impl From<apfs_core::block_device::BlockError> for FsError {
    fn from(e: apfs_core::block_device::BlockError) -> Self {
        FsError::Block(e)
    }
}

/// Lightweight attributes for a path (mount-host getattr).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    pub inode: u64,
    pub size: u64,
    pub is_dir: bool,
    /// POSIX mode (S_IFREG | rwx). Lets WinFsp / FUSE callers map the
    /// write-permission bits to FILE_ATTRIBUTE_READONLY without an extra
    /// catalog read.
    pub mode: u16,
    /// BSD file flags (u32) - UF_HIDDEN (0x8000) ↔ FILE_ATTRIBUTE_HIDDEN,
    /// UF_IMMUTABLE (0x2) ↔ read-only-ish guard. Surfaced so WinFsp can
    /// round-trip Windows-attribute state without re-parsing the inode.
    pub bsd_flags: u32,
    /// File timestamps as APFS nanoseconds since UNIX epoch (1970-01-01).
    /// Callers convert to Windows FILETIME via `(ns / 100) +
    /// 116_444_736_000_000_000`.
    pub create_time: u64,
    pub mod_time: u64,
    pub change_time: u64,
    pub access_time: u64,
}

/// A read-only filesystem view of the first user volume of an APFS container.
/// Owns the block device; memoizes path->inode and inode->listing (the volume
/// is read-only, so caches never need invalidation).
pub struct FsView<D: BlockDevice> {
    dev: D,
    catalog: Catalog,
    names_hashed: bool,
    path_cache: HashMap<String, u64>,
    dir_cache: HashMap<u64, Vec<DirEntry>>,
    /// Volume superblock paddr - needed by the WinFsp write path to fetch
    /// vsb_raw bytes for apfs-write API calls (Transaction::begin + write_file
    /// + create_file + unlink + rename all take a `vsb_raw: &[u8]` argument).
    vol_paddr: u64,
    /// Block size in bytes (from container superblock).
    bsz: usize,
    /// Which volume this view was opened with. Preserved so a writable
    /// transaction that consumes the device (`into_dev`) can rebuild the view
    /// for the SAME volume afterward (`reopen`) instead of re-running the
    /// Default heuristic, which could otherwise drift to another volume.
    selector: VolumeSelector,
}

/// Normalize a path into non-empty components (leading/trailing/duplicate
/// slashes ignored). "/" or "" -> no components (the root directory).
fn components(path: &str) -> Vec<&str> {
    path.split('/').filter(|c| !c.is_empty()).collect()
}

impl<D: BlockDevice> FsView<D> {
    /// Consume the FsView and return the underlying device, dropping all
    /// path/listing caches. The WinFsp write path uses this to lend the
    /// device to an apfs-write Transaction (which takes exclusive
    /// ownership) for one COW commit cycle, then rebuilds a fresh FsView
    /// from the returned device. The cache discard is intentional - the
    /// on-disk catalog has just changed.
    pub fn into_dev(self) -> D {
        self.dev
    }

    /// Reported volume size + free bytes for WinFsp `get_volume_info` and any
    /// Explorer/dir consumer. `total` is the underlying block device size
    /// (close to what newfs_apfs reports - the container fills its partition);
    /// `free` is derived from the volume superblock's `apfs_fs_alloc_count`
    /// counter (allocated blocks across data + metadata, kept in sync by
    /// every COW commit). Conservative - a precise free-block tally requires
    /// the full spaceman bitmap walk, which is M8 polish.
    pub fn volume_size(&mut self) -> Result<(u64, u64), FsError> {
        let total = self.dev.size();
        let mut vsb = vec![0u8; self.bsz];
        let vsb_off = self.vol_paddr * self.bsz as u64;
        self.dev
            .read_at(vsb_off, &mut vsb)
            .map_err(FsError::Block)?;
        // apfs_fs_alloc_count @ offset 0x58 (the APFS specification,
        // linux-apfs-rw apfs_raw.h apfs_superblock_t).
        let alloc_blocks = u64::from_le_bytes(
            vsb.get(0x58..0x60)
                .and_then(|s| s.try_into().ok())
                .unwrap_or([0; 8]),
        );
        let used = alloc_blocks.saturating_mul(self.bsz as u64);
        let free = total.saturating_sub(used);
        Ok((total, free))
    }

    /// Volume UUID (`apfs_vol_uuid` @ offset 0xF0 = 240 in the volume
    /// superblock). The WinFsp host derives a stable Windows volume serial
    /// number from this so the drive keeps the same serial across remounts.
    pub fn volume_uuid(&mut self) -> Result<[u8; 16], FsError> {
        let mut vsb = vec![0u8; self.bsz];
        let vsb_off = self.vol_paddr * self.bsz as u64;
        self.dev
            .read_at(vsb_off, &mut vsb)
            .map_err(FsError::Block)?;
        let uuid: [u8; 16] = vsb
            .get(0xF0..0x100)
            .and_then(|s| s.try_into().ok())
            .unwrap_or([0u8; 16]);
        Ok(uuid)
    }

    /// Whether the volume currently has any snapshots (`apfs_num_snapshots`
    /// @ offset 0xD8 in the volume superblock). The writable mount path refuses
    /// to enable writes on a snapshotted volume until the snapshot-pinned
    /// free-queue path is proven (write to clean volumes is fsck-validated;
    /// writing to a snapshotted volume is a known gap - see cross-check).
    pub fn volume_has_snapshots(&mut self) -> Result<bool, FsError> {
        let mut vsb = vec![0u8; self.bsz];
        let vsb_off = self.vol_paddr * self.bsz as u64;
        self.dev
            .read_at(vsb_off, &mut vsb)
            .map_err(FsError::Block)?;
        let n = vsb
            .get(0xD8..0xE0)
            .and_then(|s| s.try_into().ok())
            .map(u64::from_le_bytes)
            .unwrap_or(0);
        Ok(n > 0)
    }

    /// Open the user-data volume of the APFS container on `dev`
    /// (`VolumeSelector::Default`). Use [`FsView::open_selected`] to choose a
    /// specific volume by index, role, or UUID.
    pub fn open(dev: D) -> Result<Self, FsError> {
        Self::open_selected(dev, VolumeSelector::Default)
    }

    /// The selector this view was opened with.
    pub fn selector(&self) -> VolumeSelector {
        self.selector
    }

    /// Open a specific volume of the APFS container on `dev`.
    ///
    /// A macOS container can hold many volumes; `fs_oids[0]` is frequently a
    /// boot-helper (Preboot/Recovery/VM) with an empty user catalog, so a naive
    /// "first volume" choice makes the mounted root look empty. We resolve
    /// every `fs_oid` to its volume superblock, then pick per `sel`:
    /// `Default` prefers the `Data` role and falls back to the first
    /// non-system volume.
    pub fn open_selected(mut dev: D, sel: VolumeSelector) -> Result<Self, FsError> {
        use apfs_core::omap::Omap;
        use apfs_core::volume::VolumeSuperblock;

        // Copy out the container fields we need so we can drop the borrow on
        // `c` before the volume-scan loop (which re-borrows `dev` mutably).
        let c = Container::open(&mut dev)?;
        let bsz = c.superblock.block_size as usize;
        let block_size = c.superblock.block_size;
        let xid = c.superblock.obj.xid;
        let omap_oid = c.superblock.omap_oid;
        let fs_oids: Vec<u64> = c.superblock.fs_oids.to_vec();
        drop(c);

        if fs_oids.is_empty() {
            return Err(FsError::NoVolume);
        }

        let omap = Omap::open(&mut dev, omap_oid, block_size)?;

        // Resolve every volume superblock so the selector can inspect roles.
        struct Cand {
            index: usize,
            paddr: u64,
            vsb: VolumeSuperblock,
        }
        let mut cands: Vec<Cand> = Vec::with_capacity(fs_oids.len());
        for (index, &oid) in fs_oids.iter().enumerate() {
            let paddr = match omap.resolve(&mut dev, oid, xid, block_size)? {
                Some(p) => p,
                None => continue,
            };
            let mut raw = vec![0u8; bsz];
            dev.read_at(paddr * block_size as u64, &mut raw)?;
            match VolumeSuperblock::parse(&raw) {
                Ok(vsb) => cands.push(Cand { index, paddr, vsb }),
                // A volume that fails to parse is skipped, not fatal - the
                // others may still be mountable.
                Err(_) => continue,
            }
        }
        if cands.is_empty() {
            return Err(FsError::NoVolume);
        }

        for c in &cands {
            tracing::debug!(
                index = c.index,
                role = ?c.vsb.volume_role(),
                is_system = c.vsb.is_system(),
                name = %c.vsb.name,
                "APFS container volume"
            );
        }

        let chosen_pos = match sel {
            VolumeSelector::Index(i) => cands.iter().position(|c| c.index == i),
            VolumeSelector::Role(r) => cands.iter().position(|c| c.vsb.role == r),
            VolumeSelector::Uuid(u) => cands.iter().position(|c| c.vsb.uuid == u),
            VolumeSelector::Default => cands
                .iter()
                .position(|c| c.vsb.volume_role() == VolumeRole::Data)
                .or_else(|| cands.iter().position(|c| !c.vsb.is_system())),
        }
        .ok_or(FsError::NoVolume)?;

        let Cand {
            index,
            paddr: vol_paddr,
            vsb: vol,
        } = cands.swap_remove(chosen_pos);

        tracing::info!(
            selected_index = index,
            role = ?vol.volume_role(),
            name = %vol.name,
            ?sel,
            "selected APFS volume"
        );

        let names_hashed = vol.names_are_hashed();
        let catalog = Catalog::open(&mut dev, &vol, block_size)?;
        Ok(Self {
            dev,
            catalog,
            names_hashed,
            path_cache: HashMap::with_capacity(64),
            dir_cache: HashMap::with_capacity(16),
            vol_paddr,
            bsz,
            selector: sel,
        })
    }

    /// Return raw VSB and volume omap header bytes (each one block).
    /// Used by the WinFsp write path: apfs-write API calls take vsb_raw
    /// and vol_omap_raw byte slices, then the Transaction stages updated
    /// versions onto the device during commit. Reading here re-fetches
    /// the LATEST on-disk state - important after a previous transaction
    /// has just bumped the VSB xid.
    pub fn read_vsb_omap_raw(&mut self) -> Result<(Vec<u8>, Vec<u8>), FsError> {
        let mut vsb_raw = vec![0u8; self.bsz];
        self.dev
            .read_at(self.vol_paddr * self.bsz as u64, &mut vsb_raw)?;
        // VSBI_OMAP_OID lives at offset 0x80 (per apfs-write/src/file.rs).
        let omap_paddr = u64::from_le_bytes(
            vsb_raw
                .get(0x80..0x88)
                .ok_or(FsError::NoVolume)?
                .try_into()
                .map_err(|_| FsError::NoVolume)?,
        );
        let mut omap_raw = vec![0u8; self.bsz];
        self.dev
            .read_at(omap_paddr * self.bsz as u64, &mut omap_raw)?;
        Ok((vsb_raw, omap_raw))
    }

    fn listing(&mut self, ino: u64) -> Result<&[DirEntry], FsError> {
        use std::collections::hash_map::Entry;
        if let Entry::Vacant(e) = self.dir_cache.entry(ino) {
            let v = self
                .catalog
                .list_dir(&mut self.dev, ino, self.names_hashed)?;
            e.insert(v);
        }
        Ok(self.dir_cache.get(&ino).map(Vec::as_slice).unwrap_or(&[]))
    }

    /// Resolve a path to its inode number (cached). Empty/"/" -> root inode.
    ///
    /// On a case-insensitive volume (`names_hashed` true - APFS sets the
    /// `INCOMPAT_CASE_INSENSITIVE` or `INCOMPAT_NORMALIZATION_INSENSITIVE`
    /// flag) the on-disk DREC keys carry the normalised form of each
    /// component. Apple's `apfs_vnop_lookup` runs the query through
    /// `utf8_normalizeOptCaseFoldAndCompare` before any compare. We apply
    /// the full Unicode NFD + lowercase fold (UAX#15 + Unicode CaseFolding.txt)
    /// via `case_fold::apfs_names_match`, which handles Unicode special-casing (U+0130/U+0131, ß),
    /// Greek Σ/σ, accented Latin, and NFC/NFD variant inputs from Finder/AFP.
    pub fn lookup(&mut self, path: &str) -> Result<Option<u64>, FsError> {
        let comps = components(path);
        let key = format!("/{}", comps.join("/"));
        if let Some(&i) = self.path_cache.get(&key) {
            return Ok(Some(i));
        }
        let names_hashed = self.names_hashed;
        let mut ino = ROOT_DIR_INO_NUM;
        for comp in &comps {
            // For hashed (case-insensitive) volumes fold the needle to its
            // Unicode canonical form (NFD + lowercase) before comparison.
            // Both the needle and each catalog entry name are folded so that
            // NFC vs NFD variant inputs from Finder / AFP are unified.
            // For exact-match volumes we compare raw UTF-8 - no allocation.
            let needle_owned: String;
            let needle: &str = if names_hashed {
                needle_owned = case_fold::apfs_case_fold(comp);
                &needle_owned
            } else {
                comp
            };
            let next = self
                .listing(ino)?
                .iter()
                .find(|e| {
                    if names_hashed {
                        case_fold::apfs_names_match(&e.name, needle)
                    } else {
                        e.name == needle
                    }
                })
                .map(|e| e.file_id);
            match next {
                Some(i) => ino = i,
                None => return Ok(None),
            }
        }
        self.path_cache.insert(key, ino);
        Ok(Some(ino))
    }

    /// List a directory's entries (cached).
    pub fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        match self.lookup(path)? {
            Some(ino) => Ok(self.listing(ino)?.to_vec()),
            None => Ok(Vec::new()),
        }
    }

    /// Stat a path (size + is_dir + inode). `Ok(None)` if the path is absent.
    pub fn getattr(&mut self, path: &str) -> Result<Option<Attr>, FsError> {
        match self.lookup(path)? {
            None => Ok(None),
            Some(inode) => {
                let InodeStat {
                    size,
                    is_dir,
                    mode,
                    bsd_flags,
                    create_time,
                    mod_time,
                    change_time,
                    access_time,
                } = self.catalog.stat(&mut self.dev, inode)?;
                Ok(Some(Attr {
                    inode,
                    size,
                    is_dir,
                    mode,
                    bsd_flags,
                    create_time,
                    mod_time,
                    change_time,
                    access_time,
                }))
            }
        }
    }

    /// Read up to `len` bytes of a regular file at `path` from byte `offset`.
    /// Transparently decompresses decmpfs. `Ok(vec![])` past EOF / absent.
    pub fn read(&mut self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, FsError> {
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };
        let content = self.catalog.read_file_decompressed(&mut self.dev, ino)?;
        let off = offset as usize;
        if off >= content.len() {
            return Ok(Vec::new());
        }
        let end = off.saturating_add(len).min(content.len());
        Ok(content.get(off..end).unwrap_or(&[]).to_vec())
    }

    /// Return a list of all extended attribute names for the file at `path`.
    /// Filters out internal Apple system-owned attributes (those with flag
    /// `XATTR_FILE_SYSTEM_OWNED = 0x0004`) so Windows EA consumers only see
    /// user-visible attributes. Returns `Ok(vec![])` when the file has no
    /// user xattrs or the path is absent.
    pub fn list_xattrs(&mut self, path: &str) -> Result<Vec<String>, FsError> {
        use apfs_core::xattr::XATTR_FILE_SYSTEM_OWNED;
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };
        let entries = self.catalog.list_xattrs(&mut self.dev, ino)?;
        Ok(entries
            .into_iter()
            .filter(|e| e.flags & XATTR_FILE_SYSTEM_OWNED == 0)
            .map(|e| e.name)
            .collect())
    }

    /// Read the raw bytes of one extended attribute by name. Returns
    /// `Ok(None)` when the path is absent or the attribute does not exist.
    pub fn get_xattr(&mut self, path: &str, name: &str) -> Result<Option<Vec<u8>>, FsError> {
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(None),
        };
        Ok(self.catalog.get_xattr(&mut self.dev, ino, name)?)
    }

    /// Read the target of a symbolic link at `path`. Returns `Ok(None)` if
    /// the path is absent or is not a symlink. APFS stores the target as an
    /// extended attribute named `com.apple.fs.symlink` on a `j_inode` whose
    /// mode bits are `S_IFLNK` (0o120000); reading the xattr directly is the
    /// canonical resolution path.
    pub fn read_symlink(&mut self, path: &str) -> Result<Option<Vec<u8>>, FsError> {
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(None),
        };
        let stat = self.catalog.stat(&mut self.dev, ino)?;
        if stat.mode & 0o170000 != 0o120000 {
            return Ok(None);
        }
        Ok(self
            .catalog
            .get_xattr(&mut self.dev, ino, "com.apple.fs.symlink")?)
    }
}

#[cfg(test)]
mod tests {
    use super::components;

    // --- components() path parser ---

    #[test]
    fn components_empty_string_gives_no_parts() {
        assert!(components("").is_empty());
    }

    #[test]
    fn components_root_slash_gives_no_parts() {
        assert!(components("/").is_empty());
    }

    #[test]
    fn components_single_component() {
        assert_eq!(components("foo"), vec!["foo"]);
        assert_eq!(components("/foo"), vec!["foo"]);
        assert_eq!(components("/foo/"), vec!["foo"]);
    }

    #[test]
    fn components_multi_component() {
        assert_eq!(components("/a/b/c"), vec!["a", "b", "c"]);
        assert_eq!(components("a/b/c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn components_trailing_slash_ignored() {
        assert_eq!(components("/a/b/"), vec!["a", "b"]);
        assert_eq!(components("a/b/"), vec!["a", "b"]);
    }

    #[test]
    fn components_double_slash_collapsed() {
        // split('/').filter(!is_empty) collapses consecutive slashes.
        assert_eq!(components("//a//b//"), vec!["a", "b"]);
    }

    #[test]
    fn components_deep_path() {
        let path = "/usr/local/lib/apfs/module.ko";
        assert_eq!(
            components(path),
            vec!["usr", "local", "lib", "apfs", "module.ko"]
        );
    }

    #[test]
    fn components_preserves_extension_dots() {
        assert_eq!(components("/hello.txt"), vec!["hello.txt"]);
        assert_eq!(components("/dir/file.tar.gz"), vec!["dir", "file.tar.gz"]);
    }
}
