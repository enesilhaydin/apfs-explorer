//! apfs-write file-creation path (M7b).
//!
//! Builds catalog records (inode + directory entry) and inserts them into the
//! volume's fsroot B-tree via copy-on-write, so a new empty file becomes
//! visible to macOS and passes `fsck_apfs`.
//!
//! Verification-first: every mutation is proven on a scratch image with
//! fsck + mount-readback + an independent Python re-decode.

/// APFS directory-entry name hash (low 22 bits). The reader computes it
/// the same way to look a name up by key; both use
/// [`apfs_core::case_fold::apfs_name_hash`].
///
/// Canonical pipeline per Apple File System Reference (the APFS specification):
///   1. If the volume has APFS_INCOMPAT_NORMALIZATION_INSENSITIVE: NFD-decompose.
///   2. If the volume has APFS_INCOMPAT_CASE_INSENSITIVE: Unicode case-fold.
///   3. Encode the result as UTF-32-LE (one 4-byte LE word per scalar value).
///   4. CRC32C (Castagnoli, poly 0x82F63B78, init 0xFFFFFFFF, no final XOR)
///      over those bytes; the NUL terminator is NOT included.
///   5. Take the low 22 bits.
///
/// The STORED bytes on disk are the ORIGINAL UTF-8 (user's input), regardless
/// of normalization - only the hash uses the normalized form. Without NFD, a
/// kernel-formatted "café.txt" hashes differently from our raw .chars(),
/// making the DREC unfindable and fsck flagging the catalog corrupt.
pub fn apfs_name_hash(name: &str, case_fold: bool, normalize: bool) -> u32 {
    // A case-insensitive volume folds with the FULL Unicode case fold after
    // NFD - the fold macOS applies and fsck_apfs checks the hash against:
    // "straße" hashes as "strasse", a ligature as its letters, a final sigma
    // as the ordinary one. Lowercasing instead stored hashes macOS never
    // computes (measured: fsck_apfs "invalid hash ... of name (straße.txt)").
    apfs_core::case_fold::apfs_name_hash(name, case_fold, normalize)
}

/// Pack `name_len_and_hash`: high 22 bits = hash of the normalized+folded name,
/// low 10 bits = length of the STORED ORIGINAL UTF-8 bytes INCLUDING NUL.
/// (Per the APFS specification Q3/Q4 - stored bytes are not normalized.)
pub fn name_len_and_hash(name: &str, case_fold: bool, normalize: bool) -> u32 {
    let nlen = (name.len() + 1) as u32; // +1 for NUL; original UTF-8 length
    debug_assert!(nlen <= 0x3FF, "name too long for 10-bit length field");
    (apfs_name_hash(name, case_fold, normalize) << 10) | (nlen & 0x3FF)
}

// ---------------------------------------------------------------------------
// Catalog key/value constants and record builders.
// All offsets/sizes are empirically anchored to a kernel-produced empty
// regular file ("hello.txt") decoded from a scratch image (the APFS spec).
// ---------------------------------------------------------------------------

/// j_key_t object type values (top 4 bits of obj_id_and_type).
pub const APFS_TYPE_INODE: u64 = 3;
pub const APFS_TYPE_XATTR: u64 = 4;
pub const APFS_TYPE_SIBLING_LINK: u64 = 5;
/// `APFS_TYPE_SIBLING_MAP` - maps a sibling id back to the inode it names.
pub const APFS_TYPE_SIBLING_MAP: u64 = 12;
pub const APFS_TYPE_DIR_REC: u64 = 9;
pub const APFS_TYPE_FILE_EXTENT: u64 = 8;
pub const APFS_TYPE_EXTENT: u64 = 2; // physical extent record (extentref tree)
pub const APFS_TYPE_DSTREAM_ID: u64 = 6;

/// Directory entry record `flags` low 4 bits: file dirent type.
pub const DT_DIR: u16 = 4;
pub const DT_REG: u16 = 8;
pub const DT_LNK: u16 = 10;

/// Inode extended-field types.
const INO_EXT_TYPE_NAME: u8 = 4;
/// x_flags of the NAME xfield: the name does not follow the inode into a
/// clone (`XF_DO_NOT_COPY`, 0x02); a clone gets a name of its own.
const XF_DO_NOT_COPY: u8 = 0x02;

/// File mode bits.
pub const S_IFREG: u16 = 0o100000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFLNK: u16 = 0o120000;

/// j_xattr_flags bit for an inline (embedded) value.
const XATTR_DATA_EMBEDDED: u16 = 0x0002;
/// `XATTR_FILE_SYSTEM_OWNED`: the attribute belongs to the file system (a
/// symlink's target), not to the user, and is hidden from listings.
const XATTR_FILE_SYSTEM_OWNED: u16 = 0x0004;

/// j_xattr_flags bit for a stream (extent-backed) value.
const XATTR_DATA_STREAM: u16 = 0x0001;

/// Largest attribute value stored in its record; a larger one gets a stream.
/// Measured on macOS (`tests/xattr_embed_limit.rs`): 3800 bytes are embedded
/// and 3801 become a stream, which is the checker's 3804-byte limit on the
/// whole value less the record's four-byte header (length and flags).
/// [measured on macOS 15; apfsprogs raw.h APFS_XATTR_MAX_EMBEDDED_SIZE]
pub const XATTR_EMBEDDED_THRESHOLD: usize = apfs_core::xattr::XATTR_MAX_EMBEDDED_SIZE - 4;

/// Largest attribute record (key plus value) macOS stores with its value
/// embedded. A long name lowers the value limit below
/// [`XATTR_EMBEDDED_THRESHOLD`]: with a 127-byte name 3794 bytes are embedded
/// and 3795 become a stream, with a 124-byte name the cut is at 3797/3798,
/// both exactly key + 4-byte value header + value = 3936.
/// [measured on macOS 15, `tests/xattr_embed_limit.rs`]
const XATTR_EMBEDDED_RECORD_MAX: usize = 3936;

/// Whether a value of `value_len` bytes under an attribute key of `key_len`
/// bytes is stored in its record (otherwise it gets a stream).
fn xattr_value_embeds(key_len: usize, value_len: usize) -> bool {
    value_len <= XATTR_EMBEDDED_THRESHOLD
        && key_len.saturating_add(4).saturating_add(value_len) <= XATTR_EMBEDDED_RECORD_MAX
}

/// Reserved extended-attribute name used by APFS to store a symlink target.
pub const XATTR_NAME_SYMLINK: &str = "com.apple.fs.symlink";
/// The resource fork, stored as an attribute; the inode records whether it
/// has one (`INODE_HAS_RSRC_FORK` / `INODE_NO_RSRC_FORK`).
pub const XATTR_NAME_RESOURCE_FORK: &str = "com.apple.ResourceFork";
/// Finder info is a fixed 32-byte record; macOS refuses any other size
/// (measured: setxattr with 5 or 33 bytes fails with ERANGE).
pub const XATTR_NAME_FINDER_INFO: &str = "com.apple.FinderInfo";
pub const FINDER_INFO_LEN: usize = 32;
/// The longest attribute name macOS accepts (XATTR_MAXNAMELEN; measured: 127
/// bytes taken, 128 refused with ENAMETOOLONG).
pub const XATTR_NAME_MAX: usize = 127;
/// Names macOS reserves for itself and refuses from user space (EPERM).
pub const XATTR_SYSTEM_PREFIX: &str = "com.apple.system.";

/// Default inode `internal_flags` the kernel stamps on a freshly created
/// object (empirical: 0x8000 on both dir and regular-file inodes).
const INODE_DEFAULT_INTERNAL_FLAGS: u64 = 0x8000;
/// The longest symlink target macOS accepts: PATH_MAX (1024) less the
/// terminator; `ln -s` with 1024 bytes fails with ENAMETOOLONG. Anything
/// longer would also overflow the embedded attribute that stores it.
const SYMLINK_TARGET_MAX: usize = 1023;

/// `j_inode_val.internal_flags` bits set on cloned inodes (clonefile).
/// [linux-apfs-rw apfs_raw.h:343/349; IDA apfs.kext clone_item - an audit pass.]
const APFS_INODE_WAS_CLONED: u64 = 0x0000_0010;
const APFS_INODE_WAS_EVER_CLONED: u64 = 0x0000_0400;

/// Encode a j_key_t header: type in top 4 bits, object id in low 60 bits.
pub fn encode_jkey(obj_id: u64, obj_type: u64) -> u64 {
    (obj_type << 60) | (obj_id & 0x0FFF_FFFF_FFFF_FFFF)
}

fn round_up8(x: usize) -> usize {
    (x + 7) & !7
}

/// Build a hashed directory-entry record KEY for `name` under directory
/// `dir_ino`. Layout: jkey(8) + name_len_and_hash(u32) + name(UTF-8 + NUL).
pub fn build_drec_key(dir_ino: u64, name: &str, case_fold: bool, normalize: bool) -> Vec<u8> {
    let mut k = Vec::with_capacity(12 + name.len() + 1);
    k.extend_from_slice(&encode_jkey(dir_ino, APFS_TYPE_DIR_REC).to_le_bytes());
    k.extend_from_slice(&name_len_and_hash(name, case_fold, normalize).to_le_bytes());
    k.extend_from_slice(name.as_bytes());
    k.push(0u8);
    k
}

/// Build a directory-entry record VALUE (j_drec_val, 18 bytes):
/// file_id(u64) + date_added(u64) + flags(u16, low 4 bits = dirent type).
pub fn build_drec_val(file_id: u64, date_added: u64, dt_type: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(18);
    v.extend_from_slice(&file_id.to_le_bytes());
    v.extend_from_slice(&date_added.to_le_bytes());
    v.extend_from_slice(&dt_type.to_le_bytes());
    v
}

/// Build an INODE key (j_inode: just the jkey header, 8 bytes).
pub fn build_inode_key(ino: u64) -> Vec<u8> {
    encode_jkey(ino, APFS_TYPE_INODE).to_le_bytes().to_vec()
}

/// Build a FILE_EXTENT key: jkey(type=8, id=inode) + logical_addr(u64).
pub fn build_file_extent_key(ino: u64, logical_addr: u64) -> Vec<u8> {
    let mut k = Vec::with_capacity(16);
    k.extend_from_slice(&encode_jkey(ino, APFS_TYPE_FILE_EXTENT).to_le_bytes());
    k.extend_from_slice(&logical_addr.to_le_bytes());
    k
}

/// Build a FILE_EXTENT value (24 bytes): len_and_kind(u64, byte length in
/// bits\[55:0\]) + phys_block_num(u64) + crypto_id(u64)=0.
pub fn build_file_extent_val(byte_len: u64, phys_block: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(24);
    v.extend_from_slice(&byte_len.to_le_bytes()); // kind/flags high bits = 0
    v.extend_from_slice(&phys_block.to_le_bytes());
    v.extend_from_slice(&0u64.to_le_bytes()); // crypto_id
    v
}

/// Build a DSTREAM_ID key: jkey(type=6, id=inode).
/// Build a directory-entry value that also carries the sibling id binding this
/// name to the file, which is what marks it as one of several.
///
/// The extended field sits directly after the 18-byte fixed part: a two-u16
/// header (one field, eight bytes of payload), one 4-byte descriptor, then the
/// id itself.
pub fn build_drec_val_with_sibling(
    file_id: u64,
    date_added: u64,
    dt_type: u16,
    sibling_id: u64,
) -> Vec<u8> {
    const DREC_EXT_TYPE_SIBLING_ID: u8 = 1;
    let mut v = build_drec_val(file_id, date_added, dt_type);
    v.extend_from_slice(&1u16.to_le_bytes()); // xf_num_exts
    v.extend_from_slice(&8u16.to_le_bytes()); // xf_used_data
    v.extend_from_slice(&[DREC_EXT_TYPE_SIBLING_ID, 0]); // x_type, x_flags
    v.extend_from_slice(&8u16.to_le_bytes()); // x_size
    v.extend_from_slice(&sibling_id.to_le_bytes());
    v
}

/// `j_sibling_link_val`: the directory holding this name, then the name itself
/// with its NUL counted in the length.
pub fn build_sibling_link_val(parent_id: u64, name: &str) -> Vec<u8> {
    let name_len = name.len() + 1;
    let mut v = Vec::with_capacity(10 + name_len);
    v.extend_from_slice(&parent_id.to_le_bytes());
    v.extend_from_slice(&(name_len as u16).to_le_bytes());
    v.extend_from_slice(name.as_bytes());
    v.push(0);
    v
}

/// `j_sibling_map_val`: the inode the sibling id names.
pub fn build_sibling_map_val(file_id: u64) -> Vec<u8> {
    file_id.to_le_bytes().to_vec()
}

/// `j_sibling_link_key`: the inode's id under the sibling-link type, followed
/// by the sibling id the name belongs to.
pub fn build_sibling_link_key(ino: u64, sibling_id: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(16);
    v.extend_from_slice(&encode_jkey(ino, APFS_TYPE_SIBLING_LINK).to_le_bytes());
    v.extend_from_slice(&sibling_id.to_le_bytes());
    v
}

/// `j_sibling_map_key`: the sibling id is the whole key.
pub fn build_sibling_map_key(sibling_id: u64) -> Vec<u8> {
    encode_jkey(sibling_id, APFS_TYPE_SIBLING_MAP)
        .to_le_bytes()
        .to_vec()
}

/// The sibling id a directory entry carries when it is one of several names
/// for the same file, or `None` when it is the only name.
///
/// The entry's extended fields start right after the 18-byte fixed part, in
/// the same shape the inode uses: a two-u16 header, then one 4-byte descriptor
/// per field, then the payloads padded to 8 bytes each.
fn drec_sibling_id(drec_val: &[u8]) -> Option<u64> {
    const DREC_XFIELDS_OFFSET: usize = 18;
    const DREC_EXT_TYPE_SIBLING_ID: u8 = 1;
    if drec_val.len() <= DREC_XFIELDS_OFFSET + 4 {
        return None;
    }
    let count = rd_u16(drec_val, DREC_XFIELDS_OFFSET) as usize;
    let mut entry = DREC_XFIELDS_OFFSET + 4;
    let mut data = entry + count * 4;
    for _ in 0..count {
        let x_type = *drec_val.get(entry)?;
        let x_size = rd_u16(drec_val, entry + 2) as usize;
        if x_type == DREC_EXT_TYPE_SIBLING_ID && x_size >= 8 {
            return Some(rd_u64(drec_val, data));
        }
        data += round_up8(x_size);
        entry += 4;
    }
    None
}

pub fn build_dstream_id_key(ino: u64) -> Vec<u8> {
    encode_jkey(ino, APFS_TYPE_DSTREAM_ID)
        .to_le_bytes()
        .to_vec()
}

/// Build a DSTREAM_ID value (j_dstream_id_val, 4 bytes): refcnt(u32).
pub fn build_dstream_id_val(refcnt: u32) -> Vec<u8> {
    refcnt.to_le_bytes().to_vec()
}

/// Build a physical extent (extentref tree) KEY: jkey(type=2, id=phys_block).
pub fn build_phys_ext_key(phys_block: u64) -> Vec<u8> {
    encode_jkey(phys_block, APFS_TYPE_EXTENT)
        .to_le_bytes()
        .to_vec()
}

/// Physical-extent kind (bits\[63:60\] of len_and_kind). New data extent.
pub const APFS_KIND_NEW: u64 = 1;
/// Physical-extent kind for a reference-count UPDATE record (signed refcnt
/// delta). Used when the LIVE volume drops a reference to an extent pinned by a
/// snapshot (and therefore absent from the live extentref tree).
/// [apfsprogs raw.h APFS_KIND_UPDATE = 2]
pub const APFS_KIND_UPDATE: u64 = 2;
/// Sentinel owner for an UPDATE record (the live volume is not the owner; the
/// snapshot is). [apfsprogs raw.h APFS_OWNING_OBJ_ID_INVALID = ~0]
pub const APFS_OWNING_OBJ_ID_INVALID: u64 = u64::MAX;

/// Build a KIND_UPDATE physical-extent VALUE recording a reference-count `diff`
/// (e.g. `-1`) for a snapshot-pinned extent the live volume just dereferenced.
/// Mirrors `apfs_create_update_pext` (linux-apfs-rw/extents.c): kind=UPDATE,
/// owner=INVALID, refcnt=diff. fsck reconciles the snapshot's KIND_NEW (+1)
/// against this UPDATE (-1) so the physical block is reclaimed only when the
/// snapshot is deleted.
pub fn build_phys_ext_update_val(block_count: u64, refcnt_diff: i32) -> Vec<u8> {
    let mut v = Vec::with_capacity(20);
    let len_and_kind = (APFS_KIND_UPDATE << 60) | (block_count & 0x0FFF_FFFF_FFFF_FFFF);
    v.extend_from_slice(&len_and_kind.to_le_bytes());
    v.extend_from_slice(&APFS_OWNING_OBJ_ID_INVALID.to_le_bytes());
    v.extend_from_slice(&(refcnt_diff as u32).to_le_bytes());
    v
}

/// Build a physical extent (extentref tree) VALUE (20 bytes):
/// len_and_kind(u64: kind\[63:60\] | block_count\[59:0\]) + owning_obj_id(u64) +
/// refcnt(u32).
pub fn build_phys_ext_val(block_count: u64, owning_obj_id: u64, refcnt: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(20);
    let len_and_kind = (APFS_KIND_NEW << 60) | (block_count & 0x0FFF_FFFF_FFFF_FFFF);
    v.extend_from_slice(&len_and_kind.to_le_bytes());
    v.extend_from_slice(&owning_obj_id.to_le_bytes());
    v.extend_from_slice(&refcnt.to_le_bytes());
    v
}

/// Kind of a physical-extent record: bits\[63:60\] of len_and_kind.
///
/// The kind decides how the refcnt field is read: a KIND_NEW record carries an
/// absolute reference count, a KIND_UPDATE record carries a SIGNED delta that
/// applies on top of the count a snapshot's own KIND_NEW record holds.
/// [apfsprogs apfsck/extents.c parse_phys_ext_record]
pub fn phys_ext_kind(val: &[u8]) -> u64 {
    rd_u64(val, 0) >> 60
}

/// Absolute reference count of a KIND_NEW physical-extent record.
pub fn phys_ext_refcnt(val: &[u8]) -> u32 {
    rd_u32(val, 16)
}

/// Signed reference delta of a KIND_UPDATE physical-extent record.
pub fn phys_ext_update_delta(val: &[u8]) -> i32 {
    rd_u32(val, 16) as i32
}

/// True when a file-extent's physical block number marks a sparse hole.
///
/// A hole owns no physical block, so it must never take part in reference
/// counting or block freeing: keying an extentref record at block 0 both
/// duplicates a key in the extentref tree and, once its count reaches zero,
/// hands the container superblock back to the allocator.
/// [linux-apfs-rw extents.c apfs_ext_is_hole]
pub fn is_sparse_hole(phys_block: u64) -> bool {
    phys_block == 0
}

/// Data-stream sizes for a file inode's DSTREAM xfield.
#[derive(Clone, Copy)]
pub struct DstreamArgs {
    pub size: u64,         // logical EOF
    pub alloced_size: u64, // block-aligned (sum of extent byte lengths)
}

/// Parameters for [`build_inode_val`].
pub struct InodeValArgs<'a> {
    pub parent_id: u64,
    pub private_id: u64,
    pub now_ns: u64,
    pub mode: u16,
    pub nlink_or_nchildren: u32,
    pub uid: u32,
    pub gid: u32,
    pub name: &'a str,
    /// `Some` for a file with content (adds the DSTREAM xfield); `None` for an
    /// empty file / directory.
    pub dstream: Option<DstreamArgs>,
}

const INO_EXT_TYPE_DSTREAM: u8 = 8;
const XF_SYSTEM_FIELD: u8 = 0x20; // x_flags for the DSTREAM xfield (empirical)
const DSTREAM_SIZE: usize = 40;

/// Build a j_inode_val for a freshly created object.
///
/// [the APFS spec LOCKED, empirical + the APFS specification] 92-byte fixed prefix +
/// xfield blob: always a NAME xfield (type 4), plus a DSTREAM xfield (type 8)
/// when `args.dstream` is set. Extended fields are emitted in ascending
/// x_type order (NAME before DSTREAM), the one order this writer keeps; the
/// checker does not require one.
pub fn build_inode_val(args: &InodeValArgs) -> Vec<u8> {
    let name_bytes_len = args.name.len() + 1; // incl NUL
    let name_area = round_up8(name_bytes_len);
    let nxf = if args.dstream.is_some() { 2 } else { 1 };
    let dstream_area = if args.dstream.is_some() {
        round_up8(DSTREAM_SIZE)
    } else {
        0
    };
    let xf_used = name_area + dstream_area;
    let xf_hdr = 4 + nxf * 4; // num/used (4) + x_field_t entries (4 each)
    let mut v = vec![0u8; 92 + xf_hdr + xf_used];
    wr_u64(&mut v, 0, args.parent_id);
    wr_u64(&mut v, 8, args.private_id);
    wr_u64(&mut v, 16, args.now_ns); // create_time
    wr_u64(&mut v, 24, args.now_ns); // mod_time
    wr_u64(&mut v, 32, args.now_ns); // change_time
    wr_u64(&mut v, 40, args.now_ns); // access_time
    wr_u64(&mut v, 48, INODE_DEFAULT_INTERNAL_FLAGS);
    wr_u32(&mut v, 56, args.nlink_or_nchildren);
    // default_protection_class @60 = 0
    wr_u32(&mut v, 64, 1); // write_generation_counter = 1
                           // bsd_flags @68 = 0
    wr_u32(&mut v, 72, args.uid);
    wr_u32(&mut v, 76, args.gid);
    wr_u16(&mut v, 80, args.mode);
    // pad1 @82 = 0, uncompressed_size @84 = 0
    // xfields blob @92:
    wr_u16(&mut v, 92, nxf as u16); // xf_num_exts
    wr_u16(&mut v, 94, xf_used as u16); // xf_used_data
                                        // x_field_t entries @96 (4 bytes each), data area follows after all entries.
    let data_base = 96 + nxf * 4;
    // NAME (type 4) first.
    wr_bytes(&mut v, 96, &[INO_EXT_TYPE_NAME, XF_DO_NOT_COPY]);
    wr_u16(&mut v, 98, name_bytes_len as u16);
    wr_bytes(&mut v, data_base, args.name.as_bytes());
    // DSTREAM (type 8) second.
    if let Some(ds) = args.dstream {
        wr_bytes(&mut v, 100, &[INO_EXT_TYPE_DSTREAM, XF_SYSTEM_FIELD]);
        wr_u16(&mut v, 102, DSTREAM_SIZE as u16);
        let ds_off = data_base + name_area;
        wr_u64(&mut v, ds_off, ds.size); // size
        wr_u64(&mut v, ds_off + 8, ds.alloced_size); // alloced_size
                                                     // default_crypto_id @+16 = 0
        wr_u64(&mut v, ds_off + 24, ds.size); // total_bytes_written = size
                                              // total_bytes_read @+32 = 0
    }
    v
}

use crate::txn::{
    update_checksum_in_place, Transaction, TxnError, OBJECT_TYPE_FS, OBJECT_TYPE_OMAP,
};
use apfs_core::block_device::WritableBlockDevice;
use std::collections::HashSet;

mod cow_tree;
use cow_tree::{remap_catalog_nodes, CowTree, EditBatch, EditOp, Shape};

// Volume superblock field offsets.
const VSBI_FS_ALLOC_COUNT: usize = 0x58;
const VSBI_NUM_DIRECTORIES: usize = 0xC0;
const VSBI_NUM_OTHER_FSOBJECTS: usize = 0xD0;
/// `apfs_num_symlinks` - symbolic links are counted here, not with the files.
const VSBI_NUM_SYMLINKS: usize = 0xC8;
const VSBI_OMAP_OID: usize = 0x80;
const VSBI_ROOT_TREE_OID: usize = 0x88;
const VSBI_EXTENTREF_TREE_OID: usize = 0x90;
const VSBI_INCOMPAT_FEATURES: usize = 0x38;
const VSBI_LAST_MOD_TIME: usize = 0x100;
const VSBI_NEXT_OBJ_ID: usize = 0xB0;
const VSBI_NUM_FILES: usize = 0xB8;
const VSBI_NUM_SNAPSHOTS: usize = 0xD8;
const APFS_INCOMPAT_CASE_INSENSITIVE: u64 = 0x1;
const APFS_INCOMPAT_NORMALIZATION_INSENSITIVE: u64 = 0x8;
/// Sealed (signed system) volume: file extents live in a separate fext tree
/// and the seal has to be broken before any mutation.
const APFS_INCOMPAT_SEALED_VOLUME: u64 = 0x20;
/// The incompatible feature bits whose on-disk consequences this writer
/// implements: the name hashing and comparison rules. Any other bit means the
/// volume relies on a format feature the writer knows nothing about (dataless
/// snapshots, a rolled encryption key, an incomplete restore, per-file keys,
/// preallocated extent records, a secondary root tree), and Apple's own
/// implementation refuses to mount a volume with a bit it does not know.
const APFS_INCOMPAT_WRITABLE_MASK: u64 =
    APFS_INCOMPAT_CASE_INSENSITIVE | APFS_INCOMPAT_NORMALIZATION_INSENSITIVE;
/// `apfs_fs_flags` offset in the volume superblock, and its "data is in the
/// clear" bit. A FileVault volume leaves the bit clear.
const VSBI_FS_FLAGS: usize = 0x108;
const APFS_FS_UNENCRYPTED: u64 = 0x1;

// j_inode_val prefix offsets used by the write guards.
const INOI_NLINK_OR_NCHILDREN: usize = 56;
const INOI_BSD_FLAGS: usize = 68;
/// UF_COMPRESSED: the file's content lives in the com.apple.decmpfs xattr and,
/// when it does not fit inline, in the resource fork - not in the data stream.
const UF_COMPRESSED: u32 = 0x20;

/// Refuse to mutate a volume whose state we cannot maintain.
///
/// A sealed volume keeps its extents in a separate fext tree behind a hash
/// seal we do not recompute; an encrypted volume stores ciphertext, so
/// plaintext extents written into it are unreadable by macOS; any other
/// incompatible feature bit the writer does not implement means a mutation
/// would ignore an invariant the volume depends on. In every case a write
/// would appear to succeed and corrupt the volume, so refuse it up front.
pub fn ensure_volume_mutable(vsb_raw: &[u8]) -> Result<(), TxnError> {
    let incompat = vsb_raw
        .get(VSBI_INCOMPAT_FEATURES..VSBI_INCOMPAT_FEATURES + 8)
        .ok_or_else(|| {
            TxnError::VolumeParse("volume superblock too short for incompat features".into())
        })?;
    let incompat = u64::from_le_bytes(incompat.try_into().unwrap_or([0u8; 8]));
    if incompat & APFS_INCOMPAT_SEALED_VOLUME != 0 {
        return Err(TxnError::Unsupported(
            "volume is sealed; breaking the seal is not implemented".into(),
        ));
    }
    let unknown = incompat & !APFS_INCOMPAT_WRITABLE_MASK;
    if unknown != 0 {
        return Err(TxnError::Unsupported(format!(
            "volume has incompatible feature bits {unknown:#x} this writer does not implement"
        )));
    }
    // Directory records are keyed by a name hash only on a case-insensitive or
    // normalization-insensitive volume; a volume with neither bit uses the
    // plain-name key layout, which this writer does not produce. Every volume
    // macOS formats sets one of the two.
    if incompat & (APFS_INCOMPAT_CASE_INSENSITIVE | APFS_INCOMPAT_NORMALIZATION_INSENSITIVE) == 0 {
        return Err(TxnError::Unsupported(
            "volume keys its directory records by name rather than by hash (neither the \
             case-insensitive nor the normalization-insensitive feature is set); writing \
             such records is not implemented"
                .into(),
        ));
    }
    let fs_flags = vsb_raw
        .get(VSBI_FS_FLAGS..VSBI_FS_FLAGS + 8)
        .ok_or_else(|| TxnError::VolumeParse("volume superblock too short for fs_flags".into()))?;
    let fs_flags = u64::from_le_bytes(fs_flags.try_into().unwrap_or([0u8; 8]));
    if fs_flags & APFS_FS_UNENCRYPTED == 0 {
        return Err(TxnError::Unsupported(
            "volume is encrypted (FileVault); writing plaintext extents would corrupt it".into(),
        ));
    }
    Ok(())
}

/// Read the `nlink`/`nchildren` u32 and the mode from a j_inode_val prefix.
fn inode_links_and_mode(inode_val: &[u8]) -> Result<(u32, u16), TxnError> {
    let nlink = inode_val
        .get(INOI_NLINK_OR_NCHILDREN..INOI_NLINK_OR_NCHILDREN + 4)
        .ok_or_else(|| TxnError::VolumeParse("inode value too short for nlink".into()))?;
    let mode = inode_val
        .get(INOI_MODE..INOI_MODE + 2)
        .ok_or_else(|| TxnError::VolumeParse("inode value too short for mode".into()))?;
    Ok((
        u32::from_le_bytes(nlink.try_into().unwrap_or([0u8; 4])),
        u16::from_le_bytes(mode.try_into().unwrap_or([0u8; 2])),
    ))
}

/// What taking one name off an inode removes from the volume, and what it
/// leaves changed. Shared by every path that drops a name: `unlink`, and the
/// rename, move and symlink paths that replace an existing entry.
struct RemovalPlan {
    /// Records to remove. The directory entry itself is not among them.
    remove_keys: Vec<Vec<u8>>,
    /// Blocks to release once the extentref counts allow.
    data_blocks: Vec<(u64, u64)>,
    /// Records whose value changes: a shared stream's count, a hard-linked
    /// inode re-pointed at its next name.
    updates: Vec<(Vec<u8>, Vec<u8>)>,
    is_dir: bool,
    dir_nchildren: u32,
    /// Whether the inode itself goes with this name.
    last_name: bool,
    /// The inode's mode, for the volume's per-kind counters.
    mode: u16,
}

/// Work out what removing the name `drec_val` describes for `file_id` means.
///
/// A file's content is not keyed by its inode but by its data-stream id
/// (`private_id`). Usually the two are equal; a clone macOS made shares the
/// original's stream, so the clone's inode names a stream keyed by another id,
/// and the stream's own record counts how many inodes name it. A last-name
/// removal therefore comes in parts: the records that are the inode's own, the
/// streams of its attributes (never shared), and the content stream - which
/// goes only with its last reference. A name that is one of several takes only
/// itself, its sibling records and one from the link count, re-pointing the
/// inode when it was the name the inode recorded for itself.
fn plan_removal(
    all: &[(Vec<u8>, Vec<u8>)],
    file_id: u64,
    drec_val: &[u8],
    now: u64,
    bsz: usize,
) -> Result<RemovalPlan, TxnError> {
    let inode_val = all
        .iter()
        .find(|(k, _)| {
            rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF == file_id && rd_u64(k, 0) >> 60 == APFS_TYPE_INODE
        })
        .map(|(_, v)| v.clone());
    let mode = inode_val.as_ref().map_or(0, |v| rd_u16(v, 80));
    let is_dir = (mode & 0o170000) == S_IFDIR;
    // For a directory inode, nchildren lives at INODE_NCHILDREN.
    let dir_nchildren = inode_val.as_ref().map_or(0, |v| rd_u32(v, INODE_NCHILDREN));
    let stream_id = match inode_val.as_ref().map(|v| rd_u64(v, INODE_PRIVATE_ID)) {
        Some(id) if id != 0 && !is_dir => id,
        _ => file_id,
    };

    let mut owned_keys: Vec<Vec<u8>> = Vec::new();
    let mut stream_keys: Vec<Vec<u8>> = Vec::new();
    // Each extent is (phys_start, block_count) - a FILE_EXTENT can span many
    // blocks, so we must free the WHOLE run, not just its first block.
    let mut data_blocks: Vec<(u64, u64)> = Vec::new();
    let mut stream_refcnt: u32 = 1;
    // An attribute value too large to sit inline has a stream of its own,
    // keyed by an id that is neither the file's nor its content stream's.
    let mut xattr_stream_ids: Vec<u64> = Vec::new();
    for (k, v) in all {
        let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
        let ty = rd_u64(k, 0) >> 60;
        let is_stream_record = ty == APFS_TYPE_DSTREAM_ID || ty == APFS_TYPE_FILE_EXTENT;
        if oid == stream_id && is_stream_record {
            stream_keys.push(k.clone());
            if ty == APFS_TYPE_DSTREAM_ID {
                stream_refcnt = rd_u32(v, 0);
            } else {
                // j_file_extent_val: len_and_flags (low 56 bits = byte length)
                // @0, phys_block_num @8. Free every block of the extent.
                let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
                let count = len_bytes / bsz as u64;
                data_blocks.push((rd_u64(v, 8), count));
            }
        } else if oid == file_id && !is_stream_record {
            owned_keys.push(k.clone());
            if ty == APFS_TYPE_XATTR && rd_u16(v, 0) & XATTR_DATA_STREAM != 0 {
                xattr_stream_ids.push(rd_u64(v, 4));
            }
        }
    }
    // A hard link's records come in pairs: the sibling link is keyed by the
    // inode and is among its own records above; the sibling map is keyed by
    // the sibling id and is not. When the inode goes, so must every map that
    // still points at it - the one of the name being removed included, since
    // the last name of a formerly linked file still carries its pair.
    // [CERTAIN: apfsprogs/apfsck/inode.c pairs every sibling map with a link;
    //  measured: macOS keeps the pair on the last surviving name]
    let mut sibling_map_keys: Vec<Vec<u8>> = all
        .iter()
        .filter(|(k, _)| {
            rd_u64(k, 0) >> 60 == APFS_TYPE_SIBLING_LINK
                && rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF == file_id
        })
        .map(|(k, _)| build_sibling_map_key(rd_u64(k, 8)))
        .collect();
    if let Some(sid) = drec_sibling_id(drec_val) {
        let key = build_sibling_map_key(sid);
        if !sibling_map_keys.contains(&key) {
            sibling_map_keys.push(key);
        }
    }
    // An attribute's stream belongs to this inode alone (attributes are never
    // cloned), so it goes whenever the inode does, whatever the content
    // stream's count says.
    // [CERTAIN: apfsprogs/apfsck/extents.c "xattrs can't be cloned"]
    let mut xattr_stream_keys: Vec<Vec<u8>> = Vec::new();
    let mut xattr_blocks: Vec<(u64, u64)> = Vec::new();
    for sid in &xattr_stream_ids {
        for (k, v) in all {
            if rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF != *sid {
                continue;
            }
            match rd_u64(k, 0) >> 60 {
                APFS_TYPE_DSTREAM_ID => xattr_stream_keys.push(k.clone()),
                APFS_TYPE_FILE_EXTENT => {
                    xattr_stream_keys.push(k.clone());
                    let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
                    xattr_blocks.push((rd_u64(v, 8), len_bytes.div_ceil(bsz as u64)));
                }
                _ => {}
            }
        }
    }

    // A file reached through several names loses one name, not the file. Its
    // inode, data stream and blocks stay; the entry, the two records binding
    // that entry to the inode, and one from the link count are what go.
    let links = match (&inode_val, is_dir) {
        (Some(iv), false) => inode_links_and_mode(iv)?.0,
        _ => 1,
    };
    let last_name = links <= 1;
    let mut remove_keys: Vec<Vec<u8>> = Vec::new();
    let mut updates: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    if last_name && stream_refcnt > 1 {
        // Another inode still names this stream. The inode goes; the stream
        // and its blocks stay, and the stream's count drops by one.
        // [CERTAIN: linux-apfs-rw/inode.c apfs_put_dstream_rec on delete;
        //  apfsprogs/apfsck/extents.c requires the stream's count to equal the
        //  number of inodes naming it]
        remove_keys.extend(owned_keys);
        remove_keys.extend(sibling_map_keys);
        remove_keys.extend(xattr_stream_keys);
        data_blocks = xattr_blocks;
        updates.push((
            build_dstream_id_key(stream_id),
            build_dstream_id_val(stream_refcnt - 1),
        ));
    } else if last_name {
        remove_keys.extend(owned_keys);
        remove_keys.extend(sibling_map_keys);
        remove_keys.extend(stream_keys);
        remove_keys.extend(xattr_stream_keys);
        data_blocks.extend(xattr_blocks);
    } else {
        data_blocks.clear();
        let mut updated = inode_val.clone().unwrap_or_default();
        if let Some(sibling_id) = drec_sibling_id(drec_val) {
            remove_keys.push(build_sibling_link_key(file_id, sibling_id));
            remove_keys.push(build_sibling_map_key(sibling_id));
            // The inode records one of its names as its own - the primary
            // link, the one with the lowest sibling id. When that is the name
            // going, the inode has to describe the next lowest instead, or it
            // goes on naming an entry that no longer exists. macOS does the
            // same; the reader's `primary_link` is how a test sees it.
            // [CERTAIN: apfsprogs/apfsck/inode.c free_inode_names - "wrong
            //  name for primary link" / "bad parent for primary link" against
            //  the first sibling in id order; measured on macOS 15.7]
            let mut remaining: Vec<(u64, u64, String)> = all
                .iter()
                .filter(|(k, _)| {
                    rd_u64(k, 0) >> 60 == APFS_TYPE_SIBLING_LINK
                        && rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF == file_id
                        && rd_u64(k, 8) != sibling_id
                })
                .filter_map(|(k, v)| {
                    let name_len = rd_u16(v, 8) as usize;
                    let raw = v.get(10..10 + name_len)?;
                    let bytes = raw.split(|&b| b == 0).next().unwrap_or(raw);
                    let name = std::str::from_utf8(bytes).ok()?.to_string();
                    Some((rd_u64(k, 8), rd_u64(v, 0), name))
                })
                .collect();
            remaining.sort_by_key(|(sid, _, _)| *sid);
            let removed_was_primary = remaining.iter().all(|(sid, _, _)| *sid > sibling_id);
            if removed_was_primary && !updated.is_empty() {
                if let Some((_, next_parent, next_name)) = remaining.first() {
                    updated = rebuild_inode_with_name(&updated, next_name, now);
                    wr_u64(&mut updated, 0, *next_parent);
                }
            }
        }
        if !updated.is_empty() {
            wr_u32(&mut updated, INODE_NCHILDREN, links.saturating_sub(1));
            wr_u64(&mut updated, INODE_CHANGE_TIME, now);
            updates.push((build_inode_key(file_id), updated));
        }
    }
    Ok(RemovalPlan {
        remove_keys,
        data_blocks,
        updates,
        is_dir,
        dir_nchildren,
        last_name,
        mode,
    })
}

/// Take the replaced entry off the volume's per-kind counters - when its
/// inode actually went. A name that was one of several leaves the inode and
/// the counters alone.
fn uncount_replaced(vsb: &mut [u8], replaced: &TargetReplace) {
    if !replaced.last_name {
        return;
    }
    let off = object_count_offset(replaced.mode);
    let n = rd_u64(vsb, off);
    wr_u64(vsb, off, n.saturating_sub(1));
}

/// The volume superblock counter an inode of `mode` is counted in:
/// directories, symlinks and regular files each have their own, everything
/// else (FIFOs, sockets, device nodes) shares the fourth.
fn object_count_offset(mode: u16) -> usize {
    match mode & 0o170000 {
        m if m == S_IFDIR => VSBI_NUM_DIRECTORIES,
        m if m == S_IFLNK => VSBI_NUM_SYMLINKS,
        m if m == S_IFREG => VSBI_NUM_FILES,
        _ => VSBI_NUM_OTHER_FSOBJECTS,
    }
}

// Inode value field offsets (subset needed for parent-dir update).
/// `j_inode_val.private_id`: the id the file's data stream is keyed by. Equal
/// to the inode's own id unless the file shares a stream with a clone.
const INODE_PRIVATE_ID: usize = 8;
const INODE_CREATE_TIME: usize = 16;
const INODE_MOD_TIME: usize = 24;
const INODE_ACCESS_TIME: usize = 40;
const INOI_MODE: usize = 80;
const INODE_CHANGE_TIME: usize = 32;
const INODE_NCHILDREN: usize = 56;
/// `j_inode_val.write_generation_counter`: stepped by one whenever an
/// inode's content changes - a file's data (a write, a truncation) or a
/// directory's entries (one added, removed or renamed). A new inode starts
/// at 1; a change to the mode, the attributes or the inode's own name leaves
/// it alone. [measured on macOS: `tests/write_generation.rs`]
const INODE_WRITE_GENERATION: usize = 64;

/// Step `val`'s write-generation counter for a content change.
fn step_write_generation(val: &mut [u8]) {
    let next = rd_u32(val, INODE_WRITE_GENERATION).wrapping_add(1);
    wr_u32(val, INODE_WRITE_GENERATION, next);
}

/// `val` with its write-generation counter stepped for a content change.
fn with_write_generation_step(mut val: Vec<u8>) -> Vec<u8> {
    step_write_generation(&mut val);
    val
}
/// `j_inode_val.internal_flags`.
const INODE_INTERNAL_FLAGS: usize = 48;
/// The data stream has holes; set exactly when the sparse-bytes xfield is
/// present (fsck: "wrong setting for sparse flag").
const INODE_IS_SPARSE: u64 = 0x0000_0200;
/// Flags the sparse-bytes xfield carries: a system field the children inherit.
/// [CERTAIN: linux-apfs-rw/inode.c apfs_inode_set_sparse_bytes]
const XF_SPARSE_BYTES_FLAGS: u8 = 0x28;
/// The file has a resource fork.
const INODE_HAS_RSRC_FORK: u64 = 0x4000;
/// `INODE_HAS_FINDER_INFO`: the inode carries a com.apple.FinderInfo attribute.
const INODE_HAS_FINDER_INFO: u64 = 0x100;
/// The file has no resource fork - the other half of the pair above; exactly
/// one of them describes any file.
const INODE_NO_RSRC_FORK: u64 = 0x8000;
/// `uncompressed_size` holds a meaningful value.
const INODE_HAS_UNCOMPRESSED_SIZE: u64 = 0x0004_0000;
/// `j_inode_val.uncompressed_size` - the decompressed length a compressed file
/// records alongside its decmpfs xattr.
const INODE_UNCOMPRESSED_SIZE: usize = 84;

fn rd_u64(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}
fn rd_u32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}
fn rd_u16(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2)
        .and_then(|s| s.try_into().ok())
        .map(u16::from_le_bytes)
        .unwrap_or(0)
}
fn wr_u64(b: &mut [u8], o: usize, v: u64) {
    if let Some(s) = b.get_mut(o..o + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}
fn wr_u32(b: &mut [u8], o: usize, v: u32) {
    if let Some(s) = b.get_mut(o..o + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}
fn wr_u16(b: &mut [u8], o: usize, v: u16) {
    if let Some(s) = b.get_mut(o..o + 2) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}
fn wr_bytes(b: &mut [u8], o: usize, src: &[u8]) {
    if let Some(s) = b.get_mut(o..o + src.len()) {
        s.copy_from_slice(src);
    }
}

/// UNIX-epoch nanoseconds (APFS timestamps are UNIX-epoch).
fn now_ns() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Create a new empty regular file `name` in directory `parent_ino`.
/// [the APFS specification]
pub fn create_file<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
) -> Result<u64, TxnError> {
    // A regular file inode stores nlink in the nchildren_or_nlink union; an empty
    // file is always link count 1 (its single directory entry). Passing 0 here
    // makes the APFS kernel driver treat the inode as orphaned/unlinked
    // (ls shows it with nlink=0 and open()/stat()/cat() fail with ENOENT)
    // even though fsck is happy. nchildren=0 is correct for mkdir but wrong
    // for a regular file.
    create_entry(
        txn,
        vsb_raw,
        vol_omap_raw,
        parent_ino,
        name,
        S_IFREG | 0o644,
        DT_REG,
        1,
        false,
    )
}

/// Build a j_xattr_key: `jkey(type=XATTR, id=inode)` + `name_len(u16, incl
/// trailing NUL)` + name bytes + NUL.
pub fn build_xattr_key(ino: u64, name: &str) -> Vec<u8> {
    let mut k = Vec::with_capacity(8 + 2 + name.len() + 1);
    k.extend_from_slice(&encode_jkey(ino, APFS_TYPE_XATTR).to_le_bytes());
    let name_len = (name.len() + 1) as u16;
    k.extend_from_slice(&name_len.to_le_bytes());
    k.extend_from_slice(name.as_bytes());
    k.push(0);
    k
}

/// Build a j_xattr_val carrying an inline (embedded) value: `flags(u16)` +
/// `xdata_len(u16)` + `xdata` bytes. Embedded values are size-limited to
/// `XATTR_MAX_EMBEDDED_SIZE` (apfs-core::xattr) - well above any symlink
/// target. Larger values would use the dstream encoding (XATTR_DATA_STREAM).
pub fn build_xattr_val_embedded(xdata: &[u8]) -> Vec<u8> {
    build_xattr_val_embedded_with_flags(xdata, XATTR_DATA_EMBEDDED)
}

/// An embedded attribute value that belongs to the file system rather than
/// to the user - a symlink's target - and is listed to nobody.
pub fn build_xattr_val_embedded_owned(xdata: &[u8]) -> Vec<u8> {
    build_xattr_val_embedded_with_flags(xdata, XATTR_DATA_EMBEDDED | XATTR_FILE_SYSTEM_OWNED)
}

fn build_xattr_val_embedded_with_flags(xdata: &[u8], flags: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + xdata.len());
    v.extend_from_slice(&flags.to_le_bytes());
    v.extend_from_slice(&(xdata.len() as u16).to_le_bytes());
    v.extend_from_slice(xdata);
    v
}

/// Build a j_xattr_val carrying a stream (extent-backed) value.
///
/// Layout (j_xattr_val_t + inline j_xattr_dstream_t, 52 bytes total):
///   flags      u16 = XATTR_DATA_STREAM (0x0001)
///   xdata_len  u16 = 48  (sizeof j_xattr_dstream_t)
///   xattr_obj_id u64     - the private id that keys the FILE_EXTENT records
///   size         u64     - logical byte length of the attribute value
///   alloced_size u64     - block-aligned allocated bytes
///   default_crypto_id u64 = 0
///   total_bytes_written u64 = 0
///   total_bytes_read    u64 = 0
/// [Derived from APFS spec j_xattr_val + j_dstream_t; cross-confirmed with
///  linux-apfs-rw xattr.c and apfs-fuse ApfsVolume.cpp]
pub fn build_xattr_val_stream(xattr_obj_id: u64, byte_len: u64, alloced_size: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(52);
    v.extend_from_slice(&XATTR_DATA_STREAM.to_le_bytes()); // flags
    v.extend_from_slice(&48u16.to_le_bytes()); // xdata_len = sizeof(j_xattr_dstream_t)
                                               // j_xattr_dstream_t (48 bytes):
    v.extend_from_slice(&xattr_obj_id.to_le_bytes()); // xattr_obj_id @0
    v.extend_from_slice(&byte_len.to_le_bytes()); // j_dstream_t.size @8
    v.extend_from_slice(&alloced_size.to_le_bytes()); // j_dstream_t.alloced_size @16
    v.extend_from_slice(&0u64.to_le_bytes()); // j_dstream_t.default_crypto_id @24
    v.extend_from_slice(&0u64.to_le_bytes()); // j_dstream_t.total_bytes_written @32
    v.extend_from_slice(&0u64.to_le_bytes()); // j_dstream_t.total_bytes_read @40
    v
}

/// Write an extended attribute `name` with `value` bytes on inode `inode_id`.
///
/// - If the value fits its record (`xattr_value_embeds`: at most
///   `XATTR_EMBEDDED_THRESHOLD` bytes and a record of at most 3936 bytes):
///   stores inline (embedded).
/// - Otherwise: allocates a private dstream, writes data blocks, and stores a
///   stream xattr record.
///
/// If an xattr with the same name already exists on the inode it is replaced:
/// - Embedded → new embedded: old record removed, new record inserted.
/// - Stream → any: old extents freed, old DSTREAM_ID removed, new record inserted.
///
/// The inode's atime/mtime are not modified (xattr writes are metadata-only
/// from the inode's perspective; callers can follow with `set_inode_attrs`).
///
/// Multi-extent (> one contiguous run) is supported up to the free-space
/// allocator's natural chunk limit. Single-transaction only.
/// Limitation: very large xattrs (> available contiguous free space) may
/// require multiple allocation chunks; this is handled via the alloc_blocks_run
/// loop (same as write_file). Multi-GB xattrs are out of scope (M10 deferred).
/// [Derived from APFS spec - j_xattr_val + j_xattr_dstream layout]
/// The longest name macOS accepts for a directory entry, in characters:
/// 255 ASCII bytes are taken and 256 refused (ENAMETOOLONG), while 128
/// two-byte characters (256 bytes of UTF-8) are taken (measured on macOS 15).
pub const ENTRY_NAME_MAX_CHARS: usize = 255;
/// The directory record stores the name's length, terminator included, in
/// ten bits.
const ENTRY_NAME_MAX_BYTES: usize = 0x3FF - 1;

/// Refuse a directory-entry name macOS would refuse, before anything is
/// staged: empty, a slash or a NUL inside, more than 255 characters, or more
/// bytes than the record's length field can say.
pub fn validate_entry_name(name: &str) -> Result<(), TxnError> {
    if name.is_empty() {
        return Err(TxnError::InvalidArgument("name must not be empty".into()));
    }
    if name.bytes().any(|b| b == b'/' || b == 0) {
        return Err(TxnError::InvalidArgument(format!(
            "name {name:?} contains a slash or a NUL byte"
        )));
    }
    if name.chars().count() > ENTRY_NAME_MAX_CHARS {
        return Err(TxnError::InvalidArgument(format!(
            "name of {} characters exceeds the {ENTRY_NAME_MAX_CHARS}-character limit macOS \
             enforces (ENAMETOOLONG)",
            name.chars().count()
        )));
    }
    if name.len() > ENTRY_NAME_MAX_BYTES {
        return Err(TxnError::InvalidArgument(format!(
            "name of {} bytes does not fit a directory record",
            name.len()
        )));
    }
    Ok(())
}

/// Refuse an attribute name macOS would refuse, before anything is staged.
fn validate_xattr_name(name: &str) -> Result<(), TxnError> {
    if name.is_empty() {
        return Err(TxnError::InvalidArgument(
            "attribute name must not be empty".into(),
        ));
    }
    if name.len() > XATTR_NAME_MAX {
        return Err(TxnError::InvalidArgument(format!(
            "attribute name of {} bytes exceeds the {XATTR_NAME_MAX}-byte limit macOS enforces",
            name.len()
        )));
    }
    if name.starts_with(XATTR_SYSTEM_PREFIX) {
        return Err(TxnError::InvalidArgument(format!(
            "attribute {name:?} is reserved for the system; macOS refuses it from user space"
        )));
    }
    // The symlink's target lives in an attribute the file system owns; a
    // caller writing or removing it by name would turn a link into an inode
    // with no target (or a file into a half-link).
    if name == XATTR_NAME_SYMLINK {
        return Err(TxnError::InvalidArgument(format!(
            "attribute {name:?} holds a symlink's target and is owned by the file system"
        )));
    }
    Ok(())
}

/// The internal-flag change an attribute's arrival (`present`) or removal
/// implies on its inode: `(bits to set, bits to clear)`, or `None` when the
/// attribute leaves the inode alone.
///
/// [CERTAIN: measured on macOS 15 - a file given com.apple.ResourceFork
///  carries internal flags 0x4000 (HAS_RSRC_FORK), one without carries
///  0x8000 (NO_RSRC_FORK); fsck_apfs on a file given com.apple.FinderInfo
///  without the flag: "finder info internal flag is missing, need to set
///  internal_flags 0x100"; apfsprogs/apfsck/inode.c cross-checks both]
fn inode_flags_for_xattr(name: &str, present: bool) -> (u64, u64) {
    match (name, present) {
        (XATTR_NAME_RESOURCE_FORK, true) => (INODE_HAS_RSRC_FORK, INODE_NO_RSRC_FORK),
        (XATTR_NAME_RESOURCE_FORK, false) => (INODE_NO_RSRC_FORK, INODE_HAS_RSRC_FORK),
        (XATTR_NAME_FINDER_INFO, true) => (INODE_HAS_FINDER_INFO, 0),
        (XATTR_NAME_FINDER_INFO, false) => (0, INODE_HAS_FINDER_INFO),
        // Any other attribute leaves the flags alone; the inode is still
        // rewritten for its change time, which macOS moves on every
        // attribute set or removal (measured in `tests/operation_times.rs`).
        _ => (0, 0),
    }
}

/// The inode record of `inode_id` with `set` bits set and `clear` bits
/// cleared in its internal flags and its change time bumped: `(key, value)`.
fn inode_with_flags(
    all: &[(Vec<u8>, Vec<u8>)],
    inode_id: u64,
    set: u64,
    clear: u64,
    now: u64,
) -> Result<(Vec<u8>, Vec<u8>), TxnError> {
    let key = build_inode_key(inode_id);
    let mut val = all
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::NotFound(format!("inode {inode_id}")))?;
    if val.len() < INODE_INTERNAL_FLAGS + 8 {
        return Err(TxnError::VolumeParse(format!(
            "inode {inode_id} record too short"
        )));
    }
    let flags = (rd_u64(&val, INODE_INTERNAL_FLAGS) & !clear) | set;
    wr_u64(&mut val, INODE_INTERNAL_FLAGS, flags);
    wr_u64(&mut val, INODE_CHANGE_TIME, now);
    Ok((key, val))
}

pub fn set_xattr<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    inode_id: u64,
    name: &str,
    value: &[u8],
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    validate_xattr_name(name)?;
    ensure_volume_mutable(vsb_raw)?;
    if name == XATTR_NAME_FINDER_INFO && value.len() != FINDER_INFO_LEN {
        return Err(TxnError::InvalidArgument(format!(
            "Finder info is a {FINDER_INFO_LEN}-byte record; {} bytes were given",
            value.len()
        )));
    }

    // Read the volume omap node.
    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;

    // Gather the inode's catalog records.
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let all = gather_object(txn, &omap_node, root_tree_oid, inode_id, bsz)?;

    // Scan for existing xattr record with matching name on inode_id.
    let target_xattr_key = build_xattr_key(inode_id, name);
    let mut old_xattr_stream_id: Option<u64> = None; // set if old record was a stream
    let mut old_stream_extents: Vec<(u64, u64, u64)> = Vec::new(); // (logical_addr, phys, blocks)
    let mut old_dstream_id_key: Option<Vec<u8>> = None;

    for (k, v) in &all {
        // Check for old xattr record matching (inode_id, name).
        if k == &target_xattr_key {
            // Check if old xattr was stream type.
            if let Some(flag_bytes) = v.get(0..2) {
                let flags = u16::from_le_bytes(flag_bytes.try_into().unwrap_or([0; 2]));
                if flags & XATTR_DATA_STREAM != 0 && v.len() >= 12 {
                    // xdata starts at byte 4; xattr_obj_id at xdata[0..8].
                    let obj_id = u64::from_le_bytes(
                        v.get(4..12)
                            .and_then(|s| s.try_into().ok())
                            .ok_or_else(|| TxnError::NotFound("xattr stream id".into()))?,
                    );
                    old_xattr_stream_id = Some(obj_id);
                }
            }
        }
        // Collect extents belonging to the old stream xattr (keyed by old_xattr_stream_id).
        // We do a second pass below once we know the id.
    }

    // Second pass: if old xattr was stream, collect its extents and dstream_id record.
    if let Some(sid) = old_xattr_stream_id {
        for (k, v) in &all {
            let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
            let ty = rd_u64(k, 0) >> 60;
            if oid != sid {
                continue;
            }
            if ty == APFS_TYPE_FILE_EXTENT {
                let logical_addr = rd_u64(k, 8);
                let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
                let count = len_bytes.div_ceil(bsz as u64);
                let phys = rd_u64(v, 8);
                old_stream_extents.push((logical_addr, phys, count));
            }
            if ty == APFS_TYPE_DSTREAM_ID {
                old_dstream_id_key = Some(k.clone());
            }
        }
    }

    // Release the old stream's blocks (stream xattr replace/delete) the way
    // unlink releases a file's: a clone may share them, a snapshot may own
    // them, and a run may span several records.
    let old_extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    let old_runs: Vec<(u64, u64)> = old_stream_extents
        .iter()
        .map(|&(_, phys, count)| (phys, count))
        .collect();
    let release = release_runs(txn, old_extref_paddr, &old_runs, bsz)?;
    let freed_block_count: u64 = release.owned_runs.iter().map(|&(_, n)| n).sum();
    let ExtrefRelease {
        remove_keys: extref_remove_keys,
        upserts: extref_upserts,
        update_inserts: extref_update_inserts,
        owned_runs: owned_runs_to_free,
    } = release;

    // Build remove_keys: old xattr record + old dstream_id + old file_extents.
    let mut remove_keys: Vec<Vec<u8>> = Vec::new();
    // Remove old xattr key only if it existed.
    let old_xattr_existed = all.iter().any(|(k, _)| k == &target_xattr_key);
    if old_xattr_existed {
        remove_keys.push(target_xattr_key.clone());
    }
    if let Some(ref dk) = old_dstream_id_key {
        remove_keys.push(dk.clone());
    }
    for &(logical_addr, _, _) in &old_stream_extents {
        if let Some(sid) = old_xattr_stream_id {
            remove_keys.push(build_file_extent_key(sid, logical_addr));
        }
    }

    // Build new records.
    let mut new_records: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut new_block_count = 0u64;
    // New phys_ext records to insert into the extref tree.
    let mut extref_inserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

    let mut allocated_stream_id: Option<u64> = None;
    // A resource fork always lives in a stream, however short: fsck_apfs
    // rejects an embedded one ("com.apple.ResourceFork is expected to be
    // stream based").
    let embed =
        xattr_value_embeds(target_xattr_key.len(), value.len()) && name != XATTR_NAME_RESOURCE_FORK;
    if embed {
        // Embedded path: inline data, no extents.
        new_records.push((target_xattr_key, build_xattr_val_embedded(value)));
    } else {
        // Stream path: allocate a private dstream id, write data blocks, build
        // extents. The id names a filesystem object - it ends up as a data
        // stream id and as an extent record's owner - so it comes from the
        // volume's own pool, not the container's object-id counter. Taking it
        // from the container's left apfs_next_obj_id behind the ids in use and
        // fsck called the volume "corrupt and cannot be repaired"
        // ("apfs_next_obj_id is not valid (expected 1031, actual 17)").
        let stream_id = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);
        allocated_stream_id = Some(stream_id);
        let num_blocks = value.len().div_ceil(bsz);
        let mut runs: Vec<(usize, u64, usize)> = Vec::new(); // (logical_blk, phys_start, count)
        let mut blocks_remaining = num_blocks;

        while blocks_remaining > 0 {
            let (run_start, run_count) = txn.alloc_blocks_run(blocks_remaining)?;
            let logical_blk = num_blocks - blocks_remaining;
            // Write data for this run.
            for j in 0..run_count {
                let blk_idx = logical_blk + j;
                let p = run_start + j as u64;
                let mut buf = vec![0u8; bsz];
                let src_start = blk_idx * bsz;
                let src_end = ((blk_idx + 1) * bsz).min(value.len());
                buf[..src_end - src_start].copy_from_slice(&value[src_start..src_end]);
                txn.stage_raw(p, buf);
            }
            runs.push((logical_blk, run_start, run_count));
            blocks_remaining -= run_count;
        }
        new_block_count = num_blocks as u64;
        let alloced_size = (num_blocks * bsz) as u64;

        // Build xattr val (stream pointer).
        new_records.push((
            target_xattr_key,
            build_xattr_val_stream(stream_id, value.len() as u64, alloced_size),
        ));
        // No data-stream-id record: an attribute's stream is never shared,
        // so there is no count to keep. macOS writes none for one (measured:
        // a 10000-byte attribute it set added no such record) and the
        // checker reports one as an attribute that "can't be cloned".
        // FILE_EXTENT records (one per run).
        for &(lblk, pstart, cnt) in &runs {
            new_records.push((
                build_file_extent_key(stream_id, (lblk * bsz) as u64),
                build_file_extent_val((cnt * bsz) as u64, pstart),
            ));
        }

        // Collect phys_ext inserts for the extentref tree.
        for &(_, pstart, cnt) in &runs {
            extref_inserts.push((
                build_phys_ext_key(pstart),
                build_phys_ext_val(cnt as u64, stream_id, 1),
            ));
        }
    }

    // Rewrite the extentref tree (multi-node capable). [#151]
    extref_inserts.extend(extref_update_inserts);
    let extref_changed =
        !extref_inserts.is_empty() || !extref_remove_keys.is_empty() || !extref_upserts.is_empty();
    let (new_extref_paddr, old_extref_padrs, extref_node_delta) = if extref_changed {
        rewrite_extref_tree(
            txn,
            old_extref_paddr,
            new_xid,
            extref_inserts,
            &extref_remove_keys,
            &extref_upserts,
            bsz,
        )?
    } else {
        (old_extref_paddr, vec![], 0i64)
    };

    // Free old data blocks (after extref tree is rebuilt, before fstree).
    for (phys, count) in &owned_runs_to_free {
        for i in 0..*count {
            txn.free_block(phys + i)?;
        }
    }

    // The inode's change time moves with every attribute; a resource fork or
    // Finder info is recorded in its flags as well.
    {
        let (set, clear) = inode_flags_for_xattr(name, true);
        let (inode_key, inode_val) = inode_with_flags(&all, inode_id, set, clear, now)?;
        remove_keys.push(inode_key.clone());
        new_records.push((inode_key, inode_val));
    }

    // Rewrite the fstree.
    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        new_records,
        &remove_keys,
        None,
        now,
        bsz,
    )?;

    // COW the volume omap header.
    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    // Free replaced metadata BEFORE writing fs_alloc_count so frm_correction
    // (sm_fq-pinned block count) can be folded into alloc_delta. [#149 fix]
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &old_extref_padrs,
        &reclaim,
        bsz,
    )?;

    // Update and stage the volume superblock.
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    // A stream value took an id out of the volume's pool; the counter has to
    // move past it or the next allocation hands the same id out again.
    if let Some(stream_id) = allocated_stream_id {
        wr_u64(&mut new_vsb, VSBI_NEXT_OBJ_ID, stream_id + 1);
    }
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let alloc_delta = new_block_count as i64 - freed_block_count as i64
        + node_delta
        + extref_node_delta  // extref tree node count change [#151]
        + frm_correction;
    wr_u64(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (fs_alloc + alloc_delta).max(0) as u64,
    );
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    Ok(())
}

/// Delete an extended attribute `name` from inode `inode_id`.
///
/// For stream xattrs: frees allocated data blocks, removes DSTREAM_ID and
/// FILE_EXTENT records from the catalog, and removes the phys_ext entries
/// from the extentref tree.
/// For embedded xattrs: just removes the xattr catalog record.
/// Returns `Ok(())` if the xattr did not exist (idempotent).
/// [Derived from APFS spec - j_xattr_val + j_xattr_dstream layout]
pub fn delete_xattr<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    inode_id: u64,
    name: &str,
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    validate_xattr_name(name)?;
    ensure_volume_mutable(vsb_raw)?;
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;

    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let all = gather_object(txn, &omap_node, root_tree_oid, inode_id, bsz)?;

    let target_xattr_key = build_xattr_key(inode_id, name);
    // If no such xattr, return Ok (idempotent).
    let xattr_val_raw = match all.iter().find(|(k, _)| k == &target_xattr_key) {
        Some((_, v)) => v.clone(),
        None => return Ok(()),
    };

    let mut old_stream_id: Option<u64> = None;
    let mut old_stream_extents: Vec<(u64, u64, u64)> = Vec::new(); // (logical, phys, blocks)
    let mut old_dstream_key: Option<Vec<u8>> = None;

    if let Some(flag_bytes) = xattr_val_raw.get(0..2) {
        let flags = u16::from_le_bytes(flag_bytes.try_into().unwrap_or([0; 2]));
        if flags & XATTR_DATA_STREAM != 0 && xattr_val_raw.len() >= 12 {
            let sid = u64::from_le_bytes(
                xattr_val_raw
                    .get(4..12)
                    .and_then(|s| s.try_into().ok())
                    .ok_or_else(|| TxnError::NotFound("xattr stream id".into()))?,
            );
            old_stream_id = Some(sid);
            for (k, v) in &all {
                let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
                let ty = rd_u64(k, 0) >> 60;
                if oid != sid {
                    continue;
                }
                if ty == APFS_TYPE_FILE_EXTENT {
                    let logical_addr = rd_u64(k, 8);
                    let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
                    let count = len_bytes.div_ceil(bsz as u64);
                    let phys = rd_u64(v, 8);
                    old_stream_extents.push((logical_addr, phys, count));
                }
                if ty == APFS_TYPE_DSTREAM_ID {
                    old_dstream_key = Some(k.clone());
                }
            }
        }
    }

    // Build remove_keys.
    let mut remove_keys: Vec<Vec<u8>> = vec![target_xattr_key];
    if let Some(ref dk) = old_dstream_key {
        remove_keys.push(dk.clone());
    }
    for &(logical_addr, _, _) in &old_stream_extents {
        if let Some(sid) = old_stream_id {
            remove_keys.push(build_file_extent_key(sid, logical_addr));
        }
    }

    // Release the stream's blocks (if a stream) the way unlink releases a
    // file's: a clone may share them, a snapshot may own them, and a run may
    // span several records.
    let old_extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    let old_runs: Vec<(u64, u64)> = old_stream_extents
        .iter()
        .map(|&(_, phys, count)| (phys, count))
        .collect();
    let release = release_runs(txn, old_extref_paddr, &old_runs, bsz)?;
    let freed_block_count: u64 = release.owned_runs.iter().map(|&(_, n)| n).sum();
    let ExtrefRelease {
        remove_keys: extref_remove_keys,
        upserts: extref_upserts,
        update_inserts: extref_update_inserts,
        owned_runs: owned_runs_to_free,
    } = release;

    // Rewrite extref tree (multi-node capable). [#151]
    let extref_changed = !extref_remove_keys.is_empty()
        || !extref_upserts.is_empty()
        || !extref_update_inserts.is_empty();
    let (new_extref_paddr, old_extref_padrs, extref_node_delta) = if extref_changed {
        rewrite_extref_tree(
            txn,
            old_extref_paddr,
            new_xid,
            extref_update_inserts,
            &extref_remove_keys,
            &extref_upserts,
            bsz,
        )?
    } else {
        (old_extref_paddr, vec![], 0i64)
    };

    // Free old data blocks after extref tree is rebuilt.
    for (phys, count) in &owned_runs_to_free {
        for i in 0..*count {
            txn.free_block(phys + i)?;
        }
    }

    // The inode's change time moves with every attribute; taking a resource
    // fork or Finder info away is recorded in its flags as well.
    let mut new_records: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    {
        let (set, clear) = inode_flags_for_xattr(name, false);
        let (inode_key, inode_val) = inode_with_flags(&all, inode_id, set, clear, now)?;
        remove_keys.push(inode_key.clone());
        new_records.push((inode_key, inode_val));
    }

    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        new_records,
        &remove_keys,
        None,
        now,
        bsz,
    )?;

    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &old_extref_padrs,
        &reclaim,
        bsz,
    )?;

    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let alloc_delta = -(freed_block_count as i64) + node_delta + extref_node_delta + frm_correction;
    wr_u64(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (fs_alloc + alloc_delta).max(0) as u64,
    );
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    Ok(())
}

/// Create a symbolic link `name` in `parent_ino` whose target is `target`.
/// APFS represents symlinks as a `j_inode` with mode bits `S_IFLNK | 0o777`
/// (Apple's default), `nlink = 1`, and no dstream, plus a separate `j_xattr`
/// record named `com.apple.fs.symlink` whose embedded value is the target
/// path bytes followed by a NUL terminator - readers take the value length
/// minus that terminator. (Cross-checked against linux-apfs-rw symlink.c.)
/// The DREC uses `dt_type = DT_LNK (10)`.
///
/// `target` must be non-empty and free of interior NUL bytes: an empty target
/// yields a link no reader can resolve, and an interior NUL would truncate the
/// stored path at that byte.
///
/// Returns the new inode number.
pub fn create_symlink<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
    target: &[u8],
    replace_existing: bool,
) -> Result<u64, TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();

    if target.is_empty() {
        return Err(TxnError::InvalidArgument(
            "symlink target must not be empty".into(),
        ));
    }
    if target.contains(&0) {
        return Err(TxnError::InvalidArgument(
            "symlink target must not contain a NUL byte; the stored path would be \
             truncated there"
                .into(),
        ));
    }
    if target.len() > SYMLINK_TARGET_MAX {
        return Err(TxnError::InvalidArgument(format!(
            "symlink target of {} bytes exceeds the {SYMLINK_TARGET_MAX}-byte limit macOS \
             enforces (ENAMETOOLONG)",
            target.len()
        )));
    }

    validate_entry_name(name)?;
    let new_ino = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);
    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let _norm_flag_observed = incompat & APFS_INCOMPAT_NORMALIZATION_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;

    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
    // Without the replace flag a name the directory already has, as the
    // volume compares names, is a collision.
    if !replace_existing {
        let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
        if drec_exists(
            txn,
            &omap_node,
            root_tree_oid,
            &drec_key,
            bsz,
            case_fold,
            normalize,
        )? {
            return Err(TxnError::AlreadyExists(format!(
                "'{name}' already exists in directory {parent_ino}"
            )));
        }
    }
    let drec = (drec_key, build_drec_val(new_ino, now, DT_LNK));
    let inode = (
        build_inode_key(new_ino),
        build_inode_val(&InodeValArgs {
            parent_id: parent_ino,
            private_id: new_ino,
            now_ns: now,
            mode: S_IFLNK | 0o777,
            nlink_or_nchildren: 1,
            uid: 0,
            gid: 0,
            name,
            dstream: None,
        }),
    );
    // APFS stores the target as a NUL-terminated string and readers take the
    // value length minus the terminator. Storing it bare made every consumer -
    // macOS readlink included - see a target one byte short, so the link
    // resolved to the wrong path.
    let mut target_val = Vec::with_capacity(target.len() + 1);
    target_val.extend_from_slice(target);
    target_val.push(0);
    let xattr = (
        build_xattr_key(new_ino, XATTR_NAME_SYMLINK),
        build_xattr_val_embedded_owned(&target_val),
    );

    // Replacing an entry that is already there - the empty placeholder Windows
    // creates before turning it into a symlink - has to happen in the same
    // rewrite. Doing it as a separate removal first cannot work: every catalog
    // operation reads the superblock and object map it was handed, so a second
    // one in the same transaction never sees what the first did, and splitting
    // it across two transactions leaves the name deleted with nothing in its
    // place if the second one does not land.
    let replaced: Option<TargetReplace> = if replace_existing {
        let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
        let all = gather_entries(txn, &omap_node, root_tree_oid, &[&drec.0], bsz)?;
        collect_replace_target(&all, &drec.0, new_ino, true, bsz, case_fold, normalize)?
    } else {
        None
    };
    let remove_keys: Vec<Vec<u8>> = replaced
        .as_ref()
        .map(|t| t.remove_keys.clone())
        .unwrap_or_default();
    let replaced_blocks: Vec<(u64, u64)> = replaced
        .as_ref()
        .map(|t| t.data_blocks.clone())
        .unwrap_or_default();
    let replaced_updates: Vec<(Vec<u8>, Vec<u8>)> = replaced
        .as_ref()
        .map(|t| t.updates.clone())
        .unwrap_or_default();
    let replaced_existing = replaced.is_some();
    let (new_extref_paddr, extref_old_padrs, extref_node_delta, freed_count) =
        free_blocks_owned_by_live_extref(txn, vsb_raw, new_xid, &replaced_blocks, bsz)?;

    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree_impl(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        vec![drec, inode, xattr],
        &remove_keys,
        // Replacing fills a slot that already counted; only a new name adds one.
        Some((parent_ino, if replaced_existing { 0 } else { 1 })),
        &replaced_updates,
        now,
        bsz,
    )?;

    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &extref_old_padrs,
        &reclaim,
        bsz,
    )?;

    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    wr_u64(&mut new_vsb, VSBI_NEXT_OBJ_ID, new_ino + 1);
    // A symlink counts as a symlink, not as a file: counting it with the files
    // left fsck reporting both figures wrong ("apfs_num_files is not valid",
    // "apfs_num_symlinks is not valid"). When it replaces something, whatever
    // was there stops being counted - which for the placeholder Windows makes
    // is a regular file.
    let num_symlinks = rd_u64(&new_vsb, VSBI_NUM_SYMLINKS);
    wr_u64(&mut new_vsb, VSBI_NUM_SYMLINKS, num_symlinks + 1);
    if let Some(ref t) = replaced {
        uncount_replaced(&mut new_vsb, t);
    }
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    wr_u64(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (fs_alloc - freed_count as i64 + node_delta + extref_node_delta + frm_correction).max(0)
            as u64,
    );
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    Ok(new_ino)
}

/// Create a new empty directory `name` in directory `parent_ino`.
/// [the APFS specification - kernel mkdir: mode 0o40755, nchildren 0, no dstream]
pub fn mkdir<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
) -> Result<u64, TxnError> {
    create_entry(
        txn,
        vsb_raw,
        vol_omap_raw,
        parent_ino,
        name,
        S_IFDIR | 0o755,
        DT_DIR,
        0,
        true,
    )
}

/// Give an existing file another name.
///
/// The new entry points at the same inode, so nothing is copied and the link
/// count goes up by one. A file that had only one name has no sibling records
/// yet, so this also gives the original name a sibling id and its own pair of
/// records - which is why the original entry is rewritten, not just read.
///
/// Directories are refused: a second name for one would make the tree a graph,
/// which is why POSIX does not allow it either.
///
/// [linux-apfs-rw dir.c apfs_link / apfs_prepare_dentry_for_link; sibling ids
///  come from the same pool as inode numbers]
pub fn link<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    src_parent_ino: u64,
    src_name: &str,
    dst_parent_ino: u64,
    dst_name: &str,
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    validate_entry_name(dst_name)?;
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();

    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let normalize = true;
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let src_drec_key = build_drec_key(src_parent_ino, src_name, case_fold, normalize);
    let dst_drec_key = build_drec_key(dst_parent_ino, dst_name, case_fold, normalize);
    let all = gather_entries(
        txn,
        &omap_node,
        root_tree_oid,
        &[&src_drec_key, &dst_drec_key],
        bsz,
    )?;

    let src_drec_val = all
        .iter()
        .find(|(k, _)| *k == src_drec_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::SpacemanParse("name not found in directory".into()))?;
    let file_id = rd_u64(&src_drec_val, 0);

    if find_drec_collision(&all, &dst_drec_key, case_fold, normalize).is_some() {
        return Err(TxnError::AlreadyExists(dst_name.to_string()));
    }

    let inode_key = build_inode_key(file_id);
    let inode_val = all
        .iter()
        .find(|(k, _)| *k == inode_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::SpacemanParse("inode record missing for the name".into()))?;
    let (links, mode) = inode_links_and_mode(&inode_val)?;
    if mode & 0xF000 == S_IFDIR {
        return Err(TxnError::Unsupported(
            "a directory cannot be given a second name".into(),
        ));
    }
    let dt_type = (mode >> 12) & 0xF;

    // Sibling ids come from the same pool as inode numbers.
    let mut next_obj_id = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);
    let mut inserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut removes: Vec<Vec<u8>> = Vec::new();

    // The original name needs records of its own before it can be one of
    // several. Its entry gains a sibling id, which means replacing it: the key
    // stays the same, so it is removed and re-inserted rather than updated.
    if drec_sibling_id(&src_drec_val).is_none() {
        let sibling_id = next_obj_id;
        next_obj_id += 1;
        removes.push(src_drec_key.clone());
        inserts.push((
            src_drec_key,
            build_drec_val_with_sibling(file_id, rd_u64(&src_drec_val, 8), dt_type, sibling_id),
        ));
        inserts.push((
            build_sibling_link_key(file_id, sibling_id),
            build_sibling_link_val(src_parent_ino, src_name),
        ));
        inserts.push((
            build_sibling_map_key(sibling_id),
            build_sibling_map_val(file_id),
        ));
    }

    let sibling_id = next_obj_id;
    next_obj_id += 1;
    inserts.push((
        dst_drec_key,
        build_drec_val_with_sibling(file_id, now, dt_type, sibling_id),
    ));
    inserts.push((
        build_sibling_link_key(file_id, sibling_id),
        build_sibling_link_val(dst_parent_ino, dst_name),
    ));
    inserts.push((
        build_sibling_map_key(sibling_id),
        build_sibling_map_val(file_id),
    ));

    let mut updated_inode = inode_val.clone();
    wr_u32(&mut updated_inode, INODE_NCHILDREN, links.saturating_add(1));
    wr_u64(&mut updated_inode, INODE_CHANGE_TIME, now);
    let inode_updates = vec![(inode_key, updated_inode)];

    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree_impl(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        inserts,
        &removes,
        Some((dst_parent_ino, 1)),
        &inode_updates,
        now,
        bsz,
    )?;

    // --- COW the volume omap header (om_tree_oid -> new b-tree paddr). ---
    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    // --- Stage the updated (virtual) volume superblock. ---
    // No new file exists, so apfs_num_files does not move; only the id pool and
    // the block accounting do.
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_NEXT_OBJ_ID, next_obj_id);
    let frm_correction =
        free_replaced_metadata(txn, vsb_raw, vol_omap_raw, &omap_node, &[], &reclaim, bsz)?;
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    wr_u64(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (fs_alloc + node_delta + frm_correction).max(0) as u64,
    );
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;
    Ok(())
}

/// Shared orchestration for creating an empty file or directory (no data
/// stream). Mirrors the M7a snapshot COW commit envelope: the catalog (fsroot)
/// is a VIRTUAL object resolved through the volume omap, so we COW the catalog
/// leaf to a new block, remap {root_tree_oid, new_xid} -> new paddr in the
/// volume omap b-tree, COW the volume omap header, and stage the updated
/// (virtual) volume superblock. Returns the new inode number.
///
/// `bump_dirs` selects which VSB counter increments (num_directories vs
/// num_files).
#[allow(clippy::too_many_arguments)]
fn create_entry<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
    mode: u16,
    dt_type: u16,
    nchildren: u32,
    bump_dirs: bool,
) -> Result<u64, TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();

    validate_entry_name(name)?;
    let new_ino = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);
    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    // Apply NFD unconditionally: modern APFS hashes the normalized form
    // (the APFS specification). For ASCII this is a no-op; for Unicode
    // it is required even when APFS_INCOMPAT_NORMALIZATION_INSENSITIVE is unset
    // because fsck_apfs validates against NFD regardless of the flag.
    let _norm_flag_observed = incompat & APFS_INCOMPAT_NORMALIZATION_INSENSITIVE != 0;
    let normalize = true;

    // --- Read the current volume omap b-tree node. ---
    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    // Reject a name the directory already has - as the volume compares
    // names, so `FOO` beside `foo` on a case-insensitive volume - before
    // anything is allocated.
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
    if drec_exists(
        txn,
        &omap_node,
        root_tree_oid,
        &drec_key,
        bsz,
        case_fold,
        normalize,
    )? {
        return Err(TxnError::AlreadyExists(format!(
            "'{name}' already exists in directory {parent_ino}"
        )));
    }
    // --- Build the new DREC + INODE records and rebuild the FSTREE (splits
    //     into a multi-node b-tree automatically when a leaf would overflow). ---
    let drec = (drec_key, build_drec_val(new_ino, now, dt_type));
    let inode = (
        build_inode_key(new_ino),
        build_inode_val(&InodeValArgs {
            parent_id: parent_ino,
            private_id: new_ino,
            now_ns: now,
            mode,
            nlink_or_nchildren: nchildren,
            uid: 0,
            gid: 0,
            name,
            dstream: None,
        }),
    );
    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        vec![drec, inode],
        &[],
        Some((parent_ino, 1)),
        now,
        bsz,
    )?;

    // --- COW the volume omap header (om_tree_oid -> new b-tree paddr). ---
    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    // --- Stage the updated (virtual) volume superblock. ---
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_NEXT_OBJ_ID, new_ino + 1);
    let counter_off = if bump_dirs {
        VSBI_NUM_DIRECTORIES
    } else {
        VSBI_NUM_FILES
    };
    let count = rd_u64(&new_vsb, counter_off);
    wr_u64(&mut new_vsb, counter_off, count + 1);
    // Reclaim COW-replaced metadata (create_entry never touches the extentref).
    let frm_correction =
        free_replaced_metadata(txn, vsb_raw, vol_omap_raw, &omap_node, &[], &reclaim, bsz)?;

    // fs_alloc_count: no data blocks, only the net catalog node delta from the
    // repack (e.g. baseline multi-node catalog collapsing to one node).
    // frm_correction accounts for sm_fq-pinned old nodes. [#149 fix]
    // [the APFS specification Q5]
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    wr_u64(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (fs_alloc + node_delta + frm_correction).max(0) as u64,
    );
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    Ok(new_ino)
}

/// Create a new regular file `name` in directory `parent_ino` with `content`
/// (single block; `content.len()` must be <= the volume block size).
///
/// Extends [`create_file`] with a data block + DSTREAM xfield + FILE_EXTENT
/// record + an extent-ref tree entry. Returns the new inode number.
/// [the APFS specification]
pub fn write_file<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
    content: &[u8],
) -> Result<u64, TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    if content.is_empty() {
        return Err(TxnError::SpacemanParse(
            "write_file: content must be non-empty (use create_file for empty)".into(),
        ));
    }
    let num_blocks = content.len().div_ceil(bsz);
    let new_xid = txn.xid;
    let now = now_ns();

    validate_entry_name(name)?;
    let new_ino = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);
    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    // Apply NFD unconditionally: modern APFS hashes the normalized form
    // (the APFS specification). For ASCII this is a no-op; for Unicode
    // it is required even when APFS_INCOMPAT_NORMALIZATION_INSENSITIVE is unset
    // because fsck_apfs validates against NFD regardless of the flag.
    let _norm_flag_observed = incompat & APFS_INCOMPAT_NORMALIZATION_INSENSITIVE != 0;
    let normalize = true;

    // --- Read the current volume omap b-tree node. ---
    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;

    // Reject a duplicate name BEFORE allocating anything (no leak, no corruption).
    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    if drec_exists(
        txn,
        &omap_node,
        root_tree_oid,
        &drec_key,
        bsz,
        case_fold,
        normalize,
    )? {
        return Err(TxnError::AlreadyExists(format!(
            "'{name}' already exists in directory {parent_ino}"
        )));
    }

    // --- Allocate + write the data blocks (raw content, last block zero-padded). ---
    // Use alloc_blocks_run to batch-allocate contiguous runs; loop for multi-chunk.
    let mut paddrs: Vec<u64> = Vec::with_capacity(num_blocks);
    let mut blocks_remaining = num_blocks;
    while blocks_remaining > 0 {
        let (run_start, run_count) = txn.alloc_blocks_run(blocks_remaining)?;
        for j in 0..run_count {
            let i = num_blocks - blocks_remaining + j;
            let p = run_start + j as u64;
            let mut data = vec![0u8; bsz];
            let start = i * bsz;
            let end = ((i + 1) * bsz).min(content.len());
            wr_bytes(&mut data, 0, content.get(start..end).unwrap_or(&[]));
            txn.stage_raw(p, data);
            paddrs.push(p);
        }
        blocks_remaining -= run_count;
    }
    // Group the data blocks into contiguous runs -> (logical_block, phys, count).
    let mut runs: Vec<(usize, u64, u64)> = Vec::new();
    for (i, &p) in paddrs.iter().enumerate() {
        match runs.last_mut() {
            Some(last) if last.1 + last.2 == p => last.2 += 1,
            _ => runs.push((i, p, 1)),
        }
    }

    // --- Build DREC + INODE(with dstream) + DSTREAM_ID + FILE_EXTENT records. ---
    let alloced = (num_blocks * bsz) as u64;
    let mut cat_records = vec![
        (
            build_drec_key(parent_ino, name, case_fold, normalize),
            build_drec_val(new_ino, now, DT_REG),
        ),
        (
            build_inode_key(new_ino),
            // Created, then written: two steps of the counter, as under macOS.
            with_write_generation_step(build_inode_val(&InodeValArgs {
                parent_id: parent_ino,
                private_id: new_ino,
                now_ns: now,
                mode: S_IFREG | 0o644,
                nlink_or_nchildren: 1,
                uid: 0,
                gid: 0,
                name,
                dstream: Some(DstreamArgs {
                    size: content.len() as u64,
                    alloced_size: alloced,
                }),
            })),
        ),
        (build_dstream_id_key(new_ino), build_dstream_id_val(1)),
    ];
    for &(lblk, pstart, cnt) in &runs {
        cat_records.push((
            build_file_extent_key(new_ino, (lblk * bsz) as u64),
            build_file_extent_val(cnt * bsz as u64, pstart),
        ));
    }
    // --- Rebuild the FSTREE (splits automatically) + volume omap. ---
    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        cat_records,
        &[],
        Some((parent_ino, 1)),
        now,
        bsz,
    )?;

    // --- COW the extent-ref tree: one physical-extent record per run. [#151] ---
    let extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    let phys_inserts: Vec<(Vec<u8>, Vec<u8>)> = runs
        .iter()
        .map(|&(_, pstart, cnt)| {
            (
                build_phys_ext_key(pstart),
                build_phys_ext_val(cnt, new_ino, 1),
            )
        })
        .collect();
    let (new_extref_paddr, old_extref_padrs, extref_node_delta) =
        rewrite_extref_tree(txn, extref_paddr, new_xid, phys_inserts, &[], &[], bsz)?;

    // --- COW the volume omap header (om_tree_oid -> rewritten b-tree). ---
    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    // --- Stage the updated (virtual) volume superblock. ---
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    wr_u64(&mut new_vsb, VSBI_NEXT_OBJ_ID, new_ino + 1);
    let num_files = rd_u64(&new_vsb, VSBI_NUM_FILES);
    wr_u64(&mut new_vsb, VSBI_NUM_FILES, num_files + 1);
    // Reclaim the COW-replaced metadata blocks (old omap/catalog/extentref).
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &old_extref_padrs,
        &reclaim,
        bsz,
    )?;

    // fs_alloc_count: new data blocks + net catalog node delta + extref node delta.
    // frm_correction: sm_fq-pinned old nodes stay counted; add back. [#149]
    // [the APFS specification Q-D2, the APFS specification Q5]
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let new_alloc = (fs_alloc + num_blocks as i64 + node_delta + extref_node_delta + frm_correction)
        .max(0) as u64;
    wr_u64(&mut new_vsb, VSBI_FS_ALLOC_COUNT, new_alloc);
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    Ok(new_ino)
}

/// Remove file or directory `name` from directory `parent_ino`.
///
/// Removes the DREC + INODE (+ DSTREAM_ID + FILE_EXTENT for a file with data)
/// from the catalog, removes the matching extent-ref records and frees the data
/// block(s), decrements the parent's nchildren, and updates the VSB counters
/// (num_files/num_directories and fs_alloc_count). [the APFS specification]
pub fn unlink<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    // Apply NFD unconditionally: modern APFS hashes the normalized form
    // (the APFS specification). For ASCII this is a no-op; for Unicode
    // it is required even when APFS_INCOMPAT_NORMALIZATION_INSENSITIVE is unset
    // because fsck_apfs validates against NFD regardless of the flag.
    let _norm_flag_observed = incompat & APFS_INCOMPAT_NORMALIZATION_INSENSITIVE != 0;
    let normalize = true;

    // --- Read the volume omap node, then locate the target across the tree. ---
    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
    let all = gather_entries(txn, &omap_node, root_tree_oid, &[&drec_key], bsz)?;

    let drec_val = all
        .iter()
        .find(|(k, _)| *k == drec_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::SpacemanParse("name not found in directory".into()))?;
    let file_id = rd_u64(&drec_val, 0);

    let plan = plan_removal(&all, file_id, &drec_val, now, bsz)?;
    let RemovalPlan {
        remove_keys: mut plan_keys,
        data_blocks,
        updates: inode_updates,
        is_dir,
        dir_nchildren,
        last_name,
        mode: removed_mode,
    } = plan;
    let mut remove_keys: Vec<Vec<u8>> = vec![drec_key];
    remove_keys.append(&mut plan_keys);

    // Reject removing a NON-EMPTY directory (POSIX ENOTEMPTY): silently removing
    // it would orphan its children (their DRECs point at a now-gone parent
    // inode). Checked before any allocation, so the op leaves no trace.
    // [CERTAIN: empirical - unlink of a dir with a child left an orphaned DREC.]
    if is_dir && dir_nchildren > 0 {
        return Err(TxnError::DirectoryNotEmpty(format!(
            "'{name}' has {dir_nchildren} entries"
        )));
    }

    // --- Rebuild the FSTREE without the removed records (collapses nodes). ---
    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree_impl(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        vec![],
        &remove_keys,
        Some((parent_ino, -1)),
        &inode_updates,
        now,
        bsz,
    )?;

    // --- Remove extent-ref records + free data blocks. ---
    //
    // CRITICAL (snapshot safety): only free a data block if it is owned by the
    // LIVE volume, i.e. its physical-extent record is present in the LIVE
    // extentref tree. After a snapshot, create_snapshot swaps in a fresh EMPTY
    // extentref tree and the snapshot holds the OLD one - so blocks written
    // before the snapshot are NOT in the live tree and belong to the snapshot.
    // Freeing such a block lets a later write reuse it, double-referencing a
    // block the snapshot still points to → fsck "Snapshot is invalid".
    //
    // COW clone (refcnt): when clone_file shares blocks with a sibling inode
    // the extref record has refcnt > 1. We DECREMENT instead of removing; the
    // block is freed only when refcnt reaches 1 (last reference).
    // [CERTAIN: empirical red test repro_overalloc_snapshot_pinned + the APFS specification
    //  Q8/Q10 the APFS specification; refcnt derived from APFS spec
    //  physical-extent record semantics]
    let old_extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    let mut freed_count = 0u64;
    let new_extref_paddr;
    let old_extref_padrs: Vec<u64>;
    let extref_node_delta: i64;

    if !data_blocks.is_empty() {
        // The live extref records the runs cover, for refcnt inspection.
        let live_recs = extref_records_for_runs(txn, old_extref_paddr, &data_blocks, bsz)?;

        // Walk each extent over the records it covers: last reference →
        // remove + free, shared → lower the count, snapshot-owned (no live
        // record) → KIND_UPDATE(-1). A sparse hole owns no block and is
        // skipped by the walk.
        let ExtrefRelease {
            remove_keys: remove_keys_ext,
            upserts: upsert_recs,
            update_inserts,
            owned_runs,
        } = plan_extref_release(&live_recs, &data_blocks)?;

        let extref_changed =
            !remove_keys_ext.is_empty() || !upsert_recs.is_empty() || !update_inserts.is_empty();
        if extref_changed {
            // Rewrite extref tree (multi-node capable). [#151]
            // Pass decrements as upserts, KIND_UPDATE inserts as insert_recs,
            // zero-refcnt keys as remove_keys.
            let (np, op, nd) = rewrite_extref_tree(
                txn,
                old_extref_paddr,
                new_xid,
                update_inserts,
                &remove_keys_ext,
                &upsert_recs,
                bsz,
            )?;
            new_extref_paddr = np;
            old_extref_padrs = op;
            extref_node_delta = nd;
            // Free data blocks whose last reference was just removed.
            for &(phys, count) in &owned_runs {
                for b in phys..phys + count {
                    txn.free_block(b)?;
                }
                freed_count += count;
            }
        } else {
            new_extref_paddr = old_extref_paddr;
            old_extref_padrs = vec![];
            extref_node_delta = 0;
        }
    } else {
        new_extref_paddr = old_extref_paddr;
        old_extref_padrs = vec![];
        extref_node_delta = 0;
    }

    // --- COW the volume omap header (om_tree_oid -> rewritten b-tree). ---
    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    // --- Stage the updated (virtual) volume superblock. ---
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    // The object count only moves when the object itself goes; dropping one of
    // several names leaves the file, and the volume's file count, in place.
    // Which count depends on what the inode is: a symlink or a special file
    // is not among the regular files.
    if last_name {
        let counter_off = object_count_offset(removed_mode);
        let count = rd_u64(&new_vsb, counter_off);
        wr_u64(&mut new_vsb, counter_off, count.saturating_sub(1));
    }
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &old_extref_padrs,
        &reclaim,
        bsz,
    )?;

    // fs_alloc_count: -freed data blocks + net catalog node delta + extref node delta.
    // Clone-shared blocks are NOT freed (refcnt decrement path), so freed_count
    // correctly reflects only truly reclaimed blocks.
    // frm_correction: sm_fq-pinned old metadata stays counted. [#149 fix]
    // [the APFS specification Q5]
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let new_alloc =
        (fs_alloc - freed_count as i64 + node_delta + extref_node_delta + frm_correction).max(0)
            as u64;
    wr_u64(&mut new_vsb, VSBI_FS_ALLOC_COUNT, new_alloc);
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// clone_file - COW metadata-only file clone (reflink / cp -c equivalent).
//
// Creates a new inode whose j_file_extent records point to the same physical
// blocks as the source. No data copy occurs. On first write to either file the
// caller is responsible for allocating new blocks (copy-on-write at the data
// layer happens transparently in write_file / overwrite_existing_file because
// they always allocate fresh blocks and never alias with an existing extent).
//
// Physical-extent sharing is tracked via the `refcnt` field of each extref
// record. clone_file increments refcnt for each shared paddr; unlink decrements
// it and frees blocks only when refcnt reaches 0. This is the APFS-spec
// physical-extent sharing model.
//
// Restrictions:
//   - Source must be a regular file (not a directory, not a symlink with data).
//   - Destination name must not already exist.
//   - Sparse holes (paddr == 0) are copied as-is (no extref entry; already
//     correct by the W4-BUG-1 fix).
//
// [APFS spec - physical extent record refcounting; cross-checked against
//  linux-apfs-rw extents.c and apfsprogs fsck behaviour]
// ---------------------------------------------------------------------------

/// Clone `src_name` in `src_parent_ino` to `dst_name` in `dst_parent_ino`
/// using copy-on-write metadata sharing.
///
/// Returns the new inode number of the clone on success.
pub fn clone_file<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    src_parent_ino: u64,
    src_name: &str,
    dst_parent_ino: u64,
    dst_name: &str,
) -> Result<u64, TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    validate_entry_name(dst_name)?;
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;

    // --- Step 1: Reject duplicate destination BEFORE allocating anything. ---
    let dst_drec_key = build_drec_key(dst_parent_ino, dst_name, case_fold, normalize);
    if drec_exists(
        txn,
        &omap_node,
        root_tree_oid,
        &dst_drec_key,
        bsz,
        case_fold,
        normalize,
    )? {
        return Err(TxnError::AlreadyExists(format!(
            "clone_file: destination '{dst_name}' already exists in inode {dst_parent_ino}"
        )));
    }

    // --- Step 2: Locate source DREC → file_id. ---
    let src_drec_key = build_drec_key(src_parent_ino, src_name, case_fold, normalize);
    let all = gather_entries(txn, &omap_node, root_tree_oid, &[&src_drec_key], bsz)?;

    let src_file_id = all
        .iter()
        .find(|(k, _)| k.as_slice() == src_drec_key.as_slice())
        .map(|(_, v)| rd_u64(v, 0) & 0x0FFF_FFFF_FFFF_FFFF)
        .ok_or_else(|| TxnError::NotFound(format!("clone_file: source '{src_name}' not found")))?;

    // --- Step 3: Read source inode val + dstream + extents. ---
    let src_inode_key = encode_jkey(src_file_id, APFS_TYPE_INODE)
        .to_le_bytes()
        .to_vec();
    let src_inode_val = all
        .iter()
        .find(|(k, _)| *k == src_inode_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::NotFound(format!("clone_file: inode {src_file_id} not found")))?;

    // Reject if source is a directory. j_inode_val.mode is at offset 80 (the
    // 92-byte fixed prefix: uid@72, gid@76, mode@80). [#141 fix: the previous
    // offsets 40/48/52 read create-time/internal_flags, so the clone inode got a
    // garbage mode (fsck "Update inode objects" / invalid file-type).]
    let src_mode = rd_u16(&src_inode_val, 80);
    if src_mode & 0xF000 != S_IFREG {
        // A directory has no stream to share; a symlink, a FIFO or a device
        // node would come out as a regular-file entry over a non-regular inode
        // (a type mismatch fsck rejects) and would be counted as a file.
        return Err(TxnError::InvalidArgument(format!(
            "clone_file: source has mode {src_mode:#o}; only regular files may be cloned"
        )));
    }

    // The source's content is keyed by the stream id its inode names, which
    // is another inode's id when macOS cloned the source from it.
    let src_stream_id = match rd_u64(&src_inode_val, INODE_PRIVATE_ID) {
        0 => src_file_id,
        id => id,
    };

    // Collect source j_file_extent records sorted by logical offset.
    let mut src_extents: Vec<(u64, u64, u64)> = Vec::new(); // (logical_addr, paddr, len_bytes)
    for (k, v) in &all {
        let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
        let ty = rd_u64(k, 0) >> 60;
        if oid != src_stream_id || ty != APFS_TYPE_FILE_EXTENT {
            continue;
        }
        let logical_addr = rd_u64(k, 8);
        let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
        let paddr = rd_u64(v, 8);
        src_extents.push((logical_addr, paddr, len_bytes));
    }
    src_extents.sort_by_key(|&(la, _, _)| la);

    // Read the logical size from the source inode's DSTREAM xfield.
    // [Empirical (the APFS spec, on-disk decode 2026-05-30): j_inode_val has a 92-byte
    //  fixed prefix; the xfield blob begins at offset 92 - xf_num_exts (u16) +
    //  xf_used_data (u16), then xf_num_exts x_field_t descriptors (type u8,
    //  flags u8, size u16), then the 8-byte-aligned data blob. The DSTREAM
    //  field is INO_EXT_TYPE = 8.]  The previous code scanned at offset 88 for
    //  type 0x0a, never matched, and fell back to the block-rounded extent sum,
    //  so cloning a 15-byte file recorded size=4096 (#141 fsck "orphan/invalid").
    let src_size = {
        const INO_EXT_TYPE_DSTREAM: u8 = 0x08;
        const XF_HDR: usize = 92;
        let mut found_size: Option<u64> = None;
        if src_inode_val.len() > XF_HDR + 4 {
            let num_xf = rd_u16(&src_inode_val, XF_HDR) as usize;
            let mut data_off = XF_HDR + 4 + num_xf * 4;
            for xi in 0..num_xf {
                let desc_off = XF_HDR + 4 + xi * 4;
                if desc_off + 4 > src_inode_val.len() {
                    break;
                }
                let xtype = src_inode_val[desc_off];
                let xsize = rd_u16(&src_inode_val, desc_off + 2) as usize;
                if xtype == INO_EXT_TYPE_DSTREAM && data_off + 8 <= src_inode_val.len() {
                    found_size = Some(rd_u64(&src_inode_val, data_off));
                }
                data_off += (xsize + 7) & !7; // xfield data is 8-byte aligned
            }
        }
        found_size.unwrap_or_else(|| src_extents.iter().map(|(_, _, l)| l).sum::<u64>())
    };
    let src_alloced: u64 = src_extents.iter().map(|(_, _, l)| l).sum();

    // --- Step 4: Allocate new inode id. ---
    let new_ino = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);

    // --- Step 5: Build clone's catalog records. ---
    // 5a: DREC in destination directory.
    let dst_drec = (dst_drec_key, build_drec_val(new_ino, now, DT_REG));

    // 5b: Clone inode. The clone gets its OWN data-stream id (private_id =
    // new_ino), its OWN DSTREAM_ID and FILE_EXTENT records pointing at the SAME
    // physical blocks as the source, and the refcount of every shared physical
    // extent is incremented by one in the extentref tree. Both inodes are marked
    // WAS_CLONED. unlink / overwrite / truncate already gate every block free on
    // the physical-extent refcount (free only when it reaches 1), so deleting or
    // rewriting either side leaves the other intact. [Apple apfs.kext: the
    // block-free gate is a per-extent refcount; this mirrors write_file's record
    // layout so the produced image is fsck-clean.]
    let mut clone_inode_val = build_inode_val(&InodeValArgs {
        parent_id: dst_parent_ino,
        private_id: new_ino, // OWN data-stream id
        now_ns: now,
        mode: src_mode,
        nlink_or_nchildren: 1,
        uid: rd_u32(&src_inode_val, 72), // j_inode_val.owner @72
        gid: rd_u32(&src_inode_val, 76), // j_inode_val.group @76
        name: dst_name,
        dstream: Some(DstreamArgs {
            size: src_size,
            alloced_size: src_alloced,
        }),
    });
    // A clone keeps the source's birth, modification and access times and
    // gets a change time of its own (measured on macOS with `cp -c` in
    // `tests/link_clone_times.rs`).
    for at in [INODE_CREATE_TIME, INODE_MOD_TIME, INODE_ACCESS_TIME] {
        wr_u64(&mut clone_inode_val, at, rd_u64(&src_inode_val, at));
    }
    let clone_flags =
        rd_u64(&clone_inode_val, 48) | APFS_INODE_WAS_CLONED | APFS_INODE_WAS_EVER_CLONED;
    wr_u64(&mut clone_inode_val, 48, clone_flags);
    // The clone gets the same extended attributes, so it has to describe itself
    // the same way: the BSD flags (UF_COMPRESSED among them), the recorded
    // uncompressed length, and the pair of bits saying whether there is a
    // resource fork. A default-built inode says there is none, which fsck
    // catches the moment one is copied across. The Finder info attribute is
    // copied too, and so is the bit announcing it ("finder info internal
    // flag is missing" otherwise).
    wr_u32(
        &mut clone_inode_val,
        INOI_BSD_FLAGS,
        rd_u32(&src_inode_val, INOI_BSD_FLAGS),
    );
    wr_u64(
        &mut clone_inode_val,
        INODE_UNCOMPRESSED_SIZE,
        rd_u64(&src_inode_val, INODE_UNCOMPRESSED_SIZE),
    );
    const DESCRIBED_BITS: u64 = INODE_HAS_RSRC_FORK
        | INODE_NO_RSRC_FORK
        | INODE_HAS_UNCOMPRESSED_SIZE
        | INODE_HAS_FINDER_INFO;
    let described = (rd_u64(&clone_inode_val, INODE_INTERNAL_FLAGS) & !DESCRIBED_BITS)
        | (rd_u64(&src_inode_val, INODE_INTERNAL_FLAGS) & DESCRIBED_BITS);
    wr_u64(&mut clone_inode_val, INODE_INTERNAL_FLAGS, described);
    // A sparse source's clone shares its holes, so it is sparse the same way:
    // the hole count and the sparse flag come across with the extents.
    let src_sparse_bytes = apfs_core::inode::parse_unknown_xfields(&src_inode_val)
        .unwrap_or_default()
        .into_iter()
        .find(|x| x.x_type == apfs_core::inode::INO_EXT_TYPE_SPARSE_BYTES)
        .and_then(|x| {
            x.data
                .get(..8)
                .and_then(|b| b.try_into().ok())
                .map(u64::from_le_bytes)
        })
        .unwrap_or(0);
    if src_sparse_bytes > 0 {
        clone_inode_val = rebuild_inode_with_sparse_bytes(
            &clone_inode_val,
            dst_name,
            Some(DstreamArgs {
                size: src_size,
                alloced_size: src_alloced,
            }),
            src_sparse_bytes,
            now,
        );
    }

    // New records: clone DREC + inode + its own DSTREAM_ID (refcnt 1) + one
    // FILE_EXTENT per source extent (same logical offset, length, physical block).
    let mut new_cat_records: Vec<(Vec<u8>, Vec<u8>)> = vec![
        dst_drec,
        (build_inode_key(new_ino), clone_inode_val),
        (build_dstream_id_key(new_ino), build_dstream_id_val(1)),
    ];
    for &(logical_addr, paddr, len_bytes) in &src_extents {
        new_cat_records.push((
            build_file_extent_key(new_ino, logical_addr),
            build_file_extent_val(len_bytes, paddr),
        ));
    }

    // The source's extended attributes come across too. A clone that drops them
    // is not a clone: a compressed file keeps its whole content in one, and a
    // resource fork is one. An attribute small enough to sit inside its own
    // record is copied as it is; one that has spilled into a stream gets a
    // stream id of its own, and its extents point at the same blocks the source
    // uses, sharing them the same way the file's own extents do.
    let mut next_obj_id = new_ino + 1;
    let mut shared_extents = src_extents.clone();
    {
        let src_xattrs: Vec<(Vec<u8>, Vec<u8>)> = all
            .iter()
            .filter(|(k, _)| {
                rd_u64(k, 0) >> 60 == APFS_TYPE_XATTR
                    && rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF == src_file_id
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (src_key, src_val) in src_xattrs {
            // The key is the header followed by the name; only the header
            // changes.
            let mut key = encode_jkey(new_ino, APFS_TYPE_XATTR).to_le_bytes().to_vec();
            key.extend_from_slice(src_key.get(8..).unwrap_or(&[]));

            if rd_u16(&src_val, 0) & XATTR_DATA_STREAM == 0 {
                new_cat_records.push((key, src_val));
                continue;
            }
            let src_stream_id = rd_u64(&src_val, 4);
            let new_stream_id = next_obj_id;
            next_obj_id += 1;
            let mut val = src_val.clone();
            wr_u64(&mut val, 4, new_stream_id);
            new_cat_records.push((key, val));
            for (k, v) in &all {
                if rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF != src_stream_id {
                    continue;
                }
                match rd_u64(k, 0) >> 60 {
                    // An attribute stream carries no data-stream-id record
                    // (it is never shared); one on the source is an older
                    // writer's and is not copied.
                    APFS_TYPE_DSTREAM_ID => {}
                    APFS_TYPE_FILE_EXTENT => {
                        let logical_addr = rd_u64(k, 8);
                        let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
                        let paddr = rd_u64(v, 8);
                        new_cat_records.push((
                            build_file_extent_key(new_stream_id, logical_addr),
                            build_file_extent_val(len_bytes, paddr),
                        ));
                        shared_extents.push((logical_addr, paddr, len_bytes));
                    }
                    _ => {}
                }
            }
        }
    }

    // Update the source inode: mark it WAS_CLONED (Apple marks both sides). The
    // data stream is shared at the physical-extent level, not via dstream_id.
    let mut update_records: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut src_inode_updated = src_inode_val.clone();
    let src_flags =
        rd_u64(&src_inode_updated, 48) | APFS_INODE_WAS_CLONED | APFS_INODE_WAS_EVER_CLONED;
    wr_u64(&mut src_inode_updated, 48, src_flags);
    update_records.push((src_inode_key.clone(), src_inode_updated));

    // --- Step 6: Rewrite the fstree (insert clone records, mark source cloned,
    //     bump dst parent nchildren). ---
    let parent_patch = Some((dst_parent_ino, 1i64));
    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree_impl(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        new_cat_records,
        &[],
        parent_patch,
        &update_records,
        now,
        bsz,
    )?;

    // --- Step 7: Take one more reference on every shared physical extent so
    //     that unlink/overwrite/truncate free a block only when its last
    //     reference is dropped. The release walk with the opposite sign: a run
    //     can span several records (macOS splits a shared record when it
    //     rewrites part of a clone), each gains a reference over the part
    //     inside the run, and a range no live record covers (snapshot-pinned)
    //     gets a KIND_UPDATE(+1). A sparse hole owns no block and shares
    //     nothing. ---
    let old_extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    let (new_extref_paddr, old_extref_padrs, extref_node_delta) = if shared_extents.is_empty() {
        (old_extref_paddr, Vec::new(), 0i64)
    } else {
        let runs: Vec<(u64, u64)> = shared_extents
            .iter()
            .map(|&(_logical, paddr, len_bytes)| (paddr, len_bytes / bsz as u64))
            .collect();
        let live_recs = extref_records_for_runs(txn, old_extref_paddr, &runs, bsz)?;
        let plan = plan_extref_take(&live_recs, &runs)?;
        rewrite_extref_tree(
            txn,
            old_extref_paddr,
            new_xid,
            plan.update_inserts,
            &plan.remove_keys,
            &plan.upserts,
            bsz,
        )?
    };

    // --- Step 8: COW the volume omap header. ---
    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    // --- Step 9: Stage the updated volume superblock. ---
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    wr_u64(&mut new_vsb, VSBI_NEXT_OBJ_ID, next_obj_id);
    let num_files = rd_u64(&new_vsb, VSBI_NUM_FILES);
    wr_u64(&mut new_vsb, VSBI_NUM_FILES, num_files + 1);
    // Reclaim COW-replaced metadata (old omap/catalog + old extentref nodes).
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &old_extref_padrs,
        &reclaim,
        bsz,
    )?;

    // fs_alloc_count: a clone adds no data blocks; only catalog/omap + extentref
    // node deltas. frm_correction: sm_fq-pinned old nodes stay counted. [#149]
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let new_alloc = (fs_alloc + node_delta + extref_node_delta + frm_correction).max(0) as u64;
    wr_u64(&mut new_vsb, VSBI_FS_ALLOC_COUNT, new_alloc);
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    Ok(new_ino)
}

// ---------------------------------------------------------------------------
// overwrite_existing_file - single-transaction in-place content replacement.
//
// Preserves the existing inode number (file identity) and DREC so directory
// listings remain stable. Frees the old data extents (snapshot-safe), allocates
// new ones, and rewrites the catalog inode + extent records in one transaction.
// [linux-apfs-rw:file.c / inode.c / extents.c - authoritative write path]
// ---------------------------------------------------------------------------

/// Replace the content of an existing regular file in a single transaction.
///
/// The inode number is preserved; only the data extents, inode size fields, and
/// modification timestamps are updated. The parent DREC is left untouched.
///
/// Returns `Ok(())` on success, `TxnError::NotFound` if `name` does not exist
/// in `parent_ino`.
pub fn overwrite_existing_file<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
    new_content: &[u8],
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, omap_raw)?;
    let (vsb_raw, omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    // Apply NFD unconditionally: modern APFS hashes the normalized form
    // (the APFS specification). For ASCII this is a no-op.
    let _norm_flag_observed = incompat & APFS_INCOMPAT_NORMALIZATION_INSENSITIVE != 0;
    let normalize = true;

    // --- Step 1+2: Read volume omap node. ---
    let omap_tree_paddr = rd_u64(omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;

    // --- Step 3: Gather the catalog records this write reads. ---
    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
    let all = gather_entries(txn, &omap_node, root_tree_oid, &[&drec_key], bsz)?;

    // --- Step 4: Locate DREC for (parent_ino, name). ---
    let file_id = all
        .iter()
        .find(|(k, _)| *k == drec_key)
        .map(|(_, v)| rd_u64(v, 0) & 0x0FFF_FFFF_FFFF_FFFF)
        .ok_or_else(|| TxnError::NotFound(format!("'{name}' not in parent {parent_ino}")))?;

    // --- Step 6: Scan for the inode, then for the stream it names. ---
    // The stream is keyed by the inode's private_id, not by the inode's own
    // id. The two differ when the file shares a stream with a clone macOS
    // made, and the stream's own record then counts how many inodes name it.
    let mut old_inode_val = all
        .iter()
        .find(|(k, _)| {
            rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF == file_id && rd_u64(k, 0) >> 60 == APFS_TYPE_INODE
        })
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::NotFound(format!("inode {file_id} not found")))?;
    let old_stream_id = match rd_u64(&old_inode_val, INODE_PRIVATE_ID) {
        0 => file_id,
        id => id,
    };
    let mut old_stream_refcnt: u32 = 1;
    let mut data_blocks: Vec<(u64, u64)> = Vec::new(); // (phys_start, block_count)
    let mut old_extent_logical_addrs: Vec<u64> = Vec::new();

    for (k, v) in &all {
        let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
        let ty = rd_u64(k, 0) >> 60;
        if oid != old_stream_id {
            continue;
        }
        if ty == APFS_TYPE_DSTREAM_ID {
            old_stream_refcnt = rd_u32(v, 0);
        }
        if ty == APFS_TYPE_FILE_EXTENT {
            // j_file_extent_key_t: the key encodes the logical offset in bits 0..63
            // after the common obj_id_and_type header (first 8 bytes).
            let logical_addr = rd_u64(k, 8);
            old_extent_logical_addrs.push(logical_addr);
            let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
            let count = len_bytes / bsz as u64;
            data_blocks.push((rd_u64(v, 8), count));
        }
    }

    // A shared stream is left exactly as it is - its extents, its blocks, its
    // reference counts - and the file being written gets a stream of its own:
    // the shared one loses this inode's reference, and the new content goes
    // under a fresh id that the inode names from now on. Nothing the other
    // file reads is touched.
    // [CERTAIN: linux-apfs-rw/inode.c apfs_inode_create_dstream_rec on a
    //  shared stream: put the shared record, clone the extents under a new
    //  id, create the new record; apfsprogs/apfsck/extents.c requires the
    //  stream's count to equal the number of inodes naming it]
    let stream_shared = old_stream_refcnt > 1;
    let mut stream_updates: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut fresh_stream_id: Option<u64> = None;
    let new_stream_id = if !stream_shared {
        old_stream_id
    } else {
        data_blocks.clear();
        old_extent_logical_addrs.clear();
        stream_updates.push((
            build_dstream_id_key(old_stream_id),
            build_dstream_id_val(old_stream_refcnt - 1),
        ));
        if new_content.is_empty() && file_id != old_stream_id {
            // No content, no stream: the inode names itself, as a file created
            // empty does.
            file_id
        } else {
            // A fresh id, also for empty content when this inode's own number
            // IS the shared stream's id: the file a clone was made from lends
            // its number to the stream, so naming itself here would name the
            // stream the other file keeps, and the next write into this file
            // would take that stream for its own and free the other file's
            // blocks.
            let id = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);
            fresh_stream_id = Some(id);
            id
        }
    };
    wr_u64(&mut old_inode_val, INODE_PRIVATE_ID, new_stream_id);

    // A compressed file keeps its content in the com.apple.decmpfs xattr and,
    // when it does not fit inline, in the resource fork; the data stream holds
    // none of it. Writing the stream and leaving those in place would leave
    // every reader returning the old content, so the file stops being
    // compressed instead: the two xattrs and anything the resource fork owns go
    // with the old content, and the flag comes off. This is what macOS does on
    // the first write to a compressed file, minus the decompression - the whole
    // content is being replaced here, so there is nothing to carry over.
    let compressed = rd_u32(&old_inode_val, INOI_BSD_FLAGS) & UF_COMPRESSED != 0;
    let mut decmpfs_remove_keys: Vec<Vec<u8>> = Vec::new();
    if compressed {
        let cleared = rd_u32(&old_inode_val, INOI_BSD_FLAGS) & !UF_COMPRESSED;
        wr_u32(&mut old_inode_val, INOI_BSD_FLAGS, cleared);
        // The decompressed length the xattr recorded no longer describes
        // anything once the xattr is gone, and the flags that said the file has
        // a resource fork and a recorded uncompressed size stop being true.
        // fsck names both: "need to clear internal_flags 0x4000" and "need to
        // set flags (0x8000)".
        wr_u64(&mut old_inode_val, INODE_UNCOMPRESSED_SIZE, 0);
        let internal = (rd_u64(&old_inode_val, INODE_INTERNAL_FLAGS)
            & !(INODE_HAS_RSRC_FORK | INODE_HAS_UNCOMPRESSED_SIZE))
            | INODE_NO_RSRC_FORK;
        wr_u64(&mut old_inode_val, INODE_INTERNAL_FLAGS, internal);

        let mut stream_ids: Vec<u64> = Vec::new();
        for name in ["com.apple.decmpfs", "com.apple.ResourceFork"] {
            let key = build_xattr_key(file_id, name);
            let Some((_, val)) = all.iter().find(|(k, _)| *k == key) else {
                continue;
            };
            decmpfs_remove_keys.push(key);
            // A value too big to sit inline lives in its own stream, whose
            // records are keyed by that stream's id rather than the file's.
            if rd_u16(val, 0) & XATTR_DATA_STREAM != 0 {
                stream_ids.push(rd_u64(val, 4));
            }
        }
        for sid in stream_ids {
            for (k, v) in &all {
                if rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF != sid {
                    continue;
                }
                match rd_u64(k, 0) >> 60 {
                    APFS_TYPE_DSTREAM_ID => decmpfs_remove_keys.push(k.clone()),
                    APFS_TYPE_FILE_EXTENT => {
                        decmpfs_remove_keys.push(k.clone());
                        let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
                        data_blocks.push((rd_u64(v, 8), len_bytes.div_ceil(bsz as u64)));
                    }
                    _ => {}
                }
            }
        }
    }
    let old_inode_val = old_inode_val;

    // Preserve the NAME xfield from the old inode value (the full 92-byte
    // fixed prefix and all other xfields are forwarded via
    // rebuild_inode_preserving_unknown - no need to unpack them individually).
    let old_name_str = extract_inode_name(&old_inode_val).unwrap_or(name);

    // --- Step 7: Snapshot-safe accounting of old data extents (no COW yet).
    // Build the working extref buffer in memory: remove owned old phys_ext
    // records. The actual alloc + stage_raw of the COW'd extref node happens
    // ONCE in Step 10b after any new phys_ext records are also folded in -
    // otherwise two sequential alloc + stage_raw would leak one block and
    // fsck would report "overallocation". ---
    let old_data_block_count: u64 = data_blocks.iter().map(|(_, c)| c).sum();
    let old_extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    let mut freed_count = 0u64;

    // Classify old extents for extref mutation: refcnt==1 → remove+free,
    // refcnt>1 → decrement (upsert), not-in-live → KIND_UPDATE(-1) insert. [#149]
    // Collect live records once (multi-node aware). [#151]
    let mut extref_remove_keys: Vec<Vec<u8>> = Vec::new();
    let mut extref_upserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut extref_update_inserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut owned_runs_to_free: Vec<(u64, u64)> = Vec::new();

    if !data_blocks.is_empty() {
        let live_recs = extref_records_for_runs(txn, old_extref_paddr, &data_blocks, bsz)?;
        let release = plan_extref_release(&live_recs, &data_blocks)?;
        extref_remove_keys = release.remove_keys;
        extref_upserts = release.upserts;
        extref_update_inserts = release.update_inserts;
        owned_runs_to_free = release.owned_runs;
    }

    // --- Step 8: Build remove_keys (inode, and the old stream's records when
    // the stream was this file's alone; NOT the drec). ---
    let mut remove_keys: Vec<Vec<u8>> = vec![build_inode_key(file_id)];
    if !stream_shared {
        remove_keys.push(build_dstream_id_key(old_stream_id));
        for &logical_addr in &old_extent_logical_addrs {
            remove_keys.push(build_file_extent_key(old_stream_id, logical_addr));
        }
    }
    remove_keys.extend(decmpfs_remove_keys);

    // --- Steps 9/10: Build new catalog records and allocate new data blocks. ---
    // Also collect new phys_ext inserts for extref tree.
    let mut extref_new_inserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let (new_records, new_data_block_count) = if new_content.is_empty() {
        // Step 9: empty content - no dstream, no extents.
        // Use rebuild_inode_preserving_unknown so that unrecognized xfields
        // from the original inode (e.g. DOCUMENT_ID, FINDER_INFO) survive
        // the content-replace operation. [W4-FRAG-1]
        let inode_key = build_inode_key(file_id);
        // The new content has no holes: whatever sparse bytes the old stream
        // had go with it.
        let inode_val = with_write_generation_step(rebuild_inode_with_sparse_bytes(
            &old_inode_val,
            old_name_str,
            None,
            0,
            now,
        ));
        (vec![(inode_key, inode_val)], 0u64)
    } else {
        // Step 10: allocate new blocks for content.
        // Use alloc_blocks_run for contiguous batch allocation; loop for multi-chunk.
        let num_blocks = new_content.len().div_ceil(bsz);
        let mut paddrs: Vec<u64> = Vec::with_capacity(num_blocks);
        let mut blocks_remaining = num_blocks;
        while blocks_remaining > 0 {
            let (run_start, run_count) = txn.alloc_blocks_run(blocks_remaining)?;
            if run_count == 0 {
                return Err(TxnError::NoFreeBlocks);
            }
            // The run's whole blocks straight from the content, in one
            // write; a last block the content only partly fills, padded
            // with zeros.
            let start = (num_blocks - blocks_remaining) * bsz;
            let run_end = start + run_count * bsz;
            let whole_end = (new_content.len() / bsz * bsz).clamp(start, run_end);
            if whole_end > start {
                txn.write_new_data(run_start, new_content.get(start..whole_end).unwrap_or(&[]))?;
            }
            if whole_end < run_end {
                let mut tail = vec![0u8; bsz];
                wr_bytes(&mut tail, 0, new_content.get(whole_end..).unwrap_or(&[]));
                txn.write_new_data(run_start + ((whole_end - start) / bsz) as u64, &tail)?;
            }
            paddrs.extend((0..run_count as u64).map(|j| run_start + j));
            blocks_remaining -= run_count;
        }
        // Group into contiguous runs.
        let mut runs: Vec<(usize, u64, u64)> = Vec::new();
        for (i, &p) in paddrs.iter().enumerate() {
            match runs.last_mut() {
                Some(last) if last.1 + last.2 == p => last.2 += 1,
                _ => runs.push((i, p, 1)),
            }
        }

        // Collect new phys_ext records (inserted into extref tree below).
        for &(_, pstart, cnt) in &runs {
            extref_new_inserts.push((
                build_phys_ext_key(pstart),
                build_phys_ext_val(cnt, new_stream_id, 1),
            ));
        }

        let alloced = (num_blocks * bsz) as u64;
        let inode_key = build_inode_key(file_id);
        // Use rebuild_inode_preserving_unknown so that unrecognized xfields
        // from the original inode survive the content-replace operation. [W4-FRAG-1]
        // Every byte of the new content is backed by a block: no holes, so
        // no sparse bytes, whatever the old stream had.
        let inode_val = with_write_generation_step(rebuild_inode_with_sparse_bytes(
            &old_inode_val,
            old_name_str,
            Some(DstreamArgs {
                size: new_content.len() as u64,
                alloced_size: alloced,
            }),
            0,
            now,
        ));
        let mut recs = vec![
            (inode_key, inode_val),
            (build_dstream_id_key(new_stream_id), build_dstream_id_val(1)),
        ];
        for &(lblk, pstart, cnt) in &runs {
            recs.push((
                build_file_extent_key(new_stream_id, (lblk * bsz) as u64),
                build_file_extent_val(cnt * bsz as u64, pstart),
            ));
        }
        (recs, num_blocks as u64)
    };

    // --- Step 10b: Rewrite extref tree once combining all changes. [#151]
    // Combine KIND_UPDATE(-1) inserts + new phys_ext inserts into one insert batch.
    let mut all_inserts = extref_update_inserts;
    all_inserts.extend(extref_new_inserts);
    let extref_changed =
        !all_inserts.is_empty() || !extref_remove_keys.is_empty() || !extref_upserts.is_empty();
    let (new_extref_paddr, old_extref_padrs, extref_node_delta) = if extref_changed {
        let (np, op, nd) = rewrite_extref_tree(
            txn,
            old_extref_paddr,
            new_xid,
            all_inserts,
            &extref_remove_keys,
            &extref_upserts,
            bsz,
        )?;
        // Free data blocks whose last reference was just removed.
        for &(phys, count) in &owned_runs_to_free {
            for b in phys..phys + count {
                txn.free_block(b)?;
            }
            freed_count += count;
        }
        (np, op, nd)
    } else {
        (old_extref_paddr, vec![], 0i64)
    };

    // --- Step 11: Rewrite the fstree - remove old records, insert new ones,
    // and lower a shared stream's count. Parent nchildren unchanged (file
    // already existed). ---
    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree_impl(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        new_records,
        &remove_keys,
        None,
        &stream_updates,
        now,
        bsz,
    )?;

    // --- Step 12: COW the volume omap header. ---
    let new_vomap_paddr = stage_vomap_header(txn, omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    // --- Step 13+14: Update and stage the volume superblock. ---
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    // A stream of its own took an id out of the volume's pool; the counter has
    // to move past it or the next allocation hands the same id out again.
    if let Some(id) = fresh_stream_id {
        wr_u64(&mut new_vsb, VSBI_NEXT_OBJ_ID, id + 1);
    }
    // --- Step 15: Free COW-replaced metadata. ---
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        omap_raw,
        &omap_node,
        &old_extref_padrs,
        &reclaim,
        bsz,
    )?;

    // fs_alloc_count: +new data blocks, -freed data blocks, +net catalog delta + extref delta.
    // frm_correction: sm_fq-pinned old metadata stays counted. [#149 fix]
    // NUM_FILES unchanged (same inode, already counted).
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let alloc_delta = new_data_block_count as i64 - freed_count as i64
        + node_delta
        + extref_node_delta
        + frm_correction;
    wr_u64(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (fs_alloc + alloc_delta).max(0) as u64,
    );
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    let _ = old_data_block_count; // retained for clarity; not written to VSB separately
    Ok(())
}

/// Extract the raw NAME string from an inode value's xfield blob.
/// Returns `None` if the xfield is missing or not valid UTF-8.
fn extract_inode_name(inode_val: &[u8]) -> Option<&str> {
    let nn = rd_u16(inode_val, 92) as usize;
    let mut entry = 96usize;
    let mut doff = 96 + nn * 4;
    for _ in 0..nn {
        let xt = *inode_val.get(entry)?;
        let xs = rd_u16(inode_val, entry + 2) as usize;
        if xt == INO_EXT_TYPE_NAME {
            // Name is NUL-terminated; strip the NUL.
            let raw = inode_val.get(doff..doff + xs)?;
            let name_bytes = raw.split(|&b| b == 0).next().unwrap_or(raw);
            return std::str::from_utf8(name_bytes).ok();
        }
        doff += round_up8(xs);
        entry += 4;
    }
    None
}

/// Serialize a xfield blob from its constituent parts into `out` starting at
/// `INODE_XFIELDS_OFFSET` (92). Layout:
///   4-byte xf_blob_t header (xf_num_exts u16 + xf_used_data u16)
///   N × 4-byte x_field_t descriptors (x_type u8, x_flags u8, x_size u16)
///   data area: each field's payload zero-padded to round_up(x_size, 8)
///
/// Fields are emitted in ascending x_type order (NAME=4 first, then unknown
/// types in their original x_type order, DSTREAM=8 last if present) - the
/// one order this writer keeps; the checker does not require one.
/// Unknown fields with x_type < 4 are emitted before NAME; those between
/// NAME and DSTREAM or after DSTREAM are placed in natural sorted order.
///
/// The returned `Vec<u8>` is the complete inode value (92-byte prefix already
/// present in `prefix`, with xfield blob appended).
fn build_inode_val_with_xfields(
    prefix92: &[u8],
    name_bytes: &[u8],      // NUL-terminated name (incl. NUL)
    dstream: Option<&[u8]>, // raw j_dstream_t bytes (exactly 40 bytes), or None
    unknowns: &[apfs_core::inode::UnknownXfield],
) -> Vec<u8> {
    // Determine xfield count and data area size.
    // Order: all unknown fields with x_type < NAME(4), then NAME(4),
    // then unknown fields with 4 < x_type < DSTREAM(8), then DSTREAM(8) if present,
    // then unknown fields with x_type > DSTREAM(8).
    // For simplicity and correctness we sort the descriptor list by x_type.
    let name_len = name_bytes.len();
    let name_area = round_up8(name_len);
    let ds_area = dstream.map(|d| round_up8(d.len())).unwrap_or(0);
    let unk_areas: Vec<usize> = unknowns.iter().map(|u| round_up8(u.data.len())).collect();
    let unk_total: usize = unk_areas.iter().sum();
    let nxf = 1 + unknowns.len() + if dstream.is_some() { 1 } else { 0 };
    let xf_used = name_area + unk_total + ds_area;
    let xf_hdr = 4 + nxf * 4; // 4-byte xf_blob header + 4-byte descriptors
    let total = 92 + xf_hdr + xf_used;
    let mut v = vec![0u8; total];
    wr_bytes(&mut v, 0, prefix92.get(..92).unwrap_or(prefix92));

    // Build a sorted list of (x_type, x_flags, data_ref_index/-1 for name/dstream).
    // We collect descriptors in sorted x_type order.
    // Use a small scratch structure: (x_type, x_flags, slot: usize meaning in data area).
    // Emit directly in sorted order.
    //
    // Collect all entries: (x_type, x_flags, payload_slice).
    // name: x_type=4, dstream: x_type=8, unknowns: their x_type.
    let mut entries: Vec<(u8, u8, &[u8])> = Vec::with_capacity(nxf);
    entries.push((INO_EXT_TYPE_NAME, XF_DO_NOT_COPY, name_bytes));
    if let Some(d) = dstream {
        entries.push((INO_EXT_TYPE_DSTREAM, XF_SYSTEM_FIELD, d));
    }
    for u in unknowns {
        entries.push((u.x_type, u.x_flags, &u.data));
    }
    // Sort ascending by x_type (fsck requires sorted order).
    entries.sort_by_key(|&(t, _, _)| t);

    let desc_base = 92 + 4; // x_field_t array starts at byte 96
    let data_base = desc_base + entries.len() * 4;
    let mut data_off = data_base;
    for (i, &(x_type, x_flags, payload)) in entries.iter().enumerate() {
        let desc_off = desc_base + i * 4;
        wr_bytes(&mut v, desc_off, &[x_type, x_flags]);
        wr_u16(&mut v, desc_off + 2, payload.len() as u16);
        wr_bytes(&mut v, data_off, payload);
        data_off += round_up8(payload.len());
    }
    // xf_blob header.
    wr_u16(&mut v, 92, nxf as u16); // xf_num_exts
    wr_u16(&mut v, 94, xf_used as u16); // xf_used_data
    v
}

/// Rebuild an inode value with a new NAME xfield, preserving the 92-byte fixed
/// prefix (ownership, mode, timestamps), the DSTREAM xfield (if present), and
/// any unrecognized xfields from the old inode value. `change_time` is bumped
/// to `now`.
///
/// Unrecognized xfields (types other than NAME=4 and DSTREAM=8) are parsed from
/// `old` and round-tripped verbatim. This prevents silent data loss of Apple
/// metadata (DOCUMENT_ID, FINDER_INFO, etc.) on rename/move.
/// [APFS spec: xfield TLV is forward-compatible - unknown types preserved]
fn rebuild_inode_with_name(old: &[u8], new_name: &str, now: u64) -> Vec<u8> {
    // Extract the existing DSTREAM xfield (type 8) data, if any.
    let nn = rd_u16(old, 92) as usize;
    let mut entry = 96;
    let mut doff = 96 + nn * 4;
    let mut dstream: Option<Vec<u8>> = None;
    for _ in 0..nn {
        let xt = *old.get(entry).unwrap_or(&0);
        let xs = rd_u16(old, entry + 2) as usize;
        if xt == INO_EXT_TYPE_DSTREAM {
            dstream = old.get(doff..doff + xs).map(|s| s.to_vec());
        }
        doff += round_up8(xs);
        entry += 4;
    }

    // Parse unknown xfields. On parse error (corrupt inode) fall back to
    // dropping unknowns rather than failing the rename operation.
    let unknowns = apfs_core::inode::parse_unknown_xfields(old).unwrap_or_default();

    // Build NUL-terminated name bytes.
    let mut name_nul = Vec::with_capacity(new_name.len() + 1);
    name_nul.extend_from_slice(new_name.as_bytes());
    name_nul.push(0);

    // Copy fixed 92-byte prefix, bump change_time.
    let mut prefix = old.get(..92).unwrap_or(&[0u8; 92]).to_vec();
    prefix.resize(92, 0);
    wr_u64(&mut prefix, INODE_CHANGE_TIME, now);

    build_inode_val_with_xfields(&prefix, &name_nul, dstream.as_deref(), &unknowns)
}

/// Rebuild an inode value for an inode that is being rewritten with new content
/// or size, preserving the 92-byte fixed prefix, the NAME xfield, any new
/// DSTREAM data, and all unrecognized xfields from the old inode value.
///
/// Used by `overwrite_existing_file` and `truncate_file_fast` so that unknown
/// xfields (DOCUMENT_ID, FINDER_INFO, etc.) are not silently dropped on a
/// content or size update. [APFS spec: xfield TLV is forward-compatible]
///
/// `sparse_bytes` is the hole count of the stream being described: the
/// sparse-bytes xfield is set to it (or removed when it is zero) and the
/// sparse flag follows, which is the pair fsck checks against each other and
/// against the hole extents. A caller that just wrote every byte passes zero.
fn rebuild_inode_with_sparse_bytes(
    old: &[u8],
    name: &str,
    dstream: Option<DstreamArgs>,
    sparse_bytes: u64,
    now: u64,
) -> Vec<u8> {
    let mut unknowns = apfs_core::inode::parse_unknown_xfields(old).unwrap_or_default();
    unknowns.retain(|x| x.x_type != apfs_core::inode::INO_EXT_TYPE_SPARSE_BYTES);
    if sparse_bytes > 0 {
        unknowns.push(apfs_core::inode::UnknownXfield {
            x_type: apfs_core::inode::INO_EXT_TYPE_SPARSE_BYTES,
            x_flags: XF_SPARSE_BYTES_FLAGS,
            data: sparse_bytes.to_le_bytes().to_vec(),
        });
    }

    let ds_bytes: Option<Vec<u8>> = dstream.map(|ds| {
        let mut b = vec![0u8; DSTREAM_SIZE];
        wr_u64(&mut b, 0, ds.size);
        wr_u64(&mut b, 8, ds.alloced_size);
        wr_u64(&mut b, 24, ds.size);
        b
    });
    let mut name_nul = Vec::with_capacity(name.len() + 1);
    name_nul.extend_from_slice(name.as_bytes());
    name_nul.push(0);

    let mut prefix = old.get(..92).unwrap_or(&[0u8; 92]).to_vec();
    prefix.resize(92, 0);
    wr_u64(&mut prefix, INODE_MOD_TIME, now);
    wr_u64(&mut prefix, INODE_CHANGE_TIME, now);
    let flags = rd_u64(&prefix, INODE_INTERNAL_FLAGS);
    let flags = if sparse_bytes > 0 {
        flags | INODE_IS_SPARSE
    } else {
        flags & !INODE_IS_SPARSE
    };
    wr_u64(&mut prefix, INODE_INTERNAL_FLAGS, flags);

    build_inode_val_with_xfields(&prefix, &name_nul, ds_bytes.as_deref(), &unknowns)
}

/// Rename `old_name` to `new_name` within directory `parent_ino`.
///
/// Replaces the DREC (new name in the key) and the INODE (NAME xfield updated,
/// dstream + prefix preserved). The file's DSTREAM_ID / FILE_EXTENT records are
/// keyed by the unchanged file id and stay untouched. [the APFS specification]
/// Like every catalog change, it rewrites only the nodes on the paths to the
/// records it changes (fsck regressions: `rename_corruption_138`,
/// `rename_multinode_140`).
pub fn rename<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    parent_ino: u64,
    old_name: &str,
    new_name: &str,
    replace_if_exists: bool,
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    validate_entry_name(new_name)?;
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    // Apply NFD unconditionally: modern APFS hashes the normalized form
    // (the APFS specification). For ASCII this is a no-op; for Unicode
    // it is required even when APFS_INCOMPAT_NORMALIZATION_INSENSITIVE is unset
    // because fsck_apfs validates against NFD regardless of the flag.
    let _norm_flag_observed = incompat & APFS_INCOMPAT_NORMALIZATION_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let old_drec_key = build_drec_key(parent_ino, old_name, case_fold, normalize);
    let new_drec_key = build_drec_key(parent_ino, new_name, case_fold, normalize);
    let all = gather_entries(
        txn,
        &omap_node,
        root_tree_oid,
        &[&old_drec_key, &new_drec_key],
        bsz,
    )?;

    // Locate the old DREC -> file_id + dirent type + date_added.
    let drec_val = all
        .iter()
        .find(|(k, _)| *k == old_drec_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::SpacemanParse("old name not found".into()))?;
    let file_id = rd_u64(&drec_val, 0);
    let date_added = rd_u64(&drec_val, 8);
    let dt_type = rd_u16(&drec_val, 16);

    // Rebuild the inode with the new name (preserving prefix + dstream).
    let inode_key = build_inode_key(file_id);
    let old_inode_val = all
        .iter()
        .find(|(k, _)| *k == inode_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::SpacemanParse("inode not found".into()))?;
    // A name that is one of several carries the sibling id binding it to the
    // inode. The renamed entry has to keep it, and the record behind that id
    // has to be rewritten with the new name - otherwise the entry stops being a
    // link at all and the record goes on naming something that is gone.
    let sibling_id = drec_sibling_id(&drec_val);
    let sibling_updates: Vec<(Vec<u8>, Vec<u8>)> = match sibling_id {
        Some(sid) => vec![(
            build_sibling_link_key(file_id, sid),
            build_sibling_link_val(parent_ino, new_name),
        )],
        None => Vec::new(),
    };
    // The inode's own name is the primary link's - the one with the lowest
    // sibling id. Renaming any other name leaves it alone.
    let renames_primary = match sibling_id {
        None => true,
        Some(sid) => {
            let lowest = all
                .iter()
                .filter(|(k, _)| {
                    rd_u64(k, 0) >> 60 == APFS_TYPE_SIBLING_LINK
                        && rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF == file_id
                })
                .map(|(k, _)| rd_u64(k, 8))
                .min();
            lowest == Some(sid)
        }
    };
    let new_inode_val = if renames_primary {
        rebuild_inode_with_name(&old_inode_val, new_name, now)
    } else {
        old_inode_val.clone()
    };
    let new_drec = (
        new_drec_key.clone(),
        match sibling_id {
            Some(sid) => build_drec_val_with_sibling(file_id, date_added, dt_type, sid),
            None => build_drec_val(file_id, date_added, dt_type),
        },
    );

    // Detect an existing target with `new_name`. If found and
    // `replace_if_exists`, collect its records + owned data blocks for the
    // replacement transaction; otherwise return AlreadyExists.
    let target_replace = collect_replace_target(
        &all,
        &new_drec_key,
        file_id,
        replace_if_exists,
        bsz,
        case_fold,
        normalize,
    )?;

    // Build remove_keys including the target's records when replacing.
    let mut remove_keys: Vec<Vec<u8>> = vec![old_drec_key, inode_key.clone()];
    let mut data_blocks_to_free: Vec<(u64, u64)> = Vec::new();
    // Replacing the target removes ONE extra catalog entry under this parent
    // (the target's drec is dropped; the source's drec is renamed to the
    // target's slot), so parent nchildren decreases by 1 net. Without
    // replacement nchildren is unchanged.
    let parent_delta = if let Some(ref tr) = target_replace {
        remove_keys.extend(tr.remove_keys.iter().cloned());
        data_blocks_to_free.extend(tr.data_blocks.iter().copied());
        -1i64
    } else {
        0i64
    };
    // Free the target's owned data blocks via the live-extref ownership
    // pattern before the catalog rewrite. Snapshot-pinned blocks stay
    // allocated (they remain in some snapshot's frozen extref tree).
    // Returns new extref root paddr, old padrs for frm, extref node delta, freed count. [#151]
    let (new_extref_paddr, extref_old_padrs, extref_node_delta, freed_count) =
        free_blocks_owned_by_live_extref(txn, vsb_raw, new_xid, &data_blocks_to_free, bsz)?;

    // Rewrite the FSTREE: remove old DREC + INODE (+ target records if
    // replacing), insert new DREC + INODE.
    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree_impl(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        vec![new_drec, (inode_key, new_inode_val)],
        &remove_keys,
        Some((parent_ino, parent_delta)),
        &sibling_updates,
        now,
        bsz,
    )?;

    // COW the volume omap header (om_tree_oid -> rewritten b-tree).
    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    // Stage the updated (virtual) volume superblock. When a target was
    // replaced we also update the extentref tree pointer and decrement
    // num_files for the dropped inode.
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    if let Some(ref tr) = target_replace {
        uncount_replaced(&mut new_vsb, tr);
    }
    // Reclaim COW-replaced metadata. extref_old_padrs is empty when no data
    // blocks were freed (rename without replace target). [#151]
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &extref_old_padrs,
        &reclaim,
        bsz,
    )?;

    // fs_alloc_count: -freed_data_blocks + net catalog node delta + extref node delta.
    // frm_correction: sm_fq-pinned old metadata stays counted. [#149 fix]
    // [the APFS specification Q5]
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let alloc_delta = -(freed_count as i64) + node_delta + extref_node_delta + frm_correction;
    wr_u64(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (fs_alloc + alloc_delta).max(0) as u64,
    );
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    Ok(())
}

/// Records collected from a pre-existing target during a replace-mode
/// rename / move. The catalog records (DREC + INODE + DSTREAM_ID +
/// FILE_EXTENT entries) are removed from the fstree; `data_blocks` are
/// freed against the live-extref tree via [`free_blocks_owned_by_live_extref`].
struct TargetReplace {
    /// The target's directory entry and everything its removal takes.
    remove_keys: Vec<Vec<u8>>,
    data_blocks: Vec<(u64, u64)>,
    /// Records the removal changes rather than removes (a shared stream's
    /// count, a re-pointed hard-linked inode).
    updates: Vec<(Vec<u8>, Vec<u8>)>,
    /// Whether the target's inode itself went, and what kind it was - for the
    /// volume's per-kind counters.
    last_name: bool,
    mode: u16,
}

/// Look up a target DREC by exact key. If present and `replace_if_exists`,
/// collect its catalog records and owned data extents for removal in the
/// caller's transaction. Returns `AlreadyExists` when the target exists
/// but `replace_if_exists=false`, and `DirectoryNotEmpty` when the target
/// is a non-empty directory (POSIX ENOTEMPTY - replacing a non-empty dir
/// is not legal even on Windows MoveFileEx(REPLACE_EXISTING)).
fn collect_replace_target(
    all: &[(Vec<u8>, Vec<u8>)],
    target_drec_key: &[u8],
    src_file_id: u64,
    replace_if_exists: bool,
    bsz: usize,
    case_fold: bool,
    normalize: bool,
) -> Result<Option<TargetReplace>, TxnError> {
    // The target is whatever entry the new name collides with as the volume
    // compares names, not only an entry spelled the same way.
    let target_drec = match find_drec_collision(all, target_drec_key, case_fold, normalize) {
        Some(x) => x,
        None => return Ok(None),
    };
    let target_id = rd_u64(&target_drec.1, 0) & 0x0FFF_FFFF_FFFF_FFFF;
    // Self-rename (same inode with a new normalised name) is not a replace
    // - just let the normal path proceed (caller already has the existing
    // record set in remove_keys). Skip target collection in that case.
    if target_id == src_file_id {
        return Ok(None);
    }
    if !replace_if_exists {
        return Err(TxnError::AlreadyExists(format!(
            "rename target exists (inode {target_id}) and REPLACE flag not set"
        )));
    }
    // Replacing an entry is removing that name, with everything that entails
    // for a hard link, a clone's shared stream, or an attribute stream.
    let plan = plan_removal(all, target_id, &target_drec.1, now_ns(), bsz)?;
    if plan.is_dir && plan.dir_nchildren > 0 {
        return Err(TxnError::DirectoryNotEmpty(format!(
            "rename target inode {target_id} is a non-empty directory"
        )));
    }
    // The entry to remove is the one found, under the spelling it is stored
    // with - not the key built from the name being given.
    let mut remove_keys: Vec<Vec<u8>> = vec![target_drec.0.clone()];
    remove_keys.extend(plan.remove_keys);
    Ok(Some(TargetReplace {
        remove_keys,
        data_blocks: plan.data_blocks,
        updates: plan.updates,
        last_name: plan.last_name,
        mode: plan.mode,
    }))
}

/// `internal_flags` bit: the directory keeps running size and count totals
/// in a directory-statistics record (`INODE_MAINTAIN_DIR_STATS`), and every
/// change under it has to update that record and its ancestors' records.
const INODE_MAINTAIN_DIR_STATS: u64 = 0x0000_0002;

/// Refuse to change the contents of a directory that maintains directory
/// statistics. Its counts live in a separate record that macOS updates on
/// every create, remove, clone and move, together with the records of its
/// ancestors; patching the inode's child count alone would leave the
/// statistics stale, which fsck reports. Such directories are rare (nothing
/// a stock macOS makes on an external volume carries the flag), so refusing
/// the change, with the reason, is the honest answer until the statistics
/// are maintained too.
fn refuse_dir_stats(parent_inode_val: &[u8], parent_ino: u64) -> Result<(), TxnError> {
    if parent_inode_val.len() >= INODE_INTERNAL_FLAGS + 8
        && rd_u64(parent_inode_val, INODE_INTERNAL_FLAGS) & INODE_MAINTAIN_DIR_STATS != 0
    {
        return Err(TxnError::Unsupported(format!(
            "directory {parent_ino} maintains directory statistics, which this writer does \
             not update yet; changing its contents would leave them stale"
        )));
    }
    Ok(())
}

/// What releasing a file's data runs does to the live extentref tree.
struct ExtrefRelease {
    /// Records that vanish: their whole range was the last reference.
    remove_keys: Vec<Vec<u8>>,
    /// Records rewritten in place (a lowered count, a shortened range) and the
    /// new records a split leaves beside them.
    upserts: Vec<(Vec<u8>, Vec<u8>)>,
    /// Ranges no live record covers: the volume withdraws one reference from a
    /// range a snapshot owns (a KIND_UPDATE record with delta -1).
    update_inserts: Vec<(Vec<u8>, Vec<u8>)>,
    /// Block runs nothing references any more, to be freed.
    owned_runs: Vec<(u64, u64)>,
}

/// Low 60 bits of `len_and_kind`: the record's block count.
const PHYS_EXT_LEN_MASK: u64 = 0x0FFF_FFFF_FFFF_FFFF;

/// The same record value over a different number of blocks.
pub(crate) fn phys_ext_with_blocks(val: &[u8], blocks: u64) -> Vec<u8> {
    let kind = rd_u64(val, 0) & !PHYS_EXT_LEN_MASK;
    let mut v = val.to_vec();
    if let Some(head) = v.get_mut(..8) {
        head.copy_from_slice(&(kind | (blocks & PHYS_EXT_LEN_MASK)).to_le_bytes());
    }
    v
}

/// Plan the release of `runs` (physical block runs of one file) against the
/// live extentref records, the way Apple and linux-apfs-rw do it: as a walk
/// over the physical range, not a lookup of the run's first block.
///
/// A reference record covers a range of blocks, and a file's extent need not
/// coincide with one record: when macOS rewrites part of a clone it splits the
/// shared record at the rewritten range, so the original file's single extent
/// then spans three records (shared, no longer shared, shared). For every
/// piece of a run the covering record is split at the run's edges, the piece
/// inside the run loses one reference (freed when it was the last, lowered
/// otherwise, a KIND_UPDATE delta lowered likewise), and the pieces outside
/// keep their record unchanged under new keys. A piece no record covers is a
/// range a snapshot owns, and gets a KIND_UPDATE(-1) of exactly its length.
fn plan_extref_release(
    live_recs: &[CatRecord],
    runs: &[(u64, u64)],
) -> Result<ExtrefRelease, TxnError> {
    plan_extref_change(live_recs, runs, -1)
}

/// [`plan_extref_release`] of `runs` against the live extentref tree at
/// `extref_paddr`. No runs, nothing to read: an empty plan.
/// The live extent-reference records [`plan_extref_change`] reads for `runs`:
/// each run's covering record and every record starting inside it. A
/// transaction asked to rebuild in full reads the whole tree instead, the
/// reference this narrower read is checked against.
fn extref_records_for_runs<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    root: u64,
    runs: &[(u64, u64)],
    bsz: usize,
) -> Result<Vec<CatRecord>, TxnError> {
    let pairs = if txn.full_rebuild() {
        collect_extref_records(txn, root, bsz)?.0
    } else {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for &(phys, count) in runs {
            if is_sparse_hole(phys) || count == 0 {
                continue;
            }
            let end = phys.saturating_add(count);
            for (k, v) in cow_tree::extref_covering(txn, root, phys, end, bsz)? {
                if seen.insert(k.clone()) {
                    out.push((k, v));
                }
            }
        }
        out
    };
    Ok(pairs
        .into_iter()
        .map(|(key, val)| CatRecord { key, val })
        .collect())
}

fn release_runs<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    extref_paddr: u64,
    runs: &[(u64, u64)],
    bsz: usize,
) -> Result<ExtrefRelease, TxnError> {
    if runs.is_empty() {
        return Ok(ExtrefRelease {
            remove_keys: Vec::new(),
            upserts: Vec::new(),
            update_inserts: Vec::new(),
            owned_runs: Vec::new(),
        });
    }
    let live_recs = extref_records_for_runs(txn, extref_paddr, runs, bsz)?;
    plan_extref_release(&live_recs, runs)
}

/// Plan taking one more reference on `runs` (a clone sharing a file's
/// blocks): the same walk as [`plan_extref_release`] with the opposite sign.
/// Every record a run spans gains a reference over the part inside the run,
/// split at the run's edges, and a piece no record covers (a range a
/// snapshot owns) gets a KIND_UPDATE(+1) of exactly its length. Nothing is
/// freed.
fn plan_extref_take(
    live_recs: &[CatRecord],
    runs: &[(u64, u64)],
) -> Result<ExtrefRelease, TxnError> {
    plan_extref_change(live_recs, runs, 1)
}

/// The walk behind [`plan_extref_release`] (`diff` = -1) and
/// [`plan_extref_take`] (`diff` = +1).
fn plan_extref_change(
    live_recs: &[CatRecord],
    runs: &[(u64, u64)],
    diff: i32,
) -> Result<ExtrefRelease, TxnError> {
    use std::collections::BTreeMap;
    // start block -> (block count, record value)
    let mut current: BTreeMap<u64, (u64, Vec<u8>)> = BTreeMap::new();
    for r in live_recs {
        if r.key.len() < 8 || (rd_u64(&r.key, 0) >> 60) != APFS_TYPE_EXTENT {
            continue;
        }
        if r.val.len() < 20 {
            return Err(TxnError::SpacemanParse(
                "extentref record value shorter than 20 bytes".into(),
            ));
        }
        let start = rd_u64(&r.key, 0) & PHYS_EXT_LEN_MASK;
        let blocks = rd_u64(&r.val, 0) & PHYS_EXT_LEN_MASK;
        current.insert(start, (blocks, r.val.clone()));
    }
    let original = current.clone();

    let mut update_inserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut owned_runs: Vec<(u64, u64)> = Vec::new();
    for &(phys, count) in runs {
        if is_sparse_hole(phys) || count == 0 {
            continue;
        }
        let end = phys.saturating_add(count);
        let mut cur = phys;
        while cur < end {
            let covering = current
                .range(..=cur)
                .next_back()
                .filter(|(start, (blocks, _))| start.saturating_add(*blocks) > cur)
                .map(|(start, _)| *start);
            let Some(start) = covering else {
                let next_start = current.range(cur + 1..).next().map(|(s, _)| *s);
                let gap_end = next_start.map_or(end, |s| s.min(end));
                update_inserts.push((
                    build_phys_ext_key(cur),
                    build_phys_ext_update_val(gap_end - cur, diff),
                ));
                cur = gap_end;
                continue;
            };
            let (blocks, val) = match current.remove(&start) {
                Some(rec) => rec,
                None => break,
            };
            let rec_end = start.saturating_add(blocks);
            let seg_end = end.min(rec_end);
            if start < cur {
                current.insert(
                    start,
                    (cur - start, phys_ext_with_blocks(&val, cur - start)),
                );
            }
            let len = seg_end - cur;
            if phys_ext_kind(&val) == APFS_KIND_UPDATE {
                let delta = phys_ext_update_delta(&val).saturating_add(diff);
                if delta != 0 {
                    current.insert(cur, (len, build_phys_ext_update_val(len, delta)));
                }
            } else {
                let refcnt = phys_ext_refcnt(&val);
                let new_refcnt = i64::from(refcnt) + i64::from(diff);
                if new_refcnt <= 0 {
                    owned_runs.push((cur, len));
                } else {
                    let new_refcnt = u32::try_from(new_refcnt).map_err(|_| {
                        TxnError::SpacemanParse(format!(
                            "extent ({cur} + {len}) reference count overflows"
                        ))
                    })?;
                    let mut v = phys_ext_with_blocks(&val, len);
                    if let Some(rc) = v.get_mut(16..20) {
                        rc.copy_from_slice(&new_refcnt.to_le_bytes());
                    }
                    current.insert(cur, (len, v));
                }
            }
            if seg_end < rec_end {
                current.insert(
                    seg_end,
                    (
                        rec_end - seg_end,
                        phys_ext_with_blocks(&val, rec_end - seg_end),
                    ),
                );
            }
            cur = seg_end;
        }
    }

    let mut remove_keys: Vec<Vec<u8>> = Vec::new();
    let mut upserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (start, (_, val)) in &original {
        match current.get(start) {
            None => remove_keys.push(build_phys_ext_key(*start)),
            Some((_, now)) if now != val => upserts.push((build_phys_ext_key(*start), now.clone())),
            Some(_) => {}
        }
    }
    for (start, (_, val)) in &current {
        if !original.contains_key(start) {
            upserts.push((build_phys_ext_key(*start), val.clone()));
        }
    }
    Ok(ExtrefRelease {
        remove_keys,
        upserts,
        update_inserts,
        owned_runs,
    })
}

/// Free `data_blocks` against the live extref tree, COW'ing the extref node
/// when at least one block is owned by it. Returns the new extref paddr
/// (Some) when COW happened, or None when no extref change was needed (no
/// owned blocks). The block count freed is also returned for VSB
/// `fs_alloc_count` adjustment.
/// Free data blocks owned by the live extref tree for rename-replace target.
/// Returns `(new_extref_paddr, old_extref_padrs, extref_node_delta, freed_count)`.
/// Multi-node capable via `rewrite_extref_tree`. [#151]
fn free_blocks_owned_by_live_extref<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    new_xid: u64,
    data_blocks: &[(u64, u64)],
    bsz: usize,
) -> Result<(u64, Vec<u64>, i64, u64), TxnError> {
    let old_extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    if data_blocks.is_empty() {
        return Ok((old_extref_paddr, vec![], 0, 0));
    }
    // The live records the runs cover (multi-node aware). [#151]
    let live_recs = extref_records_for_runs(txn, old_extref_paddr, data_blocks, bsz)?;

    // Walk the runs over the records they cover (a run may span several,
    // or sit inside one): last reference → remove + free, shared → lower,
    // snapshot-owned → KIND_UPDATE(-1).
    let ExtrefRelease {
        remove_keys,
        upserts: upsert_recs,
        update_inserts,
        owned_runs,
    } = plan_extref_release(&live_recs, data_blocks)?;

    if remove_keys.is_empty() && upsert_recs.is_empty() && update_inserts.is_empty() {
        return Ok((old_extref_paddr, vec![], 0, 0));
    }

    let (new_extref_paddr, old_extref_padrs, node_delta) = rewrite_extref_tree(
        txn,
        old_extref_paddr,
        new_xid,
        update_inserts,
        &remove_keys,
        &upsert_recs,
        bsz,
    )?;

    let mut total: u64 = 0;
    for &(phys, count) in &owned_runs {
        for b in phys..phys + count {
            txn.free_block(b)?;
        }
        total += count;
    }
    Ok((new_extref_paddr, old_extref_padrs, node_delta, total))
}

/// Sizes inside a fixed-kv object-map node: a 4-byte `kvoff_t` per record in
/// the table of contents, 16-byte keys, 16-byte leaf values and 8-byte index
/// values (a child's physical block address).
const OMAP_TOC_ENTRY: usize = 4;
const OMAP_KEY_SIZE: usize = 16;
const OMAP_LEAF_VAL_SIZE: usize = 16;
const OMAP_INDEX_VAL_SIZE: usize = 8;
/// Deeper than any object map macOS writes; a longer chain is a cycle or a
/// corrupt image, and with every child one level below its parent the level
/// of the root already bounds the descent.
const MAX_OMAP_TREE_DEPTH: u32 = 32;

/// The bounds a fixed-kv object-map node's records are read within.
struct OmapNodeLayout {
    nkeys: usize,
    toc_base: usize,
    key_area: usize,
    val_area_end: usize,
}

/// Validate the table-of-contents geometry of a fixed-kv object-map node.
///
/// The record count, the table position and the flags all come from disk.
/// The count is capped by what the table can hold, so it never sizes an
/// allocation or drives an offset on its own; the key area has to end before
/// the value area starts; and the accessors check every key and value offset
/// against those bounds, so a crafted node yields an error instead of an
/// arithmetic panic or bytes read from the wrong place.
fn omap_node_layout(node: &[u8], bsz: usize) -> Result<OmapNodeLayout, TxnError> {
    if node.len() < bsz || bsz < DATA_BASE + BTREE_INFO_SIZE {
        return Err(TxnError::OmapParse(format!(
            "omap node of {} bytes is shorter than a {bsz}-byte block",
            node.len()
        )));
    }
    let is_root = rd_u16(node, 32) & BTNODE_ROOT != 0;
    let nkeys = rd_u32(node, 36) as usize;
    let toc_base = DATA_BASE + rd_u16(node, 40) as usize;
    let toc_len = rd_u16(node, 42) as usize;
    let key_area = toc_base + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    if key_area > val_area_end {
        return Err(TxnError::OmapParse(
            "omap node table of contents runs past the node".into(),
        ));
    }
    let capacity = toc_len / OMAP_TOC_ENTRY;
    if nkeys > capacity {
        return Err(TxnError::OmapParse(format!(
            "omap node claims {nkeys} records but its table holds {capacity}"
        )));
    }
    Ok(OmapNodeLayout {
        nkeys,
        toc_base,
        key_area,
        val_area_end,
    })
}

impl OmapNodeLayout {
    /// Start of record `i`'s 16-byte key, which lies inside the key area.
    fn key_at(&self, node: &[u8], i: usize) -> Result<usize, TxnError> {
        let k = self.key_area + rd_u16(node, self.toc_base + i * OMAP_TOC_ENTRY) as usize;
        if k + OMAP_KEY_SIZE > self.val_area_end {
            return Err(TxnError::OmapParse(format!(
                "omap record {i}: key lies outside the key area"
            )));
        }
        Ok(k)
    }

    /// Start of record `i`'s `vsz`-byte value, stored `v_off` bytes back from
    /// the value area end; a value lies between the key area and that end.
    fn val_at(&self, node: &[u8], i: usize, vsz: usize) -> Result<usize, TxnError> {
        let v_off = rd_u16(node, self.toc_base + i * OMAP_TOC_ENTRY + 2) as usize;
        self.val_area_end
            .checked_sub(v_off)
            .filter(|&v| v >= self.key_area && v + vsz <= self.val_area_end)
            .ok_or_else(|| {
                TxnError::OmapParse(format!(
                    "omap record {i}: value lies outside the value area"
                ))
            })
    }
}

/// Check that the block at `paddr`, named as a child by an omap node at
/// `parent_level`, is an object-map B-tree node one level below it, and
/// return its level. apfsprogs checks the same three things (type, subtype,
/// level) on every node it descends into.
fn omap_child_level(child: &[u8], paddr: u64, parent_level: u16) -> Result<u16, TxnError> {
    let o_type = rd_u32(child, 24) & 0x0000_FFFF;
    if o_type != OBJECT_TYPE_BTREE_NODE {
        return Err(TxnError::OmapParse(format!(
            "block {paddr} named as an omap child is not a B-tree node (type {o_type:#x})"
        )));
    }
    let subtype = rd_u32(child, 28);
    if subtype != OBJECT_TYPE_OMAP {
        return Err(TxnError::OmapParse(format!(
            "block {paddr} named as an omap child belongs to another tree (subtype {subtype:#x})"
        )));
    }
    let level = rd_u16(child, 34);
    if level.checked_add(1) != Some(parent_level) {
        return Err(TxnError::OmapParse(format!(
            "omap node {paddr} is at level {level} under a level-{parent_level} parent"
        )));
    }
    Ok(level)
}

/// Find the physical paddr mapped to `oid` (latest xid) in a fixed-kv volume
/// omap b-tree leaf node.
#[cfg(test)]
fn omap_lookup(node: &[u8], oid: u64, bsz: usize) -> Result<Option<u64>, TxnError> {
    let layout = omap_node_layout(node, bsz)?;
    let mut best: Option<(u64, u64)> = None; // (xid, paddr)
    for i in 0..layout.nkeys {
        let k = layout.key_at(node, i)?;
        if rd_u64(node, k) != oid {
            continue;
        }
        let k_xid = rd_u64(node, k + 8);
        let v = layout.val_at(node, i, OMAP_LEAF_VAL_SIZE)?;
        let paddr = rd_u64(node, v + 8);
        if best.map(|(x, _)| k_xid > x).unwrap_or(true) {
            best = Some((k_xid, paddr));
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// omap_val.ov_flags bit: this {oid,xid} mapping is a tombstone (deleted).
const OMAP_VAL_DELETED: u32 = 0x0000_0001;

/// Resolve `oid` → physical block address through a (possibly MULTI-NODE)
/// volume omap b-tree, returning the paddr of the largest version whose xid is
/// `<= target_xid` (pass `u64::MAX` for the live/latest view). Returns `None`
/// when the oid is absent or its newest matching entry is a deletion tombstone.
///
/// The omap is a FIXED-kv, BTREE_PHYSICAL tree: 16-byte keys `{ok_oid, ok_xid}`,
/// 8-byte index values (child node PHYSICAL paddr) and 16-byte leaf values
/// `{ov_flags, ov_size, ov_paddr}`. `omap_lookup` only reads ONE node and so
/// misreads an index root as a leaf once the tree splits (#157). This descends:
/// at each index level pick the largest key `<= (oid, target_xid)` and follow
/// its child paddr; at the leaf pick the largest `ok_xid <= target_xid` for the
/// matching oid. Reads go through `txn` so staged blocks are honoured.
///
/// [CERTAIN: algorithm mirrors our own proven reader `apfs_core::omap::Omap::
///  resolve` + APFS PDF §Object Maps. Bounded descent (32) - corrupt tree never
///  loops or panics, just yields None.]
fn omap_resolve<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    root_node: &[u8],
    oid: u64,
    target_xid: u64,
    bsz: usize,
) -> Result<Option<u64>, TxnError> {
    let mut node = root_node.to_vec();
    let mut level = rd_u16(&node, 34);
    // Every child is one level below its parent and no block is visited
    // twice, so a pointer loop is reported instead of followed.
    let mut visited: HashSet<u64> = HashSet::new();
    for _ in 0..MAX_OMAP_TREE_DEPTH {
        let is_leaf = level == 0;
        let layout = omap_node_layout(&node, bsz)?;
        let vsz = if is_leaf {
            OMAP_LEAF_VAL_SIZE
        } else {
            OMAP_INDEX_VAL_SIZE
        };

        // Find the best entry. Index: largest (oid,xid) <= (target). Leaf: the
        // matching oid with the largest xid <= target_xid.
        let mut best: Option<(u64, u64, usize)> = None; // (k_oid, k_xid, idx)
        for i in 0..layout.nkeys {
            let k = layout.key_at(&node, i)?;
            let k_oid = rd_u64(&node, k);
            let k_xid = rd_u64(&node, k + 8);
            if is_leaf {
                if k_oid == oid && k_xid <= target_xid {
                    let better = best.map(|(_, bx, _)| k_xid > bx).unwrap_or(true);
                    if better {
                        best = Some((k_oid, k_xid, i));
                    }
                }
            } else if k_oid < oid || (k_oid == oid && k_xid <= target_xid) {
                let better = match best {
                    None => true,
                    Some((bo, bx, _)) => k_oid > bo || (k_oid == bo && k_xid > bx),
                };
                if better {
                    best = Some((k_oid, k_xid, i));
                }
            }
        }
        let Some((_, _, idx)) = best else {
            return Ok(None);
        };
        let v = layout.val_at(&node, idx, vsz)?;
        if is_leaf {
            // omap_val: ov_flags@v (u32), ov_size@v+4, ov_paddr@v+8.
            if rd_u32(&node, v) & OMAP_VAL_DELETED != 0 {
                return Ok(None);
            }
            return Ok(Some(rd_u64(&node, v + 8)));
        }
        // Index value is an 8-byte PHYSICAL child paddr.
        let child = rd_u64(&node, v);
        if !visited.insert(child) {
            return Err(TxnError::OmapParse(format!(
                "omap node {child} is reached twice: the tree has a cycle"
            )));
        }
        let mut buf = vec![0u8; bsz];
        txn.read_block(child, &mut buf)?;
        level = omap_child_level(&buf, child, level)?;
        node = buf;
    }
    Err(TxnError::OmapParse(format!(
        "omap tree is deeper than {MAX_OMAP_TREE_DEPTH} levels"
    )))
}

// ---------------------------------------------------------------------------
// Incremental catalog B-tree primitives (M11).
//
// These functions operate directly on a single serialised leaf-node buffer
// (Vec<u8>) without reading other nodes from disk.  They are pure byte-
// manipulation helpers - no I/O, no allocation from the spaceman.
//
// Design (APFS PDF B-tree spec + linux-apfs-rw btree.c patterns):
//   • Variable-kv leaf layout: fixed-size obj_phys (32 B) + btn header
//     (24 B) = DATA_BASE (56 B) TOC start.  Each TOC entry is 8 bytes:
//     [k_off:u16, k_len:u16, v_off:u16, v_len:u16].  Keys grow forward
//     from key_area; values grow backward from val_area_end.
//   • leaf_remove_record / leaf_insert_record: byte-level in-place TOC surgery.
//     #140 replaced their use in the rename fast path with parse + repack via
//     the proven `pack_cat_leaf` / `build_fstree_node` packers (fsck-valid by
//     construction). They are RETAINED as `#[cfg(test)]` reference primitives -
//     their unit tests document the exact variable-kv leaf byte layout - but no
//     production path calls them.
//   • patch_omap_entry: a test helper that patches one mapping of a
//     single-node object map.
//   Production edits go through the path-local writer in `cow_tree`.
// ---------------------------------------------------------------------------

/// Remove the record whose key bytes exactly equal `key` from a variable-kv
/// leaf node buffer.  Returns `true` when the record was found and removed,
/// `false` when not found.  The node checksum is NOT updated here - callers
/// must call `update_checksum_in_place` after all mutations are applied.
///
/// Variable-kv FSTREE leaf layout recap:
///   [0..32]          obj_phys (checksum 8B, oid 8B, xid 8B, type 4B, subtype 4B)
///   [32..36]         btn_flags (u16) + btn_level (u16)
///   [36..40]         btn_nkeys (u32)
///   [40..48]         table_space: {off:u16, len:u16}  free_space: {off:u16, len:u16}
///   [48..56]         key_free_list + val_free_list (unused / BTOFF_INVALID)
///   DATA_BASE(56)..  TOC region: nkeys × 8B each {k_off:u16,k_len:u16,v_off:u16,v_len:u16}
///   key_area..       key bytes (grow forward)
///   ........         free space (free_space.off bytes from key_area, then free_space.len)
///   ..val_area_end   value bytes (grow backward from val_area_end)
///
/// Offsets in TOC: k_off is relative to key_area; v_off is relative to val_area_end
/// (v_off = val_area_end − abs_val_start, so LARGER v_off = closer to val_area_end).
///
/// [CERTAIN: mirrors linux-apfs-rw __apfs_btree_remove TOC-memmove + node_free_range]
/// #140: retained as a `#[cfg(test)]` reference primitive (layout documentation);
/// the rename fast path now uses parse + repack instead of in-place byte surgery.
#[cfg(test)]
fn leaf_remove_record(node: &mut Vec<u8>, key: &[u8]) -> bool {
    let bsz = node.len();
    let is_root = rd_u16(node, 32) & BTNODE_ROOT != 0;
    let nkeys = rd_u32(node, 36) as usize;
    if nkeys == 0 {
        return false;
    }
    let toc_off = rd_u16(node, 40) as usize;
    let toc_len = rd_u16(node, 42) as usize;
    let key_area = DATA_BASE + toc_off + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    let key_area_used = rd_u16(node, 44) as usize; // = total key bytes used
    let free_len = rd_u16(node, 46) as usize;

    // Find the matching TOC entry.
    let mut found_idx: Option<usize> = None;
    let mut found_k_off = 0usize;
    let mut found_k_len = 0usize;
    let mut found_v_off = 0usize;
    let mut found_v_len = 0usize;
    for i in 0..nkeys {
        let te = DATA_BASE + toc_off + i * 8;
        let k_off = rd_u16(node, te) as usize;
        let k_len = rd_u16(node, te + 2) as usize;
        let v_off = rd_u16(node, te + 4) as usize;
        let v_len = rd_u16(node, te + 6) as usize;
        let ks = key_area + k_off;
        if node.get(ks..ks + k_len) == Some(key) {
            found_idx = Some(i);
            found_k_off = k_off;
            found_k_len = k_len;
            found_v_off = v_off;
            found_v_len = v_len;
            break;
        }
    }
    let idx = match found_idx {
        Some(i) => i,
        None => return false,
    };

    // 1. Remove key bytes: close the gap at [key_area+found_k_off .. +found_k_len].
    //    Shift keys that come after it to the left.
    let key_abs = key_area + found_k_off;
    let key_data_end = key_area + key_area_used;
    // Keys after this one start at key_abs + found_k_len.
    node.copy_within(key_abs + found_k_len..key_data_end, key_abs);
    let new_key_data_end = key_data_end - found_k_len;
    node[new_key_data_end..key_data_end].fill(0);

    // 2. Remove val bytes: the val lives at abs addr = val_area_end - found_v_off,
    //    with length found_v_len, i.e. bytes [val_area_end - found_v_off - found_v_len ..
    //    val_area_end - found_v_off].
    //    val_bottom (lowest address holding any val) = key_area + key_area_used + free_len.
    //    We want to close the gap: shift everything from val_bottom..val_start UP by found_v_len,
    //    where val_start = val_area_end - found_v_off - found_v_len.
    let val_start = val_area_end - found_v_off - found_v_len; // inclusive low addr of this val
    let val_bottom = key_area + key_area_used + free_len; // lowest addr of any val
                                                          // Move [val_bottom .. val_start] upward by found_v_len.
    if val_bottom < val_start {
        node.copy_within(val_bottom..val_start, val_bottom + found_v_len);
    }
    node[val_bottom..val_bottom + found_v_len].fill(0);

    // 3. Fix up other TOC entries.
    for i in 0..nkeys {
        if i == idx {
            continue;
        }
        let te = DATA_BASE + toc_off + i * 8;
        let k_off = rd_u16(node, te) as usize;
        let v_off = rd_u16(node, te + 4) as usize;

        // Keys whose k_off > found_k_off were shifted left by found_k_len.
        if k_off > found_k_off {
            wr_u16(node, te, (k_off - found_k_len) as u16);
        }

        // v_off = val_area_end - abs_start.  A val with abs_start < val_start
        // (i.e. physically closer to the low side, meaning v_off >
        // found_v_off + found_v_len) was shifted up by found_v_len →
        // abs_start increases → v_off decreases.
        let this_val_abs_start = val_area_end - v_off - {
            // We need v_len for this entry to know its absolute start.
            rd_u16(node, te + 6) as usize
        };
        if this_val_abs_start < val_start {
            // This val was shifted up by found_v_len.
            wr_u16(node, te + 4, (v_off - found_v_len) as u16);
        }
    }

    // 4. Remove TOC entry for idx: shift entries [idx+1..nkeys] left by 8.
    let te_idx = DATA_BASE + toc_off + idx * 8;
    let te_end = DATA_BASE + toc_off + nkeys * 8;
    node.copy_within(te_idx + 8..te_end, te_idx);
    node[te_end - 8..te_end].fill(0);

    // 5a. Adjust all remaining k_off values: shrinking toc_len by 8 moves
    // key_area 8 bytes earlier, but k_off is relative to key_area.  Since
    // key_area shrinks by 8 (toc_len decreases by 8) but the actual key bytes
    // did NOT move (keys are still at the same absolute addresses), all
    // remaining k_off values must increase by 8 to keep pointing at the
    // same keys.
    let remaining = nkeys - 1;
    for i in 0..remaining {
        let te = DATA_BASE + toc_off + i * 8;
        let k_off = rd_u16(node, te) as usize;
        wr_u16(node, te, (k_off + 8) as u16);
    }

    // 5b. Update header.
    wr_u32(node, 36, remaining as u32);
    let new_toc_len = toc_len - 8;
    wr_u16(node, 42, new_toc_len as u16);
    let new_key_used = key_area_used - found_k_len;
    // free_space.off is relative to key_area. New key_area = key_area - 8.
    // The same absolute position (key_area + key_area_used - found_k_len) now
    // has a relative offset of (key_area_used - found_k_len + 8) from new_key_area.
    wr_u16(node, 44, (new_key_used + 8) as u16);
    let new_free = free_len + found_k_len + found_v_len; // TOC slot freed adds 8 to key_area offset, not to free_len
    wr_u16(node, 46, new_free as u16);

    true
}

/// Insert a new (key, val) record into a variable-kv leaf node buffer at the
/// correct sorted position according to `cat_key_cmp`.  Returns `true` on
/// success, `false` when there is insufficient free space (caller must split
/// or fall back to full rebuild).  Checksum is NOT updated.
///
/// [CERTAIN: mirrors linux-apfs-rw __apfs_btree_insert apfs_node_insert +
///  apfs_assert_query_is_valid TOC arithmetic]
/// #140: retained as a `#[cfg(test)]` reference primitive (layout documentation);
/// the rename fast path now uses parse + repack instead of in-place byte surgery.
#[cfg(test)]
fn leaf_insert_record(node: &mut Vec<u8>, key: &[u8], val: &[u8]) -> bool {
    let bsz = node.len();
    let is_root = rd_u16(node, 32) & BTNODE_ROOT != 0;
    let nkeys = rd_u32(node, 36) as usize;
    let toc_off = rd_u16(node, 40) as usize;
    let toc_len = rd_u16(node, 42) as usize;
    let key_area = DATA_BASE + toc_off + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    let key_area_used = rd_u16(node, 44) as usize;
    let free_len = rd_u16(node, 46) as usize;

    // Need: 8 (new TOC entry) + key.len() + val.len() bytes of free space.
    let needed = 8 + key.len() + val.len();
    if free_len < needed {
        return false;
    }

    // Find insertion index: first existing entry whose key > new key.
    let mut insert_idx = nkeys; // default: append
    for i in 0..nkeys {
        let te = DATA_BASE + toc_off + i * 8;
        let k_off = rd_u16(node, te) as usize;
        let k_len = rd_u16(node, te + 2) as usize;
        let ks = key_area + k_off;
        if let Some(existing_key) = node.get(ks..ks + k_len) {
            if cat_key_cmp(key, existing_key) == core::cmp::Ordering::Less {
                insert_idx = i;
                break;
            }
        }
    }

    // Step A: grow the TOC by 8 bytes - shift TOC entries [insert_idx..nkeys]
    // right by 8. The TOC lives at [DATA_BASE+toc_off .. DATA_BASE+toc_off+toc_len],
    // and key_area = DATA_BASE + toc_off + toc_len (tight packing with no gap).
    // Shifting TOC right would overwrite key_area bytes if keys are right behind.
    // build_fstree_node packs TOC+keys tightly, but free_space.len accounts for
    // the gap between key_area_end and val_bottom - we need to keep the invariant:
    //   key_area = DATA_BASE + toc_off + NEW_toc_len
    // i.e. key_area shrinks by 8 (key_area advances by 8), and all k_off values
    // increase by 8? No - key_area is DATA_BASE + toc_off + toc_len where toc_len
    // is the TOTAL toc byte count. After adding one entry, toc_len grows by 8 and
    // key_area also advances by 8 - so the existing key bytes need to move right
    // by 8 to make room.
    //
    // Revised approach: to insert a new TOC entry AND keep layout consistent:
    //   1. Move all existing key bytes right by 8 (into the free space).
    //   2. Shift TOC entries [insert_idx..nkeys] right by 8.
    //   3. Write new TOC entry at insert_idx.
    //   4. Write new key at key_area+8 (old key_area, now shifted).
    //   5. Write new val at val_bottom - val.len().
    //   6. Update all k_off in existing TOC entries (+= 8, except those that
    //      were already shifted by step 2 which don't need adjustment for key move).
    //   7. Update header.

    // Step A: move existing key bytes right by 8.
    // key_area = DATA_BASE + toc_off + toc_len (immediately after TOC)
    // Wait: key_area is already computed as DATA_BASE + toc_off + toc_len above.
    // After this insert, new_toc_len = toc_len + 8, new_key_area = key_area + 8.
    // So existing keys must move from [key_area .. key_area+key_area_used]
    // to [key_area+8 .. key_area+8+key_area_used].
    let existing_key_end = key_area + key_area_used;
    // free space check: we already verified free_len >= needed (8+key+val).
    // Moving keys right by 8 is safe because free_len >= 8.
    node.copy_within(key_area..existing_key_end, key_area + 8);
    // Zero the old key_area slot (now part of extended TOC).
    node[key_area..key_area + 8].fill(0);

    // Step B: shift TOC entries [insert_idx..nkeys] right by 8 within the TOC.
    // They currently live at [DATA_BASE+toc_off+insert_idx*8 .. DATA_BASE+toc_off+toc_len].
    let te_insert = DATA_BASE + toc_off + insert_idx * 8;
    let te_old_end = DATA_BASE + toc_off + toc_len; // = key_area (old)
                                                    // These entries now sit at [te_insert+8 .. te_old_end+8] after the key shift
                                                    // already zeroed [key_area..key_area+8]. The copy must go within the TOC region.
                                                    // te_old_end == key_area, so these TOC entries are at [te_insert..te_old_end].
                                                    // We shift them to [te_insert+8..te_old_end+8]. But te_old_end = key_area and
                                                    // key_area+8 is now where keys live. This would overwrite the keys we just moved!
                                                    //
                                                    // The problem: TOC and key area are adjacent with no gap; inserting a TOC entry
                                                    // displaces everything. The real APFS kernel maintains a separate free-space
                                                    // management per-node and does NOT pack them tightly after every mutation.
                                                    //
                                                    // Simpler correct approach: rebuild the node from scratch using existing
                                                    // parse + pack, which is exactly what insert_catalog_records already does.
                                                    // But that's O(N) and defeats the purpose.
                                                    //
                                                    // ACTUAL APFS layout insight (from build_fstree_node and parse_cat_leaf):
                                                    //   toc_off is always 0 in our implementation (table_space.off = 0).
                                                    //   toc_len = nkeys * 8 (tight packing, no reserve).
                                                    //   key_area = DATA_BASE + toc_len (immediately after TOC).
                                                    //   free_space.off = bytes used by keys (from key_area).
                                                    //   free_space.len = gap between key_end and val_bottom.
                                                    //
                                                    // This means inserting a new TOC entry REQUIRES shifting all existing keys
                                                    // right by 8 AND adjusting all k_off values. That's what we do here:

    // Shift TOC entries [insert_idx..nkeys] right by 8 within the TOC.
    // These are at [te_insert..te_old_end] and must go to [te_insert+8..te_old_end+8].
    // Since te_old_end = key_area (old) and we already copied keys to key_area+8,
    // the destination is exactly in the space we just freed (key_area..key_area+8
    // is zeroed). So this is safe.
    node.copy_within(te_insert..te_old_end, te_insert + 8);
    // Zero the slot at te_insert (will be overwritten with new entry below).
    node[te_insert..te_insert + 8].fill(0);

    // Step C: update k_off in all existing TOC entries (+= 8 because key_area
    // advanced by 8 but k_off is relative to the NEW key_area = key_area+8 =
    // DATA_BASE + toc_off + (toc_len+8)).
    // Wait: k_off is relative to key_area. Old key_area = DATA_BASE+toc_off+toc_len.
    // New key_area = DATA_BASE+toc_off+(toc_len+8).
    // Old abs addr of a key = old_key_area + k_off = new_key_area - 8 + k_off.
    // But we moved keys right by 8: new abs addr = old_abs + 8 = new_key_area + k_off.
    // So k_off relative to new_key_area is unchanged! No adjustment needed.
    // (The keys moved right by exactly 8, which is how much key_area also advanced.)

    // Step D: write the new key at new_key_area + new_k_off = key_area+8 + key_area_used.
    let new_key_area = key_area + 8;
    let new_k_off = key_area_used; // relative to new_key_area
    let key_dest = new_key_area + new_k_off;
    node[key_dest..key_dest + key.len()].copy_from_slice(key);

    // Step E: write the new val at val_bottom - val.len().
    // val_bottom (before this insert) = key_area + key_area_used + free_len.
    // After moving keys right by 8 and TOC expansion:
    //   new_key_end = new_key_area + key_area_used + key.len()
    //   new free = free_len - 8 (TOC) - key.len() - val.len()
    // But for placing the val, we use the current val_bottom BEFORE any header
    // update. val_bottom = key_area + key_area_used + free_len (from header).
    let val_bottom = key_area + key_area_used + free_len;
    let new_val_abs = val_bottom - val.len();
    node[new_val_abs..new_val_abs + val.len()].copy_from_slice(val);
    let new_v_off = val_area_end - new_val_abs; // distance from val_area_end

    // Step F: write new TOC entry at te_insert.
    wr_u16(node, te_insert, new_k_off as u16);
    wr_u16(node, te_insert + 2, key.len() as u16);
    wr_u16(node, te_insert + 4, new_v_off as u16);
    wr_u16(node, te_insert + 6, val.len() as u16);

    // Step G: update header.
    wr_u32(node, 36, (nkeys + 1) as u32);
    let new_toc_len = toc_len + 8;
    wr_u16(node, 42, new_toc_len as u16);
    // free_space.off = bytes used by keys relative to NEW key_area.
    let new_key_used = key_area_used + key.len();
    wr_u16(node, 44, new_key_used as u16);
    // free_space.len = old_free - 8(TOC) - key.len() - val.len().
    let new_free = free_len - needed;
    wr_u16(node, 46, new_free as u16);

    true
}

/// The level of catalog-shaped node `oid` (a catalog or extentref node),
/// checked to be a B-tree node one level below `parent_level` when it was
/// reached through a parent. Every walk over such a tree applies this rule,
/// so a pointer that leads sideways or back up is reported, not followed.
fn fstree_child_level(node: &[u8], oid: u64, parent_level: Option<u16>) -> Result<u16, TxnError> {
    let level = rd_u16(node, 34);
    let Some(parent) = parent_level else {
        return Ok(level);
    };
    let o_type = rd_u32(node, 24) & 0x0000_FFFF;
    if o_type != OBJECT_TYPE_BTREE_NODE {
        return Err(TxnError::VolumeParse(format!(
            "tree node {oid:#x} named as a child is not a B-tree node (type {o_type:#x})"
        )));
    }
    if level.checked_add(1) != Some(parent) {
        return Err(TxnError::VolumeParse(format!(
            "tree node {oid:#x} is at level {level} under a level-{parent} parent"
        )));
    }
    Ok(level)
}

fn fstree_cycle(oid: u64) -> TxnError {
    TxnError::VolumeParse(format!(
        "tree node {oid:#x} is reached twice: the tree has a cycle"
    ))
}

fn fstree_too_deep() -> TxnError {
    TxnError::VolumeParse(format!(
        "tree is deeper than {MAX_TREE_DEPTH} levels (possible cycle or corruption)"
    ))
}

/// Patch a single `{oid → paddr}` entry in the volume-omap node bytes
/// (in-place, no full rebuild).  Used when exactly one catalog leaf was
/// COW'd and its omap mapping must be updated.  Returns `false` when the oid
/// is not found in the omap (caller must fall back to full omap rebuild).
///
/// NOTE: only safe when the omap is a single fixed-kv root node.  Multi-node
/// omaps (very large volumes) are not handled here.
#[cfg(test)]
fn patch_omap_entry(
    omap_node: &mut Vec<u8>,
    oid: u64,
    new_paddr: u64,
    new_xid: u64,
    bsz: usize,
) -> Result<bool, TxnError> {
    let layout = omap_node_layout(omap_node, bsz)?;
    for i in 0..layout.nkeys {
        let k = layout.key_at(omap_node, i)?;
        if rd_u64(omap_node, k) != oid {
            continue;
        }
        let v = layout.val_at(omap_node, i, OMAP_LEAF_VAL_SIZE)?;
        // Update xid in key and paddr in val.
        wr_u64(omap_node, k + 8, new_xid);
        wr_u64(omap_node, v + 8, new_paddr); // ov_paddr at offset 8 within omap_val
        return Ok(true);
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Catalog (FSTREE) variable-kv leaf node insertion.
// ---------------------------------------------------------------------------

const DATA_BASE: usize = 56;
const BTREE_INFO_SIZE: usize = 40;
const BTOFF_INVALID: u16 = 0xFFFF;

/// A catalog (key, value) record pair.
type CatRec = (Vec<u8>, Vec<u8>);

// B-tree node obj types / flags for FSTREE nodes (the APFS spec + the APFS specification).
const OBJ_VIRTUAL: u32 = 0x0;
const OBJ_PHYSICAL: u32 = 0x4000_0000; // physical storage; o_oid == paddr
const OBJECT_TYPE_BTREE: u32 = 0x2; // root node
const OBJECT_TYPE_BTREE_NODE: u32 = 0x3; // non-root node
const OBJECT_TYPE_FSTREE: u32 = 0x0e; // catalog subtype
const OBJECT_TYPE_BLOCKREFTREE: u32 = 0x0f; // extentref tree subtype
const BTNODE_ROOT: u16 = 0x1;
const BTNODE_LEAF: u16 = 0x2;
const BTNODE_FIXED_KV_SIZE: u16 = 0x4; // omap b-tree nodes are fixed-kv
const FSTREE_BT_FLAGS: u32 = 0x42; // BTREE_KV_NONALIGNED | BTREE_SEQUENTIAL_INSERT
                                   // BTREE_PHYSICAL(0x10) | BTREE_KV_NONALIGNED(0x40) | BTREE_SEQUENTIAL_INSERT(0x02)
const EXTREF_BT_FLAGS: u32 = 0x52;

/// Root-only btree_info_t footer stats.
pub struct FstreeFooter {
    pub longest_key: u32,
    pub longest_val: u32,
    pub key_count: u64,  // total keys across all leaves
    pub node_count: u64, // total nodes in the tree
}

/// Build an FSTREE node (leaf or internal, root or non-root) from scratch.
/// For leaves, `recs` are (key, value) catalog records; for internal nodes,
/// each value is the child node's 8-byte virtual oid. Variable-kv layout,
/// TOC tightly packed (nkeys*8). The footer is emitted only for the root.
/// Caller must have already sorted `recs`. Fletcher is computed here.
/// [the APFS specification]
pub fn build_fstree_node(
    oid: u64,
    xid: u64,
    is_root: bool,
    level: u16,
    recs: &[(Vec<u8>, Vec<u8>)],
    footer: Option<FstreeFooter>,
    bsz: usize,
) -> Vec<u8> {
    let nkeys = recs.len();
    let mut out = vec![0u8; bsz];
    // obj_phys header.
    wr_u64(&mut out, 8, oid);
    wr_u64(&mut out, 16, xid);
    let o_type = if is_root {
        OBJ_VIRTUAL | OBJECT_TYPE_BTREE
    } else {
        OBJ_VIRTUAL | OBJECT_TYPE_BTREE_NODE
    };
    wr_u32(&mut out, 24, o_type);
    wr_u32(&mut out, 28, OBJECT_TYPE_FSTREE);
    // btn_flags / btn_level.
    let is_leaf = level == 0;
    let mut flags: u16 = 0;
    if is_root {
        flags |= BTNODE_ROOT;
    }
    if is_leaf {
        flags |= BTNODE_LEAF;
    }
    wr_u16(&mut out, 32, flags);
    wr_u16(&mut out, 34, level);
    wr_u32(&mut out, 36, nkeys as u32);
    // Free lists empty.
    wr_u16(&mut out, 48, BTOFF_INVALID);
    wr_u16(&mut out, 52, BTOFF_INVALID);

    // The table of contents is whole steps of entries (see TOC_STEP), one
    // step for an emptied node: a table of length zero is rejected outright
    // ("invalid btn_table_space.len"). Every caller sizes a node by
    // node_capacity, which keeps room for the rounding; a set of records
    // that only fits with an exact table (a caller that did not) still gets
    // one rather than an overflowing node.
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    let kv_bytes: usize = recs.iter().map(|(k, v)| k.len() + v.len()).sum();
    let toc_len = if DATA_BASE + fstree_toc_len(nkeys) + kv_bytes <= val_area_end {
        fstree_toc_len(nkeys)
    } else {
        nkeys * 8
    };
    let key_area = DATA_BASE + toc_len;
    let mut key_cursor = key_area;
    let mut val_cursor = val_area_end;
    for (i, (key, val)) in recs.iter().enumerate() {
        let k_off = key_cursor - key_area;
        wr_bytes(&mut out, key_cursor, key);
        key_cursor += key.len();
        val_cursor -= val.len();
        wr_bytes(&mut out, val_cursor, val);
        let v_off = val_area_end - val_cursor;
        let te = DATA_BASE + i * 8;
        wr_u16(&mut out, te, k_off as u16);
        wr_u16(&mut out, te + 2, key.len() as u16);
        wr_u16(&mut out, te + 4, v_off as u16);
        wr_u16(&mut out, te + 6, val.len() as u16);
    }
    wr_u16(&mut out, 40, 0); // table_space.off
    wr_u16(&mut out, 42, toc_len as u16); // table_space.len
    wr_u16(&mut out, 44, (key_cursor - key_area) as u16); // free_space.off
    wr_u16(&mut out, 46, (val_cursor - key_cursor) as u16); // free_space.len
    if let Some(f) = footer {
        let bti = bsz - BTREE_INFO_SIZE;
        wr_u32(&mut out, bti, FSTREE_BT_FLAGS);
        wr_u32(&mut out, bti + 4, bsz as u32); // node_size
                                               // key_size/val_size = 0 (variable-kv)
        wr_u32(&mut out, bti + 16, f.longest_key);
        wr_u32(&mut out, bti + 20, f.longest_val);
        wr_u64(&mut out, bti + 24, f.key_count);
        wr_u64(&mut out, bti + 32, f.node_count);
    }
    update_checksum_in_place(&mut out);
    out
}

/// Total byte usage of a set of records when packed into a node (TOC + keys +
/// values), used to decide split points and overflow.
pub fn packed_size(recs: &[(Vec<u8>, Vec<u8>)]) -> usize {
    recs.iter().map(|(k, v)| 8 + k.len() + v.len()).sum()
}

/// Maximum record bytes that fit in a node body, keeping room to round the
/// table of contents up to a whole number of [`TOC_STEP`] entries.
pub fn node_capacity(bsz: usize, is_root: bool) -> usize {
    bsz - DATA_BASE - if is_root { BTREE_INFO_SIZE } else { 0 } - (TOC_STEP - 1) * 8
}

/// A variable-size node's table of contents holds a whole number of steps of
/// this many entries. macOS grows a table by such steps and counts on it:
/// rebalancing a node whose table was cut to its exact record count, it
/// moved records between siblings without moving the parent's separator,
/// and every lookup past that separator failed (fsck: "invalid key order:
/// index N is greater than maxkey"; measured: macOS removing 200 files we
/// wrote in one directory left 6 it could list but not open).
/// [linux-apfs-rw: APFS_BTREE_TOC_ENTRY_INCREMENT; macOS-written nodes keep
///  0-7 spare entries]
const TOC_STEP: usize = 8;

/// The table of contents for `nkeys` variable-size entries: whole steps of
/// [`TOC_STEP`] entries, at least one step even when empty (a table of
/// length zero is rejected).
fn fstree_toc_len(nkeys: usize) -> usize {
    nkeys.max(1).div_ceil(TOC_STEP) * TOC_STEP * 8
}

/// Greedily split `recs` into chunks each packing within `cap` bytes
/// (TOC + key + value per record). Order is preserved. A record that does
/// not fit a node on its own is an error: a chunk of it alone would be
/// packed past the node's end.
fn partition_records(recs: &[CatRec], cap: usize) -> Result<Vec<Vec<CatRec>>, TxnError> {
    let mut out: Vec<Vec<CatRec>> = Vec::new();
    let mut cur: Vec<CatRec> = Vec::new();
    let mut sz = 0usize;
    for (k, v) in recs {
        let rsz = 8 + k.len() + v.len();
        if rsz > cap {
            return Err(TxnError::InvalidArgument(format!(
                "a record of {rsz} bytes does not fit a node of {cap}"
            )));
        }
        if sz + rsz > cap && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            sz = 0;
        }
        sz += rsz;
        cur.push((k.clone(), v.clone()));
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Extentref (BLOCKREFTREE) physical B-tree - multi-node read/rebuild.
// The extentref tree is OBJ_PHYSICAL: paddr is stored directly in the VSB,
// and child pointers in internal nodes are physical block addresses (not OIDs).
// No volume omap involved. [linux-apfs-rw:extents.c APFS_OBJ_PHYSICAL +
//  apfsprogs raw.h OBJECT_TYPE_BLOCKREFTREE=0x0f]
// ---------------------------------------------------------------------------

/// Build one extentref tree node (leaf or internal, root or non-root).
/// For leaves, `recs` are (phys_ext_key, phys_ext_val) records.
/// For internal nodes, each value is the child's 8-byte physical block address.
/// Physical b-tree: o_type uses OBJ_PHYSICAL | OBJECT_TYPE_BTREE/NODE,
///   o_subtype = OBJECT_TYPE_BLOCKREFTREE = 0x0F.
///   btree_info footer bt_flags = EXTREF_BT_FLAGS = 0x52
///   (BTREE_PHYSICAL | BTREE_KV_NONALIGNED | BTREE_SEQUENTIAL_INSERT).
/// [apfsprogs raw.h; linux-apfs-rw extents.c; the APFS spec cross-check]
fn build_extref_node(
    paddr: u64,
    xid: u64,
    is_root: bool,
    level: u16,
    recs: &[(Vec<u8>, Vec<u8>)],
    footer: Option<FstreeFooter>,
    bsz: usize,
) -> Vec<u8> {
    let nkeys = recs.len();
    let mut out = vec![0u8; bsz];
    // obj_phys header: for physical nodes o_oid == paddr.
    wr_u64(&mut out, 8, paddr);
    wr_u64(&mut out, 16, xid);
    let o_type = if is_root {
        OBJ_PHYSICAL | OBJECT_TYPE_BTREE
    } else {
        OBJ_PHYSICAL | OBJECT_TYPE_BTREE_NODE
    };
    wr_u32(&mut out, 24, o_type);
    wr_u32(&mut out, 28, OBJECT_TYPE_BLOCKREFTREE);
    // btn_flags / btn_level.
    let is_leaf = level == 0;
    let mut flags: u16 = 0;
    if is_root {
        flags |= BTNODE_ROOT;
    }
    if is_leaf {
        flags |= BTNODE_LEAF;
    }
    wr_u16(&mut out, 32, flags);
    wr_u16(&mut out, 34, level);
    wr_u32(&mut out, 36, nkeys as u32);
    // Free lists empty.
    wr_u16(&mut out, 48, BTOFF_INVALID);
    wr_u16(&mut out, 52, BTOFF_INVALID);

    // The table of contents is whole steps of entries (see TOC_STEP), one
    // step for an emptied node: a table of length zero is rejected outright
    // ("invalid btn_table_space.len"). Every caller sizes a node by
    // node_capacity, which keeps room for the rounding; a set of records
    // that only fits with an exact table (a caller that did not) still gets
    // one rather than an overflowing node.
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    let kv_bytes: usize = recs.iter().map(|(k, v)| k.len() + v.len()).sum();
    let toc_len = if DATA_BASE + fstree_toc_len(nkeys) + kv_bytes <= val_area_end {
        fstree_toc_len(nkeys)
    } else {
        nkeys * 8
    };
    let key_area = DATA_BASE + toc_len;
    let mut key_cursor = key_area;
    let mut val_cursor = val_area_end;
    for (i, (key, val)) in recs.iter().enumerate() {
        let k_off = key_cursor - key_area;
        wr_bytes(&mut out, key_cursor, key);
        key_cursor += key.len();
        val_cursor -= val.len();
        wr_bytes(&mut out, val_cursor, val);
        let v_off = val_area_end - val_cursor;
        let te = DATA_BASE + i * 8;
        wr_u16(&mut out, te, k_off as u16);
        wr_u16(&mut out, te + 2, key.len() as u16);
        wr_u16(&mut out, te + 4, v_off as u16);
        wr_u16(&mut out, te + 6, val.len() as u16);
    }
    wr_u16(&mut out, 40, 0); // table_space.off
    wr_u16(&mut out, 42, toc_len as u16); // table_space.len
    wr_u16(&mut out, 44, (key_cursor - key_area) as u16); // free_space.off
    wr_u16(&mut out, 46, (val_cursor - key_cursor) as u16); // free_space.len
    if let Some(f) = footer {
        let bti = bsz - BTREE_INFO_SIZE;
        wr_u32(&mut out, bti, EXTREF_BT_FLAGS);
        wr_u32(&mut out, bti + 4, bsz as u32); // node_size
                                               // key_size/val_size = 0 (variable-kv)
        wr_u32(&mut out, bti + 16, f.longest_key);
        wr_u32(&mut out, bti + 20, f.longest_val);
        wr_u64(&mut out, bti + 24, f.key_count);
        wr_u64(&mut out, bti + 32, f.node_count);
    }
    update_checksum_in_place(&mut out);
    out
}

/// Walk the extentref tree rooted at `root_paddr` (OBJ_PHYSICAL - no omap),
/// collecting every leaf (key, value) record and every physical node address
/// (for later freeing). Handles single-node (root+leaf) and multi-level trees.
/// [linux-apfs-rw:extents.c apfs_insert_phys_extent APFS_OBJ_PHYSICAL path]
pub(crate) fn collect_extref_records<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    root_paddr: u64,
    bsz: usize,
) -> Result<(Vec<CatRec>, Vec<u64>), TxnError> {
    let mut leaf_recs: Vec<CatRec> = Vec::new();
    let mut all_padrs: Vec<u64> = Vec::new();
    let mut visited = HashSet::new();
    collect_extref_node(
        txn,
        root_paddr,
        bsz,
        &mut leaf_recs,
        &mut all_padrs,
        &mut visited,
        None,
        0,
    )?;
    Ok((leaf_recs, all_padrs))
}

/// Maximum catalog / extentref B-tree depth. Real trees are only a few levels
/// deep; a deeper chain (or a cycle in crafted child pointers) means a
/// malformed image, which must not be allowed to recurse until the stack
/// overflows.
const MAX_TREE_DEPTH: u32 = 64;

/// One node of the extentref walk. A repeated child (the same physical block
/// named twice) would have its records collected twice, and a child at the
/// wrong level is a pointer into some other part of the tree; both are
/// errors rather than re-walks.
#[allow(clippy::too_many_arguments)]
fn collect_extref_node<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    paddr: u64,
    bsz: usize,
    leaf_recs: &mut Vec<CatRec>,
    all_padrs: &mut Vec<u64>,
    visited: &mut HashSet<u64>,
    parent_level: Option<u16>,
    depth: u32,
) -> Result<(), TxnError> {
    if depth > MAX_TREE_DEPTH {
        return Err(TxnError::SpacemanParse(
            "extref tree exceeds maximum depth (possible cycle or corruption)".into(),
        ));
    }
    if !visited.insert(paddr) {
        return Err(fstree_cycle(paddr));
    }
    all_padrs.push(paddr);
    let mut node = vec![0u8; bsz];
    txn.read_block(paddr, &mut node)?;
    let level = fstree_child_level(&node, paddr, parent_level)?;
    let recs = parse_cat_leaf(&node).map_err(|e| TxnError::SpacemanParse(e.into()))?;
    if level == 0 {
        // Leaf: collect records directly.
        leaf_recs.extend(recs.into_iter().map(|r| (r.key, r.val)));
    } else {
        // Internal: each value is an 8-byte physical child paddr.
        for r in recs {
            let child_paddr = r
                .val
                .get(..8)
                .and_then(|s| s.try_into().ok())
                .map(u64::from_le_bytes)
                .ok_or_else(|| {
                    TxnError::SpacemanParse("extref internal node: short child paddr".into())
                })?;
            collect_extref_node(
                txn,
                child_paddr,
                bsz,
                leaf_recs,
                all_padrs,
                visited,
                Some(level),
                depth + 1,
            )?;
        }
    }
    Ok(())
}

/// Rebuild the extentref tree from `recs` (already sorted by `cat_key_cmp`),
/// allocating new physical nodes and returning the new root paddr.
/// Returns `(new_root_paddr, all_new_padrs, node_delta)` where:
///   - `all_new_padrs` is every newly allocated node block (for potential future use)
///   - `node_delta` = new_node_count - old_node_count (for fs_alloc_count adjustment)
///
/// Single-node when `packed_size(recs) <= node_capacity(bsz, true)`;
/// multi-node (bottom-up build) otherwise - mirrors rewrite_fstree_impl.
/// [#151 fix; cross-check: linux-apfs-rw btree.c apfs_node_split,
///  apfsprogs btree; template: rewrite_fstree_impl in this file]
fn build_extref_tree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    recs: Vec<CatRec>,
    xid: u64,
    old_node_count: i64,
    bsz: usize,
) -> Result<(u64, i64), TxnError> {
    let longest_k = recs.iter().map(|(k, _)| k.len()).max().unwrap_or(0) as u32;
    let longest_v = recs.iter().map(|(_, v)| v.len()).max().unwrap_or(0) as u32;
    let key_count = recs.len() as u64;

    let footer = |node_count: u64| FstreeFooter {
        longest_key: longest_k,
        longest_val: longest_v,
        key_count,
        node_count,
    };

    let new_root_paddr;
    let new_node_count: i64;

    if packed_size(&recs) <= node_capacity(bsz, true) {
        // Single ROOT|LEAF node.
        let p = txn.alloc_block()?;
        let node = build_extref_node(p, xid, true, 0, &recs, Some(footer(1)), bsz);
        txn.stage_raw(p, node);
        new_root_paddr = p;
        new_node_count = 1;
    } else {
        // Bottom-up: leaves (level 0) then internal levels until root fits.
        // Physical tree: child pointers are padrs (not virtual OIDs).
        let cap = node_capacity(bsz, false);
        let mut new_padrs: Vec<u64> = Vec::new();
        let mut parent_entries: Vec<(Vec<u8>, u64)> = Vec::new(); // (pivot_key, child_paddr)

        for chunk in partition_records(&recs, cap)? {
            let p = txn.alloc_block()?;
            txn.stage_raw(p, build_extref_node(p, xid, false, 0, &chunk, None, bsz));
            new_padrs.push(p);
            let pivot = chunk.first().map(|(k, _)| k.clone()).unwrap_or_default();
            parent_entries.push((pivot, p));
        }

        let mut level: u16 = 1;
        loop {
            if level > 64 {
                return Err(TxnError::SpacemanParse(
                    "extref rebuild: tree depth exceeded 64 levels".into(),
                ));
            }
            // Build (key, paddr_bytes) records for this level.
            let lvl_recs: Vec<(Vec<u8>, Vec<u8>)> = parent_entries
                .iter()
                .map(|(k, p)| (k.clone(), p.to_le_bytes().to_vec()))
                .collect();

            if packed_size(&lvl_recs) <= node_capacity(bsz, true) {
                // This level fits in a single root.
                let rp = txn.alloc_block()?;
                let node_count = new_padrs.len() as u64 + 1;
                txn.stage_raw(
                    rp,
                    build_extref_node(
                        rp,
                        xid,
                        true,
                        level,
                        &lvl_recs,
                        Some(footer(node_count)),
                        bsz,
                    ),
                );
                new_padrs.push(rp);
                new_root_paddr = rp;
                break;
            }

            // Split into multiple internal nodes and go up another level.
            let mut next: Vec<(Vec<u8>, u64)> = Vec::new();
            for chunk in partition_records(&lvl_recs, cap)? {
                let p = txn.alloc_block()?;
                txn.stage_raw(
                    p,
                    build_extref_node(p, xid, false, level, &chunk, None, bsz),
                );
                new_padrs.push(p);
                let pivot = chunk.first().map(|(k, _)| k.clone()).unwrap_or_default();
                next.push((pivot, p));
            }
            parent_entries = next;
            level += 1;
        }
        new_node_count = new_padrs.len() as i64;
    }

    let node_delta = new_node_count - old_node_count;
    Ok((new_root_paddr, node_delta))
}

/// Rewrite the extentref tree: collect all existing leaf records, apply
/// inserts / removals / upserts, sort, and rebuild (single or multi-node).
///
/// Parameters:
///   `old_root_paddr`  - current VSB.apfs_extentref_tree_oid
///   `insert_recs`     - new (key,val) pairs to add
///   `remove_keys`     - keys to remove
///   `upsert_recs`     - (key,val) pairs to replace (insert if absent)
///
/// Returns `(new_root_paddr, old_padrs, extref_node_delta)`:
///   - `new_root_paddr` → write into VSB.apfs_extentref_tree_oid
///   - `old_padrs`      → pass to free_replaced_metadata as the extref list
///   - `extref_node_delta` → fold into fs_alloc_count (extref nodes are
///     volume-owned metadata blocks, same as catalog nodes). [#151]
///
/// [linux-apfs-rw:extents.c apfs_insert_phys_extent / apfs_put_phys_extent;
///  template: rewrite_fstree_impl (this file); the APFS spec cross-check]
#[allow(clippy::too_many_arguments)]
/// Bytes of a physical-extent record's key (`j_phys_ext_key_t`) and value
/// (`j_phys_ext_val_t`).
const PHYS_EXT_KEY_LEN: usize = 8;
const PHYS_EXT_VAL_LEN: usize = 20;

/// Refuse a record that is not shaped like a physical-extent record.
fn check_extref_record_sizes(recs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), TxnError> {
    for (k, v) in recs {
        if k.len() != PHYS_EXT_KEY_LEN || v.len() != PHYS_EXT_VAL_LEN {
            return Err(TxnError::VolumeParse(format!(
                "extent reference record of {} key bytes and {} value bytes (want {} and {})",
                k.len(),
                v.len(),
                PHYS_EXT_KEY_LEN,
                PHYS_EXT_VAL_LEN
            )));
        }
    }
    Ok(())
}

/// Refuse a sorted extentref record set no reference tree can hold: a record
/// over no blocks, a counting record with no reference, a KIND_UPDATE that
/// changes nothing, or a record starting inside the one before it. fsck_apfs
/// reports an overlap as keys out of order; the others describe references
/// that do not exist. A writer bug producing any of them fails here, before
/// the tree is written, instead of on the next check.
fn check_extref_records_disjoint(recs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), TxnError> {
    let mut prev_end: Option<u64> = None;
    for (k, v) in recs {
        let start = rd_u64(k, 0) & PHYS_EXT_LEN_MASK;
        let blocks = rd_u64(v, 0) & PHYS_EXT_LEN_MASK;
        let empty = if phys_ext_kind(v) == APFS_KIND_UPDATE {
            phys_ext_update_delta(v) == 0
        } else {
            phys_ext_refcnt(v) == 0
        };
        if blocks == 0 || empty {
            return Err(TxnError::VolumeParse(format!(
                "extent reference record ({start} + {blocks}) holds no reference"
            )));
        }
        if prev_end.is_some_and(|end| start < end) {
            return Err(TxnError::VolumeParse(format!(
                "extent reference record ({start} + {blocks}) overlaps the one before it"
            )));
        }
        prev_end = Some(start.checked_add(blocks).ok_or_else(|| {
            TxnError::VolumeParse(format!("extent reference record at {start} wraps"))
        })?);
    }
    Ok(())
}

pub(crate) fn rewrite_extref_tree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    old_root_paddr: u64,
    new_xid: u64,
    insert_recs: Vec<(Vec<u8>, Vec<u8>)>,
    remove_keys: &[Vec<u8>],
    upsert_recs: &[(Vec<u8>, Vec<u8>)],
    bsz: usize,
) -> Result<(u64, Vec<u64>, i64), TxnError> {
    if !txn.full_rebuild() {
        return rewrite_extref_paths(
            txn,
            old_root_paddr,
            new_xid,
            insert_recs,
            remove_keys,
            upsert_recs,
            bsz,
        );
    }
    // 1. Collect all existing leaf records + all old node padrs.
    let (old_leaf_recs, old_padrs) = collect_extref_records(txn, old_root_paddr, bsz)?;
    let old_node_count = old_padrs.len() as i64;

    // 2. Start with existing records, apply mutations.
    let mut recs: Vec<CatRec> = old_leaf_recs
        .into_iter()
        .filter(|(k, _)| !remove_keys.contains(k))
        .collect();

    // Apply upserts: replace value if key present, else append.
    for (uk, uv) in upsert_recs {
        match recs.iter_mut().find(|(k, _)| k == uk) {
            Some(r) => r.1 = uv.clone(),
            None => recs.push((uk.clone(), uv.clone())),
        }
    }

    // Apply inserts. A key already present (or given twice) would leave two
    // equal records in the tree, and a later removal of that key would drop
    // both - freeing blocks the surviving reference still counts on. Refused,
    // as the catalog rebuild refuses it.
    for (ik, iv) in insert_recs {
        if recs.iter().any(|(k, _)| *k == ik) {
            return Err(TxnError::AlreadyExists(format!(
                "extent reference record {ik:02x?} inserted twice"
            )));
        }
        recs.push((ik, iv));
    }

    // Every record of this tree has an 8-byte key and a 20-byte value; the
    // rebuilt root's description names those as the longest, and the checker
    // holds it to them. A record of another shape, from a caller or a
    // foreign writer, would be carried into the footer as the new longest.
    check_extref_record_sizes(&recs)?;

    // 3. Sort by key (same comparator as catalog/fstree).
    recs.sort_by(|a, b| cat_key_cmp(&a.0, &b.0));
    check_extref_records_disjoint(&recs)?;

    // 4. Rebuild and return.
    let (new_root_paddr, node_delta) = build_extref_tree(txn, recs, new_xid, old_node_count, bsz)?;
    Ok((new_root_paddr, old_padrs, node_delta))
}

/// [`rewrite_extref_tree`] rewriting only the nodes on the paths to the
/// changed records. Returns the new root, the node blocks replaced or
/// dropped, and the nodes the tree gained.
///
/// The live tree is the volume's own (taking a snapshot hands the populated
/// tree to the snapshot), so the nodes left standing are nobody else's and
/// this path serves volumes with snapshots too. Each rewritten leaf is
/// checked the way the rebuild checks the whole record set.
fn rewrite_extref_paths<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    old_root_paddr: u64,
    new_xid: u64,
    insert_recs: Vec<(Vec<u8>, Vec<u8>)>,
    remove_keys: &[Vec<u8>],
    upsert_recs: &[(Vec<u8>, Vec<u8>)],
    bsz: usize,
) -> Result<(u64, Vec<u64>, i64), TxnError> {
    check_extref_record_sizes(&insert_recs)?;
    check_extref_record_sizes(upsert_recs)?;
    let mut batch = EditBatch::new();
    for k in remove_keys {
        batch.push(k.clone(), EditOp::Remove)?;
    }
    for (k, v) in upsert_recs {
        batch.push(k.clone(), EditOp::Upsert(v.clone()))?;
    }
    for (k, v) in insert_recs {
        batch.push(k, EditOp::Insert(v))?;
    }
    let mut tree =
        CowTree::new(Shape::ExtentRef, new_xid, bsz).with_leaf_check(check_extref_records_disjoint);
    let root = tree.apply(txn, old_root_paddr, batch)?;
    Ok((root, tree.superseded, tree.nodes_added))
}

/// Recursively collect every (key, value) leaf record of the FSTREE rooted at
/// virtual `oid`, resolving child node oids through the volume omap node.
fn collect_fstree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    oid: u64,
    bsz: usize,
) -> Result<Vec<CatRec>, TxnError> {
    let mut out = Vec::new();
    let mut visited = HashSet::new();
    collect_fstree_below(txn, omap_node, oid, bsz, &mut out, &mut visited, None, 0)?;
    Ok(out)
}

/// One node of the `collect_fstree` walk. Bounded the way the extentref walk
/// is: a depth cap, no oid visited twice and every child one level below its
/// parent, so a crafted cycle is an error instead of unbounded recursion.
#[allow(clippy::too_many_arguments)]
fn collect_fstree_below<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    oid: u64,
    bsz: usize,
    out: &mut Vec<CatRec>,
    visited: &mut HashSet<u64>,
    parent_level: Option<u16>,
    depth: u32,
) -> Result<(), TxnError> {
    if depth > MAX_TREE_DEPTH {
        return Err(fstree_too_deep());
    }
    if !visited.insert(oid) {
        return Err(fstree_cycle(oid));
    }
    // Resolve through the (possibly multi-node) volume omap for the live view.
    // omap_lookup only reads one node and misreads an index root once the omap
    // splits (#157); omap_resolve descends. u64::MAX = latest version.
    let paddr = omap_resolve(txn, omap_node, oid, u64::MAX, bsz)?
        .ok_or_else(|| TxnError::SpacemanParse("fstree oid not in volume omap".into()))?;
    let mut node = vec![0u8; bsz];
    txn.read_block(paddr, &mut node)?;
    let level = fstree_child_level(&node, oid, parent_level)?;
    let recs = parse_cat_leaf(&node).map_err(|e| TxnError::SpacemanParse(e.into()))?;
    if level == 0 {
        out.extend(recs.into_iter().map(|r| (r.key, r.val)));
        return Ok(());
    }
    for r in recs {
        collect_fstree_below(
            txn,
            omap_node,
            rd_u64(&r.val, 0),
            bsz,
            out,
            visited,
            Some(level),
            depth + 1,
        )?;
    }
    Ok(())
}

/// Collect every node oid the catalog reaches, root included.
///
/// The volume object map holds a mapping per version of every catalog node,
/// and a snapshot keeps the versions its own view needs. Once the snapshot is
/// gone, a mapping whose oid the catalog no longer names points at an object
/// nothing can reach, and fsck asks to remove it. Bounded by the same depth cap
/// the extentref walk uses, so a crafted cycle cannot recurse without end.
pub(crate) fn collect_fstree_oids<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    oid: u64,
    bsz: usize,
    out: &mut HashSet<u64>,
    depth: u32,
) -> Result<(), TxnError> {
    collect_fstree_oids_below(txn, omap_node, oid, bsz, out, depth, None)
}

/// One node of the `collect_fstree_oids` walk. An oid already in `out` has
/// been walked (the set may be shared across roots that share subtrees), so
/// it is not entered again; a pointer loop cannot pass the level rule, since
/// every child sits one level below its parent. A tree deeper than the cap
/// is corruption, and a walk that quietly stopped there would report the
/// nodes past that point as unreachable and let their mappings be removed.
fn collect_fstree_oids_below<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    oid: u64,
    bsz: usize,
    out: &mut HashSet<u64>,
    depth: u32,
    parent_level: Option<u16>,
) -> Result<(), TxnError> {
    if depth > MAX_TREE_DEPTH {
        return Err(fstree_too_deep());
    }
    if !out.insert(oid) {
        return Ok(());
    }
    let Some(paddr) = omap_resolve(txn, omap_node, oid, u64::MAX, bsz)? else {
        return Ok(());
    };
    let mut node = vec![0u8; bsz];
    txn.read_block(paddr, &mut node)?;
    let level = fstree_child_level(&node, oid, parent_level)?;
    if level == 0 {
        return Ok(());
    }
    let recs = parse_cat_leaf(&node).map_err(|e| TxnError::SpacemanParse(e.into()))?;
    for r in recs {
        collect_fstree_oids_below(
            txn,
            omap_node,
            rd_u64(&r.val, 0),
            bsz,
            out,
            depth + 1,
            Some(level),
        )?;
    }
    Ok(())
}

/// The catalog records an operation reads, gathered by key range instead of
/// by walking the whole tree. Each record appears once, so code that
/// filters it the way it filtered the whole catalog finds the same records,
/// as long as every object it looks at was gathered.
struct Gathered<'a> {
    omap_node: &'a [u8],
    root_oid: u64,
    bsz: usize,
    recs: Vec<CatRec>,
    keys: HashSet<Vec<u8>>,
    objects: HashSet<u64>,
}

impl<'a> Gathered<'a> {
    fn new(omap_node: &'a [u8], root_oid: u64, bsz: usize) -> Self {
        Self {
            omap_node,
            root_oid,
            bsz,
            recs: Vec::new(),
            keys: HashSet::new(),
            objects: HashSet::new(),
        }
    }

    fn add(&mut self, recs: Vec<CatRec>) {
        for (k, v) in recs {
            if self.keys.insert(k.clone()) {
                self.recs.push((k, v));
            }
        }
    }

    /// Every record of object `id`.
    fn object<D: WritableBlockDevice>(
        &mut self,
        txn: &mut Transaction<D>,
        id: u64,
    ) -> Result<&mut Self, TxnError> {
        if self.objects.insert(id) {
            let lo = encode_jkey(id, 0).to_le_bytes();
            let hi = encode_jkey(id.saturating_add(1), 0).to_le_bytes();
            let recs =
                cow_tree::catalog_range(txn, self.omap_node, self.root_oid, &lo, &hi, self.bsz)?;
            self.add(recs);
        }
        Ok(self)
    }

    /// The entries of directory `parent` whose name hashes like the one in
    /// `drec_key`: the one `drec_key` names and any it would collide with.
    fn names_like<D: WritableBlockDevice>(
        &mut self,
        txn: &mut Transaction<D>,
        drec_key: &[u8],
    ) -> Result<&mut Self, TxnError> {
        let Some(head) = drec_key.get(..8) else {
            return Ok(self);
        };
        let hash = rd_u32(drec_key, 8) >> 10;
        let bound = |h: u32| {
            let mut k = head.to_vec();
            k.extend_from_slice(&(h << 10).to_le_bytes());
            k
        };
        let hi = match hash.checked_add(1).filter(|h| *h < 1 << 22) {
            Some(h) => bound(h),
            None => encode_jkey(
                rd_u64(drec_key, 0) & 0x0FFF_FFFF_FFFF_FFFF,
                APFS_TYPE_DIR_REC + 1,
            )
            .to_le_bytes()
            .to_vec(),
        };
        let recs = cow_tree::catalog_range(
            txn,
            self.omap_node,
            self.root_oid,
            &bound(hash),
            &hi,
            self.bsz,
        )?;
        self.add(recs);
        Ok(self)
    }

    /// What [`plan_removal`] reads to take away the name `drec_val` gives
    /// object `file_id`: the object's records, its content stream's and the
    /// streams of its attributes.
    fn removal<D: WritableBlockDevice>(
        &mut self,
        txn: &mut Transaction<D>,
        file_id: u64,
    ) -> Result<&mut Self, TxnError> {
        self.object(txn, file_id)?;
        let mut streams = Vec::new();
        for (k, v) in &self.recs {
            if rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF != file_id {
                continue;
            }
            match rd_u64(k, 0) >> 60 {
                APFS_TYPE_INODE => {
                    let mode = rd_u16(v, 80);
                    let id = rd_u64(v, INODE_PRIVATE_ID);
                    if id != 0 && (mode & 0o170000) != S_IFDIR {
                        streams.push(id);
                    }
                }
                APFS_TYPE_XATTR if rd_u16(v, 0) & XATTR_DATA_STREAM != 0 => {
                    streams.push(rd_u64(v, 4));
                }
                _ => {}
            }
        }
        for id in streams {
            self.object(txn, id)?;
        }
        Ok(self)
    }
}

/// The catalog records an operation on the entries `drec_keys` reads: each
/// entry's directory record and those it could collide with, and for every
/// object they name, what [`plan_removal`] reads to take it away. A
/// transaction asked to rebuild in full reads the whole catalog instead, the
/// reference this narrower read is checked against.
fn gather_entries<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    root_oid: u64,
    drec_keys: &[&[u8]],
    bsz: usize,
) -> Result<Vec<CatRec>, TxnError> {
    if txn.full_rebuild() {
        return collect_fstree(txn, omap_node, root_oid, bsz);
    }
    let mut g = Gathered::new(omap_node, root_oid, bsz);
    for key in drec_keys {
        g.names_like(txn, key)?;
    }
    let named: Vec<u64> = g
        .recs
        .iter()
        .filter(|(k, _)| rd_u64(k, 0) >> 60 == APFS_TYPE_DIR_REC)
        .map(|(_, v)| rd_u64(v, 0) & 0x0FFF_FFFF_FFFF_FFFF)
        .collect();
    for id in named {
        g.removal(txn, id)?;
    }
    Ok(g.recs)
}

/// The inode value of object `id`: from `near` when it holds it, else read
/// from the catalog. `near` is the whole catalog when the transaction
/// rebuilds in full.
fn inode_value<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    root_oid: u64,
    id: u64,
    near: &[CatRec],
    bsz: usize,
) -> Result<Option<Vec<u8>>, TxnError> {
    let key = build_inode_key(id);
    if let Some((_, v)) = near.iter().find(|(k, _)| *k == key) {
        return Ok(Some(v.clone()));
    }
    if txn.full_rebuild() {
        return Ok(None);
    }
    let hi = encode_jkey(id, APFS_TYPE_INODE + 1).to_le_bytes();
    Ok(
        cow_tree::catalog_range(txn, omap_node, root_oid, &key, &hi, bsz)?
            .into_iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v),
    )
}

/// The catalog records an operation on object `id` reads: what
/// [`Gathered::removal`] gathers for it. A transaction asked to rebuild in
/// full reads the whole catalog instead.
fn gather_object<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    root_oid: u64,
    id: u64,
    bsz: usize,
) -> Result<Vec<CatRec>, TxnError> {
    if txn.full_rebuild() {
        return collect_fstree(txn, omap_node, root_oid, bsz);
    }
    let mut g = Gathered::new(omap_node, root_oid, bsz);
    g.removal(txn, id)?;
    Ok(g.recs)
}

/// Return true if `drec_key` (a parent+name directory record) already exists in
/// the catalog. Used to reject duplicate-name creates/writes/renames BEFORE any
/// block is allocated, so a rejected op leaves no trace (no leaked blocks) and
/// never produces a duplicate DREC (which corrupts the fsroot tree).
/// Is there already an entry `drec_key` would collide with?
///
/// The key's stored bytes are the name as given, so two spellings of one name
/// have different keys yet are the same entry to the volume: on a
/// case-insensitive volume any spelling with the same fold (`FOO` beside
/// `foo`, `STRASSE` beside `straße`), on a normalization-insensitive one any
/// spelling with the same decomposition (NFC beside NFD). A byte comparison
/// let such a name in beside the original with the same hash, which no
/// lookup could tell apart. Entries under the same parent with the same hash
/// are compared the way the volume compares names.
fn drec_exists<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    root_tree_oid: u64,
    drec_key: &[u8],
    bsz: usize,
    case_fold: bool,
    normalize: bool,
) -> Result<bool, TxnError> {
    let near = if txn.full_rebuild() {
        collect_fstree(txn, omap_node, root_tree_oid, bsz)?
    } else {
        let mut g = Gathered::new(omap_node, root_tree_oid, bsz);
        g.names_like(txn, drec_key)?;
        g.recs
    };
    Ok(find_drec_collision(&near, drec_key, case_fold, normalize).is_some())
}

/// The entry `drec_key` names as the volume compares names, if any: same
/// parent, same hash, and a stored name that folds (case-insensitive volume)
/// or decomposes (normalization-insensitive volume) to the same string.
fn find_drec_collision<'a>(
    all: &'a [(Vec<u8>, Vec<u8>)],
    drec_key: &[u8],
    case_fold: bool,
    normalize: bool,
) -> Option<&'a (Vec<u8>, Vec<u8>)> {
    use apfs_core::case_fold::{apfs_names_match, apfs_names_match_case_sensitive};
    let Some(wanted) = drec_key_name(drec_key) else {
        return all.iter().find(|(k, _)| k.as_slice() == drec_key);
    };
    let wanted_hash = rd_u32(drec_key, 8) >> 10;
    all.iter().find(|(k, _)| {
        if k.len() < 13 || k.get(..8) != drec_key.get(..8) || rd_u32(k, 8) >> 10 != wanted_hash {
            return false;
        }
        match drec_key_name(k) {
            Some(stored) if case_fold => apfs_names_match(stored, wanted),
            Some(stored) if normalize => apfs_names_match_case_sensitive(stored, wanted),
            Some(stored) => stored == wanted,
            None => k.as_slice() == drec_key,
        }
    })
}

/// The name a hashed directory-record key stores (without its terminator).
fn drec_key_name(key: &[u8]) -> Option<&str> {
    let raw = key.get(12..)?;
    let bytes = raw.split(|&b| b == 0).next()?;
    std::str::from_utf8(bytes).ok()
}

/// Free the metadata blocks that a copy-on-write mutation replaced, so the
/// allocator does not LEAK them. A leaked block stays set in the chunk bitmap
/// but is unreferenced by the live volume, which fsck_apfs reports as container
/// "overallocation" (and skews apfs_fs_alloc_count). Every catalog/omap/extentref
/// mutation COWs to fresh blocks; without this the old blocks accumulate forever.
///
/// Snapshot-aware: a replaced block is pinned by a snapshot iff its version xid
/// is <= the newest snapshot xid (om_most_recent_snap) - such a block is part of
/// a frozen snapshot tree and must be kept (it is legitimately referenced, not a
/// leak). Blocks whose xid is GREATER (post-snapshot intermediates) are orphaned
/// and freed. With no snapshot, every replaced block is freed.
/// [the APFS specification Q1 - keep latest version <= each snapshot
///  xid; prune intermediates. fsck validates only the latest checkpoint, so
///  immediate free of unpinned blocks is CLEAN (the APFS specification Q2).]
///
/// Frees: old volume omap header (omap_phys), old omap b-tree node, every old
/// catalog b-tree node the prior omap mapped, and - when `freed_extref` - the
/// old extentref tree node. Must be called AFTER all new blocks for the txn are
/// allocated, so a freed address is never handed back out within the same txn.
/// [CERTAIN: empirical - decoded leaked omap/extref/catalog blocks == the exact
///  blocks fsck flagged as overallocation; freeing the unpinned ones is CLEAN.]
///
/// Returns the `fs_alloc_count` correction that callers MUST add to their VSB
/// delta AFTER calling this function.
///
/// # fs_alloc_count model (#149 fix)
///
/// `apfs_fs_alloc_count` must equal exactly what apfsck accumulates as
/// `v_block_count` (apfsprogs apfsck/super.c:1175):
///   live extref KIND_NEW entries + live B-tree node count
///   + snapshot.c:132 fold-in of each snapshot's own block count.
///
/// When a snapshot exists, replaced-metadata blocks (old omap header, old
/// omap b-tree node, old catalog nodes, old extentref node) whose o_xid is
/// <= newest_snap are PINNED by the snapshot and routed to sm_fq instead of
/// being freed immediately.  sm_fq-enqueued blocks stay allocated in the
/// bitmap AND stay counted in `fs_alloc_count` until `delete_snapshot` drains
/// them.  They must NOT be subtracted from `fs_alloc_count` at enqueue time.
///
/// Callers compute `node_delta = new_catalog_nodes - old_catalog_nodes`.
/// When old catalog nodes are sm_fq-pinned (not freed), the `node_delta`
/// subtraction over-counts: those old blocks stay in `fs_alloc_count` until
/// the snapshot is deleted.  This function returns the count of sm_fq-pinned
/// OLD CATALOG NODES so callers can add it back:
///
///   fs_alloc_count += alloc_delta + frm_correction
///
/// Returns the net `fs_alloc_count` correction callers must add to their delta.
///
/// Per apfsprogs Q5 model (linux-apfs-rw extents.c apfs_free_phys_ext):
/// `fs_alloc_count` is decremented when a block is sent to sm_fq (deferred
/// free), not when it is actually drained.  For non-catalog COW-pair blocks
/// (omap header, omap btree, extentref) that go to sm_fq, the correction is
/// -1 per block.  For immediately freed blocks the caller's alloc_delta
/// already nets to 0 (old freed cancels new alloc), so correction = 0.
///
/// For catalog nodes only xid > newest_snap entries are processed (the
/// intermediate ones pruned by rewrite_fstree).  Snapshot-pinned entries
/// (xid <= newest_snap) are retained in the new omap verbatim and must not
/// be freed or sm_fq'd here.
///
/// For no-snapshot volumes (newest_snap == 0) every block is immediately
/// freed, correction = 0 - identical to pre-#149 behaviour.
///
/// [Reference: apfsprogs apfsck/super.c:1175 v_block_count model;
///  linux-apfs-rw extents.c apfs_free_phys_ext sm_fq decrement;
///  empirical #149: one omap-btree block at xid=newest_snap goes to sm_fq
///  → -1 correction brings actual=26 == expected=26]
/// Copy-on-write the volume object-map header `vol_omap_raw` so that it
/// names tree root `tree_root` at transaction `xid`, and return its block. A
/// header this transaction already staged is named by nothing a checkpoint
/// holds: it is rewritten where it is, and [`free_replaced_metadata`] keeps
/// it. Any other header gets a new block and the old one is released there.
/// Every operation that releases through [`free_replaced_metadata`] stages
/// its header here, so the two agree on which case applies.
fn stage_vomap_header<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vol_omap_raw: &[u8],
    xid: u64,
    tree_root: u64,
    bsz: usize,
) -> Result<u64, TxnError> {
    let old = rd_u64(vol_omap_raw, 8);
    let in_place = txn.staged_live(old);
    let paddr = if in_place { old } else { txn.alloc_block()? };
    let mut header = vol_omap_raw.to_vec();
    header.resize(bsz, 0);
    wr_u64(&mut header, 8, paddr);
    wr_u64(&mut header, 16, xid);
    wr_u64(&mut header, 48, tree_root);
    update_checksum_in_place(&mut header);
    if in_place {
        txn.restage(paddr, header);
    } else {
        txn.stage_raw(paddr, header);
    }
    Ok(paddr)
}

fn free_replaced_metadata<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    old_omap_node: &[u8],
    old_extref_padrs: &[u64],
    reclaim: &Reclaim,
    bsz: usize,
) -> Result<i64, TxnError> {
    // Newest snapshot xid (0 = no snapshots). Pinned iff a block's xid <= this.
    let newest_snap = rd_u64(vol_omap_raw, 64); // omap_phys.om_most_recent_snap

    // Non-catalog COW-pairs (omap header, omap b-tree, old extentref nodes):
    // old freed / sm_fq'd, new allocated - net 0, never subtracted by callers.
    // old_extref_padrs: every block that was part of the old extref tree
    // (may be multiple nodes when the tree was already multi-node). [#151]
    //
    // After a rebuild, walk the OLD (possibly multi-node) omap b-tree once:
    // every tree node must be freed (a split omap has more than the root node,
    // else the non-root nodes leak → fsck "overallocation"), and every LEAF
    // mapping gives the old catalog node paddrs to reclaim. [#157] A
    // path-local rewrite already knows the few blocks it replaced; every
    // other node still stands in the new trees.
    let (old_omap_nodes, catalog_padrs): (Vec<u64>, Vec<u64>) = match reclaim {
        Reclaim::Whole => {
            let old_omap_root_paddr = rd_u64(vol_omap_raw, 48);
            let mut old_omap_triples: Vec<OmapRec> = Vec::new();
            let mut old_omap_nodes: Vec<u64> = Vec::new();
            walk_omap_tree(
                txn,
                old_omap_node,
                old_omap_root_paddr,
                bsz,
                &mut old_omap_triples,
                &mut old_omap_nodes,
            )?;
            // Catalog nodes: only process entries with xid > newest_snap (the
            // "intermediate" ones that were pruned from the new omap by
            // rewrite_fstree). Snapshot-pinned entries (xid <= newest_snap)
            // are RETAINED verbatim in the new omap - their backing blocks
            // are neither freed nor sm_fq'd here. Processing them would cause
            // duplicate sm_fq enqueues on every commit because the same paddr
            // keeps appearing in old_omap_node. [#149 fix]
            let catalog = old_omap_triples
                .iter()
                .filter(|r| r.xid > newest_snap)
                .map(|r| r.paddr)
                .collect();
            (old_omap_nodes, catalog)
        }
        Reclaim::Exact {
            omap_nodes,
            catalog,
        } => (omap_nodes.clone(), catalog.clone()),
    };

    let non_catalog: Vec<u64> = {
        // The old omap_phys header, unless it was staged in this transaction
        // and so rewritten in place (`stage_vomap_header`).
        let old_header = rd_u64(vsb_raw, VSBI_OMAP_OID);
        let mut v = Vec::with_capacity(old_omap_nodes.len() + 1);
        if !txn.staged_live(old_header) {
            v.push(old_header);
        }
        v.extend_from_slice(&old_omap_nodes); // the old omap b-tree nodes replaced
        v
    };

    // The live extent-reference tree is never shared with a snapshot: taking
    // a snapshot hands the populated tree to the snapshot and starts the live
    // volume on a fresh one, so a superseded live node is nobody else's and
    // goes straight back. Its transaction id says nothing here - the fresh
    // root carries the snapshot's own id, and judging it by that queued it as
    // pinned, so the drain at snapshot deletion took it off the volume's count
    // a second time.
    for &paddr in old_extref_padrs {
        txn.free_block(paddr)?;
    }

    // Net fs_alloc_count correction to return to callers.
    //
    // A block handed to sm_fq is NOT deducted here. fsck derives
    // apfs_fs_alloc_count by walking everything the volume still reaches, and
    // a queued block is still reached through the snapshot that pinned it -
    // the queue only defers clearing its bitmap bit. Deducting at enqueue time
    // left the field one block low for as long as the snapshot lived (fsck:
    // "apfs_fs_alloc_count is not valid"). delete_snapshot deducts when it
    // drains the queue and the block genuinely leaves the volume.
    //
    // A block freed outright needs no correction either: for the non-catalog
    // COW-pairs (omap header, omap b-tree, extentref) the caller's alloc_delta
    // already nets the new allocation against the old free, and for catalog
    // nodes node_delta covers it.
    //
    // Both branches therefore leave the correction at 0 today.

    // Process non-catalog COW-pairs (omap hdr, omap btree, extentref).
    for paddr in non_catalog {
        if newest_snap == 0 {
            txn.free_block(paddr)?;
        } else {
            let mut b = vec![0u8; bsz];
            txn.read_block(paddr, &mut b)?;
            let blk_xid = rd_u64(&b, 16);
            if blk_xid > newest_snap {
                txn.free_block(paddr)?;
            } else {
                // sm_fq: old COW-pair block sent to deferred reclaim. The
                // queue only defers clearing the bitmap bit - the snapshot
                // still reaches the block, and fsck counts fs_alloc_count by
                // what the volume reaches, so the block stays counted until
                // delete_snapshot actually drains it.
                txn.enqueue_sm_fq(newest_snap, paddr)?;
            }
        }
    }

    // Process catalog nodes (only xid > newest_snap, i.e. intermediate entries).
    for paddr in catalog_padrs {
        if newest_snap == 0 {
            txn.free_block(paddr)?;
        } else {
            // Read the block's version xid; if a snapshot pins it, enqueue it
            // to sm_fq[SFQ_MAIN] for deferred reclaim (M7b-RT Subtask D3).
            // Previously this branch silently preserved the block - but the
            // container-level space verifier in fsck does not traverse the
            // snapshot's frozen-VSB chain, so silently-preserved blocks
            // surfaced as "overallocation". Explicit enqueue makes the pin
            // visible to fsck and queues the bitmap clear for delete_snapshot.
            let mut b = vec![0u8; bsz];
            txn.read_block(paddr, &mut b)?;
            let blk_xid = rd_u64(&b, 16);
            if blk_xid > newest_snap {
                txn.free_block(paddr)?;
            } else {
                // Should not occur after filtering above, but handle correctly.
                // Same as above: still reachable through the snapshot, so
                // still counted until the queue is drained.
                txn.enqueue_sm_fq(newest_snap, paddr)?;
            }
        }
    }
    Ok(0)
}

/// Parse all entries of a fixed-kv volume omap node into (oid, xid, paddr)
/// triples. Mirrors `omap_lookup`'s layout decoding. [the APFS specification]
/// One mapping of a volume object map: the object's version and where it
/// lives, with the value's flags and size carried as stored. A rebuild that
/// wrote every mapping as a live one-block object would turn a tombstone
/// written by macOS into a mapping to a block that is no longer the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OmapRec {
    pub(crate) oid: u64,
    pub(crate) xid: u64,
    pub(crate) paddr: u64,
    pub(crate) flags: u32,
    pub(crate) size: u32,
}

impl OmapRec {
    /// A mapping to a one-block object this writer placed.
    pub(crate) fn live(oid: u64, xid: u64, paddr: u64, bsz: usize) -> Self {
        Self {
            oid,
            xid,
            paddr,
            flags: 0,
            size: bsz as u32,
        }
    }
}

pub(crate) fn parse_omap_entries(node: &[u8], bsz: usize) -> Result<Vec<OmapRec>, TxnError> {
    let layout = omap_node_layout(node, bsz)?;
    let mut out = Vec::with_capacity(layout.nkeys);
    for i in 0..layout.nkeys {
        let k = layout.key_at(node, i)?;
        let v = layout.val_at(node, i, OMAP_LEAF_VAL_SIZE)?;
        out.push(OmapRec {
            oid: rd_u64(node, k),
            xid: rd_u64(node, k + 8),
            flags: rd_u32(node, v),
            size: rd_u32(node, v + 4),
            paddr: rd_u64(node, v + 8),
        });
    }
    Ok(out)
}

/// Walk a (possibly MULTI-NODE) volume omap b-tree from `node` (its paddr is
/// `node_paddr`), collecting EVERY leaf mapping into `triples` and EVERY tree
/// node's physical address into `nodes` (root + index + leaves). The single-node
/// `parse_omap_entries` only decodes one node and so misses (and misreads) a
/// split omap - callers that must see ALL old mappings (node_delta accounting,
/// snapshot retention) or free ALL old omap blocks (#157) use this instead.
/// Index values are 8-byte PHYSICAL child paddrs (the omap is BTREE_PHYSICAL).
/// [CERTAIN: APFS PDF §Object Maps; mirrors `omap_resolve`'s descent.]
fn walk_omap_tree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    node: &[u8],
    node_paddr: u64,
    bsz: usize,
    triples: &mut Vec<OmapRec>,
    nodes: &mut Vec<u64>,
) -> Result<(), TxnError> {
    let mut visited = HashSet::new();
    walk_omap_node(txn, node, node_paddr, bsz, triples, nodes, &mut visited, 0)
}

/// One node of the `walk_omap_tree` descent. Bounded by a depth cap, a set
/// of visited blocks and the rule that a child is an omap node one level
/// below its parent, so a crafted cycle is an error rather than unbounded
/// recursion.
#[allow(clippy::too_many_arguments)]
fn walk_omap_node<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    node: &[u8],
    node_paddr: u64,
    bsz: usize,
    triples: &mut Vec<OmapRec>,
    nodes: &mut Vec<u64>,
    visited: &mut HashSet<u64>,
    depth: u32,
) -> Result<(), TxnError> {
    if depth > MAX_OMAP_TREE_DEPTH {
        return Err(TxnError::OmapParse(format!(
            "omap tree is deeper than {MAX_OMAP_TREE_DEPTH} levels"
        )));
    }
    if !visited.insert(node_paddr) {
        return Err(TxnError::OmapParse(format!(
            "omap node {node_paddr} is reached twice: the tree has a cycle"
        )));
    }
    nodes.push(node_paddr);
    let level = rd_u16(node, 34);
    if level == 0 {
        // Leaf: decode its {oid, xid} -> paddr mappings.
        triples.extend(parse_omap_entries(node, bsz)?);
        return Ok(());
    }
    // Index node: each value is an 8-byte PHYSICAL child paddr - recurse.
    let layout = omap_node_layout(node, bsz)?;
    for i in 0..layout.nkeys {
        let v = layout.val_at(node, i, OMAP_INDEX_VAL_SIZE)?;
        let child = rd_u64(node, v);
        let mut buf = vec![0u8; bsz];
        txn.read_block(child, &mut buf)?;
        omap_child_level(&buf, child, level)?;
        walk_omap_node(txn, &buf, child, bsz, triples, nodes, visited, depth + 1)?;
    }
    Ok(())
}

/// Convenience: collect ALL leaf mappings of a (possibly multi-node) volume omap
/// rooted at `root_node` (paddr `root_paddr`). See [`walk_omap_tree`].
fn collect_omap_triples<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    root_node: &[u8],
    root_paddr: u64,
    bsz: usize,
) -> Result<Vec<OmapRec>, TxnError> {
    Ok(collect_omap_tree(txn, root_node, root_paddr, bsz)?.0)
}

/// Same walk, but also returns every node block the tree occupies - a caller
/// that replaces the tree has to release all of them, not just the root.
pub(crate) fn collect_omap_tree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    root_node: &[u8],
    root_paddr: u64,
    bsz: usize,
) -> Result<(Vec<OmapRec>, Vec<u64>), TxnError> {
    let mut triples = Vec::new();
    let mut nodes = Vec::new();
    walk_omap_tree(txn, root_node, root_paddr, bsz, &mut triples, &mut nodes)?;
    Ok((triples, nodes))
}

/// Rebuild a single-node volume omap b-tree (fixed-kv, key {oid,xid} 16B,
/// val {flags,size,paddr} 16B) from `entries` (oid, xid, paddr) triples -
/// each keyed by its OWN xid so that snapshot-time versions can coexist with
/// the live version. The node's own o_xid is set to `xid` (the current txn).
/// The object header, subtype and tree description come from `template`; the
/// node is a root leaf of fixed-size records whatever the template's flags
/// said, and its description counts one node and 16-byte keys and values.
/// Entries that do not fit one node are an error, not a node with its keys
/// and values run together. [the APFS specification]
#[cfg(test)]
pub(crate) fn build_omap_node(
    template: &[u8],
    entries: &[OmapRec],
    paddr: u64,
    xid: u64,
    bsz: usize,
) -> Result<Vec<u8>, TxnError> {
    let mut sorted = entries.to_vec();
    // omap b-tree key order: oid ascending, then xid ascending.
    sorted.sort_by(|a, b| (a.oid, a.xid).cmp(&(b.oid, b.xid)));
    let nkeys = sorted.len();
    let mut out = vec![0u8; bsz];
    // obj_phys from the template (type and subtype), then this node's own
    // identity and shape.
    wr_bytes(&mut out, 0, template.get(..32).unwrap_or(&[]));
    wr_u64(&mut out, 8, paddr); // physical o_oid = paddr
    wr_u64(&mut out, 16, xid);
    wr_u16(
        &mut out,
        32,
        BTNODE_ROOT | BTNODE_LEAF | BTNODE_FIXED_KV_SIZE,
    );
    wr_u16(&mut out, 34, 0); // level
    wr_u32(&mut out, 36, nkeys as u32);
    wr_u16(&mut out, 48, BTOFF_INVALID);
    wr_u16(&mut out, 52, BTOFF_INVALID);
    // Keep the template's table reserve (448 on a macOS-written root) when it
    // holds the entries, so the layout matches the kernel's; grow it otherwise.
    let toc_len = (rd_u16(template, 42) as usize).max(nkeys * 4).div_ceil(8) * 8;
    let key_area = DATA_BASE + toc_len;
    let val_area_end = bsz.saturating_sub(BTREE_INFO_SIZE);
    if key_area + nkeys * 16 > val_area_end.saturating_sub(nkeys * 16) {
        return Err(TxnError::OmapParse(format!(
            "{nkeys} object map entries do not fit one node"
        )));
    }
    for (i, r) in sorted.iter().enumerate() {
        let k = key_area + i * 16;
        wr_u64(&mut out, k, r.oid);
        wr_u64(&mut out, k + 8, r.xid);
        let v = val_area_end - (i + 1) * 16;
        wr_u32(&mut out, v, r.flags); // ov_flags, as stored
        wr_u32(&mut out, v + 4, r.size); // ov_size, as stored
        wr_u64(&mut out, v + 8, r.paddr); // ov_paddr
        let te = DATA_BASE + i * 4; // kvoff_t
        wr_u16(&mut out, te, (i * 16) as u16);
        wr_u16(&mut out, te + 2, ((i + 1) * 16) as u16);
    }
    wr_u16(&mut out, 40, 0); // table_space.off
    wr_u16(&mut out, 42, toc_len as u16);
    let key_end = key_area + nkeys * 16;
    let val_start = val_area_end.saturating_sub(nkeys * 16);
    wr_u16(&mut out, 44, key_end.saturating_sub(key_area) as u16); // free_space.off
                                                                   // saturating_sub: never underflow-panic in a library path. This single-node
                                                                   // builder is only reached for small (single-node-omap) trees now, but stay
                                                                   // crash-safe regardless. [#157 review]
    wr_u16(&mut out, 46, val_start.saturating_sub(key_end) as u16); // free_space.len
                                                                    // Footer: the tree's flags and sizes from the template, the counts and
                                                                    // longest key/value from this node.
    let bti = val_area_end;
    wr_bytes(&mut out, bti, template.get(bti..).unwrap_or(&[]));
    wr_u32(&mut out, bti + 16, 16); // bt_longest_key
    wr_u32(&mut out, bti + 20, 16); // bt_longest_val
    wr_u64(&mut out, bti + 24, nkeys as u64); // bt_key_count
    wr_u64(&mut out, bti + 32, 1); // bt_node_count
    update_checksum_in_place(&mut out);
    Ok(out)
}

/// Serialize ONE volume-omap b-tree node (FIXED-kv, BTREE_PHYSICAL).
///
/// `entries` are `(key16, val)` pairs sorted ascending by `(oid, xid)`; `val` is
/// 16 bytes for a leaf (level 0: `{ov_flags, ov_size, ov_paddr}`) or 8 bytes for
/// an index node (a PHYSICAL child paddr). The obj_phys header is derived from
/// `template` (the live omap root): the FIXED_KV bit + omap o_subtype are
/// inherited; ROOT/LEAF are set per node; physical o_type is stamped. A tight
/// TOC (`table_space.len = nkeys*4`) is used and bounds are guaranteed by the
/// caller's capacity chunking, so there is no unchecked-arithmetic underflow
/// (unlike the single-node `build_omap_node`). The btree_info footer is emitted
/// ONLY for the root, copied from `template` with `bt_key_count` / `bt_node_count`
/// patched to the whole-tree totals. [the APFS specification; mirrors the proven
/// `build_omap_node` per-node layout, generalised to non-root + index nodes]
#[allow(clippy::too_many_arguments)]
fn serialize_omap_node(
    template: &[u8],
    level: u16,
    is_root: bool,
    entries: &[([u8; 16], Vec<u8>)],
    paddr: u64,
    xid: u64,
    key_count: u64,
    node_count: u64,
    bsz: usize,
) -> Vec<u8> {
    let is_leaf = level == 0;
    let nkeys = entries.len();
    let mut out = vec![0u8; bsz];
    wr_bytes(&mut out, 0, template.get(..32).unwrap_or(&[]));
    wr_u64(&mut out, 8, paddr); // physical o_oid == paddr
    wr_u64(&mut out, 16, xid);
    let otype = OBJ_PHYSICAL
        | if is_root {
            OBJECT_TYPE_BTREE
        } else {
            OBJECT_TYPE_BTREE_NODE
        };
    wr_u32(&mut out, 24, otype);
    wr_u32(&mut out, 28, rd_u32(template, 28)); // o_subtype = OMAP (inherit)
    let mut flags = rd_u16(template, 32) & BTNODE_FIXED_KV_SIZE;
    if is_root {
        flags |= BTNODE_ROOT;
    }
    if is_leaf {
        flags |= BTNODE_LEAF;
    }
    wr_u16(&mut out, 32, flags);
    wr_u16(&mut out, 34, level);
    wr_u32(&mut out, 36, nkeys as u32);
    // table_space (the kvoff TOC reserve) is NOT tight: fsck requires the fixed
    // per-kind reserve a full node would use - `cap * 4` where `cap` is how many
    // {16B key + val} entries fit in `bsz - DATA_BASE` (the footer is NOT
    // subtracted for this reserve). Empirically matches Apple: 448 (leaf, val=16)
    // and 576 (index, val=8). A tight `nkeys*4` is rejected with
    // "invalid btn_table_space". [APFS spec: decoded from a macOS-authored
    // multi-node omap - see omap_multinode_157 dump]
    let vsz = if is_leaf { 16 } else { 8 };
    let toc_len = ((bsz - DATA_BASE) / (16 + vsz + 4)) * 4;
    wr_u16(&mut out, 40, 0); // table_space.off
    wr_u16(&mut out, 42, toc_len as u16); // table_space.len
    wr_u16(&mut out, 48, BTOFF_INVALID); // key_free_list
    wr_u16(&mut out, 52, BTOFF_INVALID); // val_free_list
    let key_area = DATA_BASE + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    for (i, (k, v)) in entries.iter().enumerate() {
        let ka = key_area + i * 16;
        wr_bytes(&mut out, ka, k);
        let va = val_area_end - (i + 1) * vsz;
        wr_bytes(&mut out, va, v);
        let te = DATA_BASE + i * 4; // kvoff_t
        wr_u16(&mut out, te, (i * 16) as u16); // k_off (rel key_area)
        wr_u16(&mut out, te + 2, ((i + 1) * vsz) as u16); // v_off (rel val_area_end)
    }
    let key_end = key_area + nkeys * 16;
    let val_start = val_area_end.saturating_sub(nkeys * vsz);
    wr_u16(&mut out, 44, key_end.saturating_sub(key_area) as u16); // free_space.off
                                                                   // saturating_sub: the caller's capacity chunking guarantees val_start >=
                                                                   // key_end, but never underflow-panic in a library path if a future capacity
                                                                   // edit drifts (no panic in library code). [#157 review]
    wr_u16(&mut out, 46, val_start.saturating_sub(key_end) as u16); // free_space.len
    if is_root {
        let bti = bsz - BTREE_INFO_SIZE;
        wr_bytes(&mut out, bti, template.get(bti..).unwrap_or(&[]));
        wr_u64(&mut out, bti + 24, key_count); // bt_key_count (whole tree)
        wr_u64(&mut out, bti + 32, node_count); // bt_node_count (whole tree)
    }
    update_checksum_in_place(&mut out);
    out
}

/// Build the volume omap b-tree from `triples` `(oid, xid, paddr)`, staging
/// every node via `txn`, and return `(root_paddr, node_count)`.
///
/// When the mappings fit one node a single ROOT|LEAF node is emitted (identical
/// shape to `build_omap_node`). Otherwise a multi-node tree is built bottom-up -
/// leaves (level 0) chunked by leaf capacity, then physical index levels whose
/// entries are `(child_first_key, child_paddr)` - until one root holds the top
/// level. This is the omap counterpart of the FSTREE multi-node rebuild in
/// `rewrite_fstree_impl`; the single-node `omap_lookup` / `build_omap_node`
/// could neither read nor write such a tree (#157). The omap is BTREE_PHYSICAL,
/// so index children are referenced by raw paddr (no omap-of-omap).
/// [CERTAIN: APFS PDF §Object Maps + apfsprogs/mkfs omap layout; capacities are
///  conservative (computed against the root's smaller value area so any node
///  fits).]
pub(crate) fn build_omap_tree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    template: &[u8],
    triples: &[OmapRec],
    new_xid: u64,
    bsz: usize,
) -> Result<(u64, u64), TxnError> {
    let mut sorted = triples.to_vec();
    sorted.sort_by(|a, b| (a.oid, a.xid).cmp(&(b.oid, b.xid)));
    let key_count = sorted.len() as u64;

    // Conservative per-node capacity: budget against the ROOT value area
    // (bsz - footer) so a root OR non-root node always fits. Footprint per
    // entry = 4 (TOC) + 16 (key) + val (16 leaf / 8 index).
    let avail = bsz
        .saturating_sub(BTREE_INFO_SIZE)
        .saturating_sub(DATA_BASE);
    let leaf_cap = (avail / (4 + 16 + 16)).max(1);
    let idx_cap = (avail / (4 + 16 + 8)).max(1);

    // Leaf entries: key {oid, xid}, val {ov_flags=0, ov_size=bsz, ov_paddr}.
    let leaf_entries: Vec<([u8; 16], Vec<u8>)> = sorted
        .iter()
        .map(|r| {
            let mut k = [0u8; 16];
            k[..8].copy_from_slice(&r.oid.to_le_bytes());
            k[8..].copy_from_slice(&r.xid.to_le_bytes());
            let mut v = vec![0u8; 16];
            wr_u32(&mut v, 0, r.flags); // ov_flags, as stored
            wr_u32(&mut v, 4, r.size); // ov_size, as stored
            wr_u64(&mut v, 8, r.paddr); // ov_paddr
            (k, v)
        })
        .collect();

    // Single node: one ROOT|LEAF holds everything.
    if leaf_entries.len() <= leaf_cap {
        let p = txn.alloc_block()?;
        let node = serialize_omap_node(
            template,
            0,
            true,
            &leaf_entries,
            p,
            new_xid,
            key_count,
            1,
            bsz,
        );
        txn.stage_raw(p, node);
        return Ok((p, 1));
    }

    // Multi-node: leaves first (non-root), collecting (first_key, child_paddr).
    let mut node_count: u64 = 0;
    let mut parent: Vec<([u8; 16], Vec<u8>)> = Vec::new();
    for chunk in leaf_entries.chunks(leaf_cap) {
        let p = txn.alloc_block()?;
        let node = serialize_omap_node(template, 0, false, chunk, p, new_xid, 0, 0, bsz);
        txn.stage_raw(p, node);
        node_count += 1;
        // SAFETY: `chunks(leaf_cap)` with leaf_cap >= 1 never yields an empty
        // chunk, so `first()` is always Some; the fallback is unreachable.
        let first_key = chunk.first().map(|(k, _)| *k).unwrap_or([0u8; 16]);
        parent.push((first_key, p.to_le_bytes().to_vec()));
    }

    // Index levels bottom-up until a single root holds the top level.
    let mut level: u16 = 1;
    loop {
        if level > 32 {
            return Err(TxnError::SpacemanParse(
                "omap rebuild: tree depth exceeded 32 levels".into(),
            ));
        }
        if parent.len() <= idx_cap {
            let p = txn.alloc_block()?;
            node_count += 1;
            let node = serialize_omap_node(
                template, level, true, &parent, p, new_xid, key_count, node_count, bsz,
            );
            txn.stage_raw(p, node);
            return Ok((p, node_count));
        }
        let mut next: Vec<([u8; 16], Vec<u8>)> = Vec::new();
        for chunk in parent.chunks(idx_cap) {
            let p = txn.alloc_block()?;
            let node = serialize_omap_node(template, level, false, chunk, p, new_xid, 0, 0, bsz);
            txn.stage_raw(p, node);
            node_count += 1;
            // SAFETY: chunks(idx_cap) with idx_cap >= 1 never yields an empty
            // chunk; the fallback is unreachable.
            let first_key = chunk.first().map(|(k, _)| *k).unwrap_or([0u8; 16]);
            next.push((first_key, p.to_le_bytes().to_vec()));
        }
        parent = next;
        level += 1;
    }
}

/// Apply a mutation to the FSTREE and rebuild it (whole-tree rebuild), then
/// rebuild the volume omap b-tree to map every (possibly new) node. Returns the
/// new volume-omap-btree paddr for the caller to store in the COW'd omap header.
///
/// `parent_patch` = Some((parent_ino, nchildren_delta)) updates the parent dir
/// inode's nchildren + timestamps. The root keeps `root_tree_oid`; split leaves
/// get fresh oids from nx_next_oid. [the APFS specification]
#[allow(clippy::too_many_arguments)]
fn rewrite_fstree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_node: &[u8],
    new_xid: u64,
    new_records: Vec<(Vec<u8>, Vec<u8>)>,
    remove_keys: &[Vec<u8>],
    parent_patch: Option<(u64, i64)>,
    now: u64,
    bsz: usize,
) -> Result<FstreeRewrite, TxnError> {
    rewrite_fstree_impl(
        txn,
        vsb_raw,
        omap_node,
        new_xid,
        new_records,
        remove_keys,
        parent_patch,
        &[],
        now,
        bsz,
    )
}

/// The catalog and volume object map after [`rewrite_fstree_impl`].
struct FstreeRewrite {
    /// The volume object map's new tree root, for the COW'd omap header.
    omap_tree_paddr: u64,
    /// Volume-owned metadata blocks gained (negative: lost), for
    /// `fs_alloc_count`.
    node_delta: i64,
    /// What the change left for [`free_replaced_metadata`] to release.
    reclaim: Reclaim,
}

/// The catalog and object-map blocks a change superseded.
enum Reclaim {
    /// Both trees were rebuilt: every node of the old object map and every
    /// catalog node version it mapped past the newest snapshot.
    Whole,
    /// Only these blocks; every other node still stands in the new trees.
    Exact {
        omap_nodes: Vec<u64>,
        catalog: Vec<u64>,
    },
}

/// Like [`rewrite_fstree`] but additionally REPLACES the value of already-present
/// records listed in `update_records` (matched by exact key). Used by clone_file
/// to bump the source inode flags + the source dstream_id refcnt without the
/// duplicate-key rejection that `new_records` enforces. [#141]
///
/// On a volume without snapshots only the nodes on the paths to the changed
/// records are rewritten; otherwise (or when the transaction asks for it) the
/// whole tree and object map are rebuilt.
#[allow(clippy::too_many_arguments)]
fn rewrite_fstree_impl<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_node: &[u8],
    new_xid: u64,
    new_records: Vec<(Vec<u8>, Vec<u8>)>,
    remove_keys: &[Vec<u8>],
    parent_patch: Option<(u64, i64)>,
    update_records: &[(Vec<u8>, Vec<u8>)],
    now: u64,
    bsz: usize,
) -> Result<FstreeRewrite, TxnError> {
    if catalog_by_paths(txn, vsb_raw) {
        return rewrite_fstree_paths(
            txn,
            vsb_raw,
            omap_node,
            new_xid,
            new_records,
            remove_keys,
            parent_patch,
            update_records,
            now,
            bsz,
        );
    }
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let mut recs = collect_fstree(txn, omap_node, root_tree_oid, bsz)?;

    if let Some((pino, delta)) = parent_patch {
        let want = encode_jkey(pino, APFS_TYPE_INODE);
        for (k, v) in recs.iter_mut() {
            if rd_u64(k, 0) == want {
                refuse_dir_stats(v, pino)?;
                let nch = rd_u32(v, INODE_NCHILDREN) as i64;
                wr_u32(v, INODE_NCHILDREN, (nch + delta).max(0) as u32);
                wr_u64(v, INODE_MOD_TIME, now);
                wr_u64(v, INODE_CHANGE_TIME, now);
                // The directory's entries changed, whether or not their count did.
                step_write_generation(v);
            }
        }
    }
    // #141: replace the value of existing records (source inode flags, source
    // dstream_id refcnt). These keys are NOT in new_records, so no conflict.
    for (uk, uv) in update_records {
        for (k, v) in recs.iter_mut() {
            if k == uk {
                *v = uv.clone();
            }
        }
    }
    if !remove_keys.is_empty() {
        recs.retain(|(k, _)| !remove_keys.contains(k));
    }
    // Reject a new key that already exists (e.g. creating/renaming to a name that
    // is already present). Silently inserting it produces a duplicate record and
    // corrupts the catalog ("fsroot tree is invalid"); callers get a clean error.
    // [CERTAIN: empirical - duplicate DREC made fsck report the fsroot tree
    //  invalid; rejecting up front keeps the on-disk tree consistent.]
    {
        let existing: std::collections::HashSet<&[u8]> =
            recs.iter().map(|(k, _)| k.as_slice()).collect();
        for (k, _) in &new_records {
            if existing.contains(k.as_slice()) {
                return Err(TxnError::AlreadyExists(format!(
                    "catalog key {k:02x?} already present"
                )));
            }
        }
    }
    recs.extend(new_records);
    recs.sort_by(|a, b| cat_key_cmp(&a.0, &b.0));

    let longest_k = recs.iter().map(|(k, _)| k.len()).max().unwrap_or(0) as u32;
    let longest_v = recs.iter().map(|(_, v)| v.len()).max().unwrap_or(0) as u32;
    let key_count = recs.len() as u64;

    let mut omap_entries: Vec<(u64, u64)> = Vec::new();
    let footer = |node_count: u64| FstreeFooter {
        longest_key: longest_k,
        longest_val: longest_v,
        key_count,
        node_count,
    };
    if packed_size(&recs) <= node_capacity(bsz, true) {
        // Single ROOT|LEAF node.
        let p = txn.alloc_block()?;
        let node = build_fstree_node(root_tree_oid, new_xid, true, 0, &recs, Some(footer(1)), bsz);
        txn.stage_raw(p, node);
        omap_entries.push((root_tree_oid, p));
    } else {
        // Bottom-up build: leaves (level 0), then internal levels, until a
        // single root (which keeps root_tree_oid) holds all child pointers.
        // [the APFS specification - general N-level tree]
        let cap = node_capacity(bsz, false);
        let mut parent_entries: Vec<(Vec<u8>, u64)> = Vec::new();
        for chunk in partition_records(&recs, cap)? {
            let oid = txn.alloc_oid();
            let p = txn.alloc_block()?;
            txn.stage_raw(
                p,
                build_fstree_node(oid, new_xid, false, 0, &chunk, None, bsz),
            );
            omap_entries.push((oid, p));
            let pivot = chunk.first().map(|(k, _)| k.clone()).unwrap_or_default();
            parent_entries.push((pivot, oid));
        }
        let mut level: u16 = 1;
        loop {
            if level > 64 {
                return Err(TxnError::SpacemanParse(
                    "fstree rebuild: tree depth exceeded 64 levels".into(),
                ));
            }
            let lvl_recs: Vec<(Vec<u8>, Vec<u8>)> = parent_entries
                .iter()
                .map(|(k, oid)| (k.clone(), oid.to_le_bytes().to_vec()))
                .collect();
            if packed_size(&lvl_recs) <= node_capacity(bsz, true) {
                let rp = txn.alloc_block()?;
                let node_count = omap_entries.len() as u64 + 1;
                txn.stage_raw(
                    rp,
                    build_fstree_node(
                        root_tree_oid,
                        new_xid,
                        true,
                        level,
                        &lvl_recs,
                        Some(footer(node_count)),
                        bsz,
                    ),
                );
                omap_entries.push((root_tree_oid, rp));
                break;
            }
            let mut next: Vec<(Vec<u8>, u64)> = Vec::new();
            for chunk in partition_records(&lvl_recs, cap)? {
                let oid = txn.alloc_oid();
                let p = txn.alloc_block()?;
                txn.stage_raw(
                    p,
                    build_fstree_node(oid, new_xid, false, level, &chunk, None, bsz),
                );
                omap_entries.push((oid, p));
                let pivot = chunk.first().map(|(k, _)| k.clone()).unwrap_or_default();
                next.push((pivot, oid));
            }
            parent_entries = next;
            level += 1;
        }
    }

    // New nodes are stamped at new_xid. If a snapshot exists, carry forward the
    // prior omap entries (their older xids) so the snapshot can still resolve
    // its catalog versions: the old catalog blocks are NOT freed, so those
    // {oid, old_xid} -> paddr mappings stay valid. Without a snapshot we
    // collapse to the latest version (older mappings are obsolete).
    // [CERTAIN: empirical red test repro_overalloc_snapshot_pinned - the omap is
    //  keyed {oid,xid}; a snapshot at xid S resolves an oid via the largest
    //  xid <= S, so that entry must survive later live mutations.]
    // M7b-RT Subtask B (locked spec the APFS specification Q1):
    // omap retention bounded by the newest active snapshot xid. For each oid
    // we keep ONLY the entries that an active snapshot or the live view needs:
    //   - live: the new {oid, new_xid} just added above;
    //   - snapshot-pinned: prior entries with xid <= newest_snap (each active
    //     snapshot resolves via "largest xid <= its own xid").
    // Intermediate entries (newest_snap < xid < new_xid) are PRUNED here AND
    // their backing blocks are freed by `free_replaced_metadata`, which uses
    // the SAME boundary. Carrying intermediates forward while their blocks
    // are freed produces a stale paddr in the live omap → fsck overlap when
    // the allocator reuses the freed block (real-disk decoded post-Subtask-A:
    // T3 retained {root, xid=5} -> 2058 while 2058 was freed and re-allocated
    // for the container omap b-tree).
    let mut omap_triples: Vec<OmapRec> = omap_entries
        .iter()
        .map(|&(oid, p)| OmapRec::live(oid, new_xid, p, bsz))
        .collect();
    // Read EVERY old omap leaf mapping by walking the (possibly multi-node) old
    // omap tree - `parse_omap_entries` alone would see only the root node and so
    // miss every mapping below a split (#157). The omap node's o_oid (@8) is its
    // own physical paddr (BTREE_PHYSICAL). Used for snapshot retention AND the
    // node_delta accounting below.
    let old_omap_root_paddr = rd_u64(omap_node, 8);
    let (old_omap_triples, old_omap_nodes) =
        collect_omap_tree(txn, omap_node, old_omap_root_paddr, bsz)?;
    let has_snapshot = rd_u64(vsb_raw, VSBI_NUM_SNAPSHOTS) > 0;
    if has_snapshot {
        // Read om_most_recent_snap from the live volume omap header to set
        // the retention boundary (matches `free_replaced_metadata`'s rule).
        let vol_omap_paddr = rd_u64(vsb_raw, VSBI_OMAP_OID);
        let mut vom = vec![0u8; bsz];
        txn.read_block(vol_omap_paddr, &mut vom)?;
        let newest_snap = rd_u64(&vom, 64); // omap_phys.om_most_recent_snap
        let new_pairs: std::collections::HashSet<(u64, u64)> =
            omap_triples.iter().map(|r| (r.oid, r.xid)).collect();
        for r in &old_omap_triples {
            if !new_pairs.contains(&(r.oid, r.xid)) && r.xid <= newest_snap {
                omap_triples.push(*r);
            }
        }
    }

    // Build the new volume omap as a (possibly MULTI-NODE) b-tree (#157):
    // the single-node `build_omap_node` overflowed once the mappings no longer
    // fit one node. Returns the root paddr.
    let (new_omap_tree_paddr, new_omap_node_count) =
        build_omap_tree(txn, omap_node, &omap_triples, new_xid, bsz)?;

    // Net change in volume-owned catalog node count, for fs_alloc_count.
    //
    // When a snapshot is present the omap carries TWO categories of entry:
    //   A. Snapshot-pinned entries (xid <= newest_snap): retained verbatim
    //      in the new omap; their backing blocks are NOT freed and NOT newly
    //      allocated - they appear in both old and new omap at the same paddr.
    //   B. Intermediate / live entries (xid > newest_snap OR the whole node
    //      when no snapshot): replaced by the freshly-built new entries in
    //      `omap_entries`.
    //
    // node_delta must count only category-B so it correctly reflects the net
    // change in live catalog nodes.  Including category-A double-counts them
    // (old was retained, new was also allocated → apparent +N growth).
    // [#149 fix - empirical: paddr=270 xid=15 appeared in old_count on every
    //  commit even though it was never freed, causing a permanent under-count]
    // [the APFS specification Q5]
    let newest_snap_for_delta = if has_snapshot {
        let vol_omap_paddr = rd_u64(vsb_raw, VSBI_OMAP_OID);
        let mut vom = vec![0u8; bsz];
        txn.read_block(vol_omap_paddr, &mut vom)?;
        rd_u64(&vom, 64) // om_most_recent_snap
    } else {
        0
    };
    // Count only entries that are being replaced (not snapshot-pinned carries).
    // Walk the whole (possibly multi-node) old omap - counting only the root
    // node would undercount on a split omap and skew node_delta (#157).
    let old_replaceable_nodes = old_omap_triples
        .iter()
        .filter(|r| r.xid > newest_snap_for_delta)
        .count() as i64;
    let new_catalog_nodes = omap_entries.len() as i64;
    // The object map is rebuilt whole as well, and no snapshot reaches its
    // old nodes (snapshots share the live object map), so its node count
    // moves by the difference. It grows on the first write after a snapshot,
    // when every catalog node gains a second mapping; a delta taken as zero
    // left fs_alloc_count low by that growth.
    let omap_node_delta = new_omap_node_count as i64 - old_omap_nodes.len() as i64;
    let node_delta = new_catalog_nodes - old_replaceable_nodes + omap_node_delta;
    Ok(FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim: Reclaim::Whole,
    })
}

/// [`rewrite_fstree_impl`] on a volume without snapshots, rewriting only the
/// catalog nodes on the paths to the changed records and the object-map
/// nodes that map them.
///
/// The edits apply in the rebuild's order: the parent patch, then
/// `update_records`, then `remove_keys`, then `new_records`, so a key removed
/// and added again is replaced, and a new key already present is refused.
/// Whether this transaction changes the catalog path by path: unless it was
/// asked to rebuild in full. Under a snapshot as well: the snapshot keeps
/// the versions of the nodes it froze mapped (see `rewrite_catalog_batch`).
fn catalog_by_paths<D: WritableBlockDevice>(txn: &Transaction<D>, _vsb_raw: &[u8]) -> bool {
    !txn.full_rebuild()
}

#[allow(clippy::too_many_arguments)]
fn rewrite_fstree_paths<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_node: &[u8],
    new_xid: u64,
    new_records: Vec<(Vec<u8>, Vec<u8>)>,
    remove_keys: &[Vec<u8>],
    parent_patch: Option<(u64, i64)>,
    update_records: &[(Vec<u8>, Vec<u8>)],
    now: u64,
    bsz: usize,
) -> Result<FstreeRewrite, TxnError> {
    let mut batch = EditBatch::new();
    if let Some((pino, delta)) = parent_patch {
        batch.push(build_inode_key(pino), entries_changed(pino, delta, now))?;
    }
    for (k, v) in update_records {
        batch.push(k.clone(), EditOp::Update(v.clone()))?;
    }
    for k in remove_keys {
        batch.push(k.clone(), EditOp::Remove)?;
    }
    for (k, v) in new_records {
        batch.push(k, EditOp::Insert(v))?;
    }
    rewrite_catalog_batch(txn, vsb_raw, omap_node, new_xid, batch, bsz)
}

/// The change a directory's inode takes when its entries change: `delta`
/// more children, new modification and change times, and a stepped write
/// generation (the entries changed whether or not their count did).
fn entries_changed(pino: u64, delta: i64, now: u64) -> EditOp {
    EditOp::Patch(Box::new(move |v| {
        refuse_dir_stats(v, pino)?;
        let nch = rd_u32(v, INODE_NCHILDREN) as i64;
        wr_u32(v, INODE_NCHILDREN, (nch + delta).max(0) as u32);
        wr_u64(v, INODE_MOD_TIME, now);
        wr_u64(v, INODE_CHANGE_TIME, now);
        step_write_generation(v);
        Ok(())
    }))
}

/// Apply `batch` to the catalog, rewriting only the nodes on the paths to
/// the changed records and the object-map nodes that map them.
///
/// Under a snapshot the catalog nodes it froze are shared with it: a
/// rewritten node gains a mapping at this transaction and keeps every
/// mapping at or before the newest snapshot's, whose block stays the
/// snapshot's; only versions newer than the snapshot are replaced and their
/// blocks released (apfs.kext and linux-apfs-rw copy a node a snapshot
/// shares the same way, leaving its old version mapped).
fn rewrite_catalog_batch<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_node: &[u8],
    new_xid: u64,
    batch: EditBatch,
    bsz: usize,
) -> Result<FstreeRewrite, TxnError> {
    let mut tree = CowTree::new(
        Shape::Catalog {
            omap_root: omap_node,
        },
        new_xid,
        bsz,
    );
    tree.apply(txn, rd_u64(vsb_raw, VSBI_ROOT_TREE_OID), batch)?;
    let newest_snap = if rd_u64(vsb_raw, VSBI_NUM_SNAPSHOTS) > 0 {
        let mut vom = vec![0u8; bsz];
        txn.read_block(rd_u64(vsb_raw, VSBI_OMAP_OID), &mut vom)?;
        rd_u64(&vom, 64) // om_most_recent_snap
    } else {
        0
    };
    let old_omap_root_paddr = rd_u64(omap_node, 8);
    let remap = remap_catalog_nodes(
        txn,
        omap_node,
        old_omap_root_paddr,
        &tree.remapped,
        new_xid,
        newest_snap,
        bsz,
    )?;
    // The volume gains a block for each node version written and loses one
    // for each version released. Without a snapshot every rewritten node
    // releases its old version, and this is the tree's own growth; under
    // one, the versions the snapshot keeps stay counted.
    let written = tree.remapped.iter().filter(|(_, p)| p.is_some()).count() as i64;
    let released = remap.catalog_superseded.len() as i64;
    Ok(FstreeRewrite {
        omap_tree_paddr: remap.root,
        node_delta: written - released + remap.omap_nodes_added,
        reclaim: Reclaim::Exact {
            omap_nodes: remap.omap_superseded,
            catalog: remap.catalog_superseded,
        },
    })
}

/// A decoded (key, value) record pair from a catalog leaf node.
struct CatRecord {
    key: Vec<u8>,
    val: Vec<u8>,
}

/// Canonical catalog key ordering: obj_id asc, then obj_type asc, then for
/// DIR_REC by name_len_and_hash (numeric) then name bytes.
/// [the APFS specification, the APFS specification p.124]
fn cat_key_cmp(a: &[u8], b: &[u8]) -> core::cmp::Ordering {
    use core::cmp::Ordering;
    let ak = rd_u64(a, 0);
    let bk = rd_u64(b, 0);
    let (a_id, a_ty) = (ak & 0x0FFF_FFFF_FFFF_FFFF, ak >> 60);
    let (b_id, b_ty) = (bk & 0x0FFF_FFFF_FFFF_FFFF, bk >> 60);
    match a_id.cmp(&b_id) {
        Ordering::Equal => {}
        o => return o,
    }
    match a_ty.cmp(&b_ty) {
        Ordering::Equal => {}
        o => return o,
    }
    // Same (obj_id, type): the secondary key is TYPE-SPECIFIC. This mirrors
    // Apple's `apfs_key_compare` (kernelcache `_apfs_key_compare`, decompiled):
    // a flat byte comparison of the bytes after the jkey is WRONG for the
    // little-endian numeric subkeys (it compares LSB-first) and over-weights
    // name_len for DREC/XATTR. Each catalog j_obj type compares its own field.
    match a_ty {
        // DIR_REC: compare the 22-bit name HASH (name_len_and_hash >> 10), then
        // the name. Apple drops the low-10-bit name_len from the primary compare
        // (case 9: `hash = u32 >> 10`, tie -> apfs_cstrncmp(name)). The name
        // tiebreak is raw bytes here; for a case-insensitive volume Apple folds
        // case - only differs for two same-HASH names differing solely by case.
        APFS_TYPE_DIR_REC if a.len() >= 12 && b.len() >= 12 => {
            let ah = rd_u32(a, 8) >> 10;
            let bh = rd_u32(b, 8) >> 10;
            match ah.cmp(&bh) {
                Ordering::Equal => {}
                o => return o,
            }
            a.get(12..).unwrap_or(&[]).cmp(b.get(12..).unwrap_or(&[]))
        }
        // XATTR: jkey(8) + name_len(u16) + name. Apple compares the NAME
        // (case 4: apfs_cstrncmp(name@10)), NOT the name_len prefix.
        APFS_TYPE_XATTR if a.len() >= 10 && b.len() >= 10 => {
            a.get(10..).unwrap_or(&[]).cmp(b.get(10..).unwrap_or(&[]))
        }
        // FILE_EXTENT: jkey(8) + logical_addr(u64). Compare the address
        // NUMERICALLY (case 8), not as little-endian bytes.
        APFS_TYPE_FILE_EXTENT if a.len() >= 16 && b.len() >= 16 => rd_u64(a, 8).cmp(&rd_u64(b, 8)),
        // SIBLING_LINK: jkey(8) + sibling_id(u64). Numeric (case 5).
        APFS_TYPE_SIBLING_LINK if a.len() >= 16 && b.len() >= 16 => rd_u64(a, 8).cmp(&rd_u64(b, 8)),
        // INODE / DSTREAM_ID / DIR_STATS / SIBLING_MAP / CRYPTO: exactly one
        // record per (obj_id, type) - no secondary key, so equal here.
        _ => Ordering::Equal,
    }
}

/// Parse all (key, val) records out of a variable-kv leaf node.
///
/// Every geometry field is untrusted: the record count is checked against
/// what the node can physically hold before anything is reserved for it, and
/// every offset is bounds-checked, so a crafted node yields an error rather
/// than a multi-gigabyte allocation or an arithmetic panic.
fn parse_cat_leaf(node: &[u8]) -> Result<Vec<CatRecord>, &'static str> {
    const TOC_ENTRY: usize = 8;
    let bsz = node.len();
    if bsz < DATA_BASE + BTREE_INFO_SIZE {
        return Err("node shorter than its header");
    }
    let is_root = rd_u16(node, 32) & 0x1 != 0;
    let nkeys = rd_u32(node, 36) as usize;
    let toc_off = rd_u16(node, 40) as usize;
    let toc_len = rd_u16(node, 42) as usize;
    let toc_base = DATA_BASE + toc_off;
    let key_area = toc_base + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    // A table entry is 8 bytes, so the node cannot hold more records than
    // its table area has entries.
    if nkeys > toc_len / TOC_ENTRY || key_area > val_area_end {
        return Err("record count exceeds the node's table");
    }
    let mut out = Vec::with_capacity(nkeys);
    for i in 0..nkeys {
        let te = toc_base + i * TOC_ENTRY;
        let k_off = rd_u16(node, te) as usize;
        let k_len = rd_u16(node, te + 2) as usize;
        let v_off = rd_u16(node, te + 4) as usize;
        let v_len = rd_u16(node, te + 6) as usize;
        let ks = key_area + k_off;
        let vs = val_area_end.checked_sub(v_off).ok_or("val oob")?;
        let key = node.get(ks..ks + k_len).ok_or("key oob")?.to_vec();
        let val = node.get(vs..vs + v_len).ok_or("val oob")?.to_vec();
        out.push(CatRecord { key, val });
    }
    Ok(out)
}

/// Result of a successful [`verify_fsroot`] walk.
#[derive(Debug, Default, Clone, Copy)]
pub struct VerifyStats {
    /// Catalog b-tree nodes visited (root + internal + leaves).
    pub nodes: u64,
    /// Leaf records (catalog keys) counted.
    pub records: u64,
}

/// Windows-runnable structural self-check of the volume catalog (fsroot) tree.
///
/// `fsck_apfs` is macOS-only, so the production Windows path cannot run the real
/// arbiter. This is a focused mini-checker that walks the catalog b-tree through
/// the (possibly multi-node) volume omap and asserts exactly the invariants whose
/// violation produced the #156 real-USB corruption - so a host that enables it
/// after each committed transaction pinpoints the FIRST op that breaks the tree
/// (instead of only learning "corrupt" later, from macOS fsck, after many ops).
///
/// Checks, returning `Err(description)` on the first violation:
///   * every catalog node parses and is non-empty;
///   * keys are STRICTLY sorted (`cat_key_cmp`) within each node, no duplicates;
///   * every leaf key has a valid j_obj type (1..=12 - the #156 leaf showed a
///     bogus type, fsck "invalid key");
///   * the b-tree separator invariant: each child's minimum key equals the key
///     the parent stored for it (a stale separator = the #140 "leaf min changed"
///     class);
///   * every internal child OID resolves through the omap;
///   * the ROOT btree_info footer `bt_key_count` equals the walked leaf-record
///     count, and `bt_longest_key` / `bt_longest_val` are not understated
///     (the #156 root showed a `bt_key_count` drift).
///
/// Pure read-only walk over `txn`'s device (post-commit state). Cheap relative to
/// an I/O-bound mount; intended to be gated behind an opt-in flag on the host.
pub fn verify_fsroot<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    bsz: usize,
) -> Result<VerifyStats, String> {
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)
        .map_err(|e| format!("read omap root: {e}"))?;
    let root_paddr = omap_resolve(txn, &omap_node, root_tree_oid, u64::MAX, bsz)
        .map_err(|e| format!("resolve fsroot root through omap: {e}"))?
        .ok_or_else(|| "fsroot root_tree_oid not in volume omap".to_string())?;
    let mut root = vec![0u8; bsz];
    txn.read_block(root_paddr, &mut root)
        .map_err(|e| format!("read fsroot root: {e}"))?;

    // The ROOT node carries the btree_info footer.
    let bti = bsz - BTREE_INFO_SIZE;
    let foot_key_count = rd_u64(&root, bti + 24);
    let foot_longest_key = rd_u32(&root, bti + 16);
    let foot_longest_val = rd_u32(&root, bti + 20);

    let foot_node_count = rd_u64(&root, bti + 32);

    let mut stats = VerifyStats::default();
    let mut longest_key = 0u32;
    let mut longest_val = 0u32;
    verify_cat_node(
        txn,
        &omap_node,
        &root,
        NodeBounds::default(),
        bsz,
        &mut stats,
        &mut longest_key,
        &mut longest_val,
    )?;

    if foot_node_count != stats.nodes {
        return Err(format!(
            "fsroot bt_node_count footer={foot_node_count} != walked nodes={}",
            stats.nodes
        ));
    }
    if foot_key_count != stats.records {
        return Err(format!(
            "fsroot bt_key_count footer={foot_key_count} != walked leaf records={}",
            stats.records
        ));
    }
    if foot_longest_key < longest_key {
        return Err(format!(
            "fsroot bt_longest_key footer={foot_longest_key} < actual longest key={longest_key}"
        ));
    }
    if foot_longest_val < longest_val {
        return Err(format!(
            "fsroot bt_longest_val footer={foot_longest_val} < actual longest val={longest_val}"
        ));
    }
    Ok(stats)
}

/// Decode a catalog key for diagnostics: obj_id + j_obj type, plus the name for
/// a DIR_REC (`obj_id_and_type` u64, then `name_len_and_hash` u32, then name).
fn fmt_cat_key(k: &[u8]) -> String {
    if k.len() < 8 {
        return format!("<short key {} bytes>", k.len());
    }
    let w = rd_u64(k, 0);
    let oid = w & 0x0FFF_FFFF_FFFF_FFFF;
    let ty = w >> 60;
    if ty == APFS_TYPE_DIR_REC && k.len() >= 12 {
        let hash = rd_u32(k, 8);
        let name = String::from_utf8_lossy(k.get(12..).unwrap_or(&[]));
        format!(
            "(oid={oid}, type=9 DREC, name_len_and_hash={hash:#010x}, name={:?})",
            name.trim_end_matches('\0')
        )
    } else {
        format!("(oid={oid}, type={ty}, klen={})", k.len())
    }
}

/// Deeper than any catalog tree a volume holds; a chain past it is a cycle.
const VERIFY_MAX_DEPTH: u8 = 32;
/// Size of an index record's value in an unsealed catalog: the child's oid.
const CATALOG_INDEX_VAL_SIZE: usize = 8;

/// What a node's parent says about it. The root has no parent: no bounds,
/// any level, and it alone carries the root flag.
#[derive(Default, Clone, Copy)]
struct NodeBounds<'a> {
    /// The separator the parent stored for this node: its first key.
    min_key: Option<&'a [u8]>,
    /// The separator of the parent's next child: every key here is below it.
    below_key: Option<&'a [u8]>,
    /// One level below the parent's.
    level: Option<u16>,
    /// Steps from the root.
    depth: u8,
}

/// Recursive worker for [`verify_fsroot`]. Validates one catalog node and (for an
/// internal node) descends into every child, threading the bounds its parent
/// sets and the running longest-key/val + node/record counters.
#[allow(clippy::too_many_arguments)]
fn verify_cat_node<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    node: &[u8],
    bounds: NodeBounds<'_>,
    bsz: usize,
    stats: &mut VerifyStats,
    longest_key: &mut u32,
    longest_val: &mut u32,
) -> Result<(), String> {
    let level = rd_u16(node, 34);
    let flags = rd_u16(node, 32);
    if bounds.depth > VERIFY_MAX_DEPTH {
        return Err(format!(
            "catalog tree deeper than {VERIFY_MAX_DEPTH} levels (a cycle)"
        ));
    }
    if let Some(want) = bounds.level {
        if level != want {
            return Err(format!(
                "catalog child at level {level}, its parent wants {want}"
            ));
        }
    }
    let is_root = bounds.level.is_none();
    if (flags & BTNODE_ROOT != 0) != is_root {
        return Err(format!(
            "catalog node root flag {} on a {} node (level {level})",
            flags & BTNODE_ROOT != 0,
            if is_root { "root" } else { "child" }
        ));
    }
    if (flags & BTNODE_LEAF != 0) != (level == 0) {
        return Err(format!(
            "catalog node leaf flag {} at level {level}",
            flags & BTNODE_LEAF != 0
        ));
    }
    stats.nodes += 1;
    let recs =
        parse_cat_leaf(node).map_err(|e| format!("parse catalog node (level {level}): {e}"))?;
    if recs.is_empty() {
        return Err(format!("empty catalog node (level {level})"));
    }
    // Separator invariant: this node's minimum key must equal the key the parent
    // stored as the separator for it. (The root has no parent → skip.)
    if let Some(min) = bounds.min_key {
        if recs[0].key.as_slice() != min {
            return Err(format!(
                "catalog node min key != parent separator (level {level})"
            ));
        }
    }
    // Every key sits below the separator of the parent's next child.
    if let (Some(below), Some(last)) = (bounds.below_key, recs.last()) {
        if cat_key_cmp(&last.key, below) != core::cmp::Ordering::Less {
            return Err(format!(
                "catalog node last key {} reaches the next separator {} (level {level})",
                fmt_cat_key(&last.key),
                fmt_cat_key(below)
            ));
        }
    }
    // Strictly sorted, no duplicate keys.
    for w in recs.windows(2) {
        match cat_key_cmp(&w[0].key, &w[1].key) {
            core::cmp::Ordering::Less => {}
            core::cmp::Ordering::Equal => {
                return Err(format!(
                    "duplicate catalog key (level {level}): {}",
                    fmt_cat_key(&w[0].key)
                ));
            }
            core::cmp::Ordering::Greater => {
                if std::env::var("APFS_VERIFY_DUMP_LEAF").as_deref() == Ok("1") {
                    let nkeys = rd_u32(node, 36);
                    let toc_off = rd_u16(node, 40) as usize;
                    let toc_len = rd_u16(node, 42);
                    let fs_off = rd_u16(node, 44);
                    let fs_len = rd_u16(node, 46);
                    let key_area = DATA_BASE + toc_off + toc_len as usize;
                    eprintln!(
                        "--- offending leaf oid={} nkeys={nkeys} toc_off={toc_off} toc_len={toc_len} free_off={fs_off} free_len={fs_len} key_area={key_area} ---",
                        rd_u64(node, 8)
                    );
                    // Raw TOC per entry - reveals whether a duplicate is a distinct
                    // packed record or a stale TOC entry pointing into free space.
                    for i in 0..(nkeys as usize) {
                        let te = DATA_BASE + toc_off + i * 8;
                        let k_off = rd_u16(node, te);
                        let k_len = rd_u16(node, te + 2);
                        let v_off = rd_u16(node, te + 4);
                        let v_len = rd_u16(node, te + 6);
                        let kabs = key_area + k_off as usize;
                        let key = node.get(kabs..kabs + k_len as usize).unwrap_or(&[]);
                        eprintln!(
                            "  [{i}] k_off={k_off} k_len={k_len} v_off={v_off} v_len={v_len} key={}",
                            fmt_cat_key(key)
                        );
                    }
                }
                return Err(format!(
                    "catalog keys out of order (level {level}): {} then {}",
                    fmt_cat_key(&w[0].key),
                    fmt_cat_key(&w[1].key)
                ));
            }
        }
    }
    if level == 0 {
        for r in &recs {
            if r.key.len() < 8 {
                return Err("catalog leaf key shorter than 8 bytes".to_string());
            }
            let kw = rd_u64(&r.key, 0);
            let ty = kw >> 60;
            // Valid j_obj types are 1..=12; a catalog leaf key outside that range
            // is the malformed-key class fsck reports as "invalid key".
            if ty == 0 || ty > 12 {
                let obj_id = kw & 0x0FFF_FFFF_FFFF_FFFF;
                return Err(format!("invalid catalog key type {ty} (obj_id {obj_id})"));
            }
            *longest_key = (*longest_key).max(r.key.len() as u32);
            *longest_val = (*longest_val).max(r.val.len() as u32);
            stats.records += 1;
        }
    } else {
        for (i, r) in recs.iter().enumerate() {
            if r.val.len() != CATALOG_INDEX_VAL_SIZE {
                return Err(format!(
                    "internal catalog value of {} bytes, not {CATALOG_INDEX_VAL_SIZE} (level {level})",
                    r.val.len()
                ));
            }
            let child_oid = rd_u64(&r.val, 0);
            let child_paddr = omap_resolve(txn, omap_node, child_oid, u64::MAX, bsz)
                .map_err(|e| format!("resolve child oid {child_oid}: {e}"))?
                .ok_or_else(|| format!("child oid {child_oid} not in volume omap"))?;
            let mut child = vec![0u8; bsz];
            txn.read_block(child_paddr, &mut child)
                .map_err(|e| format!("read child node paddr {child_paddr}: {e}"))?;
            let next = recs
                .get(i + 1)
                .map(|n| n.key.as_slice())
                .or(bounds.below_key);
            verify_cat_node(
                txn,
                omap_node,
                &child,
                NodeBounds {
                    min_key: Some(&r.key),
                    below_key: next,
                    level: Some(level.saturating_sub(1)),
                    depth: bounds.depth.saturating_add(1),
                },
                bsz,
                stats,
                longest_key,
                longest_val,
            )?;
        }
    }
    Ok(())
}

/// Insert `new_records` into a variable-kv catalog leaf node and return the
/// rebuilt node (obj_phys header copied verbatim; caller fixes oid/xid/Fletcher).
/// Re-packs all records sorted by `cat_key_cmp`; TOC tightly packed
/// (nkeys * 8). [the APFS specification]
pub fn insert_catalog_records(
    node: &[u8],
    new_records: Vec<(Vec<u8>, Vec<u8>)>,
) -> Result<Vec<u8>, &'static str> {
    let mut recs = parse_cat_leaf(node)?;
    for (key, val) in new_records {
        recs.push(CatRecord { key, val });
    }
    recs.sort_by(|a, b| cat_key_cmp(&a.key, &b.key));
    pack_cat_leaf(node, recs)
}

/// Remove records whose key exactly matches one of `keys` from a variable-kv
/// catalog leaf and return the rebuilt node. [the APFS specification]
pub fn remove_catalog_records(node: &[u8], keys: &[Vec<u8>]) -> Result<Vec<u8>, &'static str> {
    let recs: Vec<CatRecord> = parse_cat_leaf(node)?
        .into_iter()
        .filter(|r| !keys.contains(&r.key))
        .collect();
    // Records remain in sorted order after filtering.
    pack_cat_leaf(node, recs)
}

/// Replace the value of an existing record in-place. If `key` is not present
/// the record is inserted. Returns the rebuilt node.
/// Used by clone_file to update `phys_ext` refcounts in the extentref tree.
pub fn upsert_catalog_record(
    node: &[u8],
    key: Vec<u8>,
    val: Vec<u8>,
) -> Result<Vec<u8>, &'static str> {
    let mut recs = parse_cat_leaf(node)?;
    match recs.iter_mut().find(|r| r.key == key) {
        Some(r) => r.val = val,
        None => recs.push(CatRecord { key, val }),
    }
    recs.sort_by(|a, b| cat_key_cmp(&a.key, &b.key));
    pack_cat_leaf(node, recs)
}

/// Re-pack a variable-kv leaf from `recs` (assumed already sorted), copying the
/// obj_phys header + btree_info_t footer from `node` verbatim.
fn pack_cat_leaf(node: &[u8], recs: Vec<CatRecord>) -> Result<Vec<u8>, &'static str> {
    let bsz = node.len();
    let nkeys = recs.len();
    let footer_at = bsz
        .checked_sub(BTREE_INFO_SIZE)
        .filter(|&f| f >= DATA_BASE)
        .ok_or("short node")?;

    let mut out = vec![0u8; bsz];
    // Copy obj_phys header + node fields verbatim, then overwrite layout fields.
    wr_bytes(&mut out, 0, node.get(..DATA_BASE).ok_or("short node")?);
    // btree_info_t footer copied verbatim (root node); we patch longest_*.
    wr_bytes(
        &mut out,
        footer_at,
        node.get(footer_at..).ok_or("short footer")?,
    );

    // TOC capacity: whole steps of entries (see TOC_STEP), one step for an
    // EMPTY node - the kernel's canonical empty b-tree root reserves 64 bytes
    // (8 kvloc slots), and a node packed with table_space.len == 0 is
    // rejected by fsck_apfs ("tree is invalid"). An exact table only when the
    // records leave no room for the rounding.
    // [CERTAIN: empirical - kernel fresh-format empty extentref node carries
    //  table_space(off=0,len=64), free_space(off=0,len=3936)]
    let kv_bytes: usize = recs.iter().map(|r| r.key.len() + r.val.len()).sum();
    let toc_cap = if DATA_BASE + fstree_toc_len(nkeys) + kv_bytes <= footer_at {
        fstree_toc_len(nkeys)
    } else {
        nkeys.checked_mul(8).ok_or("node overflow")?
    };
    let key_area = DATA_BASE.checked_add(toc_cap).ok_or("node overflow")?;
    let val_area_end = footer_at; // root node
    if key_area > val_area_end {
        return Err("node overflow");
    }

    let mut key_cursor = key_area;
    let mut val_cursor = val_area_end;
    let mut longest_k = 0u32;
    let mut longest_v = 0u32;
    for (i, r) in recs.iter().enumerate() {
        // Room for this record is checked before anything is written: the
        // keys grow up and the values down, and they must not cross.
        let key_end = key_cursor.checked_add(r.key.len()).ok_or("node overflow")?;
        let val_start = val_cursor.checked_sub(r.val.len()).ok_or("node overflow")?;
        if key_end > val_start {
            return Err("node overflow");
        }
        let k_off = key_cursor - key_area;
        wr_bytes(&mut out, key_cursor, &r.key);
        key_cursor = key_end;
        val_cursor = val_start;
        wr_bytes(&mut out, val_cursor, &r.val);
        let v_off = val_area_end - val_cursor;
        let te = DATA_BASE + i * 8;
        wr_u16(&mut out, te, k_off as u16);
        wr_u16(&mut out, te + 2, r.key.len() as u16);
        wr_u16(&mut out, te + 4, v_off as u16);
        wr_u16(&mut out, te + 6, r.val.len() as u16);
        longest_k = longest_k.max(r.key.len() as u32);
        longest_v = longest_v.max(r.val.len() as u32);
    }
    // Node header layout fields.
    wr_u32(&mut out, 36, nkeys as u32); // btn_nkeys
    wr_u16(&mut out, 40, 0); // table_space.off
    wr_u16(&mut out, 42, toc_cap as u16); // table_space.len
    let free_off = key_cursor - key_area;
    let free_len = val_cursor - key_cursor;
    wr_u16(&mut out, 44, free_off as u16); // free_space.off
    wr_u16(&mut out, 46, free_len as u16); // free_space.len
    wr_u16(&mut out, 48, BTOFF_INVALID); // key_free_list.off
    wr_u16(&mut out, 50, 0);
    wr_u16(&mut out, 52, BTOFF_INVALID); // val_free_list.off
    wr_u16(&mut out, 54, 0);
    // Footer longest_key/longest_val (variable-kv → ksz/vsz already 0).
    let bti = footer_at;
    wr_u32(&mut out, bti + 16, longest_k);
    wr_u32(&mut out, bti + 20, longest_v);
    wr_u64(&mut out, bti + 24, nkeys as u64); // key_count
    Ok(out)
}

// ---------------------------------------------------------------------------
// Volume superblock field offsets - volname area.
// apfs_superblock_t.apfs_volname is at absolute struct offset 0x2C0 = 704.
// volname is a 256-byte UTF-8 NUL-terminated array. [linux-apfs-rw:apfs_raw.h]
// ---------------------------------------------------------------------------
const VSBI_VOLNAME: usize = 0x2C0; // 704
const APFS_VOLNAME_LEN: usize = 256;

/// Update the volume label stored in the volume superblock.
///
/// Zeroes the 256-byte `apfs_volname` field, copies `new_label` (UTF-8, at
/// most 255 bytes), NUL-pads the rest, then stages the updated VSB. Returns
/// `TxnError::SpacemanParse` if `new_label` is longer than 255 bytes.
///
/// The caller is responsible for reading a fresh `vsb_raw` before this call
/// (same as every other write API) and for `txn.commit()` afterwards.
pub fn set_volume_label<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    new_label: &str,
) -> Result<(), TxnError> {
    let vsb_now = txn.current_vsb(vsb_raw)?;
    let vsb_raw = vsb_now.as_slice();
    ensure_volume_mutable(vsb_raw)?;
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();

    let label_bytes = new_label.as_bytes();
    if label_bytes.len() >= APFS_VOLNAME_LEN {
        return Err(TxnError::SpacemanParse(format!(
            "set_volume_label: label too long ({} bytes, max {})",
            label_bytes.len(),
            APFS_VOLNAME_LEN - 1
        )));
    }

    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);

    // Zero the volname field, then copy the new label (NUL-terminated by zeroes).
    if let Some(s) = new_vsb.get_mut(VSBI_VOLNAME..VSBI_VOLNAME + APFS_VOLNAME_LEN) {
        s.fill(0);
        s[..label_bytes.len()].copy_from_slice(label_bytes);
    }
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    // Bump transaction id in VSB header (offset 16).
    wr_u64(&mut new_vsb, 16, new_xid);

    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// move_entry - cross-directory rename (single transaction COW).
// ---------------------------------------------------------------------------

/// Move `old_name` from directory `old_parent_ino` to `new_parent_ino` with
/// `new_name`. The file's inode number, data extents, and DSTREAM_ID records
/// are preserved - only the catalog DREC + the inode's parent_id + NAME xfield
/// are updated.
///
/// Decrements `old_parent_ino.nchildren` by 1, increments
/// `new_parent_ino.nchildren` by 1. When `old_parent_ino == new_parent_ino`
/// this is identical to a same-directory rename (nchildren net zero).
///
/// [the APFS specification 1.3]
#[allow(clippy::too_many_arguments)]
pub fn move_entry<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    old_parent_ino: u64,
    old_name: &str,
    new_parent_ino: u64,
    new_name: &str,
    replace_if_exists: bool,
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    validate_entry_name(new_name)?;
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let old_drec_key = build_drec_key(old_parent_ino, old_name, case_fold, normalize);
    let new_drec_key = build_drec_key(new_parent_ino, new_name, case_fold, normalize);
    let all = gather_entries(
        txn,
        &omap_node,
        root_tree_oid,
        &[&old_drec_key, &new_drec_key],
        bsz,
    )?;

    // Locate old DREC -> file_id, date_added, dt_type.
    let drec_val = all
        .iter()
        .find(|(k, _)| *k == old_drec_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::SpacemanParse("move_entry: old name not found".into()))?;
    let file_id = rd_u64(&drec_val, 0);
    let date_added = rd_u64(&drec_val, 8);
    let dt_type = rd_u16(&drec_val, 16);

    // Locate existing inode value.
    let inode_key = build_inode_key(file_id);
    let old_inode_val = all
        .iter()
        .find(|(k, _)| *k == inode_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::SpacemanParse("move_entry: inode not found".into()))?;

    // POSIX EINVAL: refuse moving a directory into itself or one of its
    // descendants. Matches Apple's `check_parent_chain_internal` guard in
    // `apfs_vnop_renamex`. Walk the new parent's ancestor chain through the
    // catalog inode `parent_id` field (offset 0); abort if we encounter the
    // moving inode. A small depth cap guards against pathological loops in
    // a corrupt catalog (real chains are <128 deep).
    if old_parent_ino != new_parent_ino {
        let is_dir = (rd_u16(&old_inode_val, 80) & 0o170000) == S_IFDIR;
        if is_dir {
            let mut cur = new_parent_ino;
            for _ in 0..1024 {
                if cur == file_id {
                    return Err(TxnError::InvalidArgument(format!(
                        "cannot move directory ino {file_id} into its own descendant {new_parent_ino}"
                    )));
                }
                // Root reached (APFS volume root inode = 2 by convention).
                if cur <= 2 {
                    break;
                }
                match inode_value(txn, &omap_node, root_tree_oid, cur, &all, bsz)? {
                    Some(v) => cur = rd_u64(&v, 0),
                    None => break,
                }
            }
        }
    }

    // A name that is one of several carries the sibling id binding it to the
    // inode, and the record behind that id names both the name and the
    // directory holding it - a move changes both.
    let sibling_id = drec_sibling_id(&drec_val);
    let moves_primary = match sibling_id {
        None => true,
        Some(sid) => {
            let lowest = all
                .iter()
                .filter(|(k, _)| {
                    rd_u64(k, 0) >> 60 == APFS_TYPE_SIBLING_LINK
                        && rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF == file_id
                })
                .map(|(k, _)| rd_u64(k, 8))
                .min();
            lowest == Some(sid)
        }
    };

    // Rebuild inode: update parent_id + NAME xfield, bump mtime + ctime. The
    // inode describes the primary link, so moving any other name leaves both
    // alone and only the timestamps move.
    let new_inode_val = if moves_primary {
        let mut v = rebuild_inode_with_name(&old_inode_val, new_name, now);
        // Overwrite parent_id (offset 0 in j_inode_val fixed prefix).
        wr_u64(&mut v, 0, new_parent_ino);
        v
    } else {
        let mut v = old_inode_val.clone();
        wr_u64(&mut v, INODE_CHANGE_TIME, now);
        v
    };
    // Only the change time moves: the file's content is untouched by a move,
    // and macOS leaves its modification time alone (measured in
    // `tests/rename_times.rs`); the directories' times move below.

    // New DREC under new_parent_ino with new_name (preserve date_added + dt_type).
    let new_drec = (
        new_drec_key.clone(),
        match sibling_id {
            Some(sid) => build_drec_val_with_sibling(file_id, date_added, dt_type, sid),
            None => build_drec_val(file_id, date_added, dt_type),
        },
    );

    // Detect an existing target with `new_name` under `new_parent_ino`. If
    // present and `replace_if_exists`, collect its records + owned blocks
    // for the same transaction; otherwise return AlreadyExists.
    let target_replace = collect_replace_target(
        &all,
        &new_drec_key,
        file_id,
        replace_if_exists,
        bsz,
        case_fold,
        normalize,
    )?;

    // Free target's data blocks (if any are owned by the live extref tree).
    let target_data_blocks: Vec<(u64, u64)> = target_replace
        .as_ref()
        .map(|tr| tr.data_blocks.clone())
        .unwrap_or_default();
    let (new_extref_paddr, extref_old_padrs, extref_node_delta, freed_count) =
        free_blocks_owned_by_live_extref(txn, vsb_raw, new_xid, &target_data_blocks, bsz)?;

    // Decide parent nchildren patches. rewrite_fstree accepts a single
    // (parent_ino, delta); for cross-dir we apply two patches inline before
    // calling rewrite_fstree with None (no further patch).
    let mut remove_keys = vec![old_drec_key, inode_key.clone()];
    if let Some(ref tr) = target_replace {
        remove_keys.extend(tr.remove_keys.iter().cloned());
    }
    let new_records = vec![new_drec, (inode_key, new_inode_val)];

    // Apply nchildren deltas to both parents in one pass. Deltas depend on
    // whether a target was replaced and whether the move is same-parent or
    // cross-parent:
    //
    //   cross-parent, no replace: old -1, new +1  (one entry moves between dirs)
    //   cross-parent, with replace: old -1, new  0  (target slot filled by src)
    //   same-parent,  no replace: same  0          (in-place rename)
    //   same-parent,  with replace: same -1        (two entries collapse into one)
    let same_parent = old_parent_ino == new_parent_ino;
    let replacing = target_replace.is_some();
    let old_parent_delta: i64 = if same_parent {
        if replacing {
            -1
        } else {
            0
        }
    } else {
        -1
    };
    let new_parent_delta: i64 = if same_parent || replacing { 0 } else { 1 };

    let (new_omap_tree_paddr, node_delta, reclaim) = if catalog_by_paths(txn, vsb_raw) {
        // The rebuild below in edit form, in the same order: the parents,
        // the sibling link, what replacing leaves changed, the removals, then
        // the new records (refused if already present).
        let mut batch = EditBatch::new();
        batch.push(
            build_inode_key(old_parent_ino),
            entries_changed(old_parent_ino, old_parent_delta, now),
        )?;
        if !same_parent {
            batch.push(
                build_inode_key(new_parent_ino),
                entries_changed(new_parent_ino, new_parent_delta, now),
            )?;
        }
        if let Some(sid) = sibling_id {
            batch.push(
                build_sibling_link_key(file_id, sid),
                EditOp::Update(build_sibling_link_val(new_parent_ino, new_name)),
            )?;
        }
        if let Some(ref tr) = target_replace {
            for (k, v) in &tr.updates {
                batch.push(k.clone(), EditOp::Update(v.clone()))?;
            }
        }
        for k in &remove_keys {
            batch.push(k.clone(), EditOp::Remove)?;
        }
        for (k, v) in new_records {
            batch.push(k, EditOp::Insert(v))?;
        }
        let rw = rewrite_catalog_batch(txn, vsb_raw, &omap_node, new_xid, batch, bsz)?;
        (rw.omap_tree_paddr, rw.node_delta, rw.reclaim)
    } else {
        let root_oid = root_tree_oid;
        let mut recs = collect_fstree(txn, &omap_node, root_oid, bsz)?;

        for (k, v) in recs.iter_mut() {
            let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
            let ty = rd_u64(k, 0) >> 60;
            if ty != APFS_TYPE_INODE {
                continue;
            }
            if oid == old_parent_ino {
                refuse_dir_stats(v, oid)?;
                if old_parent_delta != 0 {
                    let nch = rd_u32(v, INODE_NCHILDREN) as i64;
                    wr_u32(v, INODE_NCHILDREN, (nch + old_parent_delta).max(0) as u32);
                }
                // Always bump mtime/ctime on old parent: its entries changed.
                wr_u64(v, INODE_MOD_TIME, now);
                wr_u64(v, INODE_CHANGE_TIME, now);
                step_write_generation(v);
            }
            if oid == new_parent_ino && !same_parent {
                refuse_dir_stats(v, oid)?;
                if new_parent_delta != 0 {
                    let nch = rd_u32(v, INODE_NCHILDREN) as i64;
                    wr_u32(v, INODE_NCHILDREN, (nch + new_parent_delta).max(0) as u32);
                }
                wr_u64(v, INODE_MOD_TIME, now);
                wr_u64(v, INODE_CHANGE_TIME, now);
                step_write_generation(v);
            }
        }

        // Point the moved name's sibling-link record at where it now lives.
        if let Some(sid) = sibling_id {
            let key = build_sibling_link_key(file_id, sid);
            let val = build_sibling_link_val(new_parent_ino, new_name);
            if let Some(slot) = recs.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = val;
            }
        }
        // What replacing the target leaves changed rather than removed: a
        // shared stream's count, a hard-linked inode re-pointed at its next
        // name.
        if let Some(ref tr) = target_replace {
            for (k, v) in &tr.updates {
                if let Some(slot) = recs.iter_mut().find(|(rk, _)| rk == k) {
                    slot.1 = v.clone();
                }
            }
        }

        // Remove old entries.
        recs.retain(|(k, _)| !remove_keys.contains(k));

        // Reject duplicates.
        {
            let existing: std::collections::HashSet<&[u8]> =
                recs.iter().map(|(k, _)| k.as_slice()).collect();
            for (k, _) in &new_records {
                if existing.contains(k.as_slice()) {
                    return Err(TxnError::AlreadyExists(
                        "move_entry: target name already exists".to_string(),
                    ));
                }
            }
        }

        recs.extend(new_records);
        recs.sort_by(|a, b| cat_key_cmp(&a.0, &b.0));

        let longest_k = recs.iter().map(|(k, _)| k.len()).max().unwrap_or(0) as u32;
        let longest_v = recs.iter().map(|(_, v)| v.len()).max().unwrap_or(0) as u32;
        let key_count = recs.len() as u64;
        let footer = |node_count: u64| FstreeFooter {
            longest_key: longest_k,
            longest_val: longest_v,
            key_count,
            node_count,
        };

        let mut omap_entries: Vec<(u64, u64)> = Vec::new();
        if packed_size(&recs) <= node_capacity(bsz, true) {
            let p = txn.alloc_block()?;
            let node = build_fstree_node(root_oid, new_xid, true, 0, &recs, Some(footer(1)), bsz);
            txn.stage_raw(p, node);
            omap_entries.push((root_oid, p));
        } else {
            let cap = node_capacity(bsz, false);
            let mut parent_entries: Vec<(Vec<u8>, u64)> = Vec::new();
            for chunk in partition_records(&recs, cap)? {
                let oid = txn.alloc_oid();
                let p = txn.alloc_block()?;
                txn.stage_raw(
                    p,
                    build_fstree_node(oid, new_xid, false, 0, &chunk, None, bsz),
                );
                omap_entries.push((oid, p));
                let pivot = chunk.first().map(|(k, _)| k.clone()).unwrap_or_default();
                parent_entries.push((pivot, oid));
            }
            let mut level: u16 = 1;
            loop {
                if level > 64 {
                    return Err(TxnError::SpacemanParse(
                        "fstree rebuild (snap): tree depth exceeded 64 levels".into(),
                    ));
                }
                let lvl_recs: Vec<(Vec<u8>, Vec<u8>)> = parent_entries
                    .iter()
                    .map(|(k, oid)| (k.clone(), oid.to_le_bytes().to_vec()))
                    .collect();
                if packed_size(&lvl_recs) <= node_capacity(bsz, true) {
                    let rp = txn.alloc_block()?;
                    let node_count = omap_entries.len() as u64 + 1;
                    txn.stage_raw(
                        rp,
                        build_fstree_node(
                            root_oid,
                            new_xid,
                            true,
                            level,
                            &lvl_recs,
                            Some(footer(node_count)),
                            bsz,
                        ),
                    );
                    omap_entries.push((root_oid, rp));
                    break;
                }
                let mut next: Vec<(Vec<u8>, u64)> = Vec::new();
                for chunk in partition_records(&lvl_recs, cap)? {
                    let oid = txn.alloc_oid();
                    let p = txn.alloc_block()?;
                    txn.stage_raw(
                        p,
                        build_fstree_node(oid, new_xid, false, level, &chunk, None, bsz),
                    );
                    omap_entries.push((oid, p));
                    let pivot = chunk.first().map(|(k, _)| k.clone()).unwrap_or_default();
                    next.push((pivot, oid));
                }
                parent_entries = next;
                level += 1;
            }
        }

        // Walk the WHOLE old omap. Reading just the root misses every mapping
        // once the omap has split, and misreads an index root as a leaf.
        let old_omap_triples = collect_omap_triples(txn, &omap_node, omap_tree_paddr, bsz)?;

        let mut omap_triples: Vec<OmapRec> = omap_entries
            .iter()
            .map(|&(oid, p)| OmapRec::live(oid, new_xid, p, bsz))
            .collect();
        let has_snapshot = rd_u64(vsb_raw, VSBI_NUM_SNAPSHOTS) > 0;
        let newest_snap = if has_snapshot {
            let vol_omap_paddr = rd_u64(vsb_raw, VSBI_OMAP_OID);
            let mut vom = vec![0u8; bsz];
            txn.read_block(vol_omap_paddr, &mut vom)?;
            rd_u64(&vom, 64) // om_most_recent_snap
        } else {
            0
        };
        if has_snapshot {
            let new_pairs: std::collections::HashSet<(u64, u64)> =
                omap_triples.iter().map(|r| (r.oid, r.xid)).collect();
            for r in &old_omap_triples {
                if !new_pairs.contains(&(r.oid, r.xid)) && r.xid <= newest_snap {
                    omap_triples.push(*r);
                }
            }
        }

        // Build a possibly multi-node omap. The single-node builder writes one
        // node whatever it is given: past its capacity it ran off the end of the
        // value area (a subtraction overflow in debug, a wrapped offset and a
        // corrupt node in release) and, on an already-split omap, replaced the
        // whole tree with a single node that orphaned every real leaf.
        let (new_omap_tree_paddr, _new_omap_node_count) =
            build_omap_tree(txn, &omap_node, &omap_triples, new_xid, bsz)?;

        // Count only the entries actually being replaced. Snapshot-pinned
        // carries appear in both the old and the new omap at the same paddr, so
        // counting them here would report growth that never happened.
        let old_replaceable_nodes = old_omap_triples
            .iter()
            .filter(|r| r.xid > newest_snap)
            .count() as i64;
        let new_catalog_nodes = omap_entries.len() as i64;
        let node_delta = new_catalog_nodes - old_replaceable_nodes;
        (new_omap_tree_paddr, node_delta, Reclaim::Whole)
    };

    // COW the volume omap header.
    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    // Stage the updated (virtual) volume superblock. When a target was
    // replaced we also patch the extentref tree pointer (if any blocks
    // were freed against the live tree) and decrement num_files for the
    // removed target inode.
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    // Reclaim COW-replaced metadata. extref_old_padrs is empty when no blocks freed. [#151]
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &extref_old_padrs,
        &reclaim,
        bsz,
    )?;

    if let Some(ref tr) = target_replace {
        uncount_replaced(&mut new_vsb, tr);
    }
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let alloc_delta = -(freed_count as i64) + node_delta + extref_node_delta + frm_correction;
    wr_u64(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (fs_alloc + alloc_delta).max(0) as u64,
    );
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// set_inode_attrs - persist mtime, ctime, atime, btime and mode bits.
// ---------------------------------------------------------------------------

/// Update selected inode attributes in a single COW transaction.
///
/// Locates the inode for `name` under `parent_ino`, applies the requested
/// field changes, and rewrites the catalog (inode key removed + new value
/// inserted; no parent nchildren change). Only `Some` fields are touched.
///
/// `mode_set` bits are OR'd in; `mode_clear` bits are cleared first. Both
/// may be `Some` simultaneously: result = `(old_mode & !mode_clear) | mode_set`.
///
/// Timestamps are APFS nanoseconds since UNIX epoch.
///
/// [the APFS specification 1.4]
#[allow(clippy::too_many_arguments)]
pub fn set_inode_attrs<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
    mode_set: Option<u16>,
    mode_clear: Option<u16>,
    mtime_ns: Option<u64>,
    ctime_ns: Option<u64>,
    atime_ns: Option<u64>,
    btime_ns: Option<u64>,
    bsd_flags_set: Option<u32>,
    bsd_flags_clear: Option<u32>,
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, vol_omap_raw)?;
    let (vsb_raw, vol_omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
    let all = gather_entries(txn, &omap_node, root_tree_oid, &[&drec_key], bsz)?;

    // Locate DREC -> file_id.
    let file_id = all
        .iter()
        .find(|(k, _)| *k == drec_key)
        .map(|(_, v)| rd_u64(v, 0))
        .ok_or_else(|| TxnError::NotFound(format!("set_inode_attrs: '{name}' not found")))?;

    // Locate existing inode value.
    let inode_key = build_inode_key(file_id);
    let old_inode_val = all
        .iter()
        .find(|(k, _)| *k == inode_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::NotFound(format!("set_inode_attrs: inode {file_id} not found")))?;

    // Clone and apply patches to the fixed 92-byte prefix.
    let mut new_inode_val = old_inode_val.clone();

    // j_inode_val fixed field offsets (empirical, confirmed by InodeValArgs):
    //   0: parent_id, 8: private_id, 16: create_time (btime), 24: mod_time,
    //  32: change_time, 40: access_time, 56: nchildren_or_nlink, 80: mode
    const INODE_MODE: usize = 80;

    if let Some(t) = btime_ns {
        wr_u64(&mut new_inode_val, INODE_CREATE_TIME, t);
    }
    if let Some(t) = mtime_ns {
        wr_u64(&mut new_inode_val, INODE_MOD_TIME, t);
        // A modification time set earlier than the birth time pulls the
        // birth time back to it (measured on macOS with `touch -t` in
        // `tests/link_clone_times.rs`).
        if t < rd_u64(&new_inode_val, INODE_CREATE_TIME) {
            wr_u64(&mut new_inode_val, INODE_CREATE_TIME, t);
        }
    }
    if let Some(t) = ctime_ns {
        wr_u64(&mut new_inode_val, INODE_CHANGE_TIME, t);
    } else {
        // Always bump ctime when any attr changes (POSIX requirement).
        wr_u64(&mut new_inode_val, INODE_CHANGE_TIME, now);
    }
    if let Some(t) = atime_ns {
        wr_u64(&mut new_inode_val, INODE_ACCESS_TIME, t);
    }
    if mode_set.is_some() || mode_clear.is_some() {
        let old_mode = rd_u16(&old_inode_val, INODE_MODE);
        let cleared = old_mode & !mode_clear.unwrap_or(0);
        let new_mode = cleared | mode_set.unwrap_or(0);
        wr_u16(&mut new_inode_val, INODE_MODE, new_mode);
    }
    // BSD flags (UF_HIDDEN / UF_IMMUTABLE / etc.) live at offset 68 (u32).
    if bsd_flags_set.is_some() || bsd_flags_clear.is_some() {
        const INODE_BSD_FLAGS: usize = 68;
        let old = rd_u32(&old_inode_val, INODE_BSD_FLAGS);
        let cleared = old & !bsd_flags_clear.unwrap_or(0);
        let new_flags = cleared | bsd_flags_set.unwrap_or(0);
        wr_u32(&mut new_inode_val, INODE_BSD_FLAGS, new_flags);
    }

    // Rewrite fstree: remove old inode, insert updated inode (no parent nchildren change).
    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        vec![(inode_key.clone(), new_inode_val)],
        &[inode_key],
        None,
        now,
        bsz,
    )?;

    // COW the volume omap header.
    let new_vomap_paddr = stage_vomap_header(txn, vol_omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    // Reclaim COW-replaced metadata (set_inode_attrs never touches extentref).
    let frm_correction =
        free_replaced_metadata(txn, vsb_raw, vol_omap_raw, &omap_node, &[], &reclaim, bsz)?;

    // Stage the updated (virtual) volume superblock (counters unchanged).
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    wr_u64(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (fs_alloc + node_delta + frm_correction).max(0) as u64,
    );
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;

    Ok(())
}

/// Outcome of [`truncate_file_fast`].
#[derive(Debug, PartialEq, Eq)]
pub enum TruncateOutcome {
    /// Resize applied successfully via metadata-only fast path.
    Applied,
    /// Operation falls outside the supported fast-path subset.
    /// Caller should fall back to the read+rewrite path.
    FastPathDeclined,
}

/// Resize a file without reading or rewriting its content.
///
/// An extend (`new_size >= current_size`) only moves the size: the new tail
/// is a hole that reads as zeros, and a later write into it gets blocks.
///
/// A shrink cuts the stream at the new end, wherever it falls: extents past
/// it go and the one across it is shortened ([`cut_stream`]), their blocks
/// losing the file's reference through the live extent-reference tree. The
/// block holding the new end is first rewritten with zeros past it
/// ([`write_range`], one block), so that growing the file again reads zeros
/// there.
///
/// Returns:
/// - `Ok(TruncateOutcome::Applied)` - done.
/// - `Ok(TruncateOutcome::FastPathDeclined)` - nothing changed: the file is
///   compressed, its stream is shared with a clone macOS made, or an extent
///   is encrypted or not a whole number of blocks. The caller rewrites the
///   whole content.
/// - `Err(TxnError::NotFound)` - name not in parent.
///
/// `current_size` is the file's current logical size (callers already know
/// it from `getattr`); we trust the caller rather than re-parsing the inode
/// dstream xfield.
pub fn truncate_file_fast<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
    new_size: u64,
    current_size: u64,
) -> Result<TruncateOutcome, TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, omap_raw)?;
    let (vsb_raw, omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    ensure_volume_mutable(vsb_raw)?;
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
    let all = gather_entries(txn, &omap_node, root_tree_oid, &[&drec_key], bsz)?;

    let file_id = all
        .iter()
        .find(|(k, _)| *k == drec_key)
        .map(|(_, v)| rd_u64(v, 0) & 0x0FFF_FFFF_FFFF_FFFF)
        .ok_or_else(|| TxnError::NotFound(format!("'{name}' not in parent {parent_ino}")))?;

    let old_inode_val = all
        .iter()
        .find(|(k, _)| *k == build_inode_key(file_id))
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::NotFound(format!("inode {file_id}")))?;

    // A compressed file's content lives in its decmpfs attribute and resource
    // fork; the data stream holds none of it, so there is nothing here to
    // resize and any change to the stream would be invisible to readers.
    // Truncation has to keep the part it does not remove, which means reading
    // the content back through the compression, cutting it, and writing it as
    // a plain file - the read-and-rewrite path every caller already has for a
    // declined fast path, and the one `overwrite_existing_file` completes by
    // dropping the compression along with the old content.
    if rd_u32(&old_inode_val, INOI_BSD_FLAGS) & UF_COMPRESSED != 0 {
        return Ok(TruncateOutcome::FastPathDeclined);
    }

    // The extents this path edits are keyed by the inode's stream id. A
    // stream more than one inode names is shared with a clone macOS made:
    // cutting it here would cut the other file too, so the file is first
    // given a stream of its own (`unshare_stream`, or the read-and-rewrite
    // path). A stream keyed by another id but named by this inode alone is
    // this file's.
    let stream_id = match rd_u64(&old_inode_val, INODE_PRIVATE_ID) {
        0 => file_id,
        id => id,
    };
    let stream_rec = all
        .iter()
        .find(|(k, _)| *k == build_dstream_id_key(stream_id))
        .map(|(_, v)| rd_u32(v, 0));
    if stream_rec.is_some_and(|n| n > 1) {
        return Ok(TruncateOutcome::FastPathDeclined);
    }
    // The bytes the stream's extents cover, which an extend continues from.
    let old_alloced: u64 = all
        .iter()
        .filter(|(k, _)| rd_u64(k, 0) == encode_jkey(stream_id, APFS_TYPE_FILE_EXTENT))
        .map(|(_, v)| rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF)
        .sum();

    // The full 92-byte fixed prefix and all xfields are forwarded via
    // rebuild_inode_preserving_unknown - only the name is extracted separately.
    let old_name_str = extract_inode_name(&old_inode_val).unwrap_or(name);

    // ---- No-op: same size. ----
    if new_size == current_size {
        return Ok(TruncateOutcome::Applied);
    }

    // ---- Extend (new_size > current_size): the tail becomes a hole. ----
    // The bytes past the last real block own no blocks, but the stream still
    // has to be described end to end: a hole extent (physical block 0) covers
    // the tail, the allocated size counts it, and the inode counts the hole
    // bytes and carries the sparse flag. Leaving the allocated size behind
    // the size made fsck reject the stream outright ("dstream.size is greater
    // than dstream.alloced_size", "invalid dstream"). This is what macOS
    // writes for `truncate -s` past the end, measured in `truncate_extend.rs`.
    // [CERTAIN: linux-apfs-rw/extents.c apfs_create_hole; apfsprogs/apfsck
    //  extents.c parse_extent (phys 0 counts as sparse bytes) and inode.c
    //  "wrong count of sparse bytes" / "wrong setting for sparse flag"]
    if new_size > current_size {
        let new_alloced = new_size.div_ceil(bsz as u64) * bsz as u64;
        let hole_len = new_alloced.saturating_sub(old_alloced);
        let old_sparse = apfs_core::inode::Inode::parse(&old_inode_val)
            .ok()
            .and_then(|i| i.sparse_bytes)
            .unwrap_or(0);
        let inode_key = build_inode_key(file_id);
        let new_inode_val = with_write_generation_step(rebuild_inode_with_sparse_bytes(
            &old_inode_val,
            old_name_str,
            Some(DstreamArgs {
                size: new_size,
                alloced_size: new_alloced,
            }),
            old_sparse + hole_len,
            now,
        ));
        let mut new_records = vec![(inode_key.clone(), new_inode_val)];
        if hole_len > 0 {
            new_records.push((
                build_file_extent_key(stream_id, old_alloced),
                build_file_extent_val(hole_len, 0),
            ));
        }
        // A file created empty has no stream record yet; the stream it gains
        // here needs its reference count.
        if stream_rec.is_none() {
            new_records.push((build_dstream_id_key(stream_id), build_dstream_id_val(1)));
        }
        return finish_metadata_only_rewrite(
            txn,
            vsb_raw,
            omap_raw,
            &omap_node,
            new_xid,
            now,
            new_records,
            vec![inode_key],
            None,
            0,
            bsz,
        )
        .map(|()| TruncateOutcome::Applied);
    }

    // ---- Shrink: the blocks past the new end go, the block holding it
    // keeps nothing past it. ----
    // A later extend only moves the size, so the bytes after the new end in
    // the block that holds it have to read as zeros: they are zeroed first,
    // through the range writer, in this transaction (a hole there already
    // reads as zeros). The cut then works on the stream as that left it.
    let b = bsz as u64;
    let Some(map) = stream_extent_map(&all, stream_id, b) else {
        return Ok(TruncateOutcome::FastPathDeclined);
    };
    if !new_size.is_multiple_of(b) && mapped_block(&map, new_size / b).is_some() {
        let zeros = vec![0u8; (b - new_size % b) as usize];
        if write_range(txn, vsb_raw, omap_raw, parent_ino, name, new_size, &zeros)?
            == RangeWrite::Declined
        {
            return Ok(TruncateOutcome::FastPathDeclined);
        }
    }
    cut_stream(txn, vsb_raw, omap_raw, parent_ino, name, new_size)?;
    Ok(TruncateOutcome::Applied)
}

/// Cut the stream of file `name` in directory `parent_ino` to `new_size`
/// bytes, the block holding the new end already reading as zeros past it:
/// the extents past the new end go, the one across it is shortened, and
/// the blocks they mapped lose the file's reference the way every release
/// does (freed when it was the last, lowered for a clone, recorded for a
/// snapshot that holds them).
/// [linux-apfs-rw: extents.c apfs_truncate / apfs_shrink_extent_tail;
///  apfs_range_put_reference]
fn cut_stream<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
    new_size: u64,
) -> Result<(), TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, omap_raw)?;
    let (vsb_raw, omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    let b = bsz as u64;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let case_fold = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES) & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let omap_tree_paddr = rd_u64(omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let drec_key = build_drec_key(parent_ino, name, case_fold, true);
    let all = gather_entries(txn, &omap_node, root_tree_oid, &[&drec_key], bsz)?;
    let file_id = all
        .iter()
        .find(|(k, _)| *k == drec_key)
        .map(|(_, v)| rd_u64(v, 0) & 0x0FFF_FFFF_FFFF_FFFF)
        .ok_or_else(|| TxnError::NotFound(format!("'{name}' not in parent {parent_ino}")))?;
    let inode_key = build_inode_key(file_id);
    let old_inode_val = all
        .iter()
        .find(|(k, _)| *k == inode_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::NotFound(format!("inode {file_id}")))?;
    let stream_id = match rd_u64(&old_inode_val, INODE_PRIVATE_ID) {
        0 => file_id,
        id => id,
    };
    let mut map = stream_extent_map(&all, stream_id, b).ok_or_else(|| {
        TxnError::Unsupported(format!("extents of inode {file_id} are not whole blocks"))
    })?;
    let old_map = map.clone();
    let keep = new_size.div_ceil(b);
    let end = map.last_key_value().map_or(0, |(s, (n, _))| s + n);
    let released = if end > keep {
        punch_extents(&mut map, keep, end - keep)
    } else {
        Vec::new()
    };
    let (mut remove_keys, mut new_records) = extent_record_changes(stream_id, &old_map, &map, b);
    remove_keys.push(inode_key.clone());
    let sparse: u64 = map
        .values()
        .filter(|(_, p)| *p == 0)
        .map(|(n, _)| n * b)
        .sum();
    let alloced = map.last_key_value().map_or(0, |(s, (n, _))| (s + n) * b);
    let old_name = extract_inode_name(&old_inode_val).unwrap_or(name);
    new_records.push((
        inode_key,
        with_write_generation_step(rebuild_inode_with_sparse_bytes(
            &old_inode_val,
            old_name,
            Some(DstreamArgs {
                size: new_size,
                alloced_size: alloced,
            }),
            sparse,
            now,
        )),
    ));
    // The cut blocks lose this file's reference: refcnt 1 -> removed and
    // freed, shared -> lowered and kept, held by a snapshot -> a
    // KIND_UPDATE(-1). [#0]
    let extref_update = if released.is_empty() {
        None
    } else {
        let (new_paddr, old_padrs, nd, freed) =
            free_blocks_owned_by_live_extref(txn, vsb_raw, new_xid, &released, bsz)?;
        if old_padrs.is_empty() && nd == 0 && freed == 0 {
            None
        } else {
            Some((new_paddr, old_padrs, nd, freed))
        }
    };
    finish_metadata_only_rewrite(
        txn,
        vsb_raw,
        omap_raw,
        &omap_node,
        new_xid,
        now,
        new_records,
        remove_keys,
        extref_update,
        0,
        bsz,
    )
}

/// What [`write_range`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeWrite {
    /// The bytes are written.
    Written,
    /// Nothing changed: the file's content is not in a data stream of its
    /// own (it is compressed, or its stream is shared with a clone macOS
    /// made). Replacing the whole content gives it one.
    Declined,
}

/// A data stream's extents: first logical block -> (block count, first
/// physical block, 0 for a hole).
type ExtentMap = std::collections::BTreeMap<u64, (u64, u64)>;

/// The extent map of stream `stream_id` from the catalog records `all`, in
/// blocks of `b` bytes. `None` when an extent is encrypted or not a whole
/// number of blocks, which the paths editing the map leave to a rewrite of
/// the whole file.
fn stream_extent_map(all: &[CatRec], stream_id: u64, b: u64) -> Option<ExtentMap> {
    let mut map = ExtentMap::new();
    for (k, v) in all {
        if rd_u64(k, 0) != encode_jkey(stream_id, APFS_TYPE_FILE_EXTENT) {
            continue;
        }
        let (la, len) = (rd_u64(k, 8), rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF);
        if la % b != 0 || len % b != 0 || len == 0 || rd_u64(v, 16) != 0 {
            return None;
        }
        map.insert(la / b, (len / b, rd_u64(v, 8)));
    }
    Some(map)
}

/// The file-extent records that turn stream `stream_id`'s `old` map into
/// `new`: keys to remove and records to insert (an extent whose value
/// changed is both).
fn extent_record_changes(
    stream_id: u64,
    old: &ExtentMap,
    new: &ExtentMap,
    b: u64,
) -> (Vec<Vec<u8>>, Vec<CatRec>) {
    let remove = old
        .iter()
        .filter(|(s, ext)| new.get(s) != Some(ext))
        .map(|(s, _)| build_file_extent_key(stream_id, s * b))
        .collect();
    let insert = new
        .iter()
        .filter(|(s, ext)| old.get(s) != Some(ext))
        .map(|(s, &(n, p))| {
            (
                build_file_extent_key(stream_id, s * b),
                build_file_extent_val(n * b, p),
            )
        })
        .collect();
    (remove, insert)
}

/// The physical block behind logical block `lb`, if a real extent maps it.
fn mapped_block(map: &ExtentMap, lb: u64) -> Option<u64> {
    let (&start, &(count, phys)) = map.range(..=lb).next_back()?;
    (lb < start + count && phys != 0).then(|| phys + (lb - start))
}

/// Cut logical blocks `[first, first + count)` out of `map`, keeping the
/// parts of the extents around them, and return the physical runs the cut
/// part mapped (holes map none).
fn punch_extents(map: &mut ExtentMap, first: u64, count: u64) -> Vec<(u64, u64)> {
    let end = first + count;
    let starts: Vec<u64> = map
        .range(..end)
        .filter(|(s, (n, _))| **s + *n > first)
        .map(|(s, _)| *s)
        .collect();
    let mut cut = Vec::new();
    for s in starts {
        let Some((n, p)) = map.remove(&s) else {
            continue;
        };
        let at = |lb: u64| if p == 0 { 0 } else { p + (lb - s) };
        if s < first {
            map.insert(s, (first - s, p));
        }
        if s + n > end {
            map.insert(end, (s + n - end, at(end)));
        }
        let (lo, hi) = (s.max(first), (s + n).min(end));
        if p != 0 && lo < hi {
            cut.push((at(lo), hi - lo));
        }
    }
    cut
}

/// Join neighbouring extents that continue each other: holes with holes,
/// and real extents whose blocks follow on.
fn join_extents(map: &mut ExtentMap) {
    let mut joined = ExtentMap::new();
    let mut last: Option<(u64, u64, u64)> = None;
    for (&s, &(n, p)) in map.iter() {
        match last {
            Some((ls, ln, lp))
                if ls + ln == s && ((lp == 0 && p == 0) || (lp != 0 && lp + ln == p)) =>
            {
                last = Some((ls, ln + n, lp));
            }
            _ => {
                if let Some((ls, ln, lp)) = last {
                    joined.insert(ls, (ln, lp));
                }
                last = Some((s, n, p));
            }
        }
    }
    if let Some((ls, ln, lp)) = last {
        joined.insert(ls, (ln, lp));
    }
    *map = joined;
}

/// The live extent-reference record covering physical block `phys`:
/// (first block, value).
fn extref_record_at<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    extref_root: u64,
    phys: u64,
    bsz: usize,
) -> Result<Option<(u64, Vec<u8>)>, TxnError> {
    Ok(
        cow_tree::extref_covering(txn, extref_root, phys, phys + 1, bsz)?
            .into_iter()
            .filter(|(k, _)| k.len() >= 8 && rd_u64(k, 0) >> 60 == APFS_TYPE_EXTENT)
            .map(|(k, v)| (rd_u64(&k, 0) & PHYS_EXT_LEN_MASK, v))
            .find(|(start, v)| {
                v.len() >= 20 && *start <= phys && phys < start + (rd_u64(v, 0) & PHYS_EXT_LEN_MASK)
            }),
    )
}

/// Whether the record value says its blocks belong to `stream` alone, as
/// blocks it was given new.
fn owned_alone_by(val: &[u8], stream: u64) -> bool {
    phys_ext_kind(val) == APFS_KIND_NEW && phys_ext_refcnt(val) == 1 && rd_u64(val, 8) == stream
}

/// Write `data` at byte `offset` of the regular file `name` in directory
/// `parent_ino`. A write reaching past the end makes the file that long; a
/// gap between the old end and `offset` reads as zeros (a hole).
///
/// Only the blocks the write touches change, copy-on-write: each gets a new
/// block holding its old content with the written bytes laid over it, the
/// extent that mapped it is split around it, and the old block loses this
/// file's reference (freed when it was the last, kept for a clone that
/// shares it or a snapshot that holds it). A block this transaction itself
/// gave the file, that nothing else references, is rewritten where it is:
/// no checkpoint names it yet. Neighbouring extents that continue each
/// other are joined, so writes appended in one transaction stay one
/// extent.
/// [linux-apfs-rw: inode.c write_begin (read-modify-write into a new
///  block), extents.c apfs_update_extent / apfs_split_extent /
///  apfs_range_put_reference, apfs_insert_phys_extent (a new run joins the
///  record before it when that one is the stream's alone)]
pub fn write_range<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
    offset: u64,
    data: &[u8],
) -> Result<RangeWrite, TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, omap_raw)?;
    let (vsb_raw, omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    let b = bsz as u64;
    let new_xid = txn.xid;
    let now = now_ns();
    ensure_volume_mutable(vsb_raw)?;
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let case_fold = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES) & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    // APFS file sizes are signed 64-bit; past that no block arithmetic
    // below can overflow.
    let end = offset
        .checked_add(data.len() as u64)
        .filter(|e| *e <= i64::MAX as u64)
        .ok_or_else(|| TxnError::InvalidArgument("write past the largest file size".into()))?;

    let omap_tree_paddr = rd_u64(omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let drec_key = build_drec_key(parent_ino, name, case_fold, true);
    let all = gather_entries(txn, &omap_node, root_tree_oid, &[&drec_key], bsz)?;
    let file_id = all
        .iter()
        .find(|(k, _)| *k == drec_key)
        .map(|(_, v)| rd_u64(v, 0) & 0x0FFF_FFFF_FFFF_FFFF)
        .ok_or_else(|| TxnError::NotFound(format!("'{name}' not in parent {parent_ino}")))?;
    let inode_key = build_inode_key(file_id);
    let old_inode_val = all
        .iter()
        .find(|(k, _)| *k == inode_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::NotFound(format!("inode {file_id}")))?;
    if rd_u16(&old_inode_val, 80) & 0o170000 != S_IFREG {
        return Err(TxnError::InvalidArgument(format!(
            "'{name}' is not a regular file"
        )));
    }
    // A compressed file keeps its content outside the data stream, and a
    // stream named by more than this inode is a clone's too: writing either
    // in place would be invisible or would change the other file.
    if rd_u32(&old_inode_val, INOI_BSD_FLAGS) & UF_COMPRESSED != 0 {
        return Ok(RangeWrite::Declined);
    }
    let stream_id = match rd_u64(&old_inode_val, INODE_PRIVATE_ID) {
        0 => file_id,
        id => id,
    };
    let stream_rec = all
        .iter()
        .find(|(k, _)| *k == build_dstream_id_key(stream_id))
        .map(|(_, v)| rd_u32(v, 0));
    // A stream keyed by another id but named by this inode alone (a clone
    // given its own stream, or one whose other side is gone) is this
    // file's: everything below works on `stream_id`.
    if stream_rec.is_some_and(|n| n > 1) {
        return Ok(RangeWrite::Declined);
    }
    if data.is_empty() {
        return Ok(RangeWrite::Written);
    }
    let inode = apfs_core::inode::Inode::parse(&old_inode_val)
        .map_err(|e| TxnError::VolumeParse(format!("inode {file_id}: {e:?}")))?;
    let size = inode.dstream.as_ref().map_or(0, |d| d.size);

    let Some(mut map) = stream_extent_map(&all, stream_id, b) else {
        return Ok(RangeWrite::Declined);
    };
    let old_map = map.clone();
    let new_size = size.max(end);
    let mapped_end = map.last_key_value().map_or(0, |(s, (n, _))| s + n);
    let blocks_end = new_size.div_ceil(b);
    if blocks_end > mapped_end {
        map.insert(mapped_end, (blocks_end - mapped_end, 0));
    }

    // The blocks to write: those the range covers, and the block holding
    // the old end when the write starts past it, whose bytes past the old
    // end must read as zeros from now on (a hole there already does).
    // Whole blocks between the old end and the write become a hole: blocks
    // a stream keeps past its end hold whatever they held, and would
    // otherwise show up inside the file.
    let (first, last) = (offset / b, end.div_ceil(b));
    let mut released: Vec<(u64, u64)> = Vec::new();
    if offset > size {
        let gap_first = size.div_ceil(b);
        if gap_first < first {
            released.extend(punch_extents(&mut map, gap_first, first - gap_first));
            map.insert(gap_first, (first - gap_first, 0));
        }
    }
    let mut touched: Vec<u64> = (first..last).collect();
    if offset > size && size % b != 0 && size / b < first && mapped_block(&map, size / b).is_some()
    {
        touched.insert(0, size / b);
    }
    let no_snapshots = rd_u64(vsb_raw, VSBI_NUM_SNAPSHOTS) == 0;
    let extref_root = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    // A block the data covers whole is written from the data (`None`); only
    // the blocks at the ends, which keep some old bytes, get a buffer of
    // their own. A buffer for every block, joined into one more per run,
    // held twice the write in memory, 64 MiB for one 32 MiB write.
    let whole = |lb: u64| lb * b >= offset && (lb + 1) * b <= end;
    let data_of = |lb: u64, n: u64| {
        data.get((lb * b - offset) as usize..((lb + n) * b - offset) as usize)
            .ok_or_else(|| TxnError::InvalidArgument("write range out of its data".into()))
    };
    let mut cow: Vec<(u64, Option<Vec<u8>>)> = Vec::new();
    for lb in touched {
        let phys = mapped_block(&map, lb);
        if whole(lb) {
            let in_place = match phys {
                Some(p) if no_snapshots && (txn.staged_live(p) || txn.fresh_data(p)) => {
                    extref_record_at(txn, extref_root, p, bsz)?
                        .is_some_and(|(_, v)| owned_alone_by(&v, stream_id))
                }
                _ => false,
            };
            match phys {
                Some(p) if in_place && txn.staged_live(p) => {
                    txn.restage(p, data_of(lb, 1)?.to_vec())
                }
                Some(p) if in_place => txn.rewrite_fresh_data(p, data_of(lb, 1)?)?,
                _ => cow.push((lb, None)),
            }
            continue;
        }
        let mut blk = vec![0u8; bsz];
        if let Some(p) = phys {
            txn.read_block(p, &mut blk)?;
        }
        let start = lb * b;
        if size < start + b {
            let from = size.saturating_sub(start) as usize;
            if let Some(tail) = blk.get_mut(from..) {
                tail.fill(0);
            }
        }
        let (lo, hi) = (offset.max(start), end.min(start + b));
        if lo < hi {
            let src = data
                .get((lo - offset) as usize..(hi - offset) as usize)
                .ok_or_else(|| TxnError::InvalidArgument("write range out of its data".into()))?;
            if let Some(dst) = blk.get_mut((lo - start) as usize..(hi - start) as usize) {
                dst.copy_from_slice(src);
            }
        }
        let in_place = match phys {
            Some(p) if no_snapshots && (txn.staged_live(p) || txn.fresh_data(p)) => {
                extref_record_at(txn, extref_root, p, bsz)?
                    .is_some_and(|(_, v)| owned_alone_by(&v, stream_id))
            }
            _ => false,
        };
        match phys {
            Some(p) if in_place && txn.staged_live(p) => txn.restage(p, blk),
            Some(p) if in_place => txn.rewrite_fresh_data(p, &blk)?,
            _ => cow.push((lb, Some(blk))),
        }
    }

    // Copy-on-write the rest, one run of consecutive logical blocks at a
    // time.
    let mut new_runs: Vec<(u64, u64)> = Vec::new();
    let mut added = 0u64;
    let mut i = 0;
    while i < cow.len() {
        let run_first = cow.get(i).map_or(0, |(lb, _)| *lb);
        let mut n = 1usize;
        while cow
            .get(i + n)
            .is_some_and(|(lb, _)| *lb == run_first + n as u64)
        {
            n += 1;
        }
        released.extend(punch_extents(&mut map, run_first, n as u64));
        let mut done = 0usize;
        while done < n {
            let (q, got) = txn.alloc_blocks_run(n - done)?;
            if got == 0 {
                return Err(TxnError::NoFreeBlocks);
            }
            let piece = cow
                .get_mut(i + done..i + done + got)
                .ok_or_else(|| TxnError::InvalidArgument("write run out of its blocks".into()))?;
            // Consecutive whole blocks go out as one slice of the data; an
            // edge block, from its own buffer.
            let mut k = 0usize;
            while k < piece.len() {
                let at = q + k as u64;
                let (lb, own) = match piece.get_mut(k) {
                    Some((lb, blk)) => (*lb, blk.take()),
                    None => break,
                };
                if let Some(own) = own {
                    txn.write_new_data(at, &own)?;
                    k += 1;
                    continue;
                }
                let mut m = 1usize;
                while piece.get(k + m).is_some_and(|(_, blk)| blk.is_none()) {
                    m += 1;
                }
                txn.write_new_data(at, data_of(lb, m as u64)?)?;
                k += m;
            }
            map.insert(run_first + done as u64, (got as u64, q));
            match new_runs.last_mut() {
                Some((lq, ln)) if *lq + *ln == q => *ln += got as u64,
                _ => new_runs.push((q, got as u64)),
            }
            added += got as u64;
            done += got;
        }
        i += n;
    }
    join_extents(&mut map);

    // The extent-reference tree: the replaced blocks lose this file's
    // reference, the new runs get records of their own, the first one
    // joining the record that ends where it starts when that record is
    // this stream's alone.
    let mut extref_update = None;
    if !released.is_empty() || !new_runs.is_empty() {
        let release = if released.is_empty() {
            ExtrefRelease {
                remove_keys: Vec::new(),
                upserts: Vec::new(),
                update_inserts: Vec::new(),
                owned_runs: Vec::new(),
            }
        } else {
            let live = extref_records_for_runs(txn, extref_root, &released, bsz)?;
            plan_extref_release(&live, &released)?
        };
        let touched_keys: HashSet<Vec<u8>> = release
            .remove_keys
            .iter()
            .chain(release.upserts.iter().map(|(k, _)| k))
            .cloned()
            .collect();
        let mut upserts = release.upserts;
        let mut inserts = release.update_inserts;
        for (n, &(q, count)) in new_runs.iter().enumerate() {
            let before = if n == 0 && q > 0 {
                extref_record_at(txn, extref_root, q - 1, bsz)?
            } else {
                None
            };
            match before {
                Some((s, v))
                    if owned_alone_by(&v, stream_id)
                        && s + (rd_u64(&v, 0) & PHYS_EXT_LEN_MASK) == q
                        && !touched_keys.contains(&build_phys_ext_key(s)) =>
                {
                    let blocks = (rd_u64(&v, 0) & PHYS_EXT_LEN_MASK) + count;
                    upserts.push((build_phys_ext_key(s), phys_ext_with_blocks(&v, blocks)));
                }
                _ => inserts.push((
                    build_phys_ext_key(q),
                    build_phys_ext_val(count, stream_id, 1),
                )),
            }
        }
        let (new_root, old_nodes, node_delta) = rewrite_extref_tree(
            txn,
            extref_root,
            new_xid,
            inserts,
            &release.remove_keys,
            &upserts,
            bsz,
        )?;
        let mut freed = 0u64;
        for &(phys, count) in &release.owned_runs {
            for blk in phys..phys + count {
                txn.free_block(blk)?;
            }
            freed += count;
        }
        extref_update = Some((new_root, old_nodes, node_delta, freed));
    }

    // The catalog: the extents that changed, the stream record a file
    // created empty lacks, and the inode with its new size, allocated size
    // and hole count.
    let (mut remove_keys, mut new_records) = extent_record_changes(stream_id, &old_map, &map, b);
    remove_keys.push(inode_key.clone());
    if stream_rec.is_none() {
        new_records.push((build_dstream_id_key(stream_id), build_dstream_id_val(1)));
    }
    let sparse: u64 = map
        .values()
        .filter(|(_, p)| *p == 0)
        .map(|(n, _)| n * b)
        .sum();
    let alloced = map.last_key_value().map_or(0, |(s, (n, _))| (s + n) * b);
    let old_name = extract_inode_name(&old_inode_val).unwrap_or(name);
    new_records.push((
        inode_key,
        with_write_generation_step(rebuild_inode_with_sparse_bytes(
            &old_inode_val,
            old_name,
            Some(DstreamArgs {
                size: new_size,
                alloced_size: alloced,
            }),
            sparse,
            now,
        )),
    ));
    finish_metadata_only_rewrite(
        txn,
        vsb_raw,
        omap_raw,
        &omap_node,
        new_xid,
        now,
        new_records,
        remove_keys,
        extref_update,
        added,
        bsz,
    )?;
    Ok(RangeWrite::Written)
}

/// Give the regular file `name` in directory `parent_ino` a data stream of
/// its own when it shares one with another inode (a clone macOS made names
/// the original's stream), so that its content can be edited in place.
/// Nothing is copied: the new stream's extents name the same blocks, and
/// each of those blocks gains a reference, the way [`clone_file`] shares
/// them; the shared stream loses this inode from its count. Returns whether
/// the stream was shared.
/// [linux-apfs-rw: inode.c apfs_inode_create_dstream_rec on a shared
///  stream - put the shared record, clone the extents under a new id]
pub fn unshare_stream<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
) -> Result<bool, TxnError> {
    let (vsb_now, omap_now) = txn.current_volume(vsb_raw, omap_raw)?;
    let (vsb_raw, omap_raw) = (vsb_now.as_slice(), omap_now.as_slice());
    let bsz = txn.nx.block_size as usize;
    let b = bsz as u64;
    let new_xid = txn.xid;
    let now = now_ns();
    ensure_volume_mutable(vsb_raw)?;
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let case_fold = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES) & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let omap_tree_paddr = rd_u64(omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let drec_key = build_drec_key(parent_ino, name, case_fold, true);
    let all = gather_entries(txn, &omap_node, root_tree_oid, &[&drec_key], bsz)?;
    let file_id = all
        .iter()
        .find(|(k, _)| *k == drec_key)
        .map(|(_, v)| rd_u64(v, 0) & 0x0FFF_FFFF_FFFF_FFFF)
        .ok_or_else(|| TxnError::NotFound(format!("'{name}' not in parent {parent_ino}")))?;
    let inode_key = build_inode_key(file_id);
    let mut inode_val = all
        .iter()
        .find(|(k, _)| *k == inode_key)
        .map(|(_, v)| v.clone())
        .ok_or_else(|| TxnError::NotFound(format!("inode {file_id}")))?;
    if rd_u16(&inode_val, 80) & 0o170000 != S_IFREG {
        return Err(TxnError::InvalidArgument(format!(
            "'{name}' is not a regular file"
        )));
    }
    let shared_id = match rd_u64(&inode_val, INODE_PRIVATE_ID) {
        0 => file_id,
        id => id,
    };
    let refcnt = all
        .iter()
        .find(|(k, _)| *k == build_dstream_id_key(shared_id))
        .map_or(1, |(_, v)| rd_u32(v, 0));
    if refcnt <= 1 {
        return Ok(false);
    }

    // The new stream: a fresh id (the original's own number may be the
    // shared stream's), its count, and the shared stream's extents.
    let own_id = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);
    let mut new_records = vec![(build_dstream_id_key(own_id), build_dstream_id_val(1))];
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for (k, v) in &all {
        if rd_u64(k, 0) != encode_jkey(shared_id, APFS_TYPE_FILE_EXTENT) {
            continue;
        }
        let (la, len, phys) = (
            rd_u64(k, 8),
            rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF,
            rd_u64(v, 8),
        );
        new_records.push((build_file_extent_key(own_id, la), v.clone()));
        if !is_sparse_hole(phys) && len > 0 {
            runs.push((phys, len.div_ceil(b)));
        }
    }
    wr_u64(&mut inode_val, INODE_PRIVATE_ID, own_id);
    wr_u64(&mut inode_val, INODE_CHANGE_TIME, now);
    let updates = vec![
        (
            build_dstream_id_key(shared_id),
            build_dstream_id_val(refcnt - 1),
        ),
        (inode_key, inode_val),
    ];
    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree_impl(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        new_records,
        &[],
        None,
        &updates,
        now,
        bsz,
    )?;

    // Every block the extents name gains this stream's reference.
    let old_extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    let (new_extref_paddr, old_extref_padrs, extref_node_delta) = if runs.is_empty() {
        (old_extref_paddr, Vec::new(), 0i64)
    } else {
        let live = extref_records_for_runs(txn, old_extref_paddr, &runs, bsz)?;
        let plan = plan_extref_take(&live, &runs)?;
        rewrite_extref_tree(
            txn,
            old_extref_paddr,
            new_xid,
            plan.update_inserts,
            &plan.remove_keys,
            &plan.upserts,
            bsz,
        )?
    };

    let new_vomap_paddr = stage_vomap_header(txn, omap_raw, new_xid, new_omap_tree_paddr, bsz)?;
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    wr_u64(&mut new_vsb, VSBI_NEXT_OBJ_ID, own_id + 1);
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        omap_raw,
        &omap_node,
        &old_extref_padrs,
        &reclaim,
        bsz,
    )?;
    // No data block is added or freed: only tree nodes change.
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let alloc = (fs_alloc + node_delta + extref_node_delta + frm_correction).max(0) as u64;
    wr_u64(&mut new_vsb, VSBI_FS_ALLOC_COUNT, alloc);
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;
    Ok(true)
}

/// Shared tail of [`truncate_file_fast`] and [`write_range`]: rewrite the
/// fstree with the inode replacement (+ extent changes), COW the volume omap,
/// stage the volume superblock with adjusted accounting, free COW-replaced
/// metadata.
///
/// `extref_update` is `Some((new_extref_paddr, old_padrs, extref_node_delta,
/// freed_block_count))` when the extent-reference tree changed; `None`
/// otherwise. `data_blocks_added` counts the data blocks the change
/// allocated. [#151: multi-node capable]
#[allow(clippy::too_many_arguments)]
fn finish_metadata_only_rewrite<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_raw: &[u8],
    omap_node: &[u8],
    new_xid: u64,
    now: u64,
    new_records: Vec<(Vec<u8>, Vec<u8>)>,
    remove_keys: Vec<Vec<u8>>,
    extref_update: Option<(u64, Vec<u64>, i64, u64)>,
    data_blocks_added: u64,
    bsz: usize,
) -> Result<(), TxnError> {
    let FstreeRewrite {
        omap_tree_paddr: new_omap_tree_paddr,
        node_delta,
        reclaim,
    } = rewrite_fstree(
        txn,
        vsb_raw,
        omap_node,
        new_xid,
        new_records,
        &remove_keys,
        None,
        now,
        bsz,
    )?;

    let new_vomap_paddr = stage_vomap_header(txn, omap_raw, new_xid, new_omap_tree_paddr, bsz)?;

    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    let old_extref_padrs: &[u64];
    let extref_node_delta_opt: i64;
    let freed_count: u64;
    let old_extref_padrs_owned: Vec<u64>;
    if let Some((new_extref_paddr, ref op, nd, fc)) = extref_update {
        wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
        old_extref_padrs_owned = op.clone();
        old_extref_padrs = &old_extref_padrs_owned;
        extref_node_delta_opt = nd;
        freed_count = fc;
    } else {
        old_extref_padrs_owned = vec![];
        old_extref_padrs = &old_extref_padrs_owned;
        extref_node_delta_opt = 0;
        freed_count = 0;
    }
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        omap_raw,
        omap_node,
        old_extref_padrs,
        &reclaim,
        bsz,
    )?;

    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let alloc_delta = data_blocks_added as i64 - freed_count as i64
        + node_delta
        + extref_node_delta_opt
        + frm_correction;
    wr_u64(
        &mut new_vsb,
        VSBI_FS_ALLOC_COUNT,
        (fs_alloc + alloc_delta).max(0) as u64,
    );
    wr_u64(&mut new_vsb, VSBI_LAST_MOD_TIME, now);
    let body = new_vsb.get(32..bsz).unwrap_or(&[]).to_vec();
    txn.stage_virtual(vsb_oid, OBJECT_TYPE_FS, 0, &body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn punching_an_extent_keeps_both_sides_and_returns_the_middle() {
        let mut map = ExtentMap::new();
        map.insert(0, (10, 100));
        map.insert(10, (5, 0));
        map.insert(15, (5, 300));
        // Blocks 4..17: the tail of the first extent, the hole, the head of
        // the third.
        let cut = punch_extents(&mut map, 4, 13);
        assert_eq!(cut, vec![(104, 6), (300, 2)]);
        let left: Vec<_> = map.into_iter().collect();
        assert_eq!(left, vec![(0, (4, 100)), (17, (3, 302))]);
    }

    #[test]
    fn neighbouring_extents_join_only_when_they_continue() {
        let mut map = ExtentMap::new();
        map.insert(0, (4, 100));
        map.insert(4, (2, 104)); // follows on
        map.insert(6, (1, 200)); // does not
        map.insert(7, (3, 0));
        map.insert(10, (2, 0)); // holes join
        map.insert(12, (1, 1)); // a real block after a hole stays apart
        join_extents(&mut map);
        let joined: Vec<_> = map.into_iter().collect();
        assert_eq!(
            joined,
            vec![(0, (6, 100)), (6, (1, 200)), (7, (5, 0)), (12, (1, 1))]
        );
        let mut map = ExtentMap::new();
        map.insert(0, (2, 50));
        assert_eq!(mapped_block(&map, 1), Some(51));
        assert_eq!(mapped_block(&map, 2), None);
    }

    fn live(start: u64, blocks: u64, val: Vec<u8>) -> CatRecord {
        CatRecord {
            key: build_phys_ext_key(start),
            val: phys_ext_with_blocks(&val, blocks),
        }
    }

    fn new_rec(start: u64, blocks: u64, refcnt: u32) -> CatRecord {
        live(start, blocks, build_phys_ext_val(blocks, 500, refcnt))
    }

    fn key_start(key: &[u8]) -> u64 {
        rd_u64(key, 0) & PHYS_EXT_LEN_MASK
    }

    fn upsert_at(release: &ExtrefRelease, start: u64) -> Option<&Vec<u8>> {
        release
            .upserts
            .iter()
            .find(|(k, _)| key_start(k) == start)
            .map(|(_, v)| v)
    }

    // A run that coincides with one record: the last reference removes the
    // record and frees the run; a shared one just loses a reference.
    #[test]
    fn extref_release_exact_record() {
        let recs = vec![new_rec(100, 10, 1), new_rec(200, 4, 3)];
        let r = plan_extref_release(&recs, &[(100, 10), (200, 4)]).unwrap();
        assert_eq!(r.remove_keys, vec![build_phys_ext_key(100)]);
        assert_eq!(r.owned_runs, vec![(100, 10)]);
        assert_eq!(r.update_inserts.len(), 0);
        let v = upsert_at(&r, 200).expect("shared record rewritten");
        assert_eq!(phys_ext_refcnt(v), 2);
        assert_eq!(rd_u64(v, 0) & PHYS_EXT_LEN_MASK, 4);
    }

    // One extent over three records, as macOS leaves the original after
    // rewriting part of its clone: the shared ends lose a reference, the
    // middle (held only by this file) goes away and is freed.
    #[test]
    fn extref_release_run_spanning_three_records() {
        let recs = vec![new_rec(100, 10, 2), new_rec(110, 5, 1), new_rec(115, 49, 2)];
        let r = plan_extref_release(&recs, &[(100, 64)]).unwrap();
        assert_eq!(r.remove_keys, vec![build_phys_ext_key(110)]);
        assert_eq!(r.owned_runs, vec![(110, 5)]);
        assert!(r.update_inserts.is_empty());
        assert_eq!(phys_ext_refcnt(upsert_at(&r, 100).unwrap()), 1);
        assert_eq!(phys_ext_refcnt(upsert_at(&r, 115).unwrap()), 1);
        assert_eq!(r.upserts.len(), 2);
    }

    // A run inside a larger record splits it: the parts outside keep the
    // record's count under their own keys, the part inside loses one.
    #[test]
    fn extref_release_run_inside_a_record_splits_it() {
        let recs = vec![new_rec(100, 64, 2)];
        let r = plan_extref_release(&recs, &[(110, 5)]).unwrap();
        assert!(r.remove_keys.is_empty());
        assert!(r.owned_runs.is_empty());
        let head = upsert_at(&r, 100).expect("head keeps the key");
        assert_eq!(rd_u64(head, 0) & PHYS_EXT_LEN_MASK, 10);
        assert_eq!(phys_ext_refcnt(head), 2);
        let middle = upsert_at(&r, 110).expect("middle under its own key");
        assert_eq!(rd_u64(middle, 0) & PHYS_EXT_LEN_MASK, 5);
        assert_eq!(phys_ext_refcnt(middle), 1);
        let tail = upsert_at(&r, 115).expect("tail under its own key");
        assert_eq!(rd_u64(tail, 0) & PHYS_EXT_LEN_MASK, 49);
        assert_eq!(phys_ext_refcnt(tail), 2);
        assert_eq!(r.upserts.len(), 3);

        // The same with the last reference: the middle is freed, not kept.
        let recs = vec![new_rec(100, 64, 1)];
        let r = plan_extref_release(&recs, &[(110, 5)]).unwrap();
        assert_eq!(r.owned_runs, vec![(110, 5)]);
        assert!(upsert_at(&r, 110).is_none());
        assert_eq!(
            rd_u64(upsert_at(&r, 100).unwrap(), 0) & PHYS_EXT_LEN_MASK,
            10
        );
        assert_eq!(
            rd_u64(upsert_at(&r, 115).unwrap(), 0) & PHYS_EXT_LEN_MASK,
            49
        );
    }

    // A range no live record covers belongs to a snapshot: the volume
    // withdraws one reference from exactly that range, and only that range.
    #[test]
    fn extref_release_gap_gets_an_update_of_its_own_length() {
        let recs = vec![new_rec(100, 10, 1), new_rec(130, 10, 2)];
        let r = plan_extref_release(&recs, &[(100, 40)]).unwrap();
        assert_eq!(r.remove_keys, vec![build_phys_ext_key(100)]);
        assert_eq!(r.owned_runs, vec![(100, 10)]);
        assert_eq!(r.update_inserts.len(), 1);
        let (k, v) = &r.update_inserts[0];
        assert_eq!(key_start(k), 110);
        assert_eq!(rd_u64(v, 0) & PHYS_EXT_LEN_MASK, 20);
        assert_eq!(phys_ext_kind(v), APFS_KIND_UPDATE);
        assert_eq!(phys_ext_update_delta(v), -1);
        assert_eq!(phys_ext_refcnt(upsert_at(&r, 130).unwrap()), 1);
    }

    // A KIND_UPDATE record is a signed delta over its range; a run through
    // part of it splits the range and lowers the delta of the part inside.
    #[test]
    fn extref_release_splits_an_update_record() {
        let recs = vec![live(100, 20, build_phys_ext_update_val(20, 2))];
        let r = plan_extref_release(&recs, &[(105, 5)]).unwrap();
        assert!(r.owned_runs.is_empty());
        assert!(r.remove_keys.is_empty());
        let head = upsert_at(&r, 100).unwrap();
        assert_eq!(
            (
                rd_u64(head, 0) & PHYS_EXT_LEN_MASK,
                phys_ext_update_delta(head)
            ),
            (5, 2)
        );
        let middle = upsert_at(&r, 105).unwrap();
        assert_eq!(
            (
                rd_u64(middle, 0) & PHYS_EXT_LEN_MASK,
                phys_ext_update_delta(middle)
            ),
            (5, 1)
        );
        let tail = upsert_at(&r, 110).unwrap();
        assert_eq!(
            (
                rd_u64(tail, 0) & PHYS_EXT_LEN_MASK,
                phys_ext_update_delta(tail)
            ),
            (10, 2)
        );
        // Delta reaching zero drops the piece.
        let recs = vec![live(100, 20, build_phys_ext_update_val(20, 1))];
        let r = plan_extref_release(&recs, &[(100, 20)]).unwrap();
        assert_eq!(r.remove_keys, vec![build_phys_ext_key(100)]);
        assert!(r.upserts.is_empty() && r.owned_runs.is_empty());
    }

    // A leaf whose header claims more records than its table can hold, or a
    // table entry pointing outside the node, is an error: no reservation is
    // made for the claimed count and no offset arithmetic can panic.
    #[test]
    fn parse_cat_leaf_refuses_crafted_geometry() {
        let mut node = vec![0u8; 4096];
        node[36..40].copy_from_slice(&u32::MAX.to_le_bytes());
        node[42..44].copy_from_slice(&16u16.to_le_bytes());
        assert!(parse_cat_leaf(&node).is_err());

        // One real record, then the same node with its value offset past the
        // start of the block and with the table beyond the value area.
        let mut node = vec![0u8; 4096];
        node[36..40].copy_from_slice(&1u32.to_le_bytes());
        node[42..44].copy_from_slice(&8u16.to_le_bytes());
        let toc = DATA_BASE;
        node[toc..toc + 2].copy_from_slice(&0u16.to_le_bytes());
        node[toc + 2..toc + 4].copy_from_slice(&8u16.to_le_bytes());
        node[toc + 4..toc + 6].copy_from_slice(&8u16.to_le_bytes());
        node[toc + 6..toc + 8].copy_from_slice(&8u16.to_le_bytes());
        assert_eq!(parse_cat_leaf(&node).expect("well-formed").len(), 1);
        node[toc + 4..toc + 6].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(parse_cat_leaf(&node).is_err());
        node[toc + 4..toc + 6].copy_from_slice(&8u16.to_le_bytes());
        node[42..44].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(parse_cat_leaf(&node).is_err());
        assert!(parse_cat_leaf(&node[..40]).is_err());
    }

    // The two name-rule bits are the only incompatible features a mutation
    // knows how to honour; any other bit, with or without them, is refused
    // by name, and the sealed bit keeps its own message.
    #[test]
    fn unknown_incompatible_feature_bits_refuse_mutation() {
        let vsb = |incompat: u64| {
            let mut v = vec![0u8; 0x200];
            v[VSBI_INCOMPAT_FEATURES..VSBI_INCOMPAT_FEATURES + 8]
                .copy_from_slice(&incompat.to_le_bytes());
            v[VSBI_FS_FLAGS..VSBI_FS_FLAGS + 8].copy_from_slice(&APFS_FS_UNENCRYPTED.to_le_bytes());
            v
        };
        // Neither hashing bit: the directory records use the plain-name key
        // layout this writer does not produce, so the volume is refused too.
        match ensure_volume_mutable(&vsb(0)) {
            Err(TxnError::Unsupported(msg)) => assert!(msg.contains("by name rather than by hash")),
            other => panic!("an unhashed volume must be refused as unsupported, got {other:?}"),
        }
        assert!(ensure_volume_mutable(&vsb(0x1)).is_ok());
        assert!(ensure_volume_mutable(&vsb(0x8)).is_ok());
        assert!(ensure_volume_mutable(&vsb(0x9)).is_ok());
        for bits in [0x2u64, 0x4, 0x10, 0x40, 0x80, 0x100, 0x1 | 0x2] {
            match ensure_volume_mutable(&vsb(bits)) {
                Err(TxnError::Unsupported(msg)) => assert!(
                    msg.contains(&format!("{:#x}", bits & !0x9)),
                    "{bits:#x}: the refusal names the bits: {msg}"
                ),
                other => panic!("{bits:#x} must be refused as unsupported, got {other:?}"),
            }
        }
        match ensure_volume_mutable(&vsb(0x20)) {
            Err(TxnError::Unsupported(msg)) => assert!(msg.contains("sealed")),
            other => panic!("sealed must keep its message, got {other:?}"),
        }
    }

    // A directory that maintains statistics is refused; one that does not,
    // and a value too short to carry flags, pass.
    #[test]
    fn directory_statistics_flag_refuses_the_change() {
        let mut val = vec![0u8; 96];
        assert!(refuse_dir_stats(&val, 42).is_ok());
        val[INODE_INTERNAL_FLAGS..INODE_INTERNAL_FLAGS + 8]
            .copy_from_slice(&(INODE_MAINTAIN_DIR_STATS | 0x10).to_le_bytes());
        assert!(matches!(
            refuse_dir_stats(&val, 42),
            Err(TxnError::Unsupported(_))
        ));
        val[INODE_INTERNAL_FLAGS..INODE_INTERNAL_FLAGS + 8].copy_from_slice(&0x10u64.to_le_bytes());
        assert!(refuse_dir_stats(&val, 42).is_ok());
        assert!(refuse_dir_stats(&val[..40], 42).is_ok());
    }

    /// A clone's run spanning a split record takes a reference on every
    /// piece: the record macOS left shared, the one only the source held,
    /// and a range only a snapshot owns (KIND_UPDATE(+1)). A delta that
    /// returns to zero drops its record.
    #[test]
    fn extref_take_walks_every_record_a_run_spans() {
        let recs = vec![new_rec(100, 4, 2), new_rec(104, 6, 1)];
        let r = plan_extref_take(&recs, &[(100, 10), (200, 5)]).unwrap();
        assert!(r.owned_runs.is_empty(), "taking a reference frees nothing");
        let refcnt_at = |start: u64| {
            r.upserts
                .iter()
                .find(|(k, _)| *k == build_phys_ext_key(start))
                .map(|(_, v)| phys_ext_refcnt(v))
        };
        assert_eq!(refcnt_at(100), Some(3));
        assert_eq!(refcnt_at(104), Some(2));
        assert_eq!(r.update_inserts.len(), 1);
        let (k, v) = r.update_inserts.first().expect("one KIND_UPDATE insert");
        assert_eq!(*k, build_phys_ext_key(200));
        assert_eq!(phys_ext_update_delta(v), 1);

        // Only part of a record: it splits at the run's edges.
        let r = plan_extref_take(&[new_rec(100, 10, 1)], &[(103, 2)]).unwrap();
        assert_eq!(refcnt_at_in(&r, 100), Some((3, 1)));
        assert_eq!(refcnt_at_in(&r, 103), Some((2, 2)));
        assert_eq!(refcnt_at_in(&r, 105), Some((5, 1)));

        // A snapshot-side delta of -1 cancelled by the new reference.
        let minus = CatRecord {
            key: build_phys_ext_key(300),
            val: build_phys_ext_update_val(5, -1),
        };
        let r = plan_extref_take(&[minus], &[(300, 5)]).unwrap();
        assert_eq!(r.remove_keys, vec![build_phys_ext_key(300)]);
        assert!(r.upserts.is_empty() && r.update_inserts.is_empty());
    }

    #[test]
    fn extref_records_must_be_disjoint_and_hold_a_reference() {
        let pair = |r: CatRecord| (r.key, r.val);
        let ok = vec![pair(new_rec(100, 10, 1)), pair(new_rec(110, 5, 2))];
        assert!(check_extref_records_disjoint(&ok).is_ok());
        let overlap = vec![pair(new_rec(100, 10, 1)), pair(new_rec(109, 5, 1))];
        assert!(check_extref_records_disjoint(&overlap).is_err());
        assert!(check_extref_records_disjoint(&[pair(new_rec(100, 0, 1))]).is_err());
        assert!(check_extref_records_disjoint(&[pair(new_rec(100, 3, 0))]).is_err());
        let no_change = (build_phys_ext_key(100), build_phys_ext_update_val(3, 0));
        assert!(check_extref_records_disjoint(&[no_change]).is_err());
        let minus = (build_phys_ext_key(100), build_phys_ext_update_val(3, -1));
        assert!(check_extref_records_disjoint(&[minus]).is_ok());
    }

    /// `(blocks, refcnt)` of the record a plan leaves at `start`.
    fn refcnt_at_in(r: &ExtrefRelease, start: u64) -> Option<(u64, u32)> {
        r.upserts
            .iter()
            .find(|(k, _)| *k == build_phys_ext_key(start))
            .map(|(_, v)| (rd_u64(v, 0) & PHYS_EXT_LEN_MASK, phys_ext_refcnt(v)))
    }

    #[test]
    fn extref_release_skips_holes_and_rejects_short_values() {
        let recs = vec![new_rec(100, 10, 1)];
        let r = plan_extref_release(&recs, &[(0, 5)]).unwrap();
        assert!(r.remove_keys.is_empty() && r.owned_runs.is_empty() && r.update_inserts.is_empty());
        let short = CatRecord {
            key: build_phys_ext_key(100),
            val: vec![0u8; 8],
        };
        assert!(plan_extref_release(&[short], &[(100, 1)]).is_err());
    }

    #[test]
    fn name_hash_matches_baseline_private_dir() {
        // Baseline scratch: "private-dir" -> hash22 0x2b29a3, name_len 12.
        assert_eq!(apfs_name_hash("private-dir", true, true), 0x2b29a3);
        let packed = name_len_and_hash("private-dir", true, true);
        assert_eq!(packed & 0x3FF, 12); // "private-dir" = 11 chars + NUL
        assert_eq!(packed >> 10, 0x2b29a3);
    }

    #[test]
    fn name_hash_matches_baseline_root() {
        // Baseline scratch: "root" -> hash22 0x2d9c79, name_len 5.
        assert_eq!(apfs_name_hash("root", true, true), 0x2d9c79);
        let packed = name_len_and_hash("root", true, true);
        assert_eq!(packed & 0x3FF, 5);
        assert_eq!(packed >> 10, 0x2d9c79);
    }

    #[test]
    fn name_hash_matches_kernel_hello_txt() {
        // Kernel-produced DREC for "hello.txt": name_len_and_hash = 0xe25b900a.
        assert_eq!(apfs_name_hash("hello.txt", true, true), 0x3896e4);
        assert_eq!(name_len_and_hash("hello.txt", true, true), 0xe25b_900a);
    }

    #[test]
    fn drec_key_matches_kernel_hello_txt() {
        // Kernel key hex: 02000000 00000090 0a905be2 68656c6c6f2e74787400
        let k = build_drec_key(2, "hello.txt", true, true);
        assert_eq!(
            k,
            hex("0200000000000090\
                 0a905be2\
                 68656c6c6f2e74787400")
        );
    }

    #[test]
    fn inode_val_matches_kernel_empty_file() {
        // Kernel inode for empty hello.txt (ino 20, parent 2, uid/gid 99).
        let now = 0x18b1_b337_ea00_6b27;
        let v = build_inode_val(&InodeValArgs {
            parent_id: 2,
            private_id: 20,
            now_ns: now,
            mode: S_IFREG | 0o644,
            nlink_or_nchildren: 1,
            uid: 99,
            gid: 99,
            name: "hello.txt",
            dstream: None,
        });
        assert_eq!(v.len(), 116);
        // Field-level checks (timestamps set to `now` for all four).
        assert_eq!(&v[0..8], &2u64.to_le_bytes()); // parent
        assert_eq!(&v[8..16], &20u64.to_le_bytes()); // private
        assert_eq!(u64::from_le_bytes(v[48..56].try_into().unwrap()), 0x8000);
        assert_eq!(u32::from_le_bytes(v[56..60].try_into().unwrap()), 1); // nlink
        assert_eq!(u32::from_le_bytes(v[64..68].try_into().unwrap()), 1); // write_gen
        assert_eq!(u32::from_le_bytes(v[72..76].try_into().unwrap()), 99); // uid
        assert_eq!(u16::from_le_bytes(v[80..82].try_into().unwrap()), 0o100644);
        assert_eq!(u64::from_le_bytes(v[84..92].try_into().unwrap()), 0); // uncompressed_size
        assert_eq!(u16::from_le_bytes(v[92..94].try_into().unwrap()), 1); // xf_num
        assert_eq!(u16::from_le_bytes(v[94..96].try_into().unwrap()), 16); // xf_used
        assert_eq!(v[96], 4); // INO_EXT_TYPE_NAME
        assert_eq!(v[97], 0x02); // x_flags
        assert_eq!(u16::from_le_bytes(v[98..100].try_into().unwrap()), 10); // size
        assert_eq!(&v[100..109], b"hello.txt");
        assert_eq!(v[109], 0); // NUL
    }

    #[test]
    fn inode_val_with_dstream_matches_kernel_content_file() {
        // Kernel note.txt (17 bytes): NAME(9) + DSTREAM(40), vlen 160.
        let now = 0x18b1_b337_ea00_6b27;
        let v = build_inode_val(&InodeValArgs {
            parent_id: 2,
            private_id: 18,
            now_ns: now,
            mode: S_IFREG | 0o644,
            nlink_or_nchildren: 1,
            uid: 0,
            gid: 0,
            name: "note.txt",
            dstream: Some(DstreamArgs {
                size: 17,
                alloced_size: 4096,
            }),
        });
        assert_eq!(v.len(), 160);
        assert_eq!(u16::from_le_bytes(v[92..94].try_into().unwrap()), 2); // xf_num
        assert_eq!(u16::from_le_bytes(v[94..96].try_into().unwrap()), 56); // xf_used
                                                                           // xf[0] NAME, xf[1] DSTREAM (ascending order).
        assert_eq!(v[96], 4); // NAME type
        assert_eq!(v[100], 8); // DSTREAM type
        assert_eq!(v[101], 0x20); // DSTREAM x_flags
        assert_eq!(u16::from_le_bytes(v[102..104].try_into().unwrap()), 40); // DSTREAM size
                                                                             // Data area: NAME "note.txt\0" padded to 16, then j_dstream (40B).
        assert_eq!(&v[104..112], b"note.txt");
        let ds = 104 + 16; // data_base(104) + name_area(16)
        assert_eq!(u64::from_le_bytes(v[ds..ds + 8].try_into().unwrap()), 17); // size
        assert_eq!(
            u64::from_le_bytes(v[ds + 8..ds + 16].try_into().unwrap()),
            4096
        ); // alloced
        assert_eq!(
            u64::from_le_bytes(v[ds + 24..ds + 32].try_into().unwrap()),
            17
        ); // written
    }

    #[test]
    fn file_extent_matches_kernel_note_txt() {
        // key: jkey(type8,id18) + logical_addr 0; val: len 4096, phys 203.
        let k = build_file_extent_key(18, 0);
        assert_eq!(k, hex("1200000000000080 0000000000000000"));
        let val = build_file_extent_val(4096, 203);
        assert_eq!(
            val,
            hex("0010000000000000 cb00000000000000 0000000000000000")
        );
    }

    #[test]
    fn fstree_leaf_node_nonroot_roundtrip() {
        let bsz = 4096;
        let recs = vec![
            (build_inode_key(2), vec![1u8; 108]),
            (build_inode_key(16), vec![2u8; 116]),
        ];
        let node = build_fstree_node(0x407, 5, false, 0, &recs, None, bsz);
        // Non-root leaf: flags=LEAF only, level 0, o_type BTREE_NODE, subtype FSTREE.
        assert_eq!(u16::from_le_bytes(node[32..34].try_into().unwrap()), 0x2);
        assert_eq!(u16::from_le_bytes(node[34..36].try_into().unwrap()), 0);
        assert_eq!(u32::from_le_bytes(node[24..28].try_into().unwrap()), 0x3);
        assert_eq!(u32::from_le_bytes(node[28..32].try_into().unwrap()), 0x0e);
        // Fletcher valid + parses back with is_root=false (val area to block end).
        let stored = u64::from_le_bytes(node[0..8].try_into().unwrap());
        assert_eq!(stored, apfs_core::checksum::fletcher64(&node));
        let parsed = parse_cat_leaf(&node).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].val, vec![1u8; 108]);
        assert_eq!(parsed[1].val, vec![2u8; 116]);
    }

    #[test]
    fn fstree_internal_root_node() {
        let bsz = 4096;
        // Two child pointers: pivot key -> child virtual oid (8 bytes).
        let recs = vec![
            (build_inode_key(2), 0x407u64.to_le_bytes().to_vec()),
            (build_inode_key(100), 0x408u64.to_le_bytes().to_vec()),
        ];
        let node = build_fstree_node(
            0x404,
            4,
            true,
            1,
            &recs,
            Some(FstreeFooter {
                longest_key: 8,
                longest_val: 8,
                key_count: 140,
                node_count: 3,
            }),
            bsz,
        );
        // Root internal: flags=ROOT only (0x1), level 1, o_type BTREE (0x2).
        assert_eq!(u16::from_le_bytes(node[32..34].try_into().unwrap()), 0x1);
        assert_eq!(u16::from_le_bytes(node[34..36].try_into().unwrap()), 1);
        assert_eq!(u32::from_le_bytes(node[24..28].try_into().unwrap()), 0x2);
        let bti = bsz - 40;
        assert_eq!(
            u64::from_le_bytes(node[bti + 24..bti + 32].try_into().unwrap()),
            140
        );
        assert_eq!(
            u64::from_le_bytes(node[bti + 32..bti + 40].try_into().unwrap()),
            3
        );
        // Parse back (root) -> child oids recoverable from values.
        let parsed = parse_cat_leaf(&node).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            u64::from_le_bytes(parsed[0].val[0..8].try_into().unwrap()),
            0x407
        );
        assert_eq!(
            u64::from_le_bytes(parsed[1].val[0..8].try_into().unwrap()),
            0x408
        );
    }

    #[test]
    fn dstream_id_matches_kernel_note_txt() {
        // key: jkey(type6, id18); val: refcnt 1.
        assert_eq!(build_dstream_id_key(18), hex("1200000000000060"));
        assert_eq!(build_dstream_id_val(1), hex("01000000"));
    }

    #[test]
    fn phys_ext_matches_kernel_note_txt() {
        // key: jkey(type2, id=203); val: kind1|count1, owning 18, refcnt 1.
        let k = build_phys_ext_key(203);
        assert_eq!(k, hex("cb00000000000020"));
        let val = build_phys_ext_val(1, 18, 1);
        assert_eq!(val, hex("0100000000000010 1200000000000000 01000000"));
    }

    fn hex(s: &str) -> Vec<u8> {
        let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..clean.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Build an empty variable-kv root+leaf catalog node for tests.
    fn empty_cat_leaf(bsz: usize) -> Vec<u8> {
        let mut n = vec![0u8; bsz];
        n[32..34].copy_from_slice(&0x0003u16.to_le_bytes()); // ROOT | LEAF
                                                             // free_space spans the whole data..footer gap.
        let free_len = bsz - DATA_BASE - BTREE_INFO_SIZE;
        n[46..48].copy_from_slice(&(free_len as u16).to_le_bytes());
        n[48..50].copy_from_slice(&BTOFF_INVALID.to_le_bytes());
        n[52..54].copy_from_slice(&BTOFF_INVALID.to_le_bytes());
        let bti = bsz - BTREE_INFO_SIZE;
        n[bti..bti + 4].copy_from_slice(&0x42u32.to_le_bytes()); // bt_flags
        n[bti + 4..bti + 8].copy_from_slice(&(bsz as u32).to_le_bytes()); // node_size
        n[bti + 32..bti + 40].copy_from_slice(&1u64.to_le_bytes()); // node_count
        n
    }

    /// More records than the node holds is an error, not a node whose values
    /// were written over its keys (or a subtraction that wraps).
    #[test]
    fn catalog_insert_past_the_node_is_refused() {
        let bsz = 4096;
        let base = empty_cat_leaf(bsz);
        let big: Vec<(Vec<u8>, Vec<u8>)> = (16..24u64)
            .map(|ino| (build_inode_key(ino), vec![0u8; 600]))
            .collect();
        assert_eq!(
            insert_catalog_records(&base, big).err(),
            Some("node overflow")
        );
        // One value larger than the whole value area.
        let huge = vec![(build_inode_key(16), vec![0u8; bsz])];
        assert_eq!(
            insert_catalog_records(&base, huge).err(),
            Some("node overflow")
        );
        // A block too short to hold a header and a footer.
        let short = base.get(..64).expect("a 64-byte prefix");
        assert!(insert_catalog_records(short, Vec::new()).is_err());
    }

    #[test]
    fn a_record_larger_than_a_node_is_not_partitioned() {
        let small = (vec![1u8; 8], vec![0u8; 100]);
        let chunks = partition_records(&[small.clone(), small.clone()], 200).unwrap();
        assert_eq!(chunks.len(), 2, "two records of 116 bytes split at 200");
        let huge = (vec![1u8; 8], vec![0u8; 5000]);
        assert!(partition_records(&[small, huge], node_capacity(4096, false)).is_err());
    }

    #[test]
    fn catalog_insert_roundtrip_sorted() {
        let bsz = 4096;
        let base = empty_cat_leaf(bsz);
        // Seed with the root-dir inode (oid=2), then insert a DREC + file inode.
        let inode2 = (build_inode_key(2), vec![7u8; 108]);
        let seeded = insert_catalog_records(&base, vec![inode2]).unwrap();

        let drec = (
            build_drec_key(2, "hello.txt", true, true),
            build_drec_val(16, 123, DT_REG),
        );
        let file_inode = (
            build_inode_key(16),
            build_inode_val(&InodeValArgs {
                parent_id: 2,
                private_id: 16,
                now_ns: 123,
                mode: S_IFREG | 0o644,
                nlink_or_nchildren: 1,
                uid: 0,
                gid: 0,
                name: "hello.txt",
                dstream: None,
            }),
        );
        let out = insert_catalog_records(&seeded, vec![drec, file_inode]).unwrap();

        let recs = parse_cat_leaf(&out).unwrap();
        assert_eq!(recs.len(), 3);
        // Sorted: INODE oid=2, DIR_REC oid=2, INODE oid=16.
        let kt = |k: &[u8]| {
            let v = u64::from_le_bytes(k[0..8].try_into().unwrap());
            (v & 0x0FFF_FFFF_FFFF_FFFF, v >> 60)
        };
        assert_eq!(kt(&recs[0].key), (2, APFS_TYPE_INODE));
        assert_eq!(kt(&recs[1].key), (2, APFS_TYPE_DIR_REC));
        assert_eq!(kt(&recs[2].key), (16, APFS_TYPE_INODE));
        // DREC value intact.
        assert_eq!(
            u64::from_le_bytes(recs[1].val[0..8].try_into().unwrap()),
            16
        );
        // nkeys + footer.
        assert_eq!(u32::from_le_bytes(out[36..40].try_into().unwrap()), 3);
        let bti = bsz - BTREE_INFO_SIZE;
        assert_eq!(
            u64::from_le_bytes(out[bti + 24..bti + 32].try_into().unwrap()),
            3
        );
    }

    #[test]
    fn xattr_key_round_trips_through_apfs_core() {
        // Build with apfs-write, parse with apfs-core::xattr_key_name; the
        // name (stripped of trailing NUL) must round-trip.
        let k = build_xattr_key(42, XATTR_NAME_SYMLINK);
        // Header check: top 4 bits = APFS_TYPE_XATTR, low 60 = inode id.
        let hdr = u64::from_le_bytes(k[0..8].try_into().unwrap());
        assert_eq!(hdr >> 60, APFS_TYPE_XATTR);
        assert_eq!(hdr & 0x0FFF_FFFF_FFFF_FFFF, 42);
        // name_len@8 includes trailing NUL.
        let name_len = u16::from_le_bytes(k[8..10].try_into().unwrap());
        assert_eq!(name_len as usize, XATTR_NAME_SYMLINK.len() + 1);
        let got = apfs_core::xattr::xattr_key_name(&k).expect("xattr_key_name");
        assert_eq!(got, XATTR_NAME_SYMLINK);
    }

    #[test]
    fn xattr_val_embedded_round_trips_through_apfs_core() {
        // Build with apfs-write, parse with apfs-core::xattr_val: flags must
        // carry EMBEDDED, and xdata must equal what we put in.
        let target = b"/tmp/realfile.txt";
        let v = build_xattr_val_embedded(target);
        let (flags, xdata) = apfs_core::xattr::xattr_val(&v).expect("xattr_val");
        assert_eq!(flags & 0x0002, 0x0002, "EMBEDDED flag must be set");
        assert_eq!(xdata, target);
    }

    #[test]
    fn build_xattr_val_stream_layout() {
        // 52-byte layout: flags(u16) + xdata_len(u16) + j_xattr_dstream_t(48 bytes).
        let v = build_xattr_val_stream(0xDEAD, 5000, 8192);
        assert_eq!(v.len(), 52, "stream xattr val must be 52 bytes");
        let flags = u16::from_le_bytes(v[0..2].try_into().unwrap());
        assert_eq!(flags, XATTR_DATA_STREAM, "STREAM flag must be set");
        let xdata_len = u16::from_le_bytes(v[2..4].try_into().unwrap());
        assert_eq!(
            xdata_len, 48,
            "xdata_len must be 48 (sizeof j_xattr_dstream_t)"
        );
        let xattr_obj_id = u64::from_le_bytes(v[4..12].try_into().unwrap());
        assert_eq!(xattr_obj_id, 0xDEAD, "xattr_obj_id round-trips");
        let size = u64::from_le_bytes(v[12..20].try_into().unwrap());
        assert_eq!(size, 5000, "logical size round-trips");
        let alloced = u64::from_le_bytes(v[20..28].try_into().unwrap());
        assert_eq!(alloced, 8192, "alloced_size round-trips");
        // default_crypto_id, total_bytes_written, total_bytes_read must be zero.
        assert_eq!(
            &v[28..52],
            &[0u8; 24],
            "trailing dstream fields must be zero"
        );
    }

    #[test]
    fn build_xattr_val_stream_parses_with_apfs_core() {
        // Round-trip through apfs-core::xattr_val.
        let v = build_xattr_val_stream(99, 5000, 8192);
        let (flags, xdata) = apfs_core::xattr::xattr_val(&v).expect("xattr_val parse");
        assert_ne!(
            flags & apfs_core::xattr::XATTR_DATA_STREAM,
            0,
            "STREAM flag must be set"
        );
        let ds = apfs_core::xattr::XattrDstream::parse(xdata).expect("XattrDstream::parse");
        assert_eq!(ds.xattr_obj_id, 99, "xattr_obj_id");
        assert_eq!(ds.size, 5000, "logical size");
    }

    #[test]
    fn xattr_embedded_threshold_is_the_checkers_limit_less_the_record_header() {
        // The largest value embedded plus the record's length and flags
        // words is exactly the checker's limit on the whole value; measured
        // on macOS as 3800 embedded, 3801 a stream.
        assert_eq!(XATTR_EMBEDDED_THRESHOLD, 3800);
        assert_eq!(
            build_xattr_val_embedded(&[0u8; XATTR_EMBEDDED_THRESHOLD]).len(),
            apfs_core::xattr::XATTR_MAX_EMBEDDED_SIZE,
            "the largest embedded record is exactly the limit"
        );
    }

    #[test]
    fn a_long_attribute_name_lowers_the_embedded_value_limit() {
        // Measured on macOS: with a 127-byte name 3794 bytes are embedded and
        // 3795 a stream; with a 124-byte name the cut is 3797/3798.
        let long = format!("user.{}", "n".repeat(122));
        let key = build_xattr_key(10, &long).len();
        assert!(xattr_value_embeds(key, 3794));
        assert!(!xattr_value_embeds(key, 3795));
        let shorter = format!("user.{}", "n".repeat(119));
        let key = build_xattr_key(10, &shorter).len();
        assert!(xattr_value_embeds(key, 3797));
        assert!(!xattr_value_embeds(key, 3798));
        // A short name leaves the value limit where it was.
        let key = build_xattr_key(10, "user.sized").len();
        assert!(xattr_value_embeds(key, XATTR_EMBEDDED_THRESHOLD));
        assert!(!xattr_value_embeds(key, XATTR_EMBEDDED_THRESHOLD + 1));
    }

    #[test]
    fn symlink_constants_match_posix() {
        assert_eq!(S_IFLNK, 0o120000);
        assert_eq!(DT_LNK, 10);
        // S_IFLNK / S_IFREG / S_IFDIR are mutually exclusive in the high 4 bits.
        assert_ne!(S_IFLNK & 0o170000, S_IFREG & 0o170000);
        assert_ne!(S_IFLNK & 0o170000, S_IFDIR & 0o170000);
    }

    // --- serialization helper unit tests ---

    #[test]
    fn encode_jkey_type_in_top_4_bits_id_in_low_60() {
        // APFS_TYPE_INODE = 3 per spec.
        let v = encode_jkey(0xABC, APFS_TYPE_INODE);
        assert_eq!(v >> 60, APFS_TYPE_INODE, "type must sit in bits [63:60]");
        assert_eq!(
            v & 0x0FFF_FFFF_FFFF_FFFF,
            0xABC,
            "id must sit in bits [59:0]"
        );
    }

    #[test]
    fn build_inode_key_is_8_bytes_correct_type() {
        let k = build_inode_key(7);
        assert_eq!(k.len(), 8, "inode key is always 8 bytes");
        let hdr = u64::from_le_bytes(k.try_into().unwrap());
        assert_eq!(hdr >> 60, APFS_TYPE_INODE);
        assert_eq!(hdr & 0x0FFF_FFFF_FFFF_FFFF, 7);
    }

    #[test]
    fn build_file_extent_key_length_and_fields() {
        let k = build_file_extent_key(5, 0x1000);
        assert_eq!(k.len(), 16, "file-extent key is jkey(8) + logical_addr(8)");
        let hdr = u64::from_le_bytes(k[0..8].try_into().unwrap());
        assert_eq!(hdr >> 60, APFS_TYPE_FILE_EXTENT);
        assert_eq!(hdr & 0x0FFF_FFFF_FFFF_FFFF, 5);
        let logical = u64::from_le_bytes(k[8..16].try_into().unwrap());
        assert_eq!(logical, 0x1000);
    }

    #[test]
    fn build_file_extent_val_length_and_fields() {
        let v = build_file_extent_val(8192, 42);
        assert_eq!(v.len(), 24, "file-extent value is 24 bytes");
        // byte_len stored as-is (kind bits = 0)
        let len_and_kind = u64::from_le_bytes(v[0..8].try_into().unwrap());
        assert_eq!(len_and_kind & 0x00FF_FFFF_FFFF_FFFF, 8192);
        let phys_block = u64::from_le_bytes(v[8..16].try_into().unwrap());
        assert_eq!(phys_block, 42);
        let crypto_id = u64::from_le_bytes(v[16..24].try_into().unwrap());
        assert_eq!(crypto_id, 0, "crypto_id must be zero for unencrypted");
    }

    #[test]
    fn build_dstream_id_key_type_and_id() {
        let k = build_dstream_id_key(99);
        assert_eq!(k.len(), 8);
        let hdr = u64::from_le_bytes(k.try_into().unwrap());
        assert_eq!(hdr >> 60, APFS_TYPE_DSTREAM_ID);
        assert_eq!(hdr & 0x0FFF_FFFF_FFFF_FFFF, 99);
    }

    #[test]
    fn build_dstream_id_val_is_4_byte_refcnt() {
        let v = build_dstream_id_val(1);
        assert_eq!(v.len(), 4);
        assert_eq!(u32::from_le_bytes(v.try_into().unwrap()), 1);
        let v2 = build_dstream_id_val(255);
        assert_eq!(u32::from_le_bytes(v2.try_into().unwrap()), 255);
    }

    #[test]
    fn build_phys_ext_key_type_and_id() {
        let k = build_phys_ext_key(17);
        assert_eq!(k.len(), 8);
        let hdr = u64::from_le_bytes(k.try_into().unwrap());
        assert_eq!(hdr >> 60, APFS_TYPE_EXTENT);
        assert_eq!(hdr & 0x0FFF_FFFF_FFFF_FFFF, 17);
    }

    #[test]
    fn build_phys_ext_val_encodes_kind_and_count() {
        let v = build_phys_ext_val(3, 0xBEEF, 1);
        assert_eq!(v.len(), 20, "phys-ext value is 20 bytes");
        let len_and_kind = u64::from_le_bytes(v[0..8].try_into().unwrap());
        // APFS_KIND_NEW = 1 in bits [63:60]
        assert_eq!(len_and_kind >> 60, APFS_KIND_NEW);
        assert_eq!(len_and_kind & 0x0FFF_FFFF_FFFF_FFFF, 3);
        let owning_obj_id = u64::from_le_bytes(v[8..16].try_into().unwrap());
        assert_eq!(owning_obj_id, 0xBEEF);
        let refcnt = u32::from_le_bytes(v[16..20].try_into().unwrap());
        assert_eq!(refcnt, 1);
    }

    #[test]
    fn build_drec_val_length_and_fields() {
        let v = build_drec_val(42, 1_000_000, DT_REG);
        assert_eq!(v.len(), 18, "drec value is 18 bytes");
        let file_id = u64::from_le_bytes(v[0..8].try_into().unwrap());
        assert_eq!(file_id, 42);
        let date_added = u64::from_le_bytes(v[8..16].try_into().unwrap());
        assert_eq!(date_added, 1_000_000);
        let flags = u16::from_le_bytes(v[16..18].try_into().unwrap());
        assert_eq!(flags, DT_REG);
    }

    // -----------------------------------------------------------------------
    // M11 incremental B-tree primitive tests.
    // -----------------------------------------------------------------------

    /// Build a minimal variable-kv FSTREE leaf node (ROOT|LEAF) with the
    /// given records pre-inserted.  Used as a fixture for primitive tests.
    fn make_leaf_node(recs: &[(Vec<u8>, Vec<u8>)], bsz: usize) -> Vec<u8> {
        build_fstree_node(
            1,    // oid
            5,    // xid
            true, // is_root
            0,    // level (leaf)
            recs,
            Some(FstreeFooter {
                longest_key: recs.iter().map(|(k, _)| k.len() as u32).max().unwrap_or(0),
                longest_val: recs.iter().map(|(_, v)| v.len() as u32).max().unwrap_or(0),
                key_count: recs.len() as u64,
                node_count: 1,
            }),
            bsz,
        )
    }

    #[test]
    fn leaf_remove_record_removes_existing_key() {
        let bsz = 4096usize;
        let k1 = build_drec_key(2, "alpha", true, true);
        let k2 = build_drec_key(2, "beta", true, true);
        let v1 = build_drec_val(10, 0, 8);
        let v2 = build_drec_val(11, 0, 8);
        // Sort them as the node builder would.
        let mut recs = vec![(k1.clone(), v1.clone()), (k2.clone(), v2.clone())];
        recs.sort_by(|a, b| cat_key_cmp(&a.0, &b.0));
        let mut node = make_leaf_node(&recs, bsz);

        // Debug: show layout before remove.
        let pre = parse_cat_leaf(&node).unwrap();
        assert_eq!(pre.len(), 2, "pre-remove: two records");
        let pre_keys: Vec<_> = pre.iter().map(|r| r.key.clone()).collect();
        assert!(pre_keys.contains(&k1), "k1 must be in initial node");
        assert!(pre_keys.contains(&k2), "k2 must be in initial node");

        assert!(
            leaf_remove_record(&mut node, &k1),
            "should find and remove k1"
        );
        let parsed = parse_cat_leaf(&node).unwrap();
        assert_eq!(parsed.len(), 1, "one record should remain");
        assert_eq!(parsed[0].key, k2);
        assert_eq!(parsed[0].val, v2);
        // nkeys header must be 1.
        assert_eq!(rd_u32(&node, 36), 1);
    }

    #[test]
    fn leaf_remove_record_returns_false_for_missing_key() {
        let bsz = 4096usize;
        let k1 = build_drec_key(2, "alpha", true, true);
        let k_missing = build_drec_key(2, "gamma", true, true);
        let v1 = build_drec_val(10, 0, 8);
        let recs = vec![(k1.clone(), v1.clone())];
        let mut node = make_leaf_node(&recs, bsz);

        assert!(!leaf_remove_record(&mut node, &k_missing));
        // Node must be unmodified.
        assert_eq!(rd_u32(&node, 36), 1);
    }

    #[test]
    fn leaf_insert_record_round_trips_via_parse() {
        let bsz = 4096usize;
        let k1 = build_drec_key(2, "alpha", true, true);
        let k2 = build_drec_key(2, "gamma", true, true);
        let v1 = build_drec_val(10, 0, 8);
        let v2 = build_drec_val(12, 0, 8);

        // Determine sort order first so our assertions match.
        let (k_first, v_first, k_second, v_second) =
            if cat_key_cmp(&k1, &k2) == core::cmp::Ordering::Less {
                (k1.clone(), v1.clone(), k2.clone(), v2.clone())
            } else {
                (k2.clone(), v2.clone(), k1.clone(), v1.clone())
            };

        // Start with the second-sorted key only.
        let recs = vec![(k_second.clone(), v_second.clone())];
        let mut node = make_leaf_node(&recs, bsz);

        // Insert the first-sorted key - it must go before the existing entry.
        assert!(
            leaf_insert_record(&mut node, &k_first, &v_first),
            "insert should succeed"
        );
        let parsed = parse_cat_leaf(&node).unwrap();
        assert_eq!(parsed.len(), 2);
        // Must be in cat_key_cmp sorted order.
        assert_eq!(
            parsed[0].key, k_first,
            "first parsed key should be sort-first"
        );
        assert_eq!(parsed[0].val, v_first);
        assert_eq!(
            parsed[1].key, k_second,
            "second parsed key should be sort-second"
        );
        assert_eq!(parsed[1].val, v_second);
    }

    #[test]
    fn leaf_insert_returns_false_when_full() {
        let bsz = 4096usize;
        // Fill the node to near-capacity with large records.
        let cap = node_capacity(bsz, true);
        // Each record: 8 (TOC) + key.len() + val.len().  Use val of ~100 B.
        let val_bytes = vec![0xABu8; 100];
        let mut recs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut used = 0usize;
        let mut idx = 0u64;
        loop {
            let k = build_inode_key(1000 + idx);
            let needed = 8 + k.len() + val_bytes.len();
            if used + needed + 20 > cap {
                break;
            }
            used += needed;
            recs.push((k, val_bytes.clone()));
            idx += 1;
        }
        let mut node = make_leaf_node(&recs, bsz);
        // One more big record should not fit.
        let extra_k = build_inode_key(9999);
        let extra_v = vec![0u8; 200];
        assert!(
            !leaf_insert_record(&mut node, &extra_k, &extra_v),
            "should report no space"
        );
    }

    #[test]
    fn leaf_remove_then_insert_preserves_sort_order() {
        let bsz = 4096usize;
        let k_old = build_drec_key(5, "old-name", true, true);
        let k_new = build_drec_key(5, "new-name", true, true);
        let k_other = build_drec_key(5, "middle", true, true);
        let v = build_drec_val(42, 0, 8);
        // k_other sorts between k_new and k_old (hash-dependent, but we just
        // verify parse returns a sorted list after mutation).
        let recs: Vec<(Vec<u8>, Vec<u8>)> = {
            let mut r = vec![(k_old.clone(), v.clone()), (k_other.clone(), v.clone())];
            r.sort_by(|a, b| cat_key_cmp(&a.0, &b.0));
            r
        };
        let mut node = make_leaf_node(&recs, bsz);
        assert!(leaf_remove_record(&mut node, &k_old));
        assert!(leaf_insert_record(&mut node, &k_new, &v));
        let parsed = parse_cat_leaf(&node).unwrap();
        assert_eq!(parsed.len(), 2);
        // Verify sorted order.
        for w in parsed.windows(2) {
            assert!(
                cat_key_cmp(&w[0].key, &w[1].key) != core::cmp::Ordering::Greater,
                "parsed records must be in sorted order"
            );
        }
    }

    #[test]
    fn build_xattr_key_length_name_nul_terminated() {
        let k = build_xattr_key(10, "myattr");
        // jkey(8) + name_len(2) + "myattr\0"(7) = 17 bytes
        assert_eq!(k.len(), 17);
        let hdr = u64::from_le_bytes(k[0..8].try_into().unwrap());
        assert_eq!(hdr >> 60, APFS_TYPE_XATTR);
        assert_eq!(hdr & 0x0FFF_FFFF_FFFF_FFFF, 10);
        let name_len = u16::from_le_bytes(k[8..10].try_into().unwrap());
        assert_eq!(name_len as usize, 7); // "myattr" + NUL
        assert_eq!(k[10..16], *b"myattr");
        assert_eq!(k[16], 0, "key must be NUL-terminated");
    }

    #[test]
    fn build_xattr_val_embedded_flags_and_data() {
        let data = b"hello";
        let v = build_xattr_val_embedded(data);
        // flags(2) + xdata_len(2) + data
        assert_eq!(v.len(), 4 + data.len());
        // XATTR_DATA_EMBEDDED flag must be set
        let flags = u16::from_le_bytes(v[0..2].try_into().unwrap());
        assert_ne!(flags & XATTR_DATA_EMBEDDED, 0, "EMBEDDED flag must be set");
        let xdata_len = u16::from_le_bytes(v[2..4].try_into().unwrap());
        assert_eq!(xdata_len as usize, data.len());
        assert_eq!(&v[4..], data);
    }

    // -----------------------------------------------------------------------
    // Crafted-tree guards: the omap and catalog walkers over an in-memory
    // device holding hand-built nodes.
    // -----------------------------------------------------------------------

    use crate::txn::test_support::MemDev;

    /// The header fields the omap node builders inherit from the live root:
    /// a physical B-tree root of the object map with the table reserve
    /// macOS uses.
    fn omap_template() -> Vec<u8> {
        let mut t = vec![0u8; 4096];
        wr_u32(&mut t, 24, OBJ_PHYSICAL | OBJECT_TYPE_BTREE);
        wr_u32(&mut t, 28, OBJECT_TYPE_OMAP);
        wr_u16(&mut t, 32, BTNODE_ROOT | BTNODE_LEAF | BTNODE_FIXED_KV_SIZE);
        wr_u16(&mut t, 42, 448);
        t
    }

    /// A single-node volume omap mapping each `(oid, paddr)` at xid 1.
    fn omap_root(entries: &[(u64, u64)]) -> Vec<u8> {
        let triples: Vec<OmapRec> = entries
            .iter()
            .map(|&(o, p)| OmapRec::live(o, 1, p, 4096))
            .collect();
        build_omap_node(&omap_template(), &triples, 50, 1, 4096).expect("fits one node")
    }

    // The single-node builder describes the node it makes: a root leaf of
    // fixed-size records whatever the template's flags said, one node, 16-byte
    // keys and values, and no room taken from the entries.
    #[test]
    fn single_node_omap_builder_describes_its_own_node() {
        let mut template = omap_template();
        wr_u16(&mut template, 32, BTNODE_FIXED_KV_SIZE); // neither root nor leaf
        wr_u16(&mut template, 34, 3); // a level it is not
                                      // A tombstone and a live mapping: each keeps its flags and size.
        let entries = [
            OmapRec {
                oid: 1028,
                xid: 4,
                paddr: 700,
                flags: 0x1,
                size: 4096,
            },
            OmapRec::live(1027, 2, 600, 4096),
        ];
        let node = build_omap_node(&template, &entries, 50, 9, 4096).expect("two entries fit");
        let back = parse_omap_entries(&node, 4096).expect("the node parses");
        assert_eq!(back, vec![OmapRec::live(1027, 2, 600, 4096), entries[0]]);
        assert_eq!(
            rd_u16(&node, 32),
            BTNODE_ROOT | BTNODE_LEAF | BTNODE_FIXED_KV_SIZE
        );
        assert_eq!(rd_u16(&node, 34), 0, "a single node is a leaf at level 0");
        assert_eq!(rd_u32(&node, 36), 2);
        assert_eq!(rd_u64(&node, 8), 50);
        assert_eq!(rd_u64(&node, 16), 9);
        let bti = 4096 - BTREE_INFO_SIZE;
        assert_eq!(rd_u32(&node, bti + 16), 16, "longest key");
        assert_eq!(rd_u32(&node, bti + 20), 16, "longest value");
        assert_eq!(rd_u64(&node, bti + 24), 2, "key count");
        assert_eq!(rd_u64(&node, bti + 32), 1, "node count");
        // Sorted by (oid, xid): 1027 first.
        let key_area = DATA_BASE + rd_u16(&node, 42) as usize;
        assert_eq!(rd_u64(&node, key_area), 1027);
        assert_eq!(rd_u64(&node, key_area + 16), 1028);
    }

    // The records the writer builds are the shape the extent-reference tree
    // holds; anything else is refused before it can become the footer's
    // longest key or value.
    #[test]
    fn extent_reference_records_must_have_the_trees_shape() {
        let key = build_phys_ext_key(500);
        let val = build_phys_ext_val(3, 1028, 1);
        assert_eq!((key.len(), val.len()), (PHYS_EXT_KEY_LEN, PHYS_EXT_VAL_LEN));
        assert!(check_extref_record_sizes(&[(key.clone(), val.clone())]).is_ok());
        let mut long_val = val.clone();
        long_val.extend_from_slice(&[0u8; 4]);
        assert!(check_extref_record_sizes(&[(key.clone(), long_val)]).is_err());
        assert!(check_extref_record_sizes(&[(vec![0u8; 16], val)]).is_err());
    }

    // Entries that do not fit one node are refused instead of overlapping.
    #[test]
    fn single_node_omap_builder_refuses_more_entries_than_fit() {
        let triples: Vec<OmapRec> = (0..200)
            .map(|i| OmapRec::live(1000 + i, 1, 500 + i, 4096))
            .collect();
        let err = build_omap_node(&omap_template(), &triples, 50, 1, 4096);
        assert!(
            matches!(err, Err(TxnError::OmapParse(_))),
            "got {:?}",
            err.map(|n| n.len())
        );
        let just_fits: Vec<OmapRec> = (0..110)
            .map(|i| OmapRec::live(1000 + i, 1, 500 + i, 4096))
            .collect();
        assert!(build_omap_node(&omap_template(), &just_fits, 50, 1, 4096).is_ok());
    }

    /// The 16-byte omap key `{oid, xid 1}`.
    fn omap_key(oid: u64) -> [u8; 16] {
        let mut k = [0u8; 16];
        k[..8].copy_from_slice(&oid.to_le_bytes());
        k[8..].copy_from_slice(&1u64.to_le_bytes());
        k
    }

    /// A non-root fstree node at `level`; index values are 8-byte child oids.
    fn fstree_node(oid: u64, level: u16, recs: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        build_fstree_node(oid, 1, false, level, recs, None, 4096)
    }

    fn child_ref(oid: u64) -> Vec<u8> {
        oid.to_le_bytes().to_vec()
    }

    /// A transaction over a 64-block device holding `blocks`.
    fn txn_over(blocks: &[(u64, Vec<u8>)]) -> Transaction<MemDev> {
        let mut dev = MemDev::new(64);
        for (bno, block) in blocks {
            dev.put(*bno, block);
        }
        Transaction::over(dev)
    }

    /// A record count beyond what the table of contents holds must be an
    /// error before it sizes anything or drives an offset.
    #[test]
    fn parse_omap_entries_refuses_a_record_count_beyond_the_table() {
        let mut node = omap_root(&[(0x500, 10)]);
        wr_u32(&mut node, 36, 0x7FFF_FFFF);
        update_checksum_in_place(&mut node);
        assert!(parse_omap_entries(&node, 4096).is_err());
    }

    /// A value offset reaching back past the key area points into the table
    /// or the header, not at a value.
    #[test]
    fn omap_node_readers_refuse_a_value_offset_past_the_node() {
        let mut node = omap_root(&[(0x500, 10)]);
        wr_u16(&mut node, DATA_BASE + 2, 0xFFFF); // v_off of record 0
        update_checksum_in_place(&mut node);
        assert!(omap_lookup(&node, 0x500, 4096).is_err());
        assert!(parse_omap_entries(&node, 4096).is_err());
        let mut patched = node.clone();
        assert!(patch_omap_entry(&mut patched, 0x500, 11, 2, 4096).is_err());
    }

    /// An omap index node whose child pointer leads back to itself is a
    /// cycle; the walk must report it rather than recurse until the stack
    /// runs out.
    #[test]
    fn walk_omap_tree_refuses_a_cycle() {
        let entries = [(omap_key(0x500), child_ref(30))];
        let index = serialize_omap_node(&omap_template(), 1, true, &entries, 30, 1, 1, 1, 4096);
        let mut txn = txn_over(&[(30, index.clone())]);
        let err =
            collect_omap_tree(&mut txn, &index, 30, 4096).expect_err("a cycle must be an error");
        assert!(matches!(err, TxnError::OmapParse(_)), "got: {err:?}");
    }

    /// A child one level down is the only child an omap index node may name.
    #[test]
    fn omap_walkers_refuse_a_child_at_the_wrong_level() {
        let leaf = serialize_omap_node(
            &omap_template(),
            0,
            false,
            &[(omap_key(0x500), vec![0u8; 16])],
            31,
            1,
            1,
            1,
            4096,
        );
        // A level-2 root pointing straight at the leaf.
        let index = serialize_omap_node(
            &omap_template(),
            2,
            true,
            &[(omap_key(0x500), child_ref(31))],
            30,
            1,
            1,
            2,
            4096,
        );
        let mut txn = txn_over(&[(30, index.clone()), (31, leaf)]);
        assert!(collect_omap_tree(&mut txn, &index, 30, 4096).is_err());
        assert!(omap_resolve(&mut txn, &index, 0x500, u64::MAX, 4096).is_err());
    }

    /// A catalog index node whose child oid resolves back to itself is a
    /// cycle; collecting the tree must report it, not recurse without end.
    #[test]
    fn collect_fstree_refuses_a_cycle() {
        let omap = omap_root(&[(0x500, 10)]);
        let node = fstree_node(0x500, 1, &[(build_inode_key(2), child_ref(0x500))]);
        let mut txn = txn_over(&[(10, node)]);
        let err =
            collect_fstree(&mut txn, &omap, 0x500, 4096).expect_err("a cycle must be an error");
        assert!(matches!(err, TxnError::VolumeParse(_)), "got: {err:?}");
    }

    /// A child sits one level below its parent; a leaf directly under a
    /// level-2 node means the pointers do not describe one tree.
    #[test]
    fn collect_fstree_refuses_a_child_at_the_wrong_level() {
        let omap = omap_root(&[(0x500, 10), (0x501, 11)]);
        let top = fstree_node(0x500, 2, &[(build_inode_key(2), child_ref(0x501))]);
        let leaf = fstree_node(0x501, 0, &[(build_inode_key(2), vec![7u8; 16])]);
        let mut txn = txn_over(&[(10, top), (11, leaf)]);
        assert!(collect_fstree(&mut txn, &omap, 0x500, 4096).is_err());
        let mut reached = std::collections::HashSet::new();
        assert!(collect_fstree_oids(&mut txn, &omap, 0x500, 4096, &mut reached, 0).is_err());
    }

    /// One insert through the path-local writer into the catalog at 0x500.
    fn path_insert(txn: &mut Transaction<MemDev>, omap: &[u8]) -> Result<u64, TxnError> {
        let mut batch = EditBatch::new();
        batch.push(build_inode_key(3), EditOp::Insert(vec![7u8; 16]))?;
        CowTree::new(Shape::Catalog { omap_root: omap }, 99, 4096).apply(txn, 0x500, batch)
    }

    /// The path-local writer follows the same pointers and must stop on a
    /// cycle too, instead of looping forever.
    #[test]
    fn path_edit_refuses_a_cycle() {
        let omap = omap_root(&[(0x500, 10)]);
        let node = fstree_node(0x500, 1, &[(build_inode_key(2), child_ref(0x500))]);
        let mut txn = txn_over(&[(10, node)]);
        let err = path_insert(&mut txn, &omap).expect_err("a cycle must be an error");
        assert!(matches!(err, TxnError::VolumeParse(_)), "got: {err:?}");
    }

    #[test]
    fn path_edit_refuses_a_child_at_the_wrong_level() {
        let omap = omap_root(&[(0x500, 10), (0x501, 11)]);
        let top = fstree_node(0x500, 2, &[(build_inode_key(2), child_ref(0x501))]);
        let leaf = fstree_node(0x501, 0, &[(build_inode_key(2), vec![7u8; 16])]);
        let mut txn = txn_over(&[(10, top), (11, leaf)]);
        assert!(path_insert(&mut txn, &omap).is_err());
    }

    /// An extentref index node naming the same child twice would have that
    /// child's records collected twice; a repeated child is an error.
    #[test]
    fn collect_extref_refuses_a_repeated_child() {
        let leaf = fstree_node(21, 0, &[(build_inode_key(2), vec![7u8; 16])]);
        let index = fstree_node(
            20,
            1,
            &[
                (build_inode_key(2), child_ref(21)),
                (build_inode_key(3), child_ref(21)),
            ],
        );
        let mut txn = txn_over(&[(20, index), (21, leaf)]);
        let err = collect_extref_records(&mut txn, 20, 4096)
            .expect_err("a repeated child must be an error");
        assert!(matches!(err, TxnError::VolumeParse(_)), "got: {err:?}");
    }

    #[test]
    fn collect_extref_refuses_a_child_at_the_wrong_level() {
        let leaf = fstree_node(21, 0, &[(build_inode_key(2), vec![7u8; 16])]);
        let index = fstree_node(20, 2, &[(build_inode_key(2), child_ref(21))]);
        let mut txn = txn_over(&[(20, index), (21, leaf)]);
        assert!(collect_extref_records(&mut txn, 20, 4096).is_err());
    }
}
