//! Catalog - the volume file-system (catalog) B-tree. Linkage rules
//! (VIRTUAL via the volume omap; xid = volume superblock o_xid; drec key
//! variant): ctx_search(source:"the APFS specification").
use crate::block_device::BlockDevice;
use crate::btree::{bad_level, BtreeNode};
use crate::container::ContainerError;
use crate::decmpfs::{DecmpfsError, DecmpfsHeader};
use crate::endian::{u16_le, u32_le, ParseError};
use crate::inode::Inode;
use crate::jkey::{JKey, APFS_TYPE_DIR_REC, APFS_TYPE_FILE_EXTENT};
use crate::obj::{OBJECT_TYPE_BTREE, OBJECT_TYPE_BTREE_NODE, OBJECT_TYPE_FEXT_TREE};

/// Deeper than any catalog tree macOS writes (a few levels at most); a chain
/// past this is a cycle or a corrupt image, not a large directory.
const MAX_CATALOG_TREE_DEPTH: u8 = 32;
use crate::omap::{Omap, OMAP_VAL_NOHEADER};
use crate::volume::VolumeSuperblock;

/// Key and value sizes of a file-extent tree record: key `{private_id,
/// logical_addr}`, value `{len_and_flags, phys_block_num}`.
/// [linux-apfs-rw apfs_raw.h apfs_fext_tree_key / apfs_fext_tree_val]
const FEXT_RECORD_SIZE: usize = 16;
/// Value size of an index record of a physical tree: the child's address.
const PHYSICAL_CHILD_SIZE: usize = 8;
/// Low 56 bits of `len_and_flags`: the extent's length in bytes.
const FILE_EXTENT_LEN_MASK: u64 = 0x00ff_ffff_ffff_ffff;

/// Parsed nodes a catalog keeps between lookups: at most this many (4 KiB
/// each, so about 64 MiB), enough for the whole tree of a volume with a
/// million records. A full cache starts over.
const NODE_CACHE_LIMIT: usize = 16_384;

/// The catalog's memory of what it has read. A catalog reads one checkpoint
/// of the volume (its xid is fixed), so a virtual oid resolves to the same
/// block and a block holds the same node for the catalog's whole life;
/// nothing here needs invalidating. A node's checksum is verified once,
/// when it is read from the device.
#[derive(Default)]
struct ReadCache {
    /// Virtual oid -> (physical block, object map flags).
    oids: std::collections::HashMap<u64, (u64, u32)>,
    /// Physical block -> parsed node (catalog and file-extent trees).
    nodes: std::collections::HashMap<u64, std::sync::Arc<BtreeNode>>,
}

impl ReadCache {
    fn keep(&mut self, paddr: u64, node: std::sync::Arc<BtreeNode>) {
        if self.nodes.len() >= NODE_CACHE_LIMIT {
            self.nodes.clear();
        }
        self.nodes.insert(paddr, node);
    }
}

/// What reading a sealed volume needs beyond the catalog itself.
struct Sealed {
    /// The catalog root's virtual oid: an index record of a sealed catalog
    /// names its child relative to it.
    root_oid: u64,
    /// Root of the tree holding every file extent of the volume.
    fext_root: BtreeNode,
}

/// The attribute holding a compressed file's header and, for the odd
/// methods, its content.
const DECMPFS_ATTRIBUTE: &str = "com.apple.decmpfs";
/// The attribute holding the chunks of the even methods.
const RESOURCE_FORK_ATTRIBUTE: &str = "com.apple.ResourceFork";

const fn oor() -> ContainerError {
    ContainerError::Parse(ParseError::Short {
        at: 0,
        need: 0,
        len: 0,
    })
}

/// Cheap inode metadata for a mount host's getattr (no file-content read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InodeStat {
    pub size: u64,
    pub is_dir: bool,
    /// The u32 APFS keeps at inode offset 56: a directory's child count, or a
    /// file's hard-link count.
    pub nchildren_or_nlink: u32,
    /// POSIX mode (`S_IFREG` | rwx bits). Used by host filesystems to map
    /// `FILE_ATTRIBUTE_READONLY` for Explorer / Get-ItemProperty without
    /// adding an extra read.
    pub mode: u16,
    /// BSD file flags (u32 @ offset 68 in `j_inode_val`). Carries `UF_HIDDEN`
    /// (0x8000), `UF_IMMUTABLE` (0x2), etc. Surfaced so WinFsp can map
    /// `UF_HIDDEN` to `FILE_ATTRIBUTE_HIDDEN`.
    pub bsd_flags: u32,
    /// File timestamps (APFS ns since UNIX epoch). Hosts convert to
    /// Windows FILETIME (100-ns ticks since 1601-01-01) for FileInfo.
    pub create_time: u64,
    pub mod_time: u64,
    pub change_time: u64,
    pub access_time: u64,
}

/// One file-extent record of a data stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileExtent {
    /// Byte offset within the stream.
    pub logical_addr: u64,
    /// Bytes covered (a whole number of blocks).
    pub len: u64,
    /// The flag byte above the length (the preallocated / crypto bits).
    pub flags: u8,
    /// First block, or 0 for a hole that owns no blocks.
    pub phys_block_num: u64,
}

/// The name and parent directory an inode records for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryLink {
    pub parent_id: u64,
    pub name: String,
}

/// One of the names a file is reachable through, as recorded by a
/// sibling-link record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiblingLink {
    /// Identifies this name among the file's names; also the key of the
    /// sibling-map record pointing back at the inode.
    pub sibling_id: u64,
    /// The directory the name sits in.
    pub parent_id: u64,
    pub name: String,
}

/// One directory-entry record (APFS_TYPE_DIR_REC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub file_id: u64,
    pub flags: u16,
}

/// Where a catalog key sorts, as far as a range walk needs: object id,
/// record type, and then the name hash of a hashed directory record or the
/// logical address of a file extent (0 for any other key). Keys are ordered
/// by object id, then type, then - for hashed directory records - hash and
/// name, or - for file extents - logical address, so positions order like
/// the keys.
type KeyPos = (u64, u8, u64);

/// The position of catalog key `k`. The hash is read only from a directory
/// record long enough to carry one; on a volume that does not hash names
/// those bytes are something else, and only walks that span every hash of
/// an object (which look at id and type alone) are made there.
fn key_pos(k: &[u8]) -> Result<KeyPos, ContainerError> {
    let jk = JKey::parse(k)?;
    let third = if jk.obj_type == APFS_TYPE_DIR_REC && k.len() >= 12 {
        u64::from(u32_le(k, 8)? >> 10)
    } else if jk.obj_type == APFS_TYPE_FILE_EXTENT && k.len() >= 16 {
        k.get(8..16)
            .and_then(|b| b.try_into().ok())
            .map(u64::from_le_bytes)
            .ok_or_else(oor)?
    } else {
        0
    };
    Ok((jk.obj_id, jk.obj_type, third))
}

/// The directory entry in record `(k, v)` of a directory whose names are
/// `names_hashed` or not.
fn parse_dir_entry(k: &[u8], v: &[u8], names_hashed: bool) -> Result<DirEntry, ContainerError> {
    // After the 8-byte j_key_t: hashed => u32 name_len_and_hash (name
    // length = low 10 bits), else u16 name_len; then the name bytes
    // (length includes the trailing NUL).
    let (name_off, name_len) = if names_hashed {
        let nlh = u32_le(k, 8)?;
        (12usize, (nlh & 0x0000_03ff) as usize)
    } else {
        let nl = u16_le(k, 8)?;
        (10usize, nl as usize)
    };
    let raw = k.get(name_off..name_off + name_len).ok_or_else(oor)?;
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    let name = String::from_utf8_lossy(raw.get(..end).unwrap_or(&[])).into_owned();
    // j_drec_val { u64 file_id@0; u64 date_added@8; u16 flags@16; ... }
    let file_id = u64::from_le_bytes(v.get(0..8).ok_or_else(oor)?.try_into().map_err(|_| oor())?);
    let flags = u16::from_le_bytes(
        v.get(16..18)
            .ok_or_else(oor)?
            .try_into()
            .map_err(|_| oor())?,
    );
    Ok(DirEntry {
        name,
        file_id,
        flags,
    })
}

/// A volume's catalog (file-system) tree, ready to walk.
pub struct Catalog {
    omap: Omap,
    root: BtreeNode,
    xid: u64,
    block_size: u32,
    /// Present on a sealed volume.
    sealed: Option<Sealed>,
    cache: std::cell::RefCell<ReadCache>,
}

/// Read the catalog node mapped at virtual `oid`. A node mapped with
/// `OMAP_VAL_NOHEADER` has no header to verify; any other must be a
/// checksum-valid node of `expected_type`.
fn read_catalog_node<D: BlockDevice>(
    dev: &mut D,
    omap: &Omap,
    oid: u64,
    xid: u64,
    block_size: u32,
    expected_type: u32,
) -> Result<BtreeNode, ContainerError> {
    let (paddr, flags) = omap
        .resolve_with_flags(dev, oid, xid, block_size)?
        .ok_or_else(oor)?;
    load_catalog_node(dev, paddr, flags, block_size, expected_type)
}

/// Read and check the catalog node at physical `paddr`, mapped with object
/// map `flags` (see [`read_catalog_node`]).
fn load_catalog_node<D: BlockDevice>(
    dev: &mut D,
    paddr: u64,
    flags: u32,
    block_size: u32,
    expected_type: u32,
) -> Result<BtreeNode, ContainerError> {
    let mut buf = vec![0u8; block_size as usize];
    dev.read_at(
        paddr.checked_mul(u64::from(block_size)).ok_or_else(oor)?,
        &mut buf,
    )?;
    if flags & OMAP_VAL_NOHEADER != 0 {
        return Ok(BtreeNode::parse_headerless(&buf)?);
    }
    let node = BtreeNode::parse(&buf)?;
    if node.obj.object_type() != expected_type {
        return Err(ContainerError::Parse(ParseError::BadMagic {
            expected: expected_type,
            found: node.obj.object_type(),
        }));
    }
    Ok(node)
}

/// Read a node of the sealed volume's file-extent tree at physical `paddr`:
/// a checksum-valid node of that tree.
fn read_fext_node<D: BlockDevice>(
    dev: &mut D,
    paddr: u64,
    block_size: u32,
    expected_type: u32,
) -> Result<BtreeNode, ContainerError> {
    let mut buf = vec![0u8; block_size as usize];
    dev.read_at(
        paddr.checked_mul(u64::from(block_size)).ok_or_else(oor)?,
        &mut buf,
    )?;
    let node = BtreeNode::parse(&buf)?;
    if node.obj.object_type() != expected_type {
        return Err(ContainerError::Parse(ParseError::BadMagic {
            expected: expected_type,
            found: node.obj.object_type(),
        }));
    }
    if node.obj.subtype != OBJECT_TYPE_FEXT_TREE {
        return Err(ContainerError::Parse(ParseError::BadMagic {
            expected: OBJECT_TYPE_FEXT_TREE,
            found: node.obj.subtype,
        }));
    }
    if !node.is_fixed_kv() {
        return Err(ContainerError::Parse(ParseError::BadField {
            field: "fext node flags",
            value: u64::from(node.flags),
        }));
    }
    Ok(node)
}

/// A file-extent tree key as `(private_id, logical_addr)`.
fn fext_key(k: &[u8]) -> Result<(u64, u64), ContainerError> {
    let id = k
        .get(0..8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(oor)?;
    let logical = k
        .get(8..16)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(oor)?;
    Ok((id, logical))
}

impl Catalog {
    /// Open the catalog tree of `vol`.
    /// `vol.omap_oid` (apfs_omap_oid) is a PHYSICAL oid -> `Omap::open` directly,
    /// exactly like the container omap. `vol.root_tree_oid` (apfs_root_tree_oid)
    /// is a VIRTUAL oid -> resolved through the volume omap at the volume
    /// superblock's own xid (`vol.obj.xid`).
    pub fn open<D: BlockDevice>(
        dev: &mut D,
        vol: &VolumeSuperblock,
        block_size: u32,
    ) -> Result<Self, ContainerError> {
        let omap = Omap::open(dev, vol.omap_oid, block_size)?;
        let xid = vol.obj.xid;
        let root = read_catalog_node(
            dev,
            &omap,
            vol.root_tree_oid,
            xid,
            block_size,
            OBJECT_TYPE_BTREE,
        )?;
        if !root.is_root() {
            return Err(ContainerError::Parse(ParseError::BadField {
                field: "catalog root flags",
                value: u64::from(root.flags),
            }));
        }
        // A sealed volume keeps its file extents out of the catalog, in a
        // physical tree of their own.
        let sealed = if vol.is_sealed() {
            let fext_root = read_fext_node(dev, vol.fext_tree_oid, block_size, OBJECT_TYPE_BTREE)?;
            Some(Sealed {
                root_oid: vol.root_tree_oid,
                fext_root,
            })
        } else {
            None
        };
        Ok(Self {
            omap,
            root,
            xid,
            block_size,
            sealed,
            cache: Default::default(),
        })
    }

    /// Depth-first walk; calls `f(key, value)` for every LEAF entry. Non-leaf
    /// values are 8-byte VIRTUAL child oids resolved through the volume omap.
    ///
    /// A node is a leaf when its flag and a level of zero agree, and a child
    /// is a B-tree node exactly one level below its parent; a chain deeper
    /// than any catalog tree macOS writes is a malformed image. Each of these
    /// is an error, since a walk that quietly stopped would report the
    /// records it happened to reach as the whole directory.
    #[allow(clippy::type_complexity)]
    fn for_each_leaf<D: BlockDevice>(
        &self,
        dev: &mut D,
        node: &BtreeNode,
        depth: u8,
        f: &mut dyn FnMut(&[u8], &[u8]) -> Result<(), ContainerError>,
    ) -> Result<(), ContainerError> {
        self.walk(dev, node, depth, None, f)
    }

    /// Calls `f(key, value)` for the leaf records of the leaves that can
    /// hold records of type `obj_type` of object `obj_id` (every such record
    /// among them; the caller still filters). Catalog keys are ordered by
    /// object id, then type, so child `i` of an index node, holding keys
    /// from its own key up to the next child's, is read only when that range
    /// can include them: a lookup reads one path down the tree instead of
    /// every node. Bounding the type matters for a directory, whose entries
    /// share its id: a stat read the whole directory to find the inode.
    #[allow(clippy::type_complexity)]
    fn for_each_record_of<D: BlockDevice>(
        &self,
        dev: &mut D,
        obj_id: u64,
        obj_type: u8,
        f: &mut dyn FnMut(&[u8], &[u8]) -> Result<(), ContainerError>,
    ) -> Result<(), ContainerError> {
        let range = ((obj_id, obj_type, 0), (obj_id, obj_type, u64::MAX));
        self.walk(dev, &self.root, 0, Some(range), f)
    }

    /// The walk behind [`Catalog::for_each_leaf`] (`only` = `None`),
    /// [`Catalog::for_each_record_of`] and [`Catalog::find_dir_entries`]:
    /// with `only`, child `i` of an index node is read only when its range
    /// (from its key up to the next child's) meets the positions given.
    #[allow(clippy::type_complexity)]
    fn walk<D: BlockDevice>(
        &self,
        dev: &mut D,
        node: &BtreeNode,
        depth: u8,
        only: Option<(KeyPos, KeyPos)>,
        f: &mut dyn FnMut(&[u8], &[u8]) -> Result<(), ContainerError>,
    ) -> Result<(), ContainerError> {
        if depth > MAX_CATALOG_TREE_DEPTH {
            return Err(ContainerError::Parse(ParseError::BadField {
                field: "catalog tree depth",
                value: u64::from(depth),
            }));
        }
        let is_leaf = node.is_leaf();
        if is_leaf != (node.level == 0) {
            return Err(bad_level(node.level).into());
        }
        for i in 0..node.nkeys {
            if is_leaf {
                let (k, v) = node.var_kv(i)?;
                f(k, v)?;
            } else {
                let (k, v) = node.var_kv(i)?;
                if let Some((lo, hi)) = only {
                    if key_pos(k)? > hi {
                        break;
                    }
                    let next = i.checked_add(1).filter(|&n| n < node.nkeys);
                    if let Some(n) = next {
                        let (nk, _) = node.var_kv(n)?;
                        // The next child starts below the range: nothing of
                        // it is in this one.
                        if key_pos(nk)? < lo {
                            continue;
                        }
                    }
                }
                let named =
                    u64::from_le_bytes(v.get(0..8).ok_or_else(oor)?.try_into().map_err(|_| oor())?);
                // A sealed catalog's index value is the child's oid relative
                // to the root's, followed by the child's hash.
                // [linux-apfs-rw btree.c apfs_child_from_query; decoded on a
                // macOS cryptex volume: root 1378, children named 56, 57, ...]
                let child_oid = match &self.sealed {
                    Some(s) => named.checked_add(s.root_oid).ok_or_else(oor)?,
                    None => named,
                };
                let child = self.catalog_node(dev, child_oid, OBJECT_TYPE_BTREE_NODE)?;
                if child.is_root() {
                    return Err(ContainerError::Parse(ParseError::BadField {
                        field: "catalog child flags",
                        value: u64::from(child.flags),
                    }));
                }
                if child.level.checked_add(1) != Some(node.level) {
                    return Err(bad_level(child.level).into());
                }
                self.walk(dev, &child, depth.saturating_add(1), only, f)?;
            }
        }
        Ok(())
    }

    /// The catalog node at virtual `oid`, from the cache when this catalog
    /// has read it before.
    fn catalog_node<D: BlockDevice>(
        &self,
        dev: &mut D,
        oid: u64,
        expected_type: u32,
    ) -> Result<std::sync::Arc<BtreeNode>, ContainerError> {
        let mapped = self.cache.borrow().oids.get(&oid).copied();
        let (paddr, flags) = match mapped {
            Some(m) => m,
            None => {
                let m = self
                    .omap
                    .resolve_with_flags(dev, oid, self.xid, self.block_size)?
                    .ok_or_else(oor)?;
                self.cache.borrow_mut().oids.insert(oid, m);
                m
            }
        };
        if let Some(n) = self.cache.borrow().nodes.get(&paddr) {
            return Ok(n.clone());
        }
        let node = std::sync::Arc::new(load_catalog_node(
            dev,
            paddr,
            flags,
            self.block_size,
            expected_type,
        )?);
        self.cache.borrow_mut().keep(paddr, node.clone());
        Ok(node)
    }

    /// The file-extent tree node at physical `paddr`, from the cache when
    /// this catalog has read it before.
    fn fext_node<D: BlockDevice>(
        &self,
        dev: &mut D,
        paddr: u64,
    ) -> Result<std::sync::Arc<BtreeNode>, ContainerError> {
        if let Some(n) = self.cache.borrow().nodes.get(&paddr) {
            return Ok(n.clone());
        }
        let node = std::sync::Arc::new(read_fext_node(
            dev,
            paddr,
            self.block_size,
            OBJECT_TYPE_BTREE_NODE,
        )?);
        self.cache.borrow_mut().keep(paddr, node.clone());
        Ok(node)
    }

    /// The extents of data stream `stream_id`, in logical order: the
    /// FILE_EXTENT records keyed by it, or on a sealed volume the records of
    /// the file-extent tree. A hole has physical block 0.
    fn stream_extents<D: BlockDevice>(
        &self,
        dev: &mut D,
        stream_id: u64,
    ) -> Result<Vec<FileExtent>, ContainerError> {
        self.stream_extents_in(dev, stream_id, 0, u64::MAX)
    }

    /// The extents of data stream `stream_id` that hold bytes of
    /// `from..to`, in logical order. Extents are keyed by their logical
    /// address, so only the leaves that can hold those are read: a small
    /// read of a file edited in many places reads one path down the tree,
    /// not the file's whole extent list.
    fn stream_extents_in<D: BlockDevice>(
        &self,
        dev: &mut D,
        stream_id: u64,
        from: u64,
        to: u64,
    ) -> Result<Vec<FileExtent>, ContainerError> {
        let mut out: Vec<FileExtent> = Vec::new();
        let bsz = u64::from(self.block_size);
        let mut push = |logical_addr: u64, v: &[u8]| -> Result<(), ContainerError> {
            let len_and_flags = v
                .get(0..8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(oor)?;
            // An extent covers whole blocks from a block boundary; one over
            // no blocks, or cut mid-block, is no extent macOS writes.
            // [linux-apfs-rw extents.c: "invalid length"; apfsck checks both]
            let len = len_and_flags & FILE_EXTENT_LEN_MASK;
            if len == 0 || bsz == 0 || !len.is_multiple_of(bsz) || !logical_addr.is_multiple_of(bsz)
            {
                return Err(ContainerError::Parse(ParseError::BadField {
                    field: "file extent length or offset",
                    value: len,
                }));
            }
            let phys_block_num = v
                .get(8..16)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(oor)?;
            out.push(FileExtent {
                logical_addr,
                len: len_and_flags & FILE_EXTENT_LEN_MASK,
                flags: (len_and_flags >> 56) as u8,
                phys_block_num,
            });
            Ok(())
        };
        match &self.sealed {
            Some(s) => {
                let span = (stream_id, from, to);
                self.for_each_fext(dev, &s.fext_root, span, 0, &mut |logical, v| {
                    push(logical, v)
                })?;
            }
            None => {
                // Extents do not overlap, so the one holding byte `from`
                // starts in the child whose range takes in `from`; a child
                // that ends below it holds none of them.
                let range = (
                    (stream_id, APFS_TYPE_FILE_EXTENT, from),
                    (stream_id, APFS_TYPE_FILE_EXTENT, to),
                );
                self.walk(dev, &self.root, 0, Some(range), &mut |k, v| {
                    let jk = JKey::parse(k)?;
                    if jk.obj_type != APFS_TYPE_FILE_EXTENT || jk.obj_id != stream_id {
                        return Ok(());
                    }
                    let logical_addr = k
                        .get(8..16)
                        .and_then(|b| b.try_into().ok())
                        .map(u64::from_le_bytes)
                        .ok_or_else(oor)?;
                    push(logical_addr, v)
                })?;
            }
        }
        // The leaves read hold neighbours of the range as well.
        out.retain(|e| e.logical_addr < to && e.logical_addr.saturating_add(e.len) > from);
        out.sort_by_key(|e| e.logical_addr);
        Ok(out)
    }

    /// Calls `f(logical_addr, value)` for the records of stream `span.0` in
    /// the file-extent tree below `node` that can hold bytes of
    /// `span.1..span.2` (and their neighbours in the leaves read). Records
    /// are ordered by `(private_id, logical_addr)`, so only the children
    /// whose key range can hold those are read: child `i` holds keys from
    /// its own key up to the next child's.
    #[allow(clippy::type_complexity)]
    fn for_each_fext<D: BlockDevice>(
        &self,
        dev: &mut D,
        node: &BtreeNode,
        span: (u64, u64, u64),
        depth: u8,
        f: &mut dyn FnMut(u64, &[u8]) -> Result<(), ContainerError>,
    ) -> Result<(), ContainerError> {
        let (stream_id, from, to) = span;
        if depth > MAX_CATALOG_TREE_DEPTH {
            return Err(ContainerError::Parse(ParseError::BadField {
                field: "fext tree depth",
                value: u64::from(depth),
            }));
        }
        let is_leaf = node.is_leaf();
        if is_leaf != (node.level == 0) {
            return Err(bad_level(node.level).into());
        }
        if is_leaf {
            for i in 0..node.nkeys {
                let (k, v) = node.fixed_kv(i, FEXT_RECORD_SIZE, FEXT_RECORD_SIZE)?;
                let (id, logical) = fext_key(k)?;
                if id == stream_id {
                    f(logical, v)?;
                }
            }
            return Ok(());
        }
        for i in 0..node.nkeys {
            let (k, v) = node.fixed_kv(i, FEXT_RECORD_SIZE, PHYSICAL_CHILD_SIZE)?;
            if fext_key(k)? > (stream_id, to) {
                break;
            }
            let next = i.checked_add(1).filter(|&n| n < node.nkeys);
            if let Some(n) = next {
                let (nk, _) = node.fixed_kv(n, FEXT_RECORD_SIZE, PHYSICAL_CHILD_SIZE)?;
                // The next child starts at or before the first record that
                // can hold byte `from` (extents do not overlap): nothing
                // wanted is here.
                if fext_key(nk)? <= (stream_id, from) {
                    continue;
                }
            }
            let child_paddr = v
                .get(0..8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(oor)?;
            let child = self.fext_node(dev, child_paddr)?;
            if child.level.checked_add(1) != Some(node.level) {
                return Err(bad_level(child.level).into());
            }
            self.for_each_fext(dev, &child, span, depth.saturating_add(1), f)?;
        }
        Ok(())
    }

    /// List the directory entries whose parent inode == `dir_inode`.
    /// `names_hashed` selects the j_drec key variant (see
    /// VolumeSuperblock::names_are_hashed).
    pub fn list_dir<D: BlockDevice>(
        &self,
        dev: &mut D,
        dir_inode: u64,
        names_hashed: bool,
    ) -> Result<Vec<DirEntry>, ContainerError> {
        let mut out: Vec<DirEntry> = Vec::new();
        self.for_each_record_of(dev, dir_inode, APFS_TYPE_DIR_REC, &mut |k, v| {
            let jk = JKey::parse(k)?;
            if jk.obj_type != APFS_TYPE_DIR_REC || jk.obj_id != dir_inode {
                return Ok(());
            }
            out.push(parse_dir_entry(k, v, names_hashed)?);
            Ok(())
        })?;
        Ok(out)
    }

    /// The entries of directory `dir_inode` whose stored name hash is
    /// `hash`, on a volume that hashes its names. Reads one path down the
    /// tree to them instead of the whole directory, so finding one name
    /// costs the same in a directory of ten entries as of ten thousand. The
    /// caller compares the names the way the volume does.
    pub fn find_dir_entries<D: BlockDevice>(
        &self,
        dev: &mut D,
        dir_inode: u64,
        hash: u32,
    ) -> Result<Vec<DirEntry>, ContainerError> {
        let pos = (dir_inode, APFS_TYPE_DIR_REC, u64::from(hash));
        let mut out: Vec<DirEntry> = Vec::new();
        self.walk(dev, &self.root, 0, Some((pos, pos)), &mut |k, v| {
            if key_pos(k)? == pos {
                out.push(parse_dir_entry(k, v, true)?);
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// Resolve a slash-free path given as components (e.g. ["a","b","c"])
    /// to its inode number by descending `list_dir` from the root directory.
    /// Empty `components` → the root inode. Returns `Ok(None)` if any
    /// component is absent. Does not follow symlinks.
    pub fn resolve_path<D: BlockDevice>(
        &self,
        dev: &mut D,
        components: &[&str],
        names_hashed: bool,
    ) -> Result<Option<u64>, ContainerError> {
        let mut ino = crate::jkey::ROOT_DIR_INO_NUM;
        for comp in components {
            let entries = self.list_dir(dev, ino, names_hashed)?;
            match entries.iter().find(|e| e.name == *comp) {
                Some(e) => ino = e.file_id,
                None => return Ok(None),
            }
        }
        Ok(Some(ino))
    }

    /// Assemble the byte content of a data stream identified by `stream_oid`
    /// (a file's private_id, or an xattr's xattr_obj_id): gather the
    /// APFS_TYPE_FILE_EXTENT records keyed by that oid, order by
    /// logical_addr, and truncate to `size`. Uncovered ranges read as zeros.
    fn read_stream<D: BlockDevice>(
        &self,
        dev: &mut D,
        stream_oid: u64,
        size: usize,
    ) -> Result<Vec<u8>, ContainerError> {
        self.read_stream_range(dev, stream_oid, size, 0, size)
    }

    /// Bytes `offset..offset+len` of the stream (clamped to `size`), reading
    /// from the device only the parts of the extents they touch, so a read
    /// costs what it asks for and not the file's length.
    fn read_stream_range<D: BlockDevice>(
        &self,
        dev: &mut D,
        stream_oid: u64,
        size: usize,
        offset: usize,
        len: usize,
    ) -> Result<Vec<u8>, ContainerError> {
        // A data stream cannot be larger than the device it lives on. Reject an
        // implausible on-disk dstream/xattr size before allocating the output
        // buffer, so a crafted size (up to ~16 EiB) cannot drive an OOM.
        if size as u64 > dev.size() {
            return Err(oor());
        }
        if offset >= size || len == 0 {
            return Ok(Vec::new());
        }
        let want_total = len.min(size - offset);
        let range_end = offset + want_total;
        let extents: Vec<(u64, u64, u64)> = self
            .stream_extents_in(dev, stream_oid, offset as u64, range_end as u64)?
            .into_iter()
            .map(|e| (e.logical_addr, e.len, e.phys_block_num))
            .collect();
        let mut out = vec![0u8; want_total];
        for (logical, extent_len, paddr) in extents {
            let Ok(logical) = usize::try_from(logical) else {
                continue;
            };
            // The part of this extent inside the requested range. Clamping
            // to the range (which is within `size`) is also what keeps a
            // crafted extent length (up to ~72 PB) from driving an OOM.
            let extent_end =
                logical.saturating_add(usize::try_from(extent_len).unwrap_or(usize::MAX));
            let start = logical.max(offset);
            let end = extent_end.min(range_end);
            if start >= end {
                continue;
            }
            // paddr == 0 encodes a sparse hole (APFS file_extent paddr=0
            // semantics from the APFS specification). The output buffer is
            // already zero-filled, so no disk read is required.
            if paddr == 0 {
                continue;
            }
            // Whole blocks around the part wanted: a raw disk node takes
            // only requests of whole sectors, and the tail of a file or a
            // piece of a compressed stream is rarely one.
            let bsz = u64::from(self.block_size);
            let byte_offset = paddr
                .checked_mul(bsz)
                .and_then(|b| b.checked_add((start - logical) as u64))
                .ok_or_else(oor)?;
            let lo = byte_offset / bsz * bsz;
            let hi = byte_offset
                .checked_add((end - start) as u64)
                .map(|e| e.div_ceil(bsz) * bsz)
                .ok_or_else(oor)?;
            let mut tmp = vec![0u8; usize::try_from(hi - lo).map_err(|_| oor())?];
            dev.read_content_at(lo, &mut tmp)?;
            let skip = (byte_offset - lo) as usize;
            out.get_mut(start - offset..end - offset)
                .ok_or_else(oor)?
                .copy_from_slice(tmp.get(skip..skip + (end - start)).ok_or_else(oor)?);
        }
        Ok(out)
    }

    /// Read the full content of the regular file's data stream (a compressed
    /// file's stream is empty; see `read_file_decompressed`).
    pub fn read_file<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
    ) -> Result<Vec<u8>, ContainerError> {
        self.read_file_range(dev, inode_num, 0, usize::MAX)
    }

    /// Bytes `offset..offset+len` of the file's data stream, clamped to its
    /// size; only the extents they touch are read.
    pub fn read_file_range<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
        offset: usize,
        len: usize,
    ) -> Result<Vec<u8>, ContainerError> {
        use crate::jkey::APFS_TYPE_INODE;

        let mut inode_val: Option<Vec<u8>> = None;
        self.for_each_record_of(dev, inode_num, APFS_TYPE_INODE, &mut |k, v| {
            if inode_val.is_none() {
                let jk = JKey::parse(k)?;
                if jk.obj_type == APFS_TYPE_INODE && jk.obj_id == inode_num {
                    inode_val = Some(v.to_vec());
                }
            }
            Ok(())
        })?;
        let inode = Inode::parse(&inode_val.ok_or_else(oor)?)?;
        let size = inode.dstream.as_ref().map(|d| d.size).unwrap_or(0) as usize;
        self.read_stream_range(dev, inode.private_id, size, offset, len)
    }

    /// Stat an inode WITHOUT reading file content: `is_dir` from the inode
    /// mode, `size` = the com.apple.decmpfs uncompressed_size if the file is
    /// decmpfs-compressed, else the data stream size (0 if none).
    pub fn stat<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
    ) -> Result<InodeStat, ContainerError> {
        use crate::inode::Inode;
        use crate::jkey::APFS_TYPE_INODE;

        let mut inode_val: Option<Vec<u8>> = None;
        self.for_each_record_of(dev, inode_num, APFS_TYPE_INODE, &mut |k, v| {
            if inode_val.is_none() {
                let jk = JKey::parse(k)?;
                if jk.obj_type == APFS_TYPE_INODE && jk.obj_id == inode_num {
                    inode_val = Some(v.to_vec());
                }
            }
            Ok(())
        })?;
        let inode = Inode::parse(&inode_val.ok_or_else(oor)?)?;
        let is_dir = inode.mode & 0o170000 == 0o040000; // S_IFMT / S_IFDIR
        let size = if !is_dir && inode.is_compressed() {
            self.compressed_size(dev, inode_num, &inode)?
        } else {
            inode.dstream.as_ref().map(|s| s.size).unwrap_or(0)
        };
        Ok(InodeStat {
            size,
            is_dir,
            nchildren_or_nlink: inode.nchildren_or_nlink,
            mode: inode.mode,
            bsd_flags: inode.bsd_flags,
            create_time: inode.create_time,
            mod_time: inode.mod_time,
            change_time: inode.change_time,
            access_time: inode.access_time,
        })
    }

    /// List the extended attributes (name + flags) of the given inode.
    pub fn list_xattrs<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
    ) -> Result<Vec<crate::xattr::XattrEntry>, ContainerError> {
        use crate::jkey::APFS_TYPE_XATTR;
        use crate::xattr::{xattr_key_name, xattr_val, XattrEntry};

        let mut out: Vec<XattrEntry> = Vec::new();
        self.for_each_record_of(dev, inode_num, APFS_TYPE_XATTR, &mut |k, v| {
            let jk = JKey::parse(k)?;
            if jk.obj_type == APFS_TYPE_XATTR && jk.obj_id == inode_num {
                let name = xattr_key_name(k)?;
                let (flags, _xdata) = xattr_val(v)?;
                out.push(XattrEntry { name, flags });
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// The parsed inode record of `inode_num`, or `None` when there is none.
    pub fn inode_record<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
    ) -> Result<Option<crate::inode::Inode>, ContainerError> {
        use crate::jkey::APFS_TYPE_INODE;

        let mut inode_val: Option<Vec<u8>> = None;
        self.for_each_record_of(dev, inode_num, APFS_TYPE_INODE, &mut |k, v| {
            if inode_val.is_none() {
                let jk = JKey::parse(k)?;
                if jk.obj_type == APFS_TYPE_INODE && jk.obj_id == inode_num {
                    inode_val = Some(v.to_vec());
                }
            }
            Ok(())
        })?;
        match inode_val {
            Some(val) => Ok(Some(crate::inode::Inode::parse(&val)?)),
            None => Ok(None),
        }
    }

    /// The extents of `inode_num`'s data stream, in logical order: the records
    /// keyed by the stream id the inode names. A hole has physical block 0.
    pub fn file_extents<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
    ) -> Result<Vec<FileExtent>, ContainerError> {
        let Some(inode) = self.inode_record(dev, inode_num)? else {
            return Ok(Vec::new());
        };
        let stream_id = if inode.private_id == 0 {
            inode_num
        } else {
            inode.private_id
        };
        self.stream_extents(dev, stream_id)
    }

    /// What the inode says about its own primary link: the parent directory
    /// and name recorded in the inode record itself.
    ///
    /// For a file with one name this is that name. For a file with several,
    /// it has to be the one with the lowest sibling id, and it has to be kept
    /// that way when that name is renamed, moved or removed.
    pub fn primary_link<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
    ) -> Result<Option<PrimaryLink>, ContainerError> {
        use crate::inode::Inode;
        use crate::jkey::APFS_TYPE_INODE;

        let mut inode_val: Option<Vec<u8>> = None;
        self.for_each_record_of(dev, inode_num, APFS_TYPE_INODE, &mut |k, v| {
            if inode_val.is_none() {
                let jk = JKey::parse(k)?;
                if jk.obj_type == APFS_TYPE_INODE && jk.obj_id == inode_num {
                    inode_val = Some(v.to_vec());
                }
            }
            Ok(())
        })?;
        let Some(val) = inode_val else {
            return Ok(None);
        };
        let inode = Inode::parse(&val)?;
        Ok(inode.name.map(|name| PrimaryLink {
            parent_id: inode.parent_id,
            name,
        }))
    }

    /// Whether an attribute's value is a stream, from its flags. Exactly one
    /// of the stream and embedded bits is set on a well-formed record; a
    /// value claiming neither form or both is not read either way, as
    /// linux-apfs-rw refuses it.
    fn xattr_value_is_stream(flags: u16) -> Result<bool, ContainerError> {
        use crate::xattr::{XATTR_DATA_EMBEDDED, XATTR_DATA_STREAM};
        match flags & (XATTR_DATA_STREAM | XATTR_DATA_EMBEDDED) {
            XATTR_DATA_STREAM => Ok(true),
            XATTR_DATA_EMBEDDED => Ok(false),
            _ => Err(ContainerError::Parse(crate::endian::ParseError::BadMagic {
                expected: u32::from(XATTR_DATA_STREAM | XATTR_DATA_EMBEDDED),
                found: u32::from(flags),
            })),
        }
    }

    /// Every data-stream-id record in the volume as `(stream id, reference
    /// count)`: one per shared stream a writer recorded.
    pub fn dstream_ids<D: BlockDevice>(
        &self,
        dev: &mut D,
    ) -> Result<Vec<(u64, u32)>, ContainerError> {
        use crate::jkey::APFS_TYPE_DSTREAM_ID;

        let mut out: Vec<(u64, u32)> = Vec::new();
        self.for_each_leaf(dev, &self.root, 0, &mut |k, v| {
            let jk = JKey::parse(k)?;
            if jk.obj_type != APFS_TYPE_DSTREAM_ID {
                return Ok(());
            }
            let refcnt = v
                .get(0..4)
                .and_then(|b| b.try_into().ok())
                .map(u32::from_le_bytes)
                .ok_or_else(oor)?;
            out.push((jk.obj_id, refcnt));
            Ok(())
        })?;
        out.sort_unstable();
        Ok(out)
    }

    /// Every sibling-map record in the volume as `(sibling id, inode number)`.
    ///
    /// A sibling map is keyed by the sibling id (not by the inode), so it is
    /// the one hard-link record a removal can leave behind unnoticed; listing
    /// them all lets a check pair each with a living link.
    pub fn sibling_maps<D: BlockDevice>(
        &self,
        dev: &mut D,
    ) -> Result<Vec<(u64, u64)>, ContainerError> {
        use crate::jkey::APFS_TYPE_SIBLING_MAP;

        let mut out: Vec<(u64, u64)> = Vec::new();
        self.for_each_leaf(dev, &self.root, 0, &mut |k, v| {
            let jk = JKey::parse(k)?;
            if jk.obj_type != APFS_TYPE_SIBLING_MAP {
                return Ok(());
            }
            let file_id = v
                .get(0..8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(oor)?;
            out.push((jk.obj_id, file_id));
            Ok(())
        })?;
        out.sort_unstable();
        Ok(out)
    }

    /// The names bound to `inode_num` by sibling-link records.
    ///
    /// A file with a single name carries none of these, so an empty result is
    /// the normal case; a file reached through several names has one record per
    /// name, each naming the directory it sits in. Ordered by sibling id, which
    /// is the order they were created in - the first is the one the inode's own
    /// name and parent fields describe.
    pub fn sibling_links<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
    ) -> Result<Vec<SiblingLink>, ContainerError> {
        use crate::jkey::APFS_TYPE_SIBLING_LINK;

        let mut out: Vec<SiblingLink> = Vec::new();
        self.for_each_record_of(dev, inode_num, APFS_TYPE_SIBLING_LINK, &mut |k, v| {
            let jk = JKey::parse(k)?;
            if jk.obj_type != APFS_TYPE_SIBLING_LINK || jk.obj_id != inode_num {
                return Ok(());
            }
            let sibling_id = k
                .get(8..16)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(oor)?;
            let parent_id = v
                .get(0..8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(oor)?;
            let name_len = v
                .get(8..10)
                .and_then(|b| b.try_into().ok())
                .map(u16::from_le_bytes)
                .ok_or_else(oor)? as usize;
            // The stored length counts the NUL terminator.
            let raw = v.get(10..10 + name_len).ok_or_else(oor)?;
            let bytes = raw.split(|&b| b == 0).next().unwrap_or(raw);
            out.push(SiblingLink {
                sibling_id,
                parent_id,
                name: String::from_utf8_lossy(bytes).into_owned(),
            });
            Ok(())
        })?;
        out.sort_by_key(|l| l.sibling_id);
        Ok(out)
    }

    /// Read one extended attribute's raw bytes by name. Embedded values are
    /// returned directly; data-stream values are assembled from the xattr's
    /// FILE_EXTENT records (keyed by xattr_obj_id) and truncated to the
    /// data stream's size. Returns Ok(None) if the attribute is absent.
    /// (Raw bytes only - decmpfs interpretation is M3d.)
    pub fn get_xattr<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
        name: &str,
    ) -> Result<Option<Vec<u8>>, ContainerError> {
        use crate::xattr::XattrDstream;

        let (flags, xdata) = match self.find_xattr(dev, inode_num, name)? {
            Some(h) => h,
            None => return Ok(None),
        };
        if Self::xattr_value_is_stream(flags)? {
            let ds = XattrDstream::parse(&xdata)?;
            Ok(Some(self.read_stream(
                dev,
                ds.xattr_obj_id,
                ds.size as usize,
            )?))
        } else {
            Ok(Some(xdata))
        }
    }

    /// The flags of the attribute record `name` on `inode_num` as stored
    /// (`XATTR_DATA_STREAM`, `XATTR_DATA_EMBEDDED`, `XATTR_FILE_SYSTEM_OWNED`),
    /// or `Ok(None)` when there is no such attribute.
    pub fn xattr_flags<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
        name: &str,
    ) -> Result<Option<u16>, ContainerError> {
        Ok(self
            .find_xattr(dev, inode_num, name)?
            .map(|(flags, _)| flags))
    }

    /// The attribute's record as stored: its flags and value field, which is
    /// the data itself or, for a data-stream attribute, the stream descriptor.
    fn find_xattr<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
        name: &str,
    ) -> Result<Option<(u16, Vec<u8>)>, ContainerError> {
        use crate::jkey::APFS_TYPE_XATTR;
        use crate::xattr::{xattr_key_name, xattr_val};

        let mut hit: Option<(u16, Vec<u8>)> = None;
        self.for_each_record_of(dev, inode_num, APFS_TYPE_XATTR, &mut |k, v| {
            if hit.is_none() {
                let jk = JKey::parse(k)?;
                if jk.obj_type == APFS_TYPE_XATTR
                    && jk.obj_id == inode_num
                    && xattr_key_name(k)? == name
                {
                    let (flags, xdata) = xattr_val(v)?;
                    hit = Some((flags, xdata.to_vec()));
                }
            }
            Ok(())
        })?;
        Ok(hit)
    }

    /// The logical size of a compressed file: the size the kernel recorded in
    /// the inode when it did, else the decmpfs header's. A file flagged
    /// compressed whose header is missing or unreadable has no size, not
    /// size zero: a copy that sees zero copies nothing and reports success.
    fn compressed_size<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
        inode: &Inode,
    ) -> Result<u64, ContainerError> {
        if let Some(size) = inode.recorded_uncompressed_size() {
            return Ok(size);
        }
        let attr = self
            .get_xattr(dev, inode_num, DECMPFS_ATTRIBUTE)?
            .ok_or(DecmpfsError::MissingAttribute)?;
        Ok(DecmpfsHeader::parse(&attr)?.uncompressed_size)
    }

    /// Read a file's logical content, transparently decompressing a file
    /// flagged `UF_COMPRESSED`. The flag decides, as it does for macOS: a
    /// decmpfs attribute on a file without the flag is left alone and the data
    /// stream is served. Any failure to produce the content keeps its cause.
    pub fn read_file_decompressed<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
    ) -> Result<Vec<u8>, ContainerError> {
        self.read_file_decompressed_range(dev, inode_num, 0, usize::MAX)
    }

    /// Bytes `offset..offset+len` of the file's logical content, clamped to
    /// its size. A plain file's extents are read only where the range touches
    /// them; a compressed file's resource fork is read a chunk at a time, only
    /// for the chunks the range covers, so the cost of a read is the read, not
    /// the file.
    pub fn read_file_decompressed_range<D: BlockDevice>(
        &self,
        dev: &mut D,
        inode_num: u64,
        offset: usize,
        len: usize,
    ) -> Result<Vec<u8>, ContainerError> {
        use crate::decmpfs::decompress_range;
        use crate::xattr::XattrDstream;

        let inode = self.inode_record(dev, inode_num)?.ok_or_else(oor)?;
        if !inode.is_compressed() {
            // The same as `read_file_range`, without looking the inode up
            // a second time: this runs for every read the mount answers.
            let size = inode.dstream.as_ref().map(|d| d.size).unwrap_or(0) as usize;
            return self.read_stream_range(dev, inode.private_id, size, offset, len);
        }
        let attr = self
            .get_xattr(dev, inode_num, DECMPFS_ATTRIBUTE)?
            .ok_or(DecmpfsError::MissingAttribute)?;
        if !DecmpfsHeader::parse(&attr)?.uses_resource_fork() {
            let mut none: &[u8] = &[];
            return Ok(decompress_range(&attr, 0, &mut none, offset, len)?);
        }
        let Some((flags, xdata)) = self.find_xattr(dev, inode_num, RESOURCE_FORK_ATTRIBUTE)? else {
            return Err(DecmpfsError::MissingResourceFork.into());
        };
        if Self::xattr_value_is_stream(flags)? {
            let ds = XattrDstream::parse(&xdata)?;
            let size = usize::try_from(ds.size).map_err(|_| oor())?;
            let mut stream = XattrStream {
                catalog: self,
                dev,
                oid: ds.xattr_obj_id,
                size,
            };
            decompress_range(&attr, size, &mut stream, offset, len)
        } else {
            let mut inline: &[u8] = &xdata;
            Ok(decompress_range(
                &attr,
                xdata.len(),
                &mut inline,
                offset,
                len,
            )?)
        }
    }
}

/// Resource-fork bytes read on demand from the attribute's data stream.
struct XattrStream<'a, D: BlockDevice> {
    catalog: &'a Catalog,
    dev: &'a mut D,
    oid: u64,
    size: usize,
}

impl<D: BlockDevice> crate::decmpfs::ForkBytes for XattrStream<'_, D> {
    type Error = ContainerError;
    fn read(&mut self, offset: usize, len: usize) -> Result<Vec<u8>, ContainerError> {
        let bytes = self
            .catalog
            .read_stream_range(self.dev, self.oid, self.size, offset, len)?;
        if bytes.len() != len {
            return Err(DecmpfsError::Parse(ParseError::Short {
                at: offset,
                need: len,
                len: self.size,
            })
            .into());
        }
        Ok(bytes)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]
mod tests {
    use super::*;
    use crate::block_device::{BlockError, FileBlockDevice};
    use crate::btree::{BtreeNode, BTNODE_LEAF};
    use crate::checksum::fletcher64;
    use crate::container::Container;
    use crate::jkey::{
        APFS_TYPE_FILE_EXTENT, APFS_TYPE_INODE, OBJ_ID_MASK, OBJ_TYPE_SHIFT, ROOT_DIR_INO_NUM,
    };
    use crate::obj::OBJECT_TYPE_BTREE;
    use crate::omap::{Omap, OmapPhys};
    use std::path::Path;

    // -----------------------------------------------------------------------
    // Regression test: W4-BUG-1 - sparse file hole (paddr=0) must return
    // zeros, not the NX container superblock bytes from physical block 0.
    // -----------------------------------------------------------------------

    /// Minimal in-memory block device for unit tests.
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

        /// Fill block at `paddr` with `byte` repeated for the full block.
        fn fill_block(&mut self, paddr: u64, byte: u8) {
            let data = vec![byte; self.block_size];
            self.blocks.insert(paddr * self.block_size as u64, data);
        }

        /// Place a whole block at `paddr`.
        fn insert_block(&mut self, paddr: u64, data: Vec<u8>) {
            self.blocks.insert(paddr * self.block_size as u64, data);
        }

        /// Write arbitrary bytes at byte `offset` (used to plant NX magic).
        fn write_bytes(&mut self, offset: u64, data: &[u8]) {
            let bsz = self.block_size;
            let block_offset = (offset / bsz as u64) * bsz as u64;
            let entry = self
                .blocks
                .entry(block_offset)
                .or_insert_with(|| vec![0u8; bsz]);
            let within = (offset - block_offset) as usize;
            let end = (within + data.len()).min(bsz);
            entry[within..end].copy_from_slice(&data[..end - within]);
        }
    }

    impl BlockDevice for MemDev {
        fn size(&self) -> u64 {
            // Return a nominal size covering at least 64 blocks for test purposes.
            self.block_size as u64 * 64
        }

        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let bsz = self.block_size as u64;
            let block_base = (offset / bsz) * bsz;
            if let Some(data) = self.blocks.get(&block_base) {
                let within = (offset - block_base) as usize;
                let len = buf.len().min(data.len().saturating_sub(within));
                buf[..len].copy_from_slice(&data[within..within + len]);
                Ok(())
            } else {
                buf.fill(0);
                Ok(())
            }
        }
    }

    /// Build a minimal var-kv leaf BtreeNode (validated checksum) containing
    /// `nkeys` FILE_EXTENT records for `stream_oid`. Each entry is:
    ///   `(logical_offset, byte_len, paddr)`.
    ///
    /// Layout (4 KiB block):
    ///   - DATA_BASE = 56
    ///   - toc_off = 0, toc_len = nkeys * 8 (kvloc_t per entry)
    ///   - key area starts at DATA_BASE + toc_len
    ///   - values grow backwards from byte 4095 (leaf, not root → no btree_info)
    fn build_extent_leaf(
        stream_oid: u64,
        extents: &[(u64, u64, u64)], // (logical, len, paddr)
    ) -> Vec<u8> {
        let bsz = 4096usize;
        let data_base: usize = 56;
        let toc_len = extents.len() * 8; // one kvloc_t per entry
        let key_area_start = data_base + toc_len;
        let key_size = 16usize; // 8-byte j_key_t + 8-byte logical_addr
        let val_size = 16usize; // 8-byte len_and_flags + 8-byte paddr
        let val_area_end = bsz; // leaf (not root) → no btree_info_t reservation

        let mut b = vec![0u8; bsz];

        // obj_phys header fields (object_type = BTREE at offset 24)
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        // btn_flags: BTNODE_LEAF (var-kv, not FIXED_KV)
        b[32..34].copy_from_slice(&BTNODE_LEAF.to_le_bytes());
        // btn_level = 0
        b[34..36].copy_from_slice(&0u16.to_le_bytes());
        // btn_nkeys
        b[36..40].copy_from_slice(&(extents.len() as u32).to_le_bytes());
        // btn_table_space: off=0, len=toc_len
        b[40..42].copy_from_slice(&0u16.to_le_bytes()); // toc_off
        b[42..44].copy_from_slice(&(toc_len as u16).to_le_bytes()); // toc_len

        for (i, &(logical, len, paddr)) in extents.iter().enumerate() {
            // Build j_file_extent_key_t: 8-byte j_key_t + 8-byte logical_addr.
            // j_key_t = (obj_id & OBJ_ID_MASK) | (type << 60)
            let jkey_word =
                (stream_oid & OBJ_ID_MASK) | ((APFS_TYPE_FILE_EXTENT as u64) << OBJ_TYPE_SHIFT);
            let mut key = [0u8; 16];
            key[0..8].copy_from_slice(&jkey_word.to_le_bytes());
            key[8..16].copy_from_slice(&logical.to_le_bytes());

            // j_file_extent_val_t: len_and_flags (low 56 bits = byte_len) + paddr.
            let mut val = [0u8; 16];
            val[0..8].copy_from_slice(&(len & 0x00ff_ffff_ffff_ffff).to_le_bytes());
            val[8..16].copy_from_slice(&paddr.to_le_bytes());

            let k_off = i * key_size; // relative to key_area_start
            let v_abs = val_area_end - (i + 1) * val_size; // absolute byte offset
            let v_off = val_area_end - v_abs; // TOC encodes distance back from val_area_end

            // Write kvloc_t into TOC
            let toc_entry = data_base + i * 8;
            b[toc_entry..toc_entry + 2].copy_from_slice(&(k_off as u16).to_le_bytes());
            b[toc_entry + 2..toc_entry + 4].copy_from_slice(&(key_size as u16).to_le_bytes());
            b[toc_entry + 4..toc_entry + 6].copy_from_slice(&(v_off as u16).to_le_bytes());
            b[toc_entry + 6..toc_entry + 8].copy_from_slice(&(val_size as u16).to_le_bytes());

            // Write key bytes
            let k_start = key_area_start + k_off;
            b[k_start..k_start + key_size].copy_from_slice(&key);

            // Write value bytes at absolute position
            b[v_abs..v_abs + val_size].copy_from_slice(&val);
        }

        // Compute and store Fletcher-64 checksum (obj_phys.cksum at offset 0).
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        b
    }

    /// Build a dummy Omap backed by a trivial leaf node. Its `resolve` method
    /// will never be called because `read_stream` only calls `for_each_leaf`
    /// on `self.root` which is a leaf - no omap resolution needed.
    fn dummy_omap() -> Omap {
        // We need a parseable BtreeNode for Omap::root. Build a minimal
        // FIXED_KV leaf with 0 entries using the same layout as btree tests.
        let bsz = 4096usize;
        let mut b = vec![0u8; bsz];
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        use crate::btree::BTNODE_FIXED_KV_SIZE;
        b[32..34].copy_from_slice(&(BTNODE_LEAF | BTNODE_FIXED_KV_SIZE).to_le_bytes());
        b[34..36].copy_from_slice(&0u16.to_le_bytes()); // level
        b[36..40].copy_from_slice(&0u32.to_le_bytes()); // nkeys = 0
        b[40..42].copy_from_slice(&0u16.to_le_bytes()); // toc_off
        b[42..44].copy_from_slice(&0u16.to_le_bytes()); // toc_len
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        let root = BtreeNode::parse(&b).expect("dummy omap root");

        // Build a dummy OmapPhys (parse from a valid block to satisfy the
        // parse path, but we won't call resolve so the content is irrelevant).
        // Use a raw block with OBJECT_TYPE_OMAP and valid Fletcher-64.
        use crate::obj::{OBJECT_TYPE_BTREE, OBJECT_TYPE_OMAP};
        let mut ob = vec![0u8; bsz];
        ob[24..28].copy_from_slice(&OBJECT_TYPE_OMAP.to_le_bytes());
        // The map's tree is a physical B-tree, as the parser requires.
        ob[40..44].copy_from_slice(&(OBJECT_TYPE_BTREE | 0x4000_0000).to_le_bytes());
        let ck2 = fletcher64(&ob);
        ob[0..8].copy_from_slice(&ck2.to_le_bytes());
        let phys = OmapPhys::parse(&ob).expect("dummy OmapPhys");

        Omap { phys, root }
    }

    // W4-BUG-1 regression: a sparse extent (paddr=0) must yield zero bytes,
    // not bytes from physical block 0 (the NX container superblock).
    #[test]
    fn sparse_hole_reads_as_zeros_not_nx_superblock() {
        const BSZ: u32 = 4096;
        const STREAM_OID: u64 = 100;
        const NX_MAGIC: &[u8] = b"BSXN"; // NXSB in little-endian bytes as seen on disk

        // Two extents: first is a sparse hole (paddr=0), second has real data.
        let raw_root = build_extent_leaf(
            STREAM_OID,
            &[
                (0, BSZ as u64, 0),          // sparse hole: paddr=0
                (BSZ as u64, BSZ as u64, 5), // real data at paddr=5
            ],
        );

        // Device: block 0 holds NX superblock magic + 0xDE filler,
        // block 5 holds all 0xCC (the "real file content").
        let mut dev = MemDev::new(BSZ as usize);
        // Plant NX magic at physical offset 0 so the pre-fix code would
        // corrupt the output buffer with it.
        dev.fill_block(0, 0xDE);
        dev.write_bytes(0, NX_MAGIC);
        // Plant real file content at block 5.
        dev.fill_block(5, 0xCC);

        let root = BtreeNode::parse(&raw_root).expect("parse root node");
        let cat = Catalog {
            omap: dummy_omap(),
            root,
            xid: 1,
            block_size: BSZ,
            sealed: None,
            cache: Default::default(),
        };

        let data = cat
            .read_stream(&mut dev, STREAM_OID, (BSZ as usize) * 2)
            .expect("read_stream");

        // Hole region (first block) must be all zeros - not NX superblock bytes.
        assert_eq!(data.len(), BSZ as usize * 2, "total size");
        assert!(
            data[..BSZ as usize].iter().all(|&b| b == 0),
            "sparse hole must be zero-filled; first non-zero at byte {:?}",
            data[..BSZ as usize].iter().position(|&b| b != 0)
        );
        // NX magic must never appear in the output.
        assert!(
            !data.windows(4).any(|w| w == NX_MAGIC),
            "NX superblock magic must not appear in sparse file read"
        );
        // Real data region (second block) must be 0xCC.
        assert!(
            data[BSZ as usize..].iter().all(|&b| b == 0xCC),
            "real extent region must contain file content bytes"
        );
    }

    // Malformed-image hardening: a crafted dstream size larger than the whole
    // device must be rejected, not allocated (without the guard the output
    // buffer alloc is vec![0u8; size] up to ~16 EiB = OOM/abort).
    #[test]
    fn read_stream_rejects_stream_larger_than_device() {
        const BSZ: u32 = 4096;
        const STREAM_OID: u64 = 100;
        let raw_root = build_extent_leaf(STREAM_OID, &[(0, BSZ as u64, 1)]);
        let mut dev = MemDev::new(BSZ as usize); // size() == 4096 * 64
        dev.fill_block(1, 0xCC);
        let root = BtreeNode::parse(&raw_root).expect("parse root node");
        let cat = Catalog {
            omap: dummy_omap(),
            root,
            xid: 1,
            block_size: BSZ,
            sealed: None,
            cache: Default::default(),
        };
        let r = cat.read_stream(&mut dev, STREAM_OID, usize::MAX);
        assert!(
            r.is_err(),
            "oversized stream size must be rejected, not allocated"
        );
    }

    // Malformed-image hardening: a single extent claiming a ~72 PB byte length
    // must not drive a per-extent allocation of that size; only the wanted
    // (remaining stream) bytes are allocated and read.
    /// An extent over no blocks, cut mid-block, or starting mid-block is
    /// refused rather than read.
    #[test]
    fn read_stream_refuses_a_partial_block_extent() {
        const BSZ: u32 = 4096;
        const STREAM_OID: u64 = 100;
        let mut dev = MemDev::new(BSZ as usize);
        dev.fill_block(5, 0xCC);
        for (logical, len) in [(0u64, 0u64), (0, 100), (10, 4096)] {
            let raw_root = build_extent_leaf(STREAM_OID, &[(logical, len, 5)]);
            let cat = Catalog {
                omap: dummy_omap(),
                root: BtreeNode::parse(&raw_root).expect("parse root node"),
                xid: 1,
                block_size: BSZ,
                sealed: None,
                cache: Default::default(),
            };
            assert!(
                cat.read_stream(&mut dev, STREAM_OID, BSZ as usize).is_err(),
                "extent ({logical}, {len}) must be refused"
            );
        }
    }

    #[test]
    fn read_stream_caps_oversized_extent_len() {
        const BSZ: u32 = 4096;
        const STREAM_OID: u64 = 100;
        // The largest whole-block length the low 56 bits can hold.
        let huge_len = 0x00ff_ffff_ffff_ffffu64 & !(u64::from(BSZ) - 1);
        let raw_root = build_extent_leaf(STREAM_OID, &[(0, huge_len, 5)]);
        let mut dev = MemDev::new(BSZ as usize);
        dev.fill_block(5, 0xCC);
        let root = BtreeNode::parse(&raw_root).expect("parse root node");
        let cat = Catalog {
            omap: dummy_omap(),
            root,
            xid: 1,
            block_size: BSZ,
            sealed: None,
            cache: Default::default(),
        };
        let data = cat
            .read_stream(&mut dev, STREAM_OID, BSZ as usize)
            .expect("read_stream must succeed without OOM");
        assert_eq!(data.len(), BSZ as usize);
        assert!(data.iter().all(|&b| b == 0xCC), "first block content");
    }

    #[test]
    fn lists_real_large_fixture_tree() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-large.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture --large`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let bsz = c.superblock.block_size;
        let vols = c.list_user_volumes(&mut dev).expect("list volumes");
        let vol = vols.first().expect("at least one user volume").clone();

        let cat = Catalog::open(&mut dev, &vol, bsz).expect("open catalog");
        let hashed = vol.names_are_hashed();
        let root = cat
            .list_dir(&mut dev, ROOT_DIR_INO_NUM, hashed)
            .expect("list root dir");

        let names: std::collections::BTreeSet<String> =
            root.iter().map(|e| e.name.clone()).collect();
        let expected_dirs: Vec<String> = (0..16).map(|d| format!("dir{d:02}")).collect();
        if !expected_dirs.iter().all(|d| names.contains(d)) {
            eprintln!(
                "skip: size-only fixture (hdiutil attach fell back) - \
                 re-run `cargo run -p xtask -- gen-fixture --large` on a macOS host. \
                 root names found: {names:?}"
            );
            return;
        }
        // All 16 created directories are present at root.
        for d in &expected_dirs {
            assert!(names.contains(d), "missing root dir {d}");
        }
        // dir00's children are exactly f000.bin..f063.bin.
        let dir00 = root
            .iter()
            .find(|e| e.name == "dir00")
            .expect("dir00 entry");
        let kids = cat
            .list_dir(&mut dev, dir00.file_id, hashed)
            .expect("list dir00");
        let kid_names: std::collections::BTreeSet<String> =
            kids.iter().map(|e| e.name.clone()).collect();
        for f in 0..64 {
            let want = format!("f{f:03}.bin");
            assert!(kid_names.contains(&want), "dir00 missing {want}");
        }
        assert!(
            kid_names.len() >= 64,
            "dir00 should have at least 64 files (M3d adds cmp_* samples), got {}",
            kid_names.len()
        );
    }

    #[test]
    fn reads_real_file_content_4096_a() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-large.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture --large`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let bsz = c.superblock.block_size;
        let vols = c.list_user_volumes(&mut dev).expect("list volumes");
        let vol = vols.first().expect("user volume").clone();
        let cat = Catalog::open(&mut dev, &vol, bsz).expect("open catalog");
        let hashed = vol.names_are_hashed();

        let root = cat
            .list_dir(&mut dev, ROOT_DIR_INO_NUM, hashed)
            .expect("list root");
        let dir00 = match root.iter().find(|e| e.name == "dir00") {
            Some(d) => d.clone(),
            None => {
                eprintln!(
                    "skip: size-only fixture (hdiutil attach fell back) - \
                     re-run `cargo run -p xtask -- gen-fixture --large` on a macOS host"
                );
                return;
            }
        };
        let kids = cat
            .list_dir(&mut dev, dir00.file_id, hashed)
            .expect("list dir00");
        let f000 = kids
            .iter()
            .find(|e| e.name == "f000.bin")
            .expect("f000.bin entry");

        let content = cat
            .read_file(&mut dev, f000.file_id)
            .expect("read f000.bin");
        assert_eq!(content.len(), 4096, "f000.bin logical size");
        assert!(
            content.iter().all(|&b| b == b'A'),
            "f000.bin must be 4096 bytes of 0x41"
        );
    }

    #[test]
    fn reads_real_xattrs_embedded_and_stream() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-large.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture --large`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let bsz = c.superblock.block_size;
        let vols = c.list_user_volumes(&mut dev).expect("list volumes");
        let vol = vols.first().expect("user volume").clone();
        let cat = Catalog::open(&mut dev, &vol, bsz).expect("open catalog");
        let hashed = vol.names_are_hashed();

        let root = cat
            .list_dir(&mut dev, ROOT_DIR_INO_NUM, hashed)
            .expect("list root");
        let dir00 = match root.iter().find(|e| e.name == "dir00") {
            Some(d) => d.clone(),
            None => {
                eprintln!(
                    "skip: size-only fixture (hdiutil attach fell back) - \
                     re-run `cargo run -p xtask -- gen-fixture --large` on a macOS host"
                );
                return;
            }
        };
        let kids = cat
            .list_dir(&mut dev, dir00.file_id, hashed)
            .expect("list dir00");
        let f000 = kids
            .iter()
            .find(|e| e.name == "f000.bin")
            .expect("f000.bin entry");

        let names: std::collections::BTreeSet<String> = cat
            .list_xattrs(&mut dev, f000.file_id)
            .expect("list xattrs")
            .into_iter()
            .map(|e| e.name)
            .collect();
        if !names.contains("user.apfsx_small") {
            eprintln!(
                "skip: fixture predates M3c xattrs - re-run \
                 `cargo run -p xtask -- gen-fixture --large` (updated xtask). got: {names:?}"
            );
            return;
        }
        assert!(
            names.contains("user.apfsx_big"),
            "missing user.apfsx_big; got {names:?}"
        );

        let small = cat
            .get_xattr(&mut dev, f000.file_id, "user.apfsx_small")
            .expect("get small")
            .expect("small present");
        assert_eq!(small, b"hello-apfs", "embedded xattr value");

        let big = cat
            .get_xattr(&mut dev, f000.file_id, "user.apfsx_big")
            .expect("get big")
            .expect("big present");
        assert_eq!(big.len(), 5000, "stream xattr logical length");
        assert!(
            big.iter().all(|&b| b == b'Z'),
            "stream xattr content must be 5000 bytes of 'Z'"
        );

        assert!(cat
            .get_xattr(&mut dev, f000.file_id, "user.nope")
            .expect("get absent")
            .is_none());
    }

    #[test]
    fn resolve_path_navigates_real_fixture() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-large.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture --large`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let bsz = c.superblock.block_size;
        let vol = c
            .list_user_volumes(&mut dev)
            .expect("list volumes")
            .first()
            .expect("user volume")
            .clone();
        let cat = Catalog::open(&mut dev, &vol, bsz).expect("open catalog");
        let hashed = vol.names_are_hashed();

        if cat
            .resolve_path(&mut dev, &["dir00"], hashed)
            .expect("resolve dir00")
            .is_none()
        {
            eprintln!("skip: size-only fixture - regenerate gen-fixture --large");
            return;
        }
        let ino = cat
            .resolve_path(&mut dev, &["dir00", "f000.bin"], hashed)
            .expect("resolve dir00/f000.bin")
            .expect("dir00/f000.bin must exist");
        let content = cat
            .read_file_decompressed(&mut dev, ino)
            .expect("read resolved file");
        assert_eq!(content.len(), 4096);
        assert!(content.iter().all(|&b| b == b'A'));

        assert!(cat
            .resolve_path(&mut dev, &["dir00", "does-not-exist"], hashed)
            .expect("resolve missing")
            .is_none());
        assert!(cat
            .resolve_path(&mut dev, &["nope", "x"], hashed)
            .expect("resolve missing parent")
            .is_none());
        assert_eq!(
            cat.resolve_path(&mut dev, &[], hashed)
                .expect("resolve root"),
            Some(crate::jkey::ROOT_DIR_INO_NUM)
        );
    }

    #[test]
    fn decompresses_real_macos_lzvn_files() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-large.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture --large`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let bsz = c.superblock.block_size;
        let vols = c.list_user_volumes(&mut dev).expect("list volumes");
        let vol = vols.first().expect("user volume").clone();
        let cat = Catalog::open(&mut dev, &vol, bsz).expect("open catalog");
        let hashed = vol.names_are_hashed();
        let root = cat
            .list_dir(&mut dev, ROOT_DIR_INO_NUM, hashed)
            .expect("root");
        let dir00 = match root.iter().find(|e| e.name == "dir00") {
            Some(d) => d.clone(),
            None => {
                eprintln!("skip: size-only fixture - regenerate gen-fixture --large");
                return;
            }
        };
        let kids = cat
            .list_dir(&mut dev, dir00.file_id, hashed)
            .expect("dir00");
        let find = |n: &str| kids.iter().find(|e| e.name == n).cloned();

        let mut checked = 0;
        for tag in ["s", "m"] {
            let bin = find(&format!("cmp_{tag}.bin"));
            let exp = find(&format!("cmp_{tag}.expected"));
            match (bin, exp) {
                (Some(b), Some(e)) => {
                    let got = cat
                        .read_file_decompressed(&mut dev, b.file_id)
                        .unwrap_or_else(|_| panic!("decompress real macOS LZVN sample {tag}"));
                    let want = cat.read_file(&mut dev, e.file_id).expect("expected");
                    assert_eq!(
                        got.len(),
                        want.len(),
                        "sample {tag}: decompressed length must equal macOS kernel output"
                    );
                    assert!(
                        got == want,
                        "sample {tag}: LZVN bytes must equal macOS kernel's own decompression"
                    );
                    checked += 1;
                }
                _ => eprintln!("note: M3d LZVN sample '{tag}' absent (M3D_FIXTURE_SKIP at gen)"),
            }
        }
        if checked == 0 {
            eprintln!(
                "skip: no M3d LZVN samples in fixture - re-run gen-fixture --large on a \
                 macOS host with readable /usr/bin compressed files"
            );
            return;
        }

        // Non-compressed regression: f000.bin reads via the fallback path.
        let f000 = find("f000.bin").expect("f000.bin");
        let plain = cat
            .read_file_decompressed(&mut dev, f000.file_id)
            .expect("plain via decompressed path");
        assert_eq!(plain.len(), 4096);
        assert!(plain.iter().all(|&b| b == b'A'));
    }

    #[test]
    fn stat_real_fixture_size_and_isdir() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-large.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing - run `cargo run -p xtask -- gen-fixture --large`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let bsz = c.superblock.block_size;
        let vol = c
            .list_user_volumes(&mut dev)
            .expect("vols")
            .first()
            .expect("vol")
            .clone();
        let cat = Catalog::open(&mut dev, &vol, bsz).expect("catalog");
        let hashed = vol.names_are_hashed();
        let dir00 = match cat
            .resolve_path(&mut dev, &["dir00"], hashed)
            .expect("resolve dir00")
        {
            Some(i) => i,
            None => {
                eprintln!("skip: size-only fixture - regenerate gen-fixture --large");
                return;
            }
        };
        let f000 = cat
            .resolve_path(&mut dev, &["dir00", "f000.bin"], hashed)
            .expect("resolve f000")
            .expect("f000 exists");
        let ds = cat.stat(&mut dev, dir00).expect("stat dir00");
        assert!(ds.is_dir, "dir00 must be a directory");
        let fs = cat.stat(&mut dev, f000).expect("stat f000");
        assert!(!fs.is_dir, "f000.bin is a regular file");
        assert_eq!(fs.size, 4096, "f000.bin logical size");
    }

    // --- read_stream error branches ---

    /// MemDev that returns BlockError for any access beyond its data.
    struct BoundedMemDev {
        blocks: std::collections::HashMap<u64, Vec<u8>>,
        block_size: usize,
    }
    impl BoundedMemDev {
        fn new(block_size: usize) -> Self {
            Self {
                blocks: std::collections::HashMap::new(),
                block_size,
            }
        }
        fn fill_block(&mut self, paddr: u64, byte: u8) {
            let data = vec![byte; self.block_size];
            self.blocks.insert(paddr * self.block_size as u64, data);
        }
    }
    impl BlockDevice for BoundedMemDev {
        fn size(&self) -> u64 {
            64 * self.block_size as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            if let Some(data) = self.blocks.get(&offset) {
                let n = buf.len().min(data.len());
                buf[..n].copy_from_slice(&data[..n]);
                if n < buf.len() {
                    buf[n..].fill(0);
                }
                Ok(())
            } else if offset + buf.len() as u64 <= self.size() {
                buf.fill(0);
                Ok(())
            } else {
                Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.size(),
                })
            }
        }
    }

    /// Build a Catalog backed by a given leaf node (no omap traversal needed).
    fn catalog_from_leaf(leaf_raw: Vec<u8>) -> Catalog {
        let root = BtreeNode::parse(&leaf_raw).expect("parse root");
        Catalog {
            omap: dummy_omap(),
            root,
            xid: 1,
            block_size: 4096,
            sealed: None,
            cache: Default::default(),
        }
    }

    #[test]
    fn read_stream_truncated_request_returns_only_size_bytes() {
        // request size < actual extent length: output must be clamped to size.
        const BSZ: u32 = 4096;
        const STREAM_OID: u64 = 77;
        let raw_root = build_extent_leaf(
            STREAM_OID,
            &[(0, BSZ as u64, 5)], // one extent at paddr=5
        );
        let mut dev = MemDev::new(BSZ as usize);
        dev.fill_block(5, 0xAB);
        let cat = catalog_from_leaf(raw_root);
        // Request only 512 bytes out of a 4096-byte extent.
        let data = cat
            .read_stream(&mut dev, STREAM_OID, 512)
            .expect("read_stream 512");
        assert_eq!(data.len(), 512, "output must be clamped to requested size");
        assert!(
            data.iter().all(|&b| b == 0xAB),
            "first 512 bytes must be from paddr=5"
        );
    }

    #[test]
    fn read_stream_size_zero_returns_empty() {
        // Requesting 0 bytes must return an empty vec without touching device.
        const STREAM_OID: u64 = 88;
        let raw_root = build_extent_leaf(STREAM_OID, &[(0, 4096, 5)]);
        let mut dev = MemDev::new(4096);
        dev.fill_block(5, 0xCC);
        let cat = catalog_from_leaf(raw_root);
        let data = cat
            .read_stream(&mut dev, STREAM_OID, 0)
            .expect("read_stream 0");
        assert!(data.is_empty(), "size=0 must yield empty vec");
    }

    #[test]
    fn read_stream_no_matching_oid_returns_zeros() {
        // The extent leaf has entries for OID 200; requesting OID 201 finds nothing.
        // Result must be a zero-filled vec of the requested size.
        const BSZ: usize = 4096;
        const STREAM_OID: u64 = 200;
        let raw_root = build_extent_leaf(STREAM_OID, &[(0, BSZ as u64, 7)]);
        let mut dev = MemDev::new(BSZ);
        dev.fill_block(7, 0xDD);
        let cat = catalog_from_leaf(raw_root);
        let data = cat
            .read_stream(&mut dev, 201, BSZ)
            .expect("read_stream mismatched oid");
        assert_eq!(data.len(), BSZ);
        assert!(data.iter().all(|&b| b == 0), "no extents → all zeros");
    }

    // A ranged read touches only the extents inside the range, and only the
    // part of each that the range covers.
    #[test]
    fn read_stream_range_reads_only_what_the_range_covers() {
        const BSZ: usize = 4096;
        const STREAM_OID: u64 = 77;
        let raw_root = build_extent_leaf(
            STREAM_OID,
            &[(0, BSZ as u64, 5), (BSZ as u64, BSZ as u64, 6)],
        );
        let mut dev = MemDev::new(BSZ);
        dev.fill_block(5, 0x11);
        dev.fill_block(6, 0x22);
        let cat = catalog_from_leaf(raw_root);
        let size = 2 * BSZ;

        let across = cat
            .read_stream_range(&mut dev, STREAM_OID, size, BSZ - 96, 200)
            .expect("across the extent boundary");
        assert_eq!(across.len(), 200);
        assert!(across[..96].iter().all(|&b| b == 0x11));
        assert!(across[96..].iter().all(|&b| b == 0x22));

        let head = cat
            .read_stream_range(&mut dev, STREAM_OID, size, 0, 20)
            .expect("head");
        assert_eq!(head, vec![0x11; 20]);
        let tail = cat
            .read_stream_range(&mut dev, STREAM_OID, size, size - 3, 50)
            .expect("tail");
        assert_eq!(tail, vec![0x22; 3], "clamped to the stream size");
        assert!(cat
            .read_stream_range(&mut dev, STREAM_OID, size, size, 10)
            .expect("past end")
            .is_empty());
        assert!(cat
            .read_stream_range(&mut dev, STREAM_OID, size, 5, 0)
            .expect("empty")
            .is_empty());
        // A stream shorter than its extents: the size wins.
        let short = cat
            .read_stream_range(&mut dev, STREAM_OID, 100, 90, 50)
            .expect("short");
        assert_eq!(short, vec![0x11; 10]);
    }

    #[test]
    fn read_stream_multiple_extents_assembled_in_order() {
        // Two non-overlapping extents at different logical offsets; their data must
        // appear at the correct positions in the output.
        const BSZ: usize = 4096;
        const STREAM_OID: u64 = 55;
        let raw_root = build_extent_leaf(
            STREAM_OID,
            &[
                (BSZ as u64, BSZ as u64, 6), // second logical block at paddr=6
                (0, BSZ as u64, 5),          // first logical block at paddr=5
            ],
        );
        let mut dev = MemDev::new(BSZ);
        dev.fill_block(5, 0x11); // logical block 0 content
        dev.fill_block(6, 0x22); // logical block 1 content
        let cat = catalog_from_leaf(raw_root);
        let data = cat
            .read_stream(&mut dev, STREAM_OID, BSZ * 2)
            .expect("two extents");
        assert_eq!(data.len(), BSZ * 2);
        assert!(
            data[..BSZ].iter().all(|&b| b == 0x11),
            "first extent at offset 0"
        );
        assert!(
            data[BSZ..].iter().all(|&b| b == 0x22),
            "second extent at offset BSZ"
        );
    }

    // --- for_each_leaf depth guard ---

    #[test]
    fn for_each_leaf_empty_leaf_calls_closure_zero_times() {
        // A leaf node with nkeys=0 must invoke the closure zero times.
        const BSZ: usize = 4096;
        let mut b = vec![0u8; BSZ];
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        b[32..34].copy_from_slice(&BTNODE_LEAF.to_le_bytes());
        b[34..36].copy_from_slice(&0u16.to_le_bytes()); // level
        b[36..40].copy_from_slice(&0u32.to_le_bytes()); // nkeys = 0
        b[40..42].copy_from_slice(&0u16.to_le_bytes()); // toc_off
        b[42..44].copy_from_slice(&0u16.to_le_bytes()); // toc_len
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        let root = BtreeNode::parse(&b).expect("parse empty leaf");
        let cat = Catalog {
            omap: dummy_omap(),
            root,
            xid: 1,
            block_size: 4096,
            sealed: None,
            cache: Default::default(),
        };
        let mut count = 0usize;
        let mut dev = MemDev::new(BSZ);
        cat.for_each_leaf(&mut dev, &cat.root.clone(), 0, &mut |_k, _v| {
            count += 1;
            Ok(())
        })
        .expect("for_each_leaf on empty node");
        assert_eq!(count, 0, "empty leaf must call closure zero times");
    }

    /// A var-kv catalog node of the given object type, flags and level; the
    /// values of an index node are the 8-byte oids of its children.
    fn build_varkv_node(
        o_type: u32,
        flags: u16,
        level: u16,
        records: &[(Vec<u8>, Vec<u8>)],
    ) -> Vec<u8> {
        let bsz = 4096usize;
        let data_base = 56usize;
        let toc_len = 8 * records.len();
        let key_area_start = data_base + toc_len;
        let mut b = vec![0u8; bsz];
        b[24..28].copy_from_slice(&o_type.to_le_bytes());
        b[32..34].copy_from_slice(&flags.to_le_bytes());
        b[34..36].copy_from_slice(&level.to_le_bytes());
        b[36..40].copy_from_slice(&(records.len() as u32).to_le_bytes());
        b[40..42].copy_from_slice(&0u16.to_le_bytes());
        b[42..44].copy_from_slice(&(toc_len as u16).to_le_bytes());
        let mut k_off = 0usize;
        let mut v_abs = bsz;
        for (i, (k, v)) in records.iter().enumerate() {
            v_abs -= v.len();
            let toc = data_base + 8 * i;
            b[toc..toc + 2].copy_from_slice(&(k_off as u16).to_le_bytes());
            b[toc + 2..toc + 4].copy_from_slice(&(k.len() as u16).to_le_bytes());
            b[toc + 4..toc + 6].copy_from_slice(&((bsz - v_abs) as u16).to_le_bytes());
            b[toc + 6..toc + 8].copy_from_slice(&(v.len() as u16).to_le_bytes());
            b[key_area_start + k_off..key_area_start + k_off + k.len()].copy_from_slice(k);
            b[v_abs..v_abs + v.len()].copy_from_slice(v);
            k_off += k.len();
        }
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        b
    }

    /// An object map whose root leaf maps each `(oid, paddr)` pair at xid 1.
    fn omap_mapping(entries: &[(u64, u64)]) -> Omap {
        omap_mapping_with_flags(entries, 0)
    }

    /// [`omap_mapping`] with `ov_flags` set on every mapping.
    fn omap_mapping_with_flags(entries: &[(u64, u64)], flags: u32) -> Omap {
        use crate::btree::BTNODE_FIXED_KV_SIZE;
        use crate::obj::OBJECT_TYPE_OMAP;
        const BSZ: usize = 4096;
        const DATA_BASE: usize = 56;
        let n = entries.len();
        let mut b = vec![0u8; BSZ];
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        b[28..32].copy_from_slice(&OBJECT_TYPE_OMAP.to_le_bytes());
        b[32..34].copy_from_slice(&(BTNODE_LEAF | BTNODE_FIXED_KV_SIZE).to_le_bytes());
        b[36..40].copy_from_slice(&(n as u32).to_le_bytes());
        b[42..44].copy_from_slice(&((n * 4) as u16).to_le_bytes());
        let key_area = DATA_BASE + n * 4;
        for (i, &(oid, paddr)) in entries.iter().enumerate() {
            let toc = DATA_BASE + i * 4;
            b[toc..toc + 2].copy_from_slice(&((i * 16) as u16).to_le_bytes());
            b[toc + 2..toc + 4].copy_from_slice(&(((i + 1) * 16) as u16).to_le_bytes());
            let k = key_area + i * 16;
            b[k..k + 8].copy_from_slice(&oid.to_le_bytes());
            b[k + 8..k + 16].copy_from_slice(&1u64.to_le_bytes());
            let v = BSZ - (i + 1) * 16;
            b[v..v + 4].copy_from_slice(&flags.to_le_bytes());
            b[v + 4..v + 8].copy_from_slice(&4096u32.to_le_bytes());
            b[v + 8..v + 16].copy_from_slice(&paddr.to_le_bytes());
        }
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        let mut omap = dummy_omap();
        omap.root = BtreeNode::parse(&b).expect("omap leaf parses");
        omap
    }

    /// An inode key for `ino` (the catalog's j_key_t: obj_id in the low bits,
    /// type in the top byte).
    fn key_for(ino: u64) -> Vec<u8> {
        ((APFS_TYPE_INODE as u64) << OBJ_TYPE_SHIFT | ino)
            .to_le_bytes()
            .to_vec()
    }

    fn catalog_over(root: Vec<u8>, omap: Omap) -> Catalog {
        Catalog {
            omap,
            root: BtreeNode::parse(&root).expect("root parses"),
            xid: 1,
            block_size: 4096,
            sealed: None,
            cache: Default::default(),
        }
    }

    /// A lookup by object id reads only the children whose key range can
    /// hold the object. The children it must skip are left unreadable here
    /// (an all-zero block fails its checksum), so reading one is an error.
    #[test]
    fn a_lookup_reads_only_the_children_that_can_hold_the_object() {
        use crate::obj::OBJECT_TYPE_BTREE_NODE;
        let leaf = |ino: u64| {
            build_varkv_node(
                OBJECT_TYPE_BTREE_NODE,
                BTNODE_LEAF,
                0,
                &[(key_for(ino), vec![0u8; 8])],
            )
        };
        // Children start at inodes 2, 10 and 20 (oids 0x500..0x502).
        let root = build_varkv_node(
            OBJECT_TYPE_BTREE,
            0,
            1,
            &[
                (key_for(2), 0x500u64.to_le_bytes().to_vec()),
                (key_for(10), 0x501u64.to_le_bytes().to_vec()),
                (key_for(20), 0x502u64.to_le_bytes().to_vec()),
            ],
        );
        let mapping = [(0x500, 20), (0x501, 21), (0x502, 22)];
        let cat = catalog_over(root.clone(), omap_mapping(&mapping));

        // Inode 5 can only be in the first child.
        let mut dev = MemDev::new(4096);
        dev.insert_block(20, leaf(5));
        let mut seen = Vec::new();
        cat.for_each_record_of(&mut dev, 5, APFS_TYPE_INODE, &mut |k, _| {
            seen.push(JKey::parse(k)?.obj_id);
            Ok(())
        })
        .expect("only the first child is read");
        assert_eq!(seen, vec![5]);

        // Inode 10 starts the second child, but records of it with a lower
        // type could end the first; the third child is not read. A catalog
        // reads one checkpoint and keeps the nodes it read, so a different
        // device gets a catalog of its own.
        let cat = catalog_over(root, omap_mapping(&mapping));
        let mut dev = MemDev::new(4096);
        dev.insert_block(20, leaf(9));
        dev.insert_block(21, leaf(10));
        let mut seen = Vec::new();
        cat.for_each_record_of(&mut dev, 10, APFS_TYPE_INODE, &mut |k, _| {
            seen.push(JKey::parse(k)?.obj_id);
            Ok(())
        })
        .expect("the first two children are read");
        assert_eq!(seen, vec![9, 10]);

        // The full walk reads every child, so the same tree fails it.
        let err = cat.for_each_leaf(&mut dev, &cat.root.clone(), 0, &mut |_k, _v| Ok(()));
        assert!(err.is_err(), "the full walk reaches the unreadable child");
    }

    /// A node mapped without a header (a sealed volume's catalog node) is
    /// read without a checksum to verify; the same block mapped as an
    /// ordinary node is refused for its checksum.
    #[test]
    fn a_node_mapped_without_a_header_is_read_without_a_checksum() {
        use crate::obj::OBJECT_TYPE_BTREE_NODE;
        let mut node = build_varkv_node(
            OBJECT_TYPE_BTREE_NODE,
            BTNODE_LEAF,
            0,
            &[(key_for(7), vec![0u8; 8])],
        );
        node[0..32].fill(0);
        let mut dev = MemDev::new(4096);
        dev.insert_block(20, node);

        let plain = omap_mapping(&[(0x500, 20)]);
        let err = read_catalog_node(&mut dev, &plain, 0x500, 1, 4096, OBJECT_TYPE_BTREE_NODE);
        assert!(err.is_err(), "an ordinary mapping wants a valid header");

        let headerless = omap_mapping_with_flags(&[(0x500, 20)], OMAP_VAL_NOHEADER);
        let node = read_catalog_node(
            &mut dev,
            &headerless,
            0x500,
            1,
            4096,
            OBJECT_TYPE_BTREE_NODE,
        )
        .expect("a headerless mapping reads the node as it is");
        assert_eq!(node.nkeys, 1);
    }

    /// The leaf flag and a level of zero say the same thing. A node flagged
    /// as a leaf at level 1 holds child pointers, and handing those to the
    /// caller as records would be wrong either way.
    #[test]
    fn for_each_leaf_rejects_a_leaf_flag_at_a_nonzero_level() {
        use crate::obj::OBJECT_TYPE_BTREE_NODE;
        let child = build_varkv_node(OBJECT_TYPE_BTREE_NODE, BTNODE_LEAF, 0, &[]);
        let mut dev = MemDev::new(4096);
        dev.insert_block(20, child);
        let root = build_varkv_node(
            OBJECT_TYPE_BTREE,
            BTNODE_LEAF,
            1,
            &[(key_for(2), 0x500u64.to_le_bytes().to_vec())],
        );
        let cat = catalog_over(root, omap_mapping(&[(0x500, 20)]));
        let err = cat
            .for_each_leaf(&mut dev, &cat.root.clone(), 0, &mut |_k, _v| Ok(()))
            .expect_err("a leaf flag at level 1 must be refused");
        assert!(
            matches!(
                err,
                ContainerError::Parse(ParseError::BadField {
                    field: "btn_level",
                    value: 1
                })
            ),
            "got: {err:?}"
        );
    }

    /// A child sits one level below its parent. A level-1 child under a
    /// level-1 root is a pointer loop dressed as a tree, and the walk must
    /// say so instead of descending.
    #[test]
    fn for_each_leaf_rejects_a_child_not_one_level_below() {
        use crate::obj::OBJECT_TYPE_BTREE_NODE;
        let mut dev = MemDev::new(4096);
        // Block 20: level 1 again, pointing back at itself through oid 0x500.
        let looping = build_varkv_node(
            OBJECT_TYPE_BTREE_NODE,
            0,
            1,
            &[(key_for(2), 0x500u64.to_le_bytes().to_vec())],
        );
        dev.insert_block(20, looping);
        let root = build_varkv_node(
            OBJECT_TYPE_BTREE,
            0,
            1,
            &[(key_for(2), 0x500u64.to_le_bytes().to_vec())],
        );
        let cat = catalog_over(root, omap_mapping(&[(0x500, 20)]));
        let err = cat
            .for_each_leaf(&mut dev, &cat.root.clone(), 0, &mut |_k, _v| Ok(()))
            .expect_err("a child at the parent's level must be refused");
        assert!(
            matches!(
                err,
                ContainerError::Parse(ParseError::BadField {
                    field: "btn_level",
                    value: 1
                })
            ),
            "got: {err:?}"
        );
    }

    /// A chain deeper than any real catalog tree is an error, not a walk
    /// that quietly stops and reports the records it happened to reach.
    #[test]
    fn for_each_leaf_rejects_a_tree_deeper_than_the_cap() {
        use crate::obj::OBJECT_TYPE_BTREE_NODE;
        // Levels 40 down to 0: node i lives at block 100 + i and is oid
        // 0x1000 + i; each index node points at the next one.
        const TOP: u16 = 40;
        let mut dev = MemDev::new(4096);
        let mut mapping = Vec::new();
        for i in 0..=TOP {
            let level = TOP - i;
            let recs = if level == 0 {
                vec![(key_for(2), vec![0u8; 16])]
            } else {
                vec![(
                    key_for(2),
                    (0x1000u64 + u64::from(i) + 1).to_le_bytes().to_vec(),
                )]
            };
            let flags = if level == 0 { BTNODE_LEAF } else { 0 };
            let node = build_varkv_node(OBJECT_TYPE_BTREE_NODE, flags, level, &recs);
            dev.insert_block(100 + u64::from(i), node);
            mapping.push((0x1000u64 + u64::from(i), 100 + u64::from(i)));
        }
        let root = build_varkv_node(
            OBJECT_TYPE_BTREE,
            0,
            TOP + 1,
            &[(key_for(2), 0x1000u64.to_le_bytes().to_vec())],
        );
        let cat = catalog_over(root, omap_mapping(&mapping));
        let mut seen = 0usize;
        let err = cat
            .for_each_leaf(&mut dev, &cat.root.clone(), 0, &mut |_k, _v| {
                seen += 1;
                Ok(())
            })
            .expect_err("a tree past the depth cap must be an error");
        assert!(
            matches!(
                err,
                ContainerError::Parse(ParseError::BadField {
                    field: "catalog tree depth",
                    ..
                })
            ),
            "got: {err:?}"
        );
        assert_eq!(seen, 0, "no record lies within the cap");
    }

    // ---------------------------------------------------------------------------
    // an audit pass - stat() pure unit tests
    // ---------------------------------------------------------------------------

    /// Build a minimal var-kv leaf containing a single APFS_TYPE_INODE entry for
    /// `inode_num`. The inode value is the raw 92-byte j_inode_val_t with no
    /// xfields (num_exts=0).
    ///
    /// Fields written (all LE):
    ///   [0..8]   parent_id
    ///   [8..16]  private_id
    ///   [16..24] create_time = 0
    ///   [24..32] mod_time    = 0
    ///   [32..40] change_time = 0
    ///   [40..48] access_time = 0
    ///   [48..56] internal_flags = 0
    ///   [56..60] nchildren_or_nlink = 0
    ///   [60..64] default_protection_class = 0
    ///   [64..68] write_generation_counter = 0
    ///   [68..72] bsd_flags
    ///   [72..76] uid
    ///   [76..80] gid
    ///   [80..82] mode
    ///   [82..84] pad1 = 0
    ///   [84..92] pad2 = 0
    ///   [92..94] xf_num_exts = 0
    ///   [94..96] xf_used_data = 0
    fn build_inode_leaf(inode_num: u64, mode: u16, bsd_flags: u32) -> Vec<u8> {
        let bsz = 4096usize;
        let data_base: usize = 56;

        // Key: j_key_t (8 bytes) only - no per-key extension for inode keys
        let key_size = 8usize;
        // Value: 96-byte j_inode_val_t (92 fixed + 4 bytes xf_blob header)
        let val_size = 96usize;
        let toc_len = 8usize; // one kvloc_t entry
        let val_area_end = bsz;

        let mut b = vec![0u8; bsz];
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        b[32..34].copy_from_slice(&BTNODE_LEAF.to_le_bytes());
        b[34..36].copy_from_slice(&0u16.to_le_bytes()); // level = 0
        b[36..40].copy_from_slice(&1u32.to_le_bytes()); // nkeys = 1
        b[40..42].copy_from_slice(&0u16.to_le_bytes()); // toc_off
        b[42..44].copy_from_slice(&(toc_len as u16).to_le_bytes()); // toc_len

        // j_key_t for TYPE_INODE
        let jkey_word = (inode_num & OBJ_ID_MASK) | ((APFS_TYPE_INODE as u64) << OBJ_TYPE_SHIFT);
        let key_area_start = data_base + toc_len;
        b[key_area_start..key_area_start + 8].copy_from_slice(&jkey_word.to_le_bytes());

        // j_inode_val_t (96 bytes = 92 fixed + 4-byte xf_blob_t header)
        let mut val = vec![0u8; val_size];
        val[0..8].copy_from_slice(&inode_num.to_le_bytes()); // parent_id
        val[8..16].copy_from_slice(&inode_num.to_le_bytes()); // private_id
        val[68..72].copy_from_slice(&bsd_flags.to_le_bytes());
        val[80..82].copy_from_slice(&mode.to_le_bytes());
        // xf_num_exts = 0 at offset 92
        val[92..94].copy_from_slice(&0u16.to_le_bytes());
        val[94..96].copy_from_slice(&0u16.to_le_bytes());

        let v_abs = val_area_end - val_size;
        let v_off = val_area_end - v_abs;
        let k_off = 0usize; // relative to key_area_start

        // kvloc_t at toc position
        let toc_entry = data_base;
        b[toc_entry..toc_entry + 2].copy_from_slice(&(k_off as u16).to_le_bytes());
        b[toc_entry + 2..toc_entry + 4].copy_from_slice(&(key_size as u16).to_le_bytes());
        b[toc_entry + 4..toc_entry + 6].copy_from_slice(&(v_off as u16).to_le_bytes());
        b[toc_entry + 6..toc_entry + 8].copy_from_slice(&(val_size as u16).to_le_bytes());

        b[v_abs..v_abs + val_size].copy_from_slice(&val);

        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        b
    }

    #[test]
    fn stat_inode_not_found_returns_error() {
        // Leaf has inode 42; requesting inode 99 must return ContainerError.
        let raw = build_inode_leaf(42, 0o100644, 0);
        let cat = catalog_from_leaf(raw);
        let mut dev = MemDev::new(4096);
        assert!(cat.stat(&mut dev, 99).is_err(), "unknown inode must error");
    }

    #[test]
    fn stat_regular_file_inode_returns_correct_fields() {
        // S_IFREG | 0644 = 0o100644 = 0x81A4
        const MODE_REG: u16 = 0o100644;
        const BSD_FLAGS: u32 = 0x0000_8000; // UF_HIDDEN
        const INODE: u64 = 55;
        let raw = build_inode_leaf(INODE, MODE_REG, BSD_FLAGS);
        let cat = catalog_from_leaf(raw);
        let mut dev = MemDev::new(4096);
        let s = cat.stat(&mut dev, INODE).expect("stat regular file");
        assert!(!s.is_dir, "regular file must not be is_dir");
        assert_eq!(s.mode, MODE_REG, "mode round-trips");
        assert_eq!(s.bsd_flags, BSD_FLAGS, "bsd_flags round-trips");
        assert_eq!(s.size, 0, "no dstream → size=0");
    }

    #[test]
    fn stat_directory_inode_sets_is_dir() {
        // S_IFDIR | 0755 = 0o040755 = 0x41ED
        const MODE_DIR: u16 = 0o040755;
        const INODE: u64 = 77;
        let raw = build_inode_leaf(INODE, MODE_DIR, 0);
        let cat = catalog_from_leaf(raw);
        let mut dev = MemDev::new(4096);
        let s = cat.stat(&mut dev, INODE).expect("stat directory");
        assert!(s.is_dir, "directory inode must have is_dir=true");
        assert_eq!(s.mode, MODE_DIR);
    }

    // -----------------------------------------------------------------------
    // build_xattr_leaf: produce a var-kv leaf with one embedded xattr record.
    //
    // j_xattr_key_t layout:
    //   [0..8]  j_key_t: (obj_id & OBJ_ID_MASK) | (APFS_TYPE_XATTR << 60)
    //   [8..10] name_len (u16, includes trailing NUL)
    //   [10..]  name bytes + NUL
    //
    // j_xattr_val_t layout:
    //   [0..2]  flags (u16)
    //   [2..4]  xdata_len (u16)
    //   [4..]   xdata bytes
    // -----------------------------------------------------------------------
    fn build_xattr_leaf(inode_num: u64, attr_name: &str, attr_data: &[u8]) -> Vec<u8> {
        use crate::jkey::{APFS_TYPE_XATTR, OBJ_ID_MASK, OBJ_TYPE_SHIFT};
        use crate::xattr::XATTR_DATA_EMBEDDED;

        let bsz = 4096usize;
        let data_base: usize = 56;

        // Build key bytes
        let name_bytes: Vec<u8> = attr_name.bytes().chain(std::iter::once(0u8)).collect();
        let name_len = name_bytes.len() as u16;
        let jkey_word = (inode_num & OBJ_ID_MASK) | ((APFS_TYPE_XATTR as u64) << OBJ_TYPE_SHIFT);
        let mut key: Vec<u8> = Vec::new();
        key.extend_from_slice(&jkey_word.to_le_bytes());
        key.extend_from_slice(&name_len.to_le_bytes());
        key.extend_from_slice(&name_bytes);
        let key_size = key.len();

        // Build val bytes: flags=EMBEDDED, xdata_len, xdata
        let mut val: Vec<u8> = Vec::new();
        val.extend_from_slice(&XATTR_DATA_EMBEDDED.to_le_bytes());
        val.extend_from_slice(&(attr_data.len() as u16).to_le_bytes());
        val.extend_from_slice(attr_data);
        let val_size = val.len();

        let toc_len = 8usize; // one kvloc_t (8 bytes)
        let key_area_start = data_base + toc_len;
        let val_area_end = bsz;

        let mut b = vec![0u8; bsz];
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        b[32..34].copy_from_slice(&BTNODE_LEAF.to_le_bytes()); // var-kv leaf
        b[36..40].copy_from_slice(&1u32.to_le_bytes()); // nkeys=1
        b[40..42].copy_from_slice(&0u16.to_le_bytes()); // toc_off=0
        b[42..44].copy_from_slice(&(toc_len as u16).to_le_bytes());

        // kvloc_t: k_off=0, k_len, v_off, v_len
        let v_off = val_size as u16;
        b[data_base..data_base + 2].copy_from_slice(&0u16.to_le_bytes());
        b[data_base + 2..data_base + 4].copy_from_slice(&(key_size as u16).to_le_bytes());
        b[data_base + 4..data_base + 6].copy_from_slice(&v_off.to_le_bytes());
        b[data_base + 6..data_base + 8].copy_from_slice(&(val_size as u16).to_le_bytes());

        // Write key
        b[key_area_start..key_area_start + key_size].copy_from_slice(&key);

        // Write val (counting back from val_area_end)
        let v_abs = val_area_end - val_size;
        b[v_abs..v_abs + val_size].copy_from_slice(&val);

        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        b
    }

    /// A var-kv leaf holding the given (key, value) records in order.
    fn build_leaf_records(records: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let bsz = 4096usize;
        let data_base = 56usize;
        let toc_len = 8 * records.len();
        let key_area_start = data_base + toc_len;
        let mut b = vec![0u8; bsz];
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        b[32..34].copy_from_slice(&BTNODE_LEAF.to_le_bytes());
        b[36..40].copy_from_slice(&(records.len() as u32).to_le_bytes());
        b[40..42].copy_from_slice(&0u16.to_le_bytes());
        b[42..44].copy_from_slice(&(toc_len as u16).to_le_bytes());
        let mut k_off = 0usize;
        let mut v_abs = bsz;
        for (i, (k, v)) in records.iter().enumerate() {
            v_abs -= v.len();
            let toc = data_base + 8 * i;
            b[toc..toc + 2].copy_from_slice(&(k_off as u16).to_le_bytes());
            b[toc + 2..toc + 4].copy_from_slice(&(k.len() as u16).to_le_bytes());
            b[toc + 4..toc + 6].copy_from_slice(&((bsz - v_abs) as u16).to_le_bytes());
            b[toc + 6..toc + 8].copy_from_slice(&(v.len() as u16).to_le_bytes());
            b[key_area_start + k_off..key_area_start + k_off + k.len()].copy_from_slice(k);
            b[v_abs..v_abs + v.len()].copy_from_slice(v);
            k_off += k.len();
        }
        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());
        b
    }

    fn inode_record(
        inode_num: u64,
        mode: u16,
        bsd_flags: u32,
        internal_flags: u64,
        uncompressed_size: u64,
    ) -> (Vec<u8>, Vec<u8>) {
        let jkey = (inode_num & OBJ_ID_MASK) | ((APFS_TYPE_INODE as u64) << OBJ_TYPE_SHIFT);
        let mut val = vec![0u8; 96];
        val[0..8].copy_from_slice(&inode_num.to_le_bytes());
        val[8..16].copy_from_slice(&inode_num.to_le_bytes());
        val[48..56].copy_from_slice(&internal_flags.to_le_bytes());
        val[68..72].copy_from_slice(&bsd_flags.to_le_bytes());
        val[80..82].copy_from_slice(&mode.to_le_bytes());
        val[84..92].copy_from_slice(&uncompressed_size.to_le_bytes());
        (jkey.to_le_bytes().to_vec(), val)
    }

    fn xattr_record(inode_num: u64, name: &str, data: &[u8]) -> (Vec<u8>, Vec<u8>) {
        use crate::jkey::APFS_TYPE_XATTR;
        use crate::xattr::XATTR_DATA_EMBEDDED;
        let jkey = (inode_num & OBJ_ID_MASK) | ((APFS_TYPE_XATTR as u64) << OBJ_TYPE_SHIFT);
        let mut k = jkey.to_le_bytes().to_vec();
        k.extend_from_slice(&((name.len() + 1) as u16).to_le_bytes());
        k.extend_from_slice(name.as_bytes());
        k.push(0);
        let mut v = XATTR_DATA_EMBEDDED.to_le_bytes().to_vec();
        v.extend_from_slice(&(data.len() as u16).to_le_bytes());
        v.extend_from_slice(data);
        (k, v)
    }

    /// A method-9 decmpfs attribute holding `content` inline.
    fn plain_decmpfs(content: &[u8]) -> Vec<u8> {
        let mut x = vec![0x66, 0x70, 0x6d, 0x63];
        x.extend_from_slice(&9u32.to_le_bytes());
        x.extend_from_slice(&(content.len() as u64).to_le_bytes());
        x.push(0xCC);
        x.extend_from_slice(content);
        x
    }

    const S_IFREG_RW: u16 = 0o100644;
    const UF_COMPRESSED_FLAG: u32 = 0x20;
    const HAS_UNCOMPRESSED_SIZE: u64 = 0x0004_0000;

    // A decmpfs attribute on a file whose UF_COMPRESSED flag is clear is
    // just an attribute: macOS serves the data stream, and so must we
    // (measured: the file reads back as its data fork, not the attribute).
    #[test]
    fn compressed_state_flag_clear_means_not_compressed() {
        let leaf = build_leaf_records(&[
            inode_record(42, S_IFREG_RW, 0, 0, 0),
            xattr_record(
                42,
                "com.apple.decmpfs",
                &plain_decmpfs(b"attribute content"),
            ),
        ]);
        let cat = catalog_from_leaf(leaf);
        let mut dev = MemDev::new(4096);
        let st = cat.stat(&mut dev, 42).expect("stat");
        assert_eq!(st.size, 0, "size comes from the (absent) data stream");
        let content = cat.read_file_decompressed(&mut dev, 42).expect("read");
        assert!(
            content.is_empty(),
            "content comes from the (absent) data stream"
        );
    }

    // A compressed file whose header cannot be parsed is an error, never a
    // zero-length file: a copy tool that sees size 0 copies nothing and
    // reports success.
    #[test]
    fn compressed_state_unparseable_header_is_an_error_not_size_zero() {
        let leaf = build_leaf_records(&[
            inode_record(42, S_IFREG_RW, UF_COMPRESSED_FLAG, 0, 0),
            xattr_record(
                42,
                "com.apple.decmpfs",
                b"XXXX\x09\0\0\0\x05\0\0\0\0\0\0\0\xCChello",
            ),
        ]);
        let cat = catalog_from_leaf(leaf);
        let mut dev = MemDev::new(4096);
        assert!(cat.stat(&mut dev, 42).is_err(), "bad magic must fail stat");
        assert!(cat.read_file_decompressed(&mut dev, 42).is_err());
    }

    // The flag without the attribute is likewise an error, not an empty file.
    #[test]
    fn compressed_state_flag_without_attribute_is_an_error() {
        let leaf = build_leaf_records(&[inode_record(42, S_IFREG_RW, UF_COMPRESSED_FLAG, 0, 0)]);
        let cat = catalog_from_leaf(leaf);
        let mut dev = MemDev::new(4096);
        assert!(cat.stat(&mut dev, 42).is_err());
        assert!(cat.read_file_decompressed(&mut dev, 42).is_err());
    }

    // When the kernel has recorded the uncompressed size in the inode itself
    // (internal flag 0x40000), that is the size, as Apple reads it, and the
    // attribute need not be consulted.
    #[test]
    fn compressed_state_prefers_the_size_recorded_in_the_inode() {
        let leaf = build_leaf_records(&[
            inode_record(
                42,
                S_IFREG_RW,
                UF_COMPRESSED_FLAG,
                HAS_UNCOMPRESSED_SIZE,
                777,
            ),
            xattr_record(
                42,
                "com.apple.decmpfs",
                &plain_decmpfs(b"seventeen bytes!!"),
            ),
        ]);
        let cat = catalog_from_leaf(leaf);
        let mut dev = MemDev::new(4096);
        assert_eq!(cat.stat(&mut dev, 42).expect("stat").size, 777);
        let leaf = build_leaf_records(&[
            inode_record(42, S_IFREG_RW, UF_COMPRESSED_FLAG, 0, 777),
            xattr_record(
                42,
                "com.apple.decmpfs",
                &plain_decmpfs(b"seventeen bytes!!"),
            ),
        ]);
        let cat = catalog_from_leaf(leaf);
        assert_eq!(
            cat.stat(&mut dev, 42).expect("stat").size,
            17,
            "without the internal flag the field is not meaningful"
        );
    }

    #[test]
    fn compressed_state_valid_inline_file_reads_its_content() {
        let leaf = build_leaf_records(&[
            inode_record(42, S_IFREG_RW, UF_COMPRESSED_FLAG, 0, 0),
            xattr_record(
                42,
                "com.apple.decmpfs",
                &plain_decmpfs(b"seventeen bytes!!"),
            ),
        ]);
        let cat = catalog_from_leaf(leaf);
        let mut dev = MemDev::new(4096);
        assert_eq!(cat.stat(&mut dev, 42).expect("stat").size, 17);
        assert_eq!(
            cat.read_file_decompressed(&mut dev, 42).expect("read"),
            b"seventeen bytes!!"
        );
    }

    // A decoding failure keeps its cause on the way out.
    #[test]
    fn compressed_state_decoding_errors_keep_their_cause() {
        let mut attr = vec![0x66, 0x70, 0x6d, 0x63];
        attr.extend_from_slice(&8u32.to_le_bytes());
        attr.extend_from_slice(&100u64.to_le_bytes());
        let leaf = build_leaf_records(&[
            inode_record(42, S_IFREG_RW, UF_COMPRESSED_FLAG, 0, 0),
            xattr_record(42, "com.apple.decmpfs", &attr),
        ]);
        let cat = catalog_from_leaf(leaf);
        let mut dev = MemDev::new(4096);
        assert!(matches!(
            cat.read_file_decompressed(&mut dev, 42),
            Err(ContainerError::Decmpfs(
                crate::decmpfs::DecmpfsError::MissingResourceFork
            ))
        ));
    }

    #[test]
    fn list_xattrs_synthetic_returns_entry() {
        let inode: u64 = 55;
        let raw = build_xattr_leaf(inode, "user.test", b"value");
        let cat = catalog_from_leaf(raw);
        let mut dev = MemDev::new(4096);
        let entries = cat.list_xattrs(&mut dev, inode).expect("list_xattrs");
        assert_eq!(entries.len(), 1, "must find one xattr");
        assert_eq!(entries[0].name, "user.test");
    }

    #[test]
    fn list_xattrs_wrong_inode_returns_empty() {
        let raw = build_xattr_leaf(55, "user.test", b"value");
        let cat = catalog_from_leaf(raw);
        let mut dev = MemDev::new(4096);
        let entries = cat
            .list_xattrs(&mut dev, 999)
            .expect("list_xattrs wrong inode");
        assert!(entries.is_empty(), "wrong inode must return no xattrs");
    }

    #[test]
    fn get_xattr_embedded_returns_data() {
        let inode: u64 = 66;
        let data = b"hello-world";
        let raw = build_xattr_leaf(inode, "user.embed", data);
        let cat = catalog_from_leaf(raw);
        let mut dev = MemDev::new(4096);
        let result = cat
            .get_xattr(&mut dev, inode, "user.embed")
            .expect("get_xattr ok")
            .expect("must be Some");
        assert_eq!(result, data, "embedded xattr must return exact bytes");
    }

    #[test]
    fn get_xattr_absent_returns_none() {
        let raw = build_xattr_leaf(66, "user.embed", b"x");
        let cat = catalog_from_leaf(raw);
        let mut dev = MemDev::new(4096);
        let result = cat.get_xattr(&mut dev, 66, "user.nope").expect("ok");
        assert!(result.is_none(), "absent xattr must return None");
    }

    #[test]
    fn for_each_leaf_depth_guard_terminates() {
        // Construct a non-leaf node whose child omap resolution will fail (omap
        // has no entries). The depth guard at 32 must prevent infinite descent
        // and return Ok(()) rather than panicking or looping forever.
        use crate::jkey::{OBJ_ID_MASK, OBJ_TYPE_SHIFT};
        // APFS_TYPE_DIR_REC = 9
        const APFS_TYPE_DIR_REC_VAL: u64 = 9;

        let bsz = 4096usize;
        let data_base = 56usize;

        // Build a non-leaf var-kv node with one entry (a fake child oid).
        // Non-leaf values in catalog: 8-byte virtual child oid.
        let child_oid: u64 = 0xDEAD; // won't be in omap → resolve returns None → oor()

        let jkey_word = (2u64 & OBJ_ID_MASK) | (APFS_TYPE_DIR_REC_VAL << OBJ_TYPE_SHIFT);
        let mut key = Vec::new();
        key.extend_from_slice(&jkey_word.to_le_bytes()); // 8 bytes j_key_t
        key.extend_from_slice(&0u16.to_le_bytes()); // 2-byte name_len=0
                                                    // name: empty (just NUL)
        key.push(0u8);
        let key_size = key.len();

        let mut val = Vec::new();
        val.extend_from_slice(&child_oid.to_le_bytes()); // 8-byte child oid
        let val_size = val.len();

        let toc_len = 8usize;
        let key_area_start = data_base + toc_len;
        let val_area_end = bsz;

        let mut b = vec![0u8; bsz];
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        // Non-leaf: level=1, NOT BTNODE_LEAF
        b[34..36].copy_from_slice(&1u16.to_le_bytes()); // level=1
        b[36..40].copy_from_slice(&1u32.to_le_bytes()); // nkeys=1
        b[40..42].copy_from_slice(&0u16.to_le_bytes()); // toc_off=0
        b[42..44].copy_from_slice(&(toc_len as u16).to_le_bytes());

        b[data_base..data_base + 2].copy_from_slice(&0u16.to_le_bytes());
        b[data_base + 2..data_base + 4].copy_from_slice(&(key_size as u16).to_le_bytes());
        b[data_base + 4..data_base + 6].copy_from_slice(&(val_size as u16).to_le_bytes());
        b[data_base + 6..data_base + 8].copy_from_slice(&(val_size as u16).to_le_bytes());

        b[key_area_start..key_area_start + key_size].copy_from_slice(&key);
        let v_abs = val_area_end - val_size;
        b[v_abs..v_abs + val_size].copy_from_slice(&val);

        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());

        // catalog_from_leaf builds Catalog with dummy_omap (empty, resolve → None)
        let cat = catalog_from_leaf(b);
        let mut dev = MemDev::new(4096);
        // list_dir calls for_each_leaf; non-leaf child resolution fails (omap returns None)
        // → oor() error propagated. The key thing: no panic, no infinite loop.
        let _ = cat.list_dir(&mut dev, 2, false); // ok if Err (oor), not ok if panic
    }

    #[test]
    fn catalog_open_rejects_bad_root_object_type() {
        // Build a real fixture-style open via MemDev with:
        //   - valid omap block at addr 1
        //   - omap tree root (empty FIXED_KV leaf) at addr 2
        //   - volume omap resolves root_tree_oid → paddr 3
        //   - block 3 has wrong object type (not OBJECT_TYPE_BTREE)
        use crate::btree::{BTNODE_FIXED_KV_SIZE, BTNODE_LEAF, BTNODE_ROOT};
        use crate::obj::{ObjPhys, OBJECT_TYPE_OMAP};
        use crate::volume::{VolumeSuperblock, APFS_MAGIC};

        // Build a VolumeSuperblock with omap_oid=1, root_tree_oid=42, xid=1.
        // We craft the raw block manually to control exact bytes.
        use crate::obj::OBJECT_TYPE_FS;
        let mut vsb_raw = [0u8; 4096];
        vsb_raw[24..28].copy_from_slice(&OBJECT_TYPE_FS.to_le_bytes()); // o_type
        vsb_raw[16..24].copy_from_slice(&1u64.to_le_bytes()); // xid=1
        vsb_raw[32..36].copy_from_slice(&APFS_MAGIC.to_le_bytes());
        vsb_raw[128..136].copy_from_slice(&1u64.to_le_bytes()); // omap_oid=1 (physical)
        vsb_raw[136..144].copy_from_slice(&42u64.to_le_bytes()); // root_tree_oid=42 (virtual)
                                                                 // uuid at 240 (16 bytes), name at 704 - leave as zeros, role at 964
        let ck = fletcher64(&vsb_raw);
        vsb_raw[0..8].copy_from_slice(&ck.to_le_bytes());
        let vol = VolumeSuperblock::parse(&vsb_raw).expect("parse vsb");

        // Build omap at block 1 (tree_oid=2)
        let mut omap_raw = vec![0u8; 4096];
        omap_raw[8..16].copy_from_slice(&1u64.to_le_bytes());
        omap_raw[24..28].copy_from_slice(&OBJECT_TYPE_OMAP.to_le_bytes());
        omap_raw[40..44]
            .copy_from_slice(&(crate::obj::OBJECT_TYPE_BTREE | 0x4000_0000).to_le_bytes());
        omap_raw[48..56].copy_from_slice(&2u64.to_le_bytes()); // tree_oid=2
        let ck2 = fletcher64(&omap_raw);
        omap_raw[0..8].copy_from_slice(&ck2.to_le_bytes());

        // Build FIXED_KV ROOT LEAF at block 2 with one entry: key(oid=42,xid=1)→paddr=3
        let mut root_raw = vec![0u8; 4096];
        root_raw[8..16].copy_from_slice(&2u64.to_le_bytes());
        root_raw[16..24].copy_from_slice(&1u64.to_le_bytes());
        root_raw[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        let flags = BTNODE_LEAF | BTNODE_FIXED_KV_SIZE | BTNODE_ROOT;
        root_raw[32..34].copy_from_slice(&flags.to_le_bytes());
        root_raw[36..40].copy_from_slice(&1u32.to_le_bytes()); // nkeys=1
        root_raw[40..42].copy_from_slice(&0u16.to_le_bytes()); // toc_off=0
        root_raw[42..44].copy_from_slice(&4u16.to_le_bytes()); // toc_len=4 (1 entry)
                                                               // DATA_BASE=56: kvoff_t { k_off=0, v_off=16 }
        root_raw[56..58].copy_from_slice(&0u16.to_le_bytes());
        root_raw[58..60].copy_from_slice(&16u16.to_le_bytes());
        // key at 60: omap_key {oid=42, xid=1}
        root_raw[60..68].copy_from_slice(&42u64.to_le_bytes());
        root_raw[68..76].copy_from_slice(&1u64.to_le_bytes());
        // ROOT: val_area_end = 4096-40=4056; val at 4056-16=4040: omap_val {flags=0,size=4096,paddr=3}
        root_raw[4040..4044].copy_from_slice(&0u32.to_le_bytes()); // flags=0 (not deleted)
        root_raw[4044..4048].copy_from_slice(&4096u32.to_le_bytes());
        root_raw[4048..4056].copy_from_slice(&3i64.to_le_bytes()); // paddr=3
        let ck3 = fletcher64(&root_raw);
        root_raw[0..8].copy_from_slice(&ck3.to_le_bytes());

        // Block 3: wrong object type (OBJECT_TYPE_OMAP instead of BTREE)
        let mut bad_cat_root = vec![0u8; 4096];
        bad_cat_root[8..16].copy_from_slice(&3u64.to_le_bytes());
        bad_cat_root[24..28].copy_from_slice(&OBJECT_TYPE_OMAP.to_le_bytes());
        let ck4 = fletcher64(&bad_cat_root);
        bad_cat_root[0..8].copy_from_slice(&ck4.to_le_bytes());

        // Assemble MemDev (block-addressed)
        struct AddrMemDev {
            blocks: std::collections::HashMap<u64, Vec<u8>>,
        }
        impl crate::block_device::BlockDevice for AddrMemDev {
            fn size(&self) -> u64 {
                4096 * 64
            }
            fn read_at(
                &mut self,
                off: u64,
                buf: &mut [u8],
            ) -> Result<(), crate::block_device::BlockError> {
                let addr = off / 4096;
                match self.blocks.get(&addr) {
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
        let mut dev = AddrMemDev {
            blocks: std::collections::HashMap::new(),
        };
        dev.blocks.insert(1, omap_raw);
        dev.blocks.insert(2, root_raw);
        dev.blocks.insert(3, bad_cat_root);

        let err = Catalog::open(&mut dev, &vol, 4096);
        assert!(
            err.is_err(),
            "wrong catalog root object type must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // list_xattrs: leaf with two xattr entries for the same inode.
    // -----------------------------------------------------------------------
    #[test]
    fn list_xattrs_multiple_entries_same_inode() {
        use crate::jkey::{APFS_TYPE_XATTR, OBJ_ID_MASK, OBJ_TYPE_SHIFT};
        use crate::xattr::XATTR_DATA_EMBEDDED;

        let inode: u64 = 77;
        let bsz = 4096usize;
        let data_base = 56usize;

        // Build two xattr entries in one leaf.
        let make_xattr_kv = |name: &[u8], payload: &[u8]| -> (Vec<u8>, Vec<u8>) {
            let jkey = (inode & OBJ_ID_MASK) | ((APFS_TYPE_XATTR as u64) << OBJ_TYPE_SHIFT);
            let mut k = Vec::new();
            k.extend_from_slice(&jkey.to_le_bytes());
            k.extend_from_slice(&((name.len() + 1) as u16).to_le_bytes()); // name_len incl NUL
            k.extend_from_slice(name);
            k.push(0u8); // NUL
            let mut v = Vec::new();
            v.extend_from_slice(&XATTR_DATA_EMBEDDED.to_le_bytes());
            v.extend_from_slice(&(payload.len() as u16).to_le_bytes());
            v.extend_from_slice(payload);
            (k, v)
        };

        let (k1, v1) = make_xattr_kv(b"user.alpha", b"aaa");
        let (k2, v2) = make_xattr_kv(b"user.beta", b"bb");

        // Two-entry var-kv leaf.
        let toc_len = 2 * 8; // 2 × kvloc_t (8 bytes each)
        let key_area_start = data_base + toc_len;
        let val_area_end = bsz;

        let k1_off = 0usize;
        let k2_off = k1.len();
        let v1_abs = val_area_end - v1.len();
        let v2_abs = v1_abs - v2.len();

        let mut b = vec![0u8; bsz];
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes());
        b[32..34].copy_from_slice(&BTNODE_LEAF.to_le_bytes());
        b[36..40].copy_from_slice(&2u32.to_le_bytes()); // nkeys=2
        b[40..42].copy_from_slice(&0u16.to_le_bytes());
        b[42..44].copy_from_slice(&(toc_len as u16).to_le_bytes());

        // toc entry 0
        b[data_base..data_base + 2].copy_from_slice(&(k1_off as u16).to_le_bytes());
        b[data_base + 2..data_base + 4].copy_from_slice(&(k1.len() as u16).to_le_bytes());
        b[data_base + 4..data_base + 6]
            .copy_from_slice(&((val_area_end - v1_abs) as u16).to_le_bytes());
        b[data_base + 6..data_base + 8].copy_from_slice(&(v1.len() as u16).to_le_bytes());
        // toc entry 1
        b[data_base + 8..data_base + 10].copy_from_slice(&(k2_off as u16).to_le_bytes());
        b[data_base + 10..data_base + 12].copy_from_slice(&(k2.len() as u16).to_le_bytes());
        b[data_base + 12..data_base + 14]
            .copy_from_slice(&((val_area_end - v2_abs) as u16).to_le_bytes());
        b[data_base + 14..data_base + 16].copy_from_slice(&(v2.len() as u16).to_le_bytes());

        b[key_area_start..key_area_start + k1.len()].copy_from_slice(&k1);
        b[key_area_start + k1.len()..key_area_start + k1.len() + k2.len()].copy_from_slice(&k2);
        b[v1_abs..v1_abs + v1.len()].copy_from_slice(&v1);
        b[v2_abs..v2_abs + v2.len()].copy_from_slice(&v2);

        let ck = fletcher64(&b);
        b[0..8].copy_from_slice(&ck.to_le_bytes());

        let cat = catalog_from_leaf(b);
        let mut dev = MemDev::new(4096);
        let entries = cat.list_xattrs(&mut dev, inode).expect("list_xattrs");
        assert_eq!(entries.len(), 2, "must find both xattr entries");
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"user.alpha"), "must contain user.alpha");
        assert!(names.contains(&"user.beta"), "must contain user.beta");
    }

    // -----------------------------------------------------------------------
    // get_xattr: name present but different inode → None (inode filter).
    // -----------------------------------------------------------------------
    #[test]
    fn get_xattr_different_inode_returns_none() {
        // Leaf has xattr for inode 55; query for inode 56 must return None.
        let raw = build_xattr_leaf(55, "user.x", b"data");
        let cat = catalog_from_leaf(raw);
        let mut dev = MemDev::new(4096);
        let result = cat.get_xattr(&mut dev, 56, "user.x").expect("no error");
        assert!(
            result.is_none(),
            "xattr keyed to different inode must return None"
        );
    }

    // -----------------------------------------------------------------------
    // get_xattr: correct inode, wrong name → None.
    // -----------------------------------------------------------------------
    #[test]
    fn get_xattr_wrong_name_same_inode_returns_none() {
        let raw = build_xattr_leaf(42, "user.present", b"data");
        let cat = catalog_from_leaf(raw);
        let mut dev = MemDev::new(4096);
        let result = cat
            .get_xattr(&mut dev, 42, "user.absent")
            .expect("no error");
        assert!(
            result.is_none(),
            "wrong name must return None even if inode matches"
        );
    }
}
