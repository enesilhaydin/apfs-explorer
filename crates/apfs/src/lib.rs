#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! apfs - safe high-level filesystem API over `apfs-core`. `FsView` is the
//! OS-agnostic layer a mount host (winfsp-host / FUSE) delegates to.
use std::collections::{HashMap, HashSet};

pub use apfs_core::case_fold;

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
    /// 0-based slot in the container's `nx_fs_oid` array (raw, unfiltered:
    /// a volume after a deleted one keeps its slot).
    Index(usize),
    /// First volume whose raw `apfs_role` bitfield equals this value.
    Role(u16),
    /// Volume whose `apfs_vol_uuid` equals this UUID.
    Uuid([u8; 16]),
}

/// Which snapshot of a volume to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotRef {
    /// The snapshot with this name.
    Name(String),
    /// The snapshot that froze this transaction.
    Xid(u64),
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
    /// The selected volume's file data is encrypted (FileVault). Encryption is
    /// a non-goal; refusing here is what keeps callers from receiving
    /// ciphertext that looks like a successful read.
    #[error("volume {name:?} is encrypted (FileVault); encrypted volumes are not supported")]
    Encrypted { name: String, role: u16 },
    /// The path names nothing in the volume.
    #[error("no such path: {0}")]
    NotFound(String),
    /// The volume has no snapshot by that name or transaction.
    #[error("no such snapshot: {0}")]
    NoSnapshot(String),
    /// The snapshot's content is not on this volume (a dataless snapshot).
    #[error("snapshot {0:?} is dataless: its content is not on this volume")]
    DatalessSnapshot(String),
    /// The view reads a snapshot, which is never written.
    #[error("this view reads snapshot {xid:#x}, which cannot be written")]
    ReadOnlySnapshot { xid: u64 },
}

impl From<ContainerError> for FsError {
    fn from(e: ContainerError) -> Self {
        FsError::Container(e)
    }
}

/// The two attributes that hold a compressed file's content. macOS hides
/// both from every caller but its own decompressor while the file is
/// compressed.
fn is_compressed_storage(name: &str) -> bool {
    name == "com.apple.decmpfs" || name == "com.apple.ResourceFork"
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
    /// The u32 APFS keeps at inode offset 56: a directory's child count, or a
    /// file's hard-link count. A host filesystem reports the latter as nlink.
    pub nchildren_or_nlink: u32,
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
/// How many resolved paths and listed directories a view keeps before it
/// starts over: enough for a large tree, bounded for a mount that walks
/// millions of paths over its lifetime.
const PATH_CACHE_LIMIT: usize = 65_536;
const DIR_CACHE_LIMIT: usize = 4_096;

pub struct FsView<D: BlockDevice> {
    dev: D,
    catalog: Catalog,
    names_hashed: bool,
    /// Whether names that differ only in case are the same name. A hashed
    /// volume is not necessarily case-insensitive (see APFSX).
    case_insensitive: bool,
    path_cache: HashMap<String, u64>,
    dir_cache: HashMap<u64, DirListing>,
    /// The transaction of the snapshot this view reads, or `None` for the
    /// volume as it is now. A snapshot view hands out nothing to write with.
    snapshot: Option<u64>,
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

/// A view's cached paths and listings, carried to a new view of the same
/// volume state by [`FsView::take_names`] and [`FsView::adopt_names`].
pub struct NameCache {
    vol_paddr: u64,
    paths: HashMap<String, u64>,
    dirs: HashMap<u64, DirListing>,
}

/// A directory's entries as listed, with each name's comparison key mapped
/// to the first entry carrying it, so a name lookup is one hash probe
/// instead of a fold of every entry.
struct DirListing {
    /// Sorted by name, byte for byte: the order a listing is paged in.
    entries: Vec<DirEntry>,
    /// Index into `entries` by the key the volume compares names by.
    by_key: HashMap<String, usize>,
    /// Entries removed since the listing was read, by index. They stay in
    /// `entries` so a removal costs no shift of a large directory.
    removed: HashSet<usize>,
}

/// How names compare on a volume: folded (case-insensitive), normalized
/// only (hashed but case-sensitive), or byte for byte.
#[derive(Clone, Copy)]
enum NameMatch {
    Folded,
    Normalized,
    Exact,
}

impl NameMatch {
    /// The key a stored name is compared by.
    fn key(self, name: &str) -> String {
        match self {
            NameMatch::Folded => case_fold::apfs_case_fold(name),
            NameMatch::Normalized => case_fold::apfs_normalize(name),
            NameMatch::Exact => name.to_string(),
        }
    }
    /// The key a looked-up name is compared by: the needle is folded (or
    /// normalized) once to its canonical form and then compared the way a
    /// stored name is, which keys it twice.
    fn needle_key(self, name: &str) -> String {
        match self {
            NameMatch::Folded | NameMatch::Normalized => self.key(&self.key(name)),
            NameMatch::Exact => name.to_string(),
        }
    }
}

impl DirListing {
    fn new(mut entries: Vec<DirEntry>, how: NameMatch) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        let mut by_key = HashMap::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            by_key.entry(how.key(&e.name)).or_insert(i);
        }
        Self {
            entries,
            by_key,
            removed: HashSet::new(),
        }
    }

    /// Drop the entry `name` (as the volume stores it) and give its inode,
    /// or `None` when the listing has no such name.
    fn remove(&mut self, name: &str, how: NameMatch) -> Option<u64> {
        let i = self.by_key.remove(&how.key(name))?;
        self.removed.insert(i);
        self.entries.get(i).map(|e| e.file_id)
    }

    /// The entries still in the directory, in listing order.
    fn live(&self) -> impl Iterator<Item = &DirEntry> {
        self.entries
            .iter()
            .enumerate()
            .filter(|(i, _)| !self.removed.contains(i))
            .map(|(_, e)| e)
    }
}

/// `apfs_superblock_t`: `apfs_fs_quota_block_count` (a volume's block quota,
/// zero when it has none) and `apfs_fs_alloc_count` (blocks the volume holds).
const VSB_QUOTA_BLOCK_COUNT: usize = 0x50;
const VSB_ALLOC_COUNT: usize = 0x58;

/// Up to `max` entries of `listing` that sort after `after`.
fn page_after(listing: &DirListing, after: Option<&str>, max: usize) -> Vec<DirEntry> {
    let entries = &listing.entries;
    let start = after.map_or(0, |a| entries.partition_point(|e| e.name.as_str() <= a));
    entries
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(i, _)| !listing.removed.contains(i))
        .take(max)
        .map(|(_, e)| e.clone())
        .collect()
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
    /// Explorer/dir consumer, as macOS reports them through statfs: the total
    /// is the container's block count and the free space is the space
    /// manager's free-block count, both in bytes, so every volume of the
    /// container and its own metadata are accounted for. A volume with a
    /// quota reports the quota as its total and what is left of it as free,
    /// since the quota, not the container, bounds what can be written to it.
    /// Measured against `statvfs` on macOS 26.
    pub fn volume_size(&mut self) -> Result<(u64, u64), FsError> {
        let container = apfs_core::container::Container::open(&mut self.dev)?;
        let bsz = self.bsz as u64;
        let mut vsb = vec![0u8; self.bsz];
        let vsb_off = self.vol_paddr * bsz;
        self.dev
            .read_at(vsb_off, &mut vsb)
            .map_err(FsError::Block)?;
        let field = |at: usize| -> u64 {
            vsb.get(at..at + 8)
                .and_then(|s| s.try_into().ok())
                .map(u64::from_le_bytes)
                .unwrap_or(0)
        };
        let quota_blocks = field(VSB_QUOTA_BLOCK_COUNT);
        let alloc_blocks = field(VSB_ALLOC_COUNT);
        if quota_blocks != 0 {
            let left = quota_blocks.saturating_sub(alloc_blocks);
            return Ok((quota_blocks.saturating_mul(bsz), left.saturating_mul(bsz)));
        }
        let total = container.superblock.block_count.saturating_mul(bsz);
        let free = container
            .free_block_count(&mut self.dev)?
            .saturating_mul(bsz);
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

    /// The device this view reads, for a caller that writes through it (an
    /// open write transaction). Cached lookups may be stale after a write;
    /// [`Self::rebind`] drops them.
    pub fn dev_mut(&mut self) -> &mut D {
        &mut self.dev
    }

    /// Bind this view to the volume superblock at `vol_paddr` and drop every
    /// cached lookup. A host that keeps a write transaction open reads
    /// through it, and after each operation rebinds to the superblock the
    /// transaction last staged, so lookups see the operation's changes
    /// before they are committed.
    pub fn rebind(&mut self, vol_paddr: u64) -> Result<(), FsError> {
        self.path_cache.clear();
        self.dir_cache.clear();
        self.rebind_to(vol_paddr)
    }

    /// [`Self::rebind`] after an operation that changed no name: no entry
    /// added, removed or renamed (a write, a resize, new times or
    /// attributes, an extended attribute). Every cached path and listing
    /// still holds, and keeping them spares the next lookups a walk from
    /// the root: a file copied in took five operations and some fifteen
    /// such walks.
    pub fn rebind_keeping_names(&mut self, vol_paddr: u64) -> Result<(), FsError> {
        self.rebind_to(vol_paddr)
    }

    /// [`Self::rebind`] after an operation that removed the entry `name` (as
    /// the volume stores it) of directory `parent` and changed no other
    /// name. The directory's cached listing loses that one entry instead of
    /// being read again: a program deleting a large folder lists it between
    /// deletes, and reading it again each time made every delete cost the
    /// folder's size.
    pub fn rebind_removing(
        &mut self,
        vol_paddr: u64,
        parent: u64,
        name: &str,
    ) -> Result<(), FsError> {
        // Cheap to resolve again, and any spelling of the removed path goes.
        self.path_cache.clear();
        if let Err(e) = self.rebind_to(vol_paddr) {
            // The name is gone from the volume, whatever this view now reads.
            self.dir_cache.clear();
            return Err(e);
        }
        let how = self.name_match();
        let gone = self.dir_cache.get_mut(&parent).map(|l| l.remove(name, how));
        match gone {
            // A removed directory's own listing goes with it.
            Some(Some(ino)) => {
                self.dir_cache.remove(&ino);
            }
            // The listing did not hold the name: it no longer says what
            // the directory holds.
            Some(None) => {
                self.dir_cache.remove(&parent);
            }
            None => {}
        }
        Ok(())
    }

    /// Take the cached paths and listings, to hand to the view that reads
    /// the volume once this view's changes are committed.
    pub fn take_names(&mut self) -> NameCache {
        NameCache {
            vol_paddr: self.vol_paddr,
            paths: std::mem::take(&mut self.path_cache),
            dirs: std::mem::take(&mut self.dir_cache),
        }
    }

    /// Use `names`, taken from a view of the same state of the volume: the
    /// view whose changes were committed into what this view reads. Names
    /// taken at another volume superblock are dropped.
    pub fn adopt_names(&mut self, names: NameCache) {
        if self.snapshot.is_none() && names.vol_paddr == self.vol_paddr {
            self.path_cache = names.paths;
            self.dir_cache = names.dirs;
        }
    }

    fn rebind_to(&mut self, vol_paddr: u64) -> Result<(), FsError> {
        use apfs_core::volume::VolumeSuperblock;
        if let Some(xid) = self.snapshot {
            return Err(FsError::ReadOnlySnapshot { xid });
        }
        let mut raw = vec![0u8; self.bsz];
        self.dev
            .read_at(vol_paddr.saturating_mul(self.bsz as u64), &mut raw)?;
        let vol = VolumeSuperblock::parse(&raw)?;
        if vol.is_encrypted() {
            return Err(FsError::Encrypted {
                name: vol.name.clone(),
                role: vol.role,
            });
        }
        self.catalog = Catalog::open(&mut self.dev, &vol, self.bsz as u32)?;
        self.names_hashed = vol.names_are_hashed();
        self.case_insensitive = vol.is_case_insensitive();
        self.vol_paddr = vol_paddr;
        Ok(())
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
        let (vol_paddr, vol, bsz) = Self::select_volume(&mut dev, sel)?;
        let catalog = Catalog::open(&mut dev, &vol, bsz as u32)?;
        Ok(Self::from_parts(
            dev, catalog, &vol, vol_paddr, bsz, sel, None,
        ))
    }

    /// Open the volume picked by `sel` as snapshot `snap` holds it, read
    /// only. The snapshot's frozen volume superblock names its catalog;
    /// the live volume's object map, which snapshots share, maps it at the
    /// snapshot's transaction.
    /// [linux-apfs-rw snapshot.c apfs_switch_to_snapshot; apfs.kext
    ///  apfs_snap_lookup_internal: name -> xid -> snapshot metadata]
    pub fn open_snapshot(
        mut dev: D,
        sel: VolumeSelector,
        snap: &SnapshotRef,
    ) -> Result<Self, FsError> {
        use apfs_core::volume::VolumeSuperblock;
        let (_, vol, bsz) = Self::select_volume(&mut dev, sel)?;
        let all = apfs_core::snapshot::list(&mut dev, vol.snap_meta_tree_oid, bsz as u32)?;
        let found = all
            .into_iter()
            .find(|s| match snap {
                SnapshotRef::Name(n) => s.name == *n,
                SnapshotRef::Xid(x) => s.xid == *x,
            })
            .ok_or_else(|| {
                FsError::NoSnapshot(match snap {
                    SnapshotRef::Name(n) => n.clone(),
                    SnapshotRef::Xid(x) => format!("{x:#x}"),
                })
            })?;
        if found.flags & apfs_core::snapshot::SNAP_META_PENDING_DATALESS != 0 {
            return Err(FsError::DatalessSnapshot(found.name));
        }
        let mut raw = vec![0u8; bsz];
        dev.read_at(found.sblock_oid.saturating_mul(bsz as u64), &mut raw)?;
        let frozen = VolumeSuperblock::parse(&raw)?;
        let catalog = Catalog::open_at(&mut dev, &frozen, vol.omap_oid, found.xid, bsz as u32)?;
        tracing::info!(name = %found.name, xid = found.xid, "opened APFS snapshot");
        Ok(Self::from_parts(
            dev,
            catalog,
            &frozen,
            found.sblock_oid,
            bsz,
            sel,
            Some(found.xid),
        ))
    }

    /// The snapshots of the volume this view reads, oldest first (none for
    /// a view of a snapshot: a snapshot holds no snapshots).
    pub fn snapshots(&mut self) -> Result<Vec<apfs_core::SnapshotMeta>, FsError> {
        use apfs_core::volume::VolumeSuperblock;
        let mut raw = vec![0u8; self.bsz];
        self.dev
            .read_at(self.vol_paddr.saturating_mul(self.bsz as u64), &mut raw)?;
        let vol = VolumeSuperblock::parse(&raw)?;
        Ok(apfs_core::snapshot::list(
            &mut self.dev,
            vol.snap_meta_tree_oid,
            self.bsz as u32,
        )?)
    }

    /// The transaction of the snapshot this view reads, if it reads one.
    pub fn snapshot_xid(&self) -> Option<u64> {
        self.snapshot
    }

    fn from_parts(
        dev: D,
        catalog: Catalog,
        vol: &apfs_core::volume::VolumeSuperblock,
        vol_paddr: u64,
        bsz: usize,
        selector: VolumeSelector,
        snapshot: Option<u64>,
    ) -> Self {
        Self {
            dev,
            catalog,
            names_hashed: vol.names_are_hashed(),
            case_insensitive: vol.is_case_insensitive(),
            path_cache: HashMap::with_capacity(64),
            dir_cache: HashMap::with_capacity(16),
            vol_paddr,
            bsz,
            selector,
            snapshot,
        }
    }

    /// The volume `sel` picks: its superblock's address, the superblock,
    /// and the container's block size. An encrypted volume is refused.
    fn select_volume(
        dev: &mut D,
        sel: VolumeSelector,
    ) -> Result<(u64, apfs_core::volume::VolumeSuperblock, usize), FsError> {
        use apfs_core::omap::Omap;
        use apfs_core::volume::VolumeSuperblock;

        // Copy out the container fields we need so we can drop the borrow on
        // `c` before the volume-scan loop (which re-borrows `dev` mutably).
        let c = Container::open(dev)?;
        let bsz = c.superblock.block_size as usize;
        let block_size = c.superblock.block_size;
        let xid = c.superblock.obj.xid;
        let omap_oid = c.superblock.omap_oid;
        let fs_slots: Vec<(u32, u64)> = c.superblock.fs_slots.to_vec();
        drop(c);

        if fs_slots.is_empty() {
            return Err(FsError::NoVolume);
        }

        let omap = Omap::open(dev, omap_oid, block_size)?;

        // Resolve every volume superblock so the selector can inspect roles.
        struct Cand {
            index: usize,
            paddr: u64,
            vsb: VolumeSuperblock,
        }
        let mut cands: Vec<Cand> = Vec::with_capacity(fs_slots.len());
        for &(slot, oid) in &fs_slots {
            let index = slot as usize;
            let paddr = match omap.resolve(dev, oid, xid, block_size)? {
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

        // A FileVault volume stores ciphertext in its file extents. We hold no
        // keybag material, so every read would return garbage that is
        // indistinguishable from real content. Refuse the mount instead.
        if vol.is_encrypted() {
            tracing::warn!(
                name = %vol.name,
                role = ?vol.volume_role(),
                "refusing encrypted volume: file data would be served as ciphertext"
            );
            return Err(FsError::Encrypted {
                name: vol.name.clone(),
                role: vol.role,
            });
        }

        Ok((vol_paddr, vol, bsz))
    }

    /// Return raw VSB and volume omap header bytes (each one block).
    /// Used by the WinFsp write path: apfs-write API calls take vsb_raw
    /// and vol_omap_raw byte slices, then the Transaction stages updated
    /// versions onto the device during commit. Reading here re-fetches
    /// the LATEST on-disk state - important after a previous transaction
    /// has just bumped the VSB xid.
    pub fn read_vsb_omap_raw(&mut self) -> Result<(Vec<u8>, Vec<u8>), FsError> {
        if let Some(xid) = self.snapshot {
            return Err(FsError::ReadOnlySnapshot { xid });
        }
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

    /// Every mapping of the volume's object map as stored, in key order.
    pub fn volume_omap_entries(&mut self) -> Result<Vec<apfs_core::omap::OmapEntry>, FsError> {
        let (vsb_raw, _) = self.read_vsb_omap_raw()?;
        let omap_paddr = u64::from_le_bytes(
            vsb_raw
                .get(0x80..0x88)
                .ok_or(FsError::NoVolume)?
                .try_into()
                .map_err(|_| FsError::NoVolume)?,
        );
        let omap = apfs_core::omap::Omap::open(&mut self.dev, omap_paddr, self.bsz as u32)?;
        Ok(omap.entries(&mut self.dev, self.bsz as u32)?)
    }

    fn listing(&mut self, ino: u64) -> Result<&DirListing, FsError> {
        use std::collections::hash_map::Entry;
        if self.dir_cache.len() >= DIR_CACHE_LIMIT && !self.dir_cache.contains_key(&ino) {
            self.dir_cache.clear();
        }
        let how = self.name_match();
        Ok(match self.dir_cache.entry(ino) {
            Entry::Occupied(o) => o.into_mut(),
            Entry::Vacant(e) => {
                let v = self
                    .catalog
                    .list_dir(&mut self.dev, ino, self.names_hashed)?;
                e.insert(DirListing::new(v, how))
            }
        })
    }

    /// How names compare on this volume.
    fn name_match(&self) -> NameMatch {
        match (self.names_hashed, self.case_insensitive) {
            (true, true) => NameMatch::Folded,
            (true, false) => NameMatch::Normalized,
            (false, _) => NameMatch::Exact,
        }
    }

    /// The inode the entry `name` of directory `ino` names, if any.
    fn child(&mut self, ino: u64, name: &str) -> Result<Option<u64>, FsError> {
        Ok(self.child_entry(ino, name)?.map(|e| e.file_id))
    }

    /// The entry of directory `ino` that `name` names, compared the way the
    /// volume compares names. A directory already listed is looked up in
    /// its listing; otherwise, on a volume that hashes names, the few
    /// records with the name's hash are read instead of the whole directory
    /// (after a write the listings are gone, and listing a large directory
    /// for every lookup made each operation cost its size).
    fn child_entry(&mut self, ino: u64, name: &str) -> Result<Option<DirEntry>, FsError> {
        let how = self.name_match();
        if self.names_hashed && !self.dir_cache.contains_key(&ino) {
            let (fold, normalize) = (matches!(how, NameMatch::Folded), true);
            let hash = case_fold::apfs_name_hash(name, fold, normalize);
            let found = self.catalog.find_dir_entries(&mut self.dev, ino, hash)?;
            let wanted = how.needle_key(name);
            return Ok(found.into_iter().find(|e| how.key(&e.name) == wanted));
        }
        self.listing(ino)?;
        let key = how.needle_key(name);
        Ok(self.dir_cache.get(&ino).and_then(|l| {
            let i = l.by_key.get(&key).copied()?;
            l.entries.get(i).cloned()
        }))
    }

    /// The entry `path` names: its directory's inode and the name as the
    /// volume stores it, which may differ from `path`'s in case or
    /// normalization. `Ok(None)` when the directory or the entry is absent.
    pub fn entry(&mut self, path: &str) -> Result<Option<(u64, DirEntry)>, FsError> {
        let comps = components(path);
        let Some((last, dirs)) = comps.split_last() else {
            return Ok(None);
        };
        let Some(dir) = self.lookup(&format!("/{}", dirs.join("/")))? else {
            return Ok(None);
        };
        Ok(self.child_entry(dir, last)?.map(|e| (dir, e)))
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
        // Start below the longest prefix already resolved. Each component is
        // then one probe of its directory's name index: on a hashed volume
        // the needle is folded (case-insensitive) or only normalized
        // (case-sensitive: README and readme stay two files) exactly as each
        // stored name is, so NFC and NFD spellings from Finder / AFP meet.
        let mut start = 0;
        let mut ino = ROOT_DIR_INO_NUM;
        for n in (1..comps.len()).rev() {
            let prefix = format!("/{}", comps[..n].join("/"));
            if let Some(&i) = self.path_cache.get(&prefix) {
                start = n;
                ino = i;
                break;
            }
        }
        for (n, comp) in comps.iter().enumerate().skip(start) {
            match self.child(ino, comp)? {
                Some(i) => ino = i,
                None => return Ok(None),
            }
            if self.path_cache.len() >= PATH_CACHE_LIMIT {
                self.path_cache.clear();
            }
            self.path_cache
                .insert(format!("/{}", comps[..=n].join("/")), ino);
        }
        Ok(Some(ino))
    }

    /// List a directory's entries (cached), sorted by name byte for byte.
    pub fn read_dir(&mut self, path: &str) -> Result<Vec<DirEntry>, FsError> {
        match self.lookup(path)? {
            Some(ino) => Ok(self.listing(ino)?.live().cloned().collect()),
            None => Err(FsError::NotFound(path.to_string())),
        }
    }

    /// Up to `max` entries of a directory that sort after `after` (from the
    /// first with `None`), in the order of [`read_dir`](Self::read_dir). A
    /// caller paging through a large directory copies only each page, not
    /// the whole listing every time.
    pub fn read_dir_after(
        &mut self,
        path: &str,
        after: Option<&str>,
        max: usize,
    ) -> Result<Vec<DirEntry>, FsError> {
        let Some(ino) = self.lookup(path)? else {
            return Err(FsError::NotFound(path.to_string()));
        };
        Ok(page_after(self.listing(ino)?, after, max))
    }

    /// Stat a path (size + is_dir + inode). `Ok(None)` if the path is absent.
    pub fn getattr(&mut self, path: &str) -> Result<Option<Attr>, FsError> {
        match self.lookup(path)? {
            None => Ok(None),
            Some(inode) => {
                let InodeStat {
                    size,
                    is_dir,
                    nchildren_or_nlink,
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
                    nchildren_or_nlink,
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
    /// Only what the range touches is read: the extents it covers, or for a
    /// compressed file the chunks, so a read of a large file costs the read.
    pub fn read(&mut self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>, FsError> {
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Err(FsError::NotFound(path.to_string())),
        };
        let Ok(offset) = usize::try_from(offset) else {
            return Ok(Vec::new());
        };
        Ok(self
            .catalog
            .read_file_decompressed_range(&mut self.dev, ino, offset, len)?)
    }

    /// Return a list of all extended attribute names for the file at `path`,
    /// as macOS shows them to an ordinary caller: internal Apple system-owned
    /// attributes (flag `XATTR_FILE_SYSTEM_OWNED = 0x0004`) are left out, and
    /// so are a compressed file's `com.apple.decmpfs` and
    /// `com.apple.ResourceFork`, which hold its content rather than describe
    /// it (on a file that is not compressed they are ordinary attributes).
    /// Returns `Ok(vec![])` when the file has no user xattrs or the path is
    /// absent.
    pub fn list_xattrs(&mut self, path: &str) -> Result<Vec<String>, FsError> {
        use apfs_core::xattr::XATTR_FILE_SYSTEM_OWNED;
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };
        let compressed = self.is_compressed(ino)?;
        let entries = self.catalog.list_xattrs(&mut self.dev, ino)?;
        Ok(entries
            .into_iter()
            .filter(|e| e.flags & XATTR_FILE_SYSTEM_OWNED == 0)
            .filter(|e| !(compressed && is_compressed_storage(&e.name)))
            .map(|e| e.name)
            .collect())
    }

    /// Whether the inode stores its content compressed (`UF_COMPRESSED`).
    fn is_compressed(&mut self, ino: u64) -> Result<bool, FsError> {
        Ok(self
            .catalog
            .inode_record(&mut self.dev, ino)?
            .is_some_and(|inode| inode.is_compressed()))
    }

    /// Every name the file at `path` is reachable through.
    ///
    /// A file with a single name carries no sibling-link records, so the result
    /// is empty; that is not the same as the file being absent, which also
    /// gives an empty result. Ordered by sibling id, oldest first.
    pub fn sibling_links(
        &mut self,
        path: &str,
    ) -> Result<Vec<apfs_core::catalog::SiblingLink>, FsError> {
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };
        Ok(self.catalog.sibling_links(&mut self.dev, ino)?)
    }

    /// Every data-stream-id record of the volume as `(stream id, reference count)`.
    pub fn dstream_ids(&mut self) -> Result<Vec<(u64, u32)>, FsError> {
        Ok(self.catalog.dstream_ids(&mut self.dev)?)
    }

    /// Every sibling-map record of the volume as `(sibling id, inode number)`.
    pub fn sibling_maps(&mut self) -> Result<Vec<(u64, u64)>, FsError> {
        Ok(self.catalog.sibling_maps(&mut self.dev)?)
    }

    /// The parsed inode record at `path`, or `Ok(None)` when the path is absent.
    pub fn inode_record(&mut self, path: &str) -> Result<Option<apfs_core::inode::Inode>, FsError> {
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(None),
        };
        Ok(self.catalog.inode_record(&mut self.dev, ino)?)
    }

    /// The data-stream extents of the file at `path`, in logical order; empty
    /// when the path is absent or the file has no stream.
    pub fn file_extents(
        &mut self,
        path: &str,
    ) -> Result<Vec<apfs_core::catalog::FileExtent>, FsError> {
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(Vec::new()),
        };
        Ok(self.catalog.file_extents(&mut self.dev, ino)?)
    }

    /// The parent directory and name the inode at `path` records for itself.
    /// Returns `Ok(None)` when the path is absent.
    pub fn primary_link(
        &mut self,
        path: &str,
    ) -> Result<Option<apfs_core::catalog::PrimaryLink>, FsError> {
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(None),
        };
        Ok(self.catalog.primary_link(&mut self.dev, ino)?)
    }

    /// Read the raw bytes of one extended attribute by name. Returns
    /// `Ok(None)` when the path is absent, the attribute does not exist, or
    /// it is the storage of a compressed file (see [`FsView::list_xattrs`]).
    /// The stored flags of attribute `name` at `path` (data stream, embedded,
    /// file-system owned), `Ok(None)` when the path or the attribute is absent.
    pub fn xattr_flags(&mut self, path: &str, name: &str) -> Result<Option<u16>, FsError> {
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(None),
        };
        Ok(self.catalog.xattr_flags(&mut self.dev, ino, name)?)
    }

    pub fn get_xattr(&mut self, path: &str, name: &str) -> Result<Option<Vec<u8>>, FsError> {
        let ino = match self.lookup(path)? {
            Some(i) => i,
            None => return Ok(None),
        };
        if is_compressed_storage(name) && self.is_compressed(ino)? {
            return Ok(None);
        }
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
        // APFS stores the target NUL-terminated; the target is the value
        // length minus that terminator. Strip it here so every consumer sees
        // the path itself rather than each of them guessing.
        Ok(self
            .catalog
            .get_xattr(&mut self.dev, ino, "com.apple.fs.symlink")?
            .map(|mut v| {
                if v.last() == Some(&0) {
                    v.pop();
                }
                v
            }))
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

    // --- listings ---

    use super::{page_after, DirEntry, DirListing, NameMatch};

    fn entry(name: &str, id: u64) -> DirEntry {
        DirEntry {
            name: name.to_string(),
            file_id: id,
            flags: 0,
        }
    }

    #[test]
    fn a_listing_is_sorted_and_found_by_key() {
        let names = ["b.txt", "A.txt", "café", "a.txt", "Z", "10", "9"];
        let l = DirListing::new(
            names.iter().zip(1..).map(|(n, i)| entry(n, i)).collect(),
            NameMatch::Exact,
        );
        let got: Vec<&str> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(got, ["10", "9", "A.txt", "Z", "a.txt", "b.txt", "café"]);
        let i = l.by_key["café"];
        assert_eq!(l.entries[i].file_id, 3);
    }

    #[test]
    fn pages_cover_a_listing_once_in_order() {
        let l = DirListing::new(
            (0..1000).map(|i| entry(&format!("f{i}"), i)).collect(),
            NameMatch::Exact,
        );
        let mut seen = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = page_after(&l, after.as_deref(), 64);
            if page.is_empty() {
                break;
            }
            after = page.last().map(|e| e.name.clone());
            seen.extend(page.into_iter().map(|e| e.name));
        }
        let want: Vec<String> = l.entries.iter().map(|e| e.name.clone()).collect();
        assert_eq!(seen, want);
        // A marker that is not itself a name resumes after where it sorts.
        let page = page_after(&l, Some("f5000"), 2);
        assert_eq!(page[0].name, "f501");
    }

    #[test]
    fn a_removed_entry_leaves_pages_and_lookups() {
        let mut l = DirListing::new(
            (0..10).map(|i| entry(&format!("F{i}"), 100 + i)).collect(),
            NameMatch::Folded,
        );
        // Removed by the name as stored; any case finds it before.
        assert!(l.by_key.contains_key(&NameMatch::Folded.needle_key("f3")));
        assert_eq!(l.remove("F3", NameMatch::Folded), Some(103));
        assert_eq!(l.remove("F3", NameMatch::Folded), None);
        assert!(!l.by_key.contains_key(&NameMatch::Folded.needle_key("f3")));
        assert_eq!(l.remove("F0", NameMatch::Folded), Some(100));
        assert_eq!(l.remove("F9", NameMatch::Folded), Some(109));
        let names = |v: Vec<DirEntry>| v.into_iter().map(|e| e.name).collect::<Vec<_>>();
        // Paging around and across the removed names skips them.
        assert_eq!(names(page_after(&l, None, 3)), ["F1", "F2", "F4"]);
        assert_eq!(names(page_after(&l, Some("F2"), 2)), ["F4", "F5"]);
        assert_eq!(names(page_after(&l, Some("F8"), 5)), Vec::<String>::new());
        assert_eq!(names(l.live().cloned().collect()).len(), 7);
        // The other names are still found by key.
        let i = l.by_key[&NameMatch::Folded.needle_key("f4")];
        assert_eq!(l.entries[i].file_id, 104);
    }
}
