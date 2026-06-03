//! apfs-write file-creation path (M7b).
//!
//! Builds catalog records (inode + directory entry) and inserts them into the
//! volume's fsroot B-tree via copy-on-write, so a new empty file becomes
//! visible to macOS and passes `fsck_apfs`.
//!
//! Verification-first: every mutation is proven on a scratch image with
//! fsck + mount-readback + an independent Python re-decode.

/// CRC32C (Castagnoli) reflected polynomial.
const CRC32C_POLY: u32 = 0x82F6_3B78;

/// Compute one CRC32C table entry (reflected).
const fn crc32c_byte(mut c: u32) -> u32 {
    let mut i = 0;
    while i < 8 {
        c = if c & 1 != 0 {
            (c >> 1) ^ CRC32C_POLY
        } else {
            c >> 1
        };
        i += 1;
    }
    c
}

/// CRC32C over a byte slice, reflected, init 0xFFFFFFFF, NO final xor.
fn crc32c(buf: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in buf {
        let idx = ((crc ^ b as u32) & 0xFF) as u8;
        crc = (crc >> 8) ^ crc32c_byte(idx as u32);
    }
    crc
}

/// APFS directory-entry name hash (low 22 bits).
///
/// [the APFS spec LOCKED, empirical: matched baseline scratch values
///  private-dir=0x2b29a3, root=0x2d9c79]
///
/// Algorithm: CRC32C (init 0xFFFFFFFF, no final xor) over the UTF-32-LE
/// encoding of the case-folded name (lowercase on case-insensitive volumes),
/// take the low 22 bits. The name is NOT NUL-terminated for hashing.
/// Compute the APFS directory-entry name hash.
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
    use unicode_normalization::UnicodeNormalization;
    // Step 1: NFD decomposition when the volume requires it.
    let normalized: String = if normalize {
        name.nfd().collect()
    } else {
        name.to_string()
    };
    // Step 2: case-fold when the volume requires it. (Rust's to_lowercase is
    // close to Unicode CaseFolding.txt for most scripts; full-fidelity locale
    // edge cases - Turkish dotted-I, German ß, Greek final-sigma - should use
    // an icu_casemap-style fold for case-INsensitive volumes. apfs-spec
    // Q12: noted as a hardening follow-up; case-sensitive volumes are the
    // common default and unaffected.)
    let folded = if case_fold {
        normalized.to_lowercase()
    } else {
        normalized
    };
    // Step 3+4: UTF-32-LE per scalar, then CRC32C over those bytes (no NUL).
    let mut buf = Vec::with_capacity(folded.chars().count() * 4);
    for ch in folded.chars() {
        buf.extend_from_slice(&(ch as u32).to_le_bytes());
    }
    crc32c(&buf) & 0x003F_FFFF
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
const XF_DATA_DEPENDENT: u8 = 0x02; // x_flags for the NAME xfield

/// File mode bits.
pub const S_IFREG: u16 = 0o100000;
pub const S_IFDIR: u16 = 0o040000;
pub const S_IFLNK: u16 = 0o120000;

/// j_xattr_flags bit for an inline (embedded) value.
const XATTR_DATA_EMBEDDED: u16 = 0x0002;

/// j_xattr_flags bit for a stream (extent-backed) value.
const XATTR_DATA_STREAM: u16 = 0x0001;

/// Threshold above which xattr data is stored as a stream instead of inline.
/// Conservative choice: Apple uses ~3804 but we use 2048 to leave B-tree node
/// headroom. Values ≤ this limit use the embedded (inline) encoding;
/// values above use the dstream + FILE_EXTENT encoding.
/// [Derived from APFS spec j_xattr_val layout + linux-apfs-rw xattr.c]
pub const XATTR_EMBEDDED_THRESHOLD: usize = 2048;

/// Reserved extended-attribute name used by APFS to store a symlink target.
pub const XATTR_NAME_SYMLINK: &str = "com.apple.fs.symlink";

/// Default inode `internal_flags` the kernel stamps on a freshly created
/// object (empirical: 0x8000 on both dir and regular-file inodes).
const INODE_DEFAULT_INTERNAL_FLAGS: u64 = 0x8000;

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
/// x_type order (NAME before DSTREAM) as fsck requires.
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
    wr_bytes(&mut v, 96, &[INO_EXT_TYPE_NAME, XF_DATA_DEPENDENT]);
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

use crate::txn::{update_checksum_in_place, Transaction, TxnError, OBJECT_TYPE_FS};
use apfs_core::block_device::WritableBlockDevice;

// Volume superblock field offsets.
const VSBI_FS_ALLOC_COUNT: usize = 0x58;
const VSBI_NUM_DIRECTORIES: usize = 0xC0;
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

// Inode value field offsets (subset needed for parent-dir update).
const INODE_MOD_TIME: usize = 24;
const INODE_CHANGE_TIME: usize = 32;
const INODE_NCHILDREN: usize = 56;

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
    let mut v = Vec::with_capacity(4 + xdata.len());
    v.extend_from_slice(&XATTR_DATA_EMBEDDED.to_le_bytes());
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
/// - If `value.len() <= XATTR_EMBEDDED_THRESHOLD`: stores inline (embedded).
/// - If `value.len() > XATTR_EMBEDDED_THRESHOLD`: allocates a private dstream,
///   writes data blocks, and stores a stream xattr record.
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
pub fn set_xattr<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    inode_id: u64,
    name: &str,
    value: &[u8],
) -> Result<(), TxnError> {
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();

    // Read the volume omap node.
    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;

    // Collect all fstree catalog records.
    let all = collect_fstree(txn, &omap_node, rd_u64(vsb_raw, VSBI_ROOT_TREE_OID), bsz)?;

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

    // Collect old phys blocks to free (for stream xattr replace/delete).
    let old_extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    let mut freed_block_count = 0u64;
    // Keys of old owned phys_ext records to remove from the extref tree.
    let mut extref_remove_keys: Vec<Vec<u8>> = Vec::new();
    let mut owned_runs_to_free: Vec<(u64, u64)> = Vec::new();

    if !old_stream_extents.is_empty() {
        // Collect live extref keys so we can distinguish owned vs. snapshot-pinned.
        let (live_recs, _) = collect_extref_records(txn, old_extref_paddr, bsz)?;
        let live_keys: Vec<Vec<u8>> = live_recs.into_iter().map(|(k, _)| k).collect();
        for &(_, phys, count) in &old_stream_extents {
            let key = build_phys_ext_key(phys);
            if live_keys.contains(&key) {
                extref_remove_keys.push(key);
                owned_runs_to_free.push((phys, count));
                freed_block_count += count;
            }
        }
    }

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

    if value.len() <= XATTR_EMBEDDED_THRESHOLD {
        // Embedded path: inline data, no extents.
        new_records.push((target_xattr_key, build_xattr_val_embedded(value)));
    } else {
        // Stream path: allocate a private dstream id, write data blocks, build extents.
        let stream_id = txn.alloc_oid();
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
        // DSTREAM_ID record for the stream's private id.
        new_records.push((build_dstream_id_key(stream_id), build_dstream_id_val(1)));
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
    let extref_changed = !extref_inserts.is_empty() || !extref_remove_keys.is_empty();
    let (new_extref_paddr, old_extref_padrs, extref_node_delta) = if extref_changed {
        rewrite_extref_tree(
            txn,
            old_extref_paddr,
            new_xid,
            extref_inserts,
            &extref_remove_keys,
            &[],
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

    // Rewrite the fstree.
    let (new_omap_tree_paddr, node_delta) = rewrite_fstree(
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
    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = vol_omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

    // Free replaced metadata BEFORE writing fs_alloc_count so frm_correction
    // (sm_fq-pinned block count) can be folded into alloc_delta. [#149 fix]
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &old_extref_padrs,
        false,
        bsz,
    )?;

    // Update and stage the volume superblock.
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
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
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;

    let all = collect_fstree(txn, &omap_node, rd_u64(vsb_raw, VSBI_ROOT_TREE_OID), bsz)?;

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

    // Free old phys extents if stream.
    let old_extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    let mut freed_block_count = 0u64;

    // Collect extref remove keys + data blocks to free (if stream xattr). [#151]
    let mut extref_remove_keys: Vec<Vec<u8>> = Vec::new();
    let mut owned_runs_to_free: Vec<(u64, u64)> = Vec::new();
    if !old_stream_extents.is_empty() {
        let (live_recs, _) = collect_extref_records(txn, old_extref_paddr, bsz)?;
        let live_keys: Vec<Vec<u8>> = live_recs.into_iter().map(|(k, _)| k).collect();
        for &(_, phys, count) in &old_stream_extents {
            let key = build_phys_ext_key(phys);
            if live_keys.contains(&key) {
                extref_remove_keys.push(key);
                owned_runs_to_free.push((phys, count));
                freed_block_count += count;
            }
        }
    }

    // Rewrite extref tree (multi-node capable). [#151]
    let (new_extref_paddr, old_extref_padrs, extref_node_delta) = if !extref_remove_keys.is_empty()
    {
        rewrite_extref_tree(
            txn,
            old_extref_paddr,
            new_xid,
            vec![],
            &extref_remove_keys,
            &[],
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

    let (new_omap_tree_paddr, node_delta) = rewrite_fstree(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        vec![],
        &remove_keys,
        None,
        now,
        bsz,
    )?;

    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = vol_omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &old_extref_padrs,
        false,
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
/// path bytes. (Cross-checked against linux-apfs-rw symlink.c.) The DREC
/// uses `dt_type = DT_LNK (10)`.
///
/// Returns the new inode number.
pub fn create_symlink<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    parent_ino: u64,
    name: &str,
    target: &[u8],
) -> Result<u64, TxnError> {
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();

    let new_ino = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let _norm_flag_observed = incompat & APFS_INCOMPAT_NORMALIZATION_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;

    let drec = (
        build_drec_key(parent_ino, name, case_fold, normalize),
        build_drec_val(new_ino, now, DT_LNK),
    );
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
    let xattr = (
        build_xattr_key(new_ino, XATTR_NAME_SYMLINK),
        build_xattr_val_embedded(target),
    );

    let (new_omap_tree_paddr, node_delta) = rewrite_fstree(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        vec![drec, inode, xattr],
        &[],
        Some((parent_ino, 1)),
        now,
        bsz,
    )?;

    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = vol_omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    let frm_correction =
        free_replaced_metadata(txn, vsb_raw, vol_omap_raw, &omap_node, &[], false, bsz)?;

    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_NEXT_OBJ_ID, new_ino + 1);
    let num_files = rd_u64(&new_vsb, VSBI_NUM_FILES);
    wr_u64(&mut new_vsb, VSBI_NUM_FILES, num_files + 1);
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
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();

    let new_ino = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);
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
    // --- Build the new DREC + INODE records and rebuild the FSTREE (splits
    //     into a multi-node b-tree automatically when a leaf would overflow). ---
    let drec = (
        build_drec_key(parent_ino, name, case_fold, normalize),
        build_drec_val(new_ino, now, dt_type),
    );
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
    let (new_omap_tree_paddr, node_delta) = rewrite_fstree(
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
    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = vol_omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr); // om_tree_oid
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

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
        free_replaced_metadata(txn, vsb_raw, vol_omap_raw, &omap_node, &[], false, bsz)?;

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
    let bsz = txn.nx.block_size as usize;
    if content.is_empty() {
        return Err(TxnError::SpacemanParse(
            "write_file: content must be non-empty (use create_file for empty)".into(),
        ));
    }
    let num_blocks = content.len().div_ceil(bsz);
    let new_xid = txn.xid;
    let now = now_ns();

    let new_ino = rd_u64(vsb_raw, VSBI_NEXT_OBJ_ID);
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
    if drec_exists(txn, &omap_node, root_tree_oid, &drec_key, bsz)? {
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
            build_inode_val(&InodeValArgs {
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
            }),
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
    let (new_omap_tree_paddr, node_delta) = rewrite_fstree(
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
    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = vol_omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

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
        false,
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
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
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
    let all = collect_fstree(txn, &omap_node, root_tree_oid, bsz)?;

    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
    let file_id = all
        .iter()
        .find(|(k, _)| *k == drec_key)
        .map(|(_, v)| rd_u64(v, 0))
        .ok_or_else(|| TxnError::SpacemanParse("name not found in directory".into()))?;

    let mut remove_keys: Vec<Vec<u8>> = vec![drec_key];
    // Each extent is (phys_start, block_count) - a FILE_EXTENT can span many
    // blocks, so we must free the WHOLE run, not just its first block.
    let mut data_blocks: Vec<(u64, u64)> = Vec::new();
    let mut is_dir = false;
    let mut dir_nchildren = 0u32;
    for (k, v) in &all {
        let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
        let ty = rd_u64(k, 0) >> 60;
        if oid != file_id {
            continue;
        }
        remove_keys.push(k.clone());
        if ty == APFS_TYPE_INODE {
            is_dir = (rd_u16(v, 80) & 0o170000) == S_IFDIR;
            // For a directory inode, nchildren lives at INODE_NCHILDREN.
            dir_nchildren = rd_u32(v, INODE_NCHILDREN);
        }
        if ty == APFS_TYPE_FILE_EXTENT {
            // j_file_extent_val: len_and_flags (low 56 bits = byte length) @0,
            // phys_block_num @8. Free every block of the extent.
            let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
            let count = len_bytes / bsz as u64;
            data_blocks.push((rd_u64(v, 8), count));
        }
    }

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
    let (new_omap_tree_paddr, node_delta) = rewrite_fstree(
        txn,
        vsb_raw,
        &omap_node,
        new_xid,
        vec![],
        &remove_keys,
        Some((parent_ino, -1)),
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
        // Collect all live extref records for refcnt inspection. [#151: multi-node aware]
        let (live_recs_pairs, _) = collect_extref_records(txn, old_extref_paddr, bsz)?;
        let live_recs: Vec<CatRecord> = live_recs_pairs
            .into_iter()
            .map(|(k, v)| CatRecord { key: k, val: v })
            .collect();
        let live_keys: Vec<Vec<u8>> = live_recs.iter().map(|r| r.key.clone()).collect();

        // Classify each extent: refcnt == 1 → remove+free; refcnt > 1 → decrement;
        // not in live tree → snapshot-pinned, insert a KIND_UPDATE(-1) record.
        let mut remove_keys_ext: Vec<Vec<u8>> = Vec::new();
        let mut upsert_recs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new(); // decrements
        let mut update_inserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new(); // KIND_UPDATE(-1)
        let mut owned_runs: Vec<(u64, u64)> = Vec::new();

        for &(phys, count) in &data_blocks {
            let key = build_phys_ext_key(phys);
            if !live_keys.contains(&key) {
                // #149: not in the live extref tree → snapshot-pinned. The live
                // volume drops a reference: record a KIND_UPDATE(-1) phys_ext
                // (apfs_create_update_pext); do NOT free (snapshot still owns it).
                update_inserts.push((key, build_phys_ext_update_val(count, -1)));
                continue;
            }
            // Read the extref val to get current refcnt (bytes 16..20).
            let refcnt = live_recs
                .iter()
                .find(|r| r.key == key)
                .and_then(|r| r.val.get(16..20))
                .map(|b| u32::from_le_bytes(b.try_into().unwrap_or([0u8; 4])))
                .unwrap_or(1);
            if refcnt <= 1 {
                // Last reference: remove record and free blocks.
                remove_keys_ext.push(key);
                owned_runs.push((phys, count));
            } else {
                // Shared block: decrement refcnt, keep blocks allocated.
                // Rebuild val with refcnt - 1, preserving len_and_kind + owning_obj_id.
                let new_val = live_recs
                    .iter()
                    .find(|r| r.key == key)
                    .map(|r| {
                        let mut v = r.val.clone();
                        if v.len() >= 20 {
                            let new_rc = (refcnt - 1).to_le_bytes();
                            v[16..20].copy_from_slice(&new_rc);
                        }
                        v
                    })
                    .unwrap_or_default();
                upsert_recs.push((key, new_val));
            }
        }

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
    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = vol_omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

    // --- Stage the updated (virtual) volume superblock. ---
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    let counter_off = if is_dir {
        VSBI_NUM_DIRECTORIES
    } else {
        VSBI_NUM_FILES
    };
    let count = rd_u64(&new_vsb, counter_off);
    wr_u64(&mut new_vsb, counter_off, count.saturating_sub(1));
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &old_extref_padrs,
        false,
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
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;

    // --- Step 1: Reject duplicate destination BEFORE allocating anything. ---
    let dst_drec_key = build_drec_key(dst_parent_ino, dst_name, case_fold, normalize);
    if drec_exists(txn, &omap_node, root_tree_oid, &dst_drec_key, bsz)? {
        return Err(TxnError::AlreadyExists(format!(
            "clone_file: destination '{dst_name}' already exists in inode {dst_parent_ino}"
        )));
    }

    // --- Step 2: Locate source DREC → file_id. ---
    let src_drec_key = build_drec_key(src_parent_ino, src_name, case_fold, normalize);
    let all = collect_fstree(txn, &omap_node, root_tree_oid, bsz)?;

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
    if src_mode & 0xF000 == S_IFDIR as u16 {
        return Err(TxnError::InvalidArgument(
            "clone_file: source is a directory; only regular files may be cloned".into(),
        ));
    }

    // Collect source j_file_extent records sorted by logical offset.
    let mut src_extents: Vec<(u64, u64, u64)> = Vec::new(); // (logical_addr, paddr, len_bytes)
    for (k, v) in &all {
        let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
        let ty = rd_u64(k, 0) >> 60;
        if oid != src_file_id || ty != APFS_TYPE_FILE_EXTENT {
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

    // 5b: Clone inode - Apple's clonefile SHARES the source's data stream
    // (IDA apfs.kext clone_item, an audit pass): the clone inode's private_id points at
    // the SOURCE's data-stream id, both inodes are marked WAS_CLONED, and the
    // source's dstream_id refcnt is bumped. The clone creates NO new dstream_id /
    // file_extent records and does NOT touch the extentref tree. apfs-core
    // read_file resolves content by inode.private_id, so the clone transparently
    // reads the shared stream.
    let mut clone_inode_val = build_inode_val(&InodeValArgs {
        parent_id: dst_parent_ino,
        private_id: src_file_id, // SHARE the source's data-stream id
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
    let clone_flags =
        rd_u64(&clone_inode_val, 48) | APFS_INODE_WAS_CLONED | APFS_INODE_WAS_EVER_CLONED;
    wr_u64(&mut clone_inode_val, 48, clone_flags);
    let new_inode = (build_inode_key(new_ino), clone_inode_val);

    // New records: the clone's DREC + inode only (NO dstream_id / file_extent).
    let mut new_cat_records: Vec<(Vec<u8>, Vec<u8>)> = vec![dst_drec, new_inode];

    // Updates to EXISTING records: mark the source inode WAS_CLONED and bump the
    // source dstream_id refcnt (the new sharer).
    let mut update_records: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut src_inode_updated = src_inode_val.clone();
    let src_flags =
        rd_u64(&src_inode_updated, 48) | APFS_INODE_WAS_CLONED | APFS_INODE_WAS_EVER_CLONED;
    wr_u64(&mut src_inode_updated, 48, src_flags);
    update_records.push((src_inode_key.clone(), src_inode_updated));

    let src_dsid_key = build_dstream_id_key(src_file_id);
    match all.iter().find(|(k, _)| *k == src_dsid_key) {
        Some((_, v)) => {
            let mut nv = v.clone();
            let rc = rd_u32(&nv, 0).saturating_add(1);
            wr_u32(&mut nv, 0, rc);
            update_records.push((src_dsid_key, nv));
        }
        None => {
            // Source has content but no dstream_id record - create one with
            // refcnt = 2 (source + clone).
            new_cat_records.push((src_dsid_key, build_dstream_id_val(2)));
        }
    }

    // --- Step 6: Rewrite the fstree (insert clone DREC+inode, bump dst parent,
    //     update source inode flags + source dstream_id refcnt). ---
    let parent_patch = Some((dst_parent_ino, 1i64));
    let (new_omap_tree_paddr, node_delta) = rewrite_fstree_impl(
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

    // --- Step 7: COW the volume omap header. (No extentref change: the shared
    //     physical extents are untouched; only the dstream_id refcnt moved.) ---
    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = vol_omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

    // --- Step 8: Stage the updated volume superblock. ---
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_NEXT_OBJ_ID, new_ino + 1);
    let num_files = rd_u64(&new_vsb, VSBI_NUM_FILES);
    wr_u64(&mut new_vsb, VSBI_NUM_FILES, num_files + 1);
    // Reclaim old metadata blocks (omap, catalog). The extentref tree is NOT
    // COW'd by a clone, so it must not be marked freed here.
    let frm_correction =
        free_replaced_metadata(txn, vsb_raw, vol_omap_raw, &omap_node, &[], false, bsz)?;

    // fs_alloc_count: clone adds no data blocks; only the catalog/omap node delta.
    // frm_correction: sm_fq-pinned old nodes stay counted. [#149 fix]
    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let new_alloc = (fs_alloc + node_delta + frm_correction).max(0) as u64;
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
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
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

    // --- Step 3: Collect all fstree catalog records. ---
    let all = collect_fstree(txn, &omap_node, root_tree_oid, bsz)?;

    // --- Step 4: Locate DREC for (parent_ino, name). ---
    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
    let file_id = all
        .iter()
        .find(|(k, _)| *k == drec_key)
        .map(|(_, v)| rd_u64(v, 0) & 0x0FFF_FFFF_FFFF_FFFF)
        .ok_or_else(|| TxnError::NotFound(format!("'{name}' not in parent {parent_ino}")))?;

    // --- Step 6: Scan for inode + extent records for file_id. ---
    let mut old_inode_val: Option<Vec<u8>> = None;
    let mut data_blocks: Vec<(u64, u64)> = Vec::new(); // (phys_start, block_count)
    let mut old_extent_logical_addrs: Vec<u64> = Vec::new();

    for (k, v) in &all {
        let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
        let ty = rd_u64(k, 0) >> 60;
        if oid != file_id {
            continue;
        }
        if ty == APFS_TYPE_INODE {
            old_inode_val = Some(v.clone());
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

    let old_inode_val =
        old_inode_val.ok_or_else(|| TxnError::NotFound(format!("inode {file_id} not found")))?;

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
        let (live_recs_pairs, _) = collect_extref_records(txn, old_extref_paddr, bsz)?;
        let live_recs: Vec<CatRecord> = live_recs_pairs
            .into_iter()
            .map(|(k, v)| CatRecord { key: k, val: v })
            .collect();
        for &(phys, count) in &data_blocks {
            let key = build_phys_ext_key(phys);
            let rec = match live_recs.iter().find(|r| r.key == key) {
                Some(r) => r,
                None => {
                    extref_update_inserts.push((key, build_phys_ext_update_val(count, -1)));
                    continue;
                }
            };
            let refcnt = rec
                .val
                .get(16..20)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap_or([0u8; 4])))
                .unwrap_or(1);
            if refcnt <= 1 {
                extref_remove_keys.push(key);
                owned_runs_to_free.push((phys, count));
            } else {
                let mut v = rec.val.clone();
                if v.len() >= 20 {
                    v[16..20].copy_from_slice(&(refcnt - 1).to_le_bytes());
                }
                extref_upserts.push((key, v));
            }
        }
    }

    // --- Step 8: Build remove_keys (inode + dstream_id + file_extents, NOT drec). ---
    let mut remove_keys: Vec<Vec<u8>> =
        vec![build_inode_key(file_id), build_dstream_id_key(file_id)];
    for &logical_addr in &old_extent_logical_addrs {
        remove_keys.push(build_file_extent_key(file_id, logical_addr));
    }

    // --- Steps 9/10: Build new catalog records and allocate new data blocks. ---
    // Also collect new phys_ext inserts for extref tree.
    let mut extref_new_inserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let (new_records, new_data_block_count) = if new_content.is_empty() {
        // Step 9: empty content - no dstream, no extents.
        // Use rebuild_inode_preserving_unknown so that unrecognized xfields
        // from the original inode (e.g. DOCUMENT_ID, FINDER_INFO) survive
        // the content-replace operation. [W4-FRAG-1]
        let inode_key = build_inode_key(file_id);
        let inode_val = rebuild_inode_preserving_unknown(&old_inode_val, old_name_str, None, now);
        (vec![(inode_key, inode_val)], 0u64)
    } else {
        // Step 10: allocate new blocks for content.
        // Use alloc_blocks_run for contiguous batch allocation; loop for multi-chunk.
        let num_blocks = new_content.len().div_ceil(bsz);
        let mut paddrs: Vec<u64> = Vec::with_capacity(num_blocks);
        let mut blocks_remaining = num_blocks;
        while blocks_remaining > 0 {
            let (run_start, run_count) = txn.alloc_blocks_run(blocks_remaining)?;
            for j in 0..run_count {
                let i = num_blocks - blocks_remaining + j;
                let p = run_start + j as u64;
                let mut data = vec![0u8; bsz];
                let start = i * bsz;
                let end = ((i + 1) * bsz).min(new_content.len());
                wr_bytes(&mut data, 0, new_content.get(start..end).unwrap_or(&[]));
                txn.stage_raw(p, data);
                paddrs.push(p);
            }
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
                build_phys_ext_val(cnt, file_id, 1),
            ));
        }

        let alloced = (num_blocks * bsz) as u64;
        let inode_key = build_inode_key(file_id);
        // Use rebuild_inode_preserving_unknown so that unrecognized xfields
        // from the original inode survive the content-replace operation. [W4-FRAG-1]
        let inode_val = rebuild_inode_preserving_unknown(
            &old_inode_val,
            old_name_str,
            Some(DstreamArgs {
                size: new_content.len() as u64,
                alloced_size: alloced,
            }),
            now,
        );
        let mut recs = vec![
            (inode_key, inode_val),
            (build_dstream_id_key(file_id), build_dstream_id_val(1)),
        ];
        for &(lblk, pstart, cnt) in &runs {
            recs.push((
                build_file_extent_key(file_id, (lblk * bsz) as u64),
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

    // --- Step 11: Rewrite the fstree - remove old records, insert new ones.
    // Parent nchildren unchanged (file already existed). ---
    let (new_omap_tree_paddr, node_delta) = rewrite_fstree(
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

    // --- Step 12: COW the volume omap header. ---
    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

    // --- Step 13+14: Update and stage the volume superblock. ---
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    // --- Step 15: Free COW-replaced metadata. ---
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        omap_raw,
        &omap_node,
        &old_extref_padrs,
        false,
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
/// types in their original x_type order, DSTREAM=8 last if present).
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
    entries.push((INO_EXT_TYPE_NAME, XF_DATA_DEPENDENT, name_bytes));
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
fn rebuild_inode_preserving_unknown(
    old: &[u8],
    name: &str,
    dstream: Option<DstreamArgs>,
    now: u64,
) -> Vec<u8> {
    // Parse unknown xfields; on error fall back to empty (no unknown preservation).
    let unknowns = apfs_core::inode::parse_unknown_xfields(old).unwrap_or_default();

    let ds_bytes: Option<Vec<u8>> = dstream.map(|ds| {
        let mut b = vec![0u8; DSTREAM_SIZE];
        wr_u64(&mut b, 0, ds.size);
        wr_u64(&mut b, 8, ds.alloced_size);
        // default_crypto_id @16 = 0
        wr_u64(&mut b, 24, ds.size); // total_bytes_written = size
                                     // total_bytes_read @32 = 0
        b
    });

    let mut name_nul = Vec::with_capacity(name.len() + 1);
    name_nul.extend_from_slice(name.as_bytes());
    name_nul.push(0);

    // Copy fixed 92-byte prefix, bump mod_time + change_time.
    let mut prefix = old.get(..92).unwrap_or(&[0u8; 92]).to_vec();
    prefix.resize(92, 0);
    wr_u64(&mut prefix, INODE_MOD_TIME, now);
    wr_u64(&mut prefix, INODE_CHANGE_TIME, now);

    build_inode_val_with_xfields(&prefix, &name_nul, ds_bytes.as_deref(), &unknowns)
}

/// Rename `old_name` to `new_name` within directory `parent_ino`.
///
/// Replaces the DREC (new name in the key) and the INODE (NAME xfield updated,
/// dstream + prefix preserved). The file's DSTREAM_ID / FILE_EXTENT records are
/// keyed by the unchanged file id and stay untouched. [the APFS specification]
/// #140: the incremental rename fast path (`incremental_rename_drec`) was
/// reworked to be fsck-valid and re-enabled. It now (a) edits each touched leaf
/// by parse + repack through the SAME proven packers as the full rebuild
/// (`pack_cat_leaf` for the single-node root, `build_fstree_node` for non-root
/// leaves) instead of fragile in-place byte surgery; (b) falls back to
/// `rewrite_fstree` whenever a non-root leaf's minimum key (its parent's
/// separator) would change, it would empty, or it would overflow - so internal
/// nodes never go stale; and (c) grows the multi-node root's bt_longest_key /
/// bt_longest_val footer high-water marks when a rename introduces a longer key
/// or value (an audit pass root cause 1). All blocks are allocated before any are
/// freed, closing the #138 in-transaction block-reuse window. Gated fsck
/// regressions: `rename_corruption_138` (single-node) + `rename_multinode_140`
/// (multi-node). The path still self-falls-back to the full rebuild for
/// snapshots and multi-level omaps.
const RENAME_INCREMENTAL_FAST_PATH: bool = true;

pub fn rename<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    parent_ino: u64,
    old_name: &str,
    new_name: &str,
    replace_if_exists: bool,
) -> Result<(), TxnError> {
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
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
    let all = collect_fstree(txn, &omap_node, root_tree_oid, bsz)?;

    // Locate the old DREC -> file_id + dirent type + date_added.
    let old_drec_key = build_drec_key(parent_ino, old_name, case_fold, normalize);
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
    let new_inode_val = rebuild_inode_with_name(&old_inode_val, new_name, now);
    let new_drec_key = build_drec_key(parent_ino, new_name, case_fold, normalize);
    let new_drec = (
        new_drec_key.clone(),
        build_drec_val(file_id, date_added, dt_type),
    );

    // Detect an existing target with `new_name`. If found and
    // `replace_if_exists`, collect its records + owned data blocks for the
    // replacement transaction; otherwise return AlreadyExists.
    let target_replace =
        collect_replace_target(&all, &new_drec_key, file_id, replace_if_exists, bsz)?;

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

    // Rebuild the FSTREE: remove old DREC + INODE (+ target records if
    // replacing), insert new DREC + INODE.
    //
    // M11 fast path: when this is a simple rename (no target replacement, no
    // parent nchildren change) we try the incremental path first - only the
    // touched leaf nodes are COW'd, making rename O(log N) instead of
    // O(catalog).  Falls back to full rebuild when a leaf split is needed,
    // when a snapshot is present (omap retention is complex), or when the
    // omap is multi-level.
    // Whether the incremental fast path actually ran (vs fell back). It governs
    // whether free_replaced_metadata reclaims the old catalog nodes: the fast
    // path keeps untouched leaves at their existing paddrs and frees only the
    // ones it COW'd itself, so a blanket old-catalog reclaim would underallocate.
    let mut fast_path_taken = false;
    let (new_omap_tree_paddr, node_delta) =
        if RENAME_INCREMENTAL_FAST_PATH && target_replace.is_none() && parent_delta == 0 {
            match incremental_rename_drec(
                txn,
                vsb_raw,
                &omap_node,
                new_xid,
                &remove_keys[0], // old_drec_key
                &new_drec.0,     // new_drec_key
                &new_drec.1,     // new_drec_val
                &remove_keys[1], // inode_key
                &new_inode_val,
                bsz,
            )? {
                Some(result) => {
                    fast_path_taken = true;
                    result
                }
                None => rewrite_fstree(
                    txn,
                    vsb_raw,
                    &omap_node,
                    new_xid,
                    vec![new_drec, (inode_key, new_inode_val)],
                    &remove_keys,
                    Some((parent_ino, parent_delta)),
                    now,
                    bsz,
                )?,
            }
        } else {
            rewrite_fstree(
                txn,
                vsb_raw,
                &omap_node,
                new_xid,
                vec![new_drec, (inode_key, new_inode_val)],
                &remove_keys,
                Some((parent_ino, parent_delta)),
                now,
                bsz,
            )?
        };

    // COW the volume omap header (om_tree_oid -> rewritten b-tree).
    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = vol_omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

    // Stage the updated (virtual) volume superblock. When a target was
    // replaced we also update the extentref tree pointer and decrement
    // num_files for the dropped inode.
    let vsb_oid = rd_u64(vsb_raw, 8);
    let mut new_vsb = vsb_raw.to_vec();
    new_vsb.resize(bsz, 0);
    wr_u64(&mut new_vsb, VSBI_OMAP_OID, new_vomap_paddr);
    wr_u64(&mut new_vsb, VSBI_EXTENTREF_TREE_OID, new_extref_paddr);
    if target_replace.is_some() {
        let num_files = rd_u64(&new_vsb, VSBI_NUM_FILES);
        wr_u64(&mut new_vsb, VSBI_NUM_FILES, num_files.saturating_sub(1));
    }
    // Reclaim COW-replaced metadata. extref_old_padrs is empty when no data
    // blocks were freed (rename without replace target). [#151]
    // #140: when the incremental fast path ran it already freed exactly the
    // catalog leaves it COW'd and kept the rest, so skip the full old-catalog
    // reclaim (which would underallocate the retained leaves).
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &extref_old_padrs,
        fast_path_taken,
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
    remove_keys: Vec<Vec<u8>>,
    data_blocks: Vec<(u64, u64)>,
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
) -> Result<Option<TargetReplace>, TxnError> {
    let target_drec = match all.iter().find(|(k, _)| k.as_slice() == target_drec_key) {
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
    let mut remove_keys: Vec<Vec<u8>> = vec![target_drec_key.to_vec()];
    let mut data_blocks: Vec<(u64, u64)> = Vec::new();
    let mut is_dir = false;
    let mut nchildren = 0u32;
    for (k, v) in all {
        let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
        let ty = rd_u64(k, 0) >> 60;
        if oid != target_id {
            continue;
        }
        remove_keys.push(k.clone());
        if ty == APFS_TYPE_INODE {
            is_dir = (rd_u16(v, 80) & 0o170000) == S_IFDIR;
            nchildren = rd_u32(v, INODE_NCHILDREN);
        }
        if ty == APFS_TYPE_FILE_EXTENT {
            let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
            let count = len_bytes / bsz as u64;
            data_blocks.push((rd_u64(v, 8), count));
        }
    }
    if is_dir && nchildren > 0 {
        return Err(TxnError::DirectoryNotEmpty(format!(
            "rename target inode {target_id} is a non-empty directory"
        )));
    }
    Ok(Some(TargetReplace {
        remove_keys,
        data_blocks,
    }))
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
    // Collect live records (multi-node aware). [#151]
    let (live_recs_pairs, _) = collect_extref_records(txn, old_extref_paddr, bsz)?;
    let live_recs: Vec<CatRecord> = live_recs_pairs
        .into_iter()
        .map(|(k, v)| CatRecord { key: k, val: v })
        .collect();

    // Classify: refcnt==1 → remove+free; refcnt>1 → decrement; absent → KIND_UPDATE(-1).
    let mut remove_keys: Vec<Vec<u8>> = Vec::new();
    let mut owned_runs: Vec<(u64, u64)> = Vec::new();
    let mut upsert_recs: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut update_inserts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

    for &(phys, count) in data_blocks {
        let key = build_phys_ext_key(phys);
        let rec = match live_recs.iter().find(|r| r.key == key) {
            Some(r) => r,
            None => {
                // #149: snapshot-pinned (absent from live tree) → KIND_UPDATE(-1).
                update_inserts.push((key, build_phys_ext_update_val(count, -1)));
                continue;
            }
        };
        let refcnt = rec
            .val
            .get(16..20)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap_or([0u8; 4])))
            .unwrap_or(1);
        if refcnt <= 1 {
            remove_keys.push(key);
            owned_runs.push((phys, count));
        } else {
            let mut v = rec.val.clone();
            if v.len() >= 20 {
                v[16..20].copy_from_slice(&(refcnt - 1).to_le_bytes());
            }
            upsert_recs.push((key, v));
        }
    }

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

/// Find the physical paddr mapped to `oid` (latest xid) in a fixed-kv volume
/// omap b-tree leaf node.
fn omap_lookup(node: &[u8], oid: u64, bsz: usize) -> Option<u64> {
    let nkeys = rd_u32(node, 36) as usize;
    let toc_off = rd_u16(node, 40) as usize;
    let toc_len = rd_u16(node, 42) as usize;
    let is_root = rd_u16(node, 32) & 0x1 != 0;
    let key_area = DATA_BASE + toc_off + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    let mut best: Option<(u64, u64)> = None; // (xid, paddr)
    for i in 0..nkeys {
        let te = DATA_BASE + toc_off + i * 4;
        let k_off = rd_u16(node, te) as usize;
        let v_off = rd_u16(node, te + 2) as usize;
        let k = key_area + k_off;
        let k_oid = rd_u64(node, k);
        let k_xid = rd_u64(node, k + 8);
        if k_oid != oid {
            continue;
        }
        let v = val_area_end - v_off;
        let paddr = rd_u64(node, v + 8);
        if best.map(|(x, _)| k_xid > x).unwrap_or(true) {
            best = Some((k_xid, paddr));
        }
    }
    best.map(|(_, p)| p)
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
    for _ in 0..32 {
        let is_leaf = rd_u16(&node, 34) == 0;
        let nkeys = rd_u32(&node, 36) as usize;
        let toc_off = rd_u16(&node, 40) as usize;
        let toc_len = rd_u16(&node, 42) as usize;
        let is_root = rd_u16(&node, 32) & BTNODE_ROOT != 0;
        let key_area = DATA_BASE + toc_off + toc_len;
        let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };

        // Find the best entry. Index: largest (oid,xid) <= (target). Leaf: the
        // matching oid with the largest xid <= target_xid.
        let mut best: Option<(u64, u64, usize)> = None; // (k_oid, k_xid, idx)
        for i in 0..nkeys {
            let te = DATA_BASE + toc_off + i * 4; // kvoff_t (4-byte TOC)
            let k_off = rd_u16(&node, te) as usize;
            let k = key_area + k_off;
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
        let te = DATA_BASE + toc_off + idx * 4;
        let v_off = rd_u16(&node, te + 2) as usize;
        let v = val_area_end - v_off;
        if is_leaf {
            // omap_val: ov_flags@v (u32), ov_size@v+4, ov_paddr@v+8.
            if rd_u32(&node, v) & OMAP_VAL_DELETED != 0 {
                return Ok(None);
            }
            return Ok(Some(rd_u64(&node, v + 8)));
        }
        // Index value is an 8-byte PHYSICAL child paddr.
        let child = rd_u64(&node, v);
        let mut buf = vec![0u8; bsz];
        txn.read_block(child, &mut buf)?;
        node = buf;
    }
    Ok(None)
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
//   • find_leaf_with_key: walk the fstree from root to the leaf that owns
//     a given key.  Returns (leaf_oid, leaf_node_bytes, leaf_paddr).
//   • patch_omap_entry: replace a single {oid → paddr} mapping in the
//     volume-omap node bytes (no full rebuild needed when only one node
//     changed and the omap is a single-node root).
//   • incremental_rename_drec: orchestrates remove-old-drec + insert-new-
//     drec + update-inode-name across the minimal set of touched leaves,
//     COWs each touched leaf, updates the omap, returns new omap paddr.
//     Falls back to None when a leaf split would be needed (rare) so the
//     caller can use rewrite_fstree instead.
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

/// Result of a leaf-search: the leaf node's virtual oid, physical address, and
/// raw bytes.
struct LeafLocation {
    oid: u64,
    paddr: u64,
    node: Vec<u8>,
}

/// Walk the fstree from its virtual root (resolving through the omap) down to
/// the leaf that would contain `key`.  For a single-level root|leaf the root
/// itself is returned.  Returns `None` if the tree has no level where the key
/// could live (empty tree or parse error).
///
/// Only reads the path from root to the target leaf - O(log N) disk reads.
fn find_leaf_with_key<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    root_oid: u64,
    key: &[u8],
    bsz: usize,
) -> Result<Option<LeafLocation>, TxnError> {
    let mut oid = root_oid;
    loop {
        let paddr = match omap_lookup(omap_node, oid, bsz) {
            Some(p) => p,
            None => return Ok(None),
        };
        let mut node = vec![0u8; bsz];
        txn.read_block(paddr, &mut node)?;
        let level = rd_u16(&node, 34);
        if level == 0 {
            // Leaf node - this is where key lives (or should live).
            return Ok(Some(LeafLocation { oid, paddr, node }));
        }
        // Internal node: find the child whose pivot key is <= target key.
        let nkeys = rd_u32(&node, 36) as usize;
        let toc_off = rd_u16(&node, 40) as usize;
        let toc_len = rd_u16(&node, 42) as usize;
        let key_area = DATA_BASE + toc_off + toc_len;
        let is_root = rd_u16(&node, 32) & BTNODE_ROOT != 0;
        let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };

        // Internal vals are 8-byte child oids.  Walk entries finding the last
        // pivot <= key (rightmost entry whose key <= target).
        let mut chosen_oid = 0u64;
        for i in 0..nkeys {
            let te = DATA_BASE + toc_off + i * 8;
            let k_off = rd_u16(&node, te) as usize;
            let k_len = rd_u16(&node, te + 2) as usize;
            let v_off = rd_u16(&node, te + 4) as usize;
            let ks = key_area + k_off;
            let pivot = match node.get(ks..ks + k_len) {
                Some(s) => s,
                None => break,
            };
            let child_oid = rd_u64(&node, val_area_end - v_off);
            if cat_key_cmp(pivot, key) != core::cmp::Ordering::Greater {
                chosen_oid = child_oid;
            } else {
                break;
            }
        }
        if chosen_oid == 0 {
            return Ok(None);
        }
        oid = chosen_oid;
    }
}

/// Patch a single `{oid → paddr}` entry in the volume-omap node bytes
/// (in-place, no full rebuild).  Used when exactly one catalog leaf was
/// COW'd and its omap mapping must be updated.  Returns `false` when the oid
/// is not found in the omap (caller must fall back to full omap rebuild).
///
/// NOTE: only safe when the omap is a single fixed-kv root node.  Multi-node
/// omaps (very large volumes) are not handled here.
fn patch_omap_entry(
    omap_node: &mut Vec<u8>,
    oid: u64,
    new_paddr: u64,
    new_xid: u64,
    bsz: usize,
) -> bool {
    let nkeys = rd_u32(omap_node, 36) as usize;
    let toc_off = rd_u16(omap_node, 40) as usize;
    let toc_len = rd_u16(omap_node, 42) as usize;
    let is_root = rd_u16(omap_node, 32) & BTNODE_ROOT != 0;
    let key_area = DATA_BASE + toc_off + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };

    for i in 0..nkeys {
        let te = DATA_BASE + toc_off + i * 4; // omap uses kvoff_t (4-byte TOC entries)
        let k_off = rd_u16(omap_node, te) as usize;
        let v_off = rd_u16(omap_node, te + 2) as usize;
        let k = key_area + k_off;
        let entry_oid = rd_u64(omap_node, k);
        if entry_oid != oid {
            continue;
        }
        // Update xid in key and paddr in val.
        wr_u64(omap_node, k + 8, new_xid);
        let v = val_area_end - v_off;
        wr_u64(omap_node, v + 8, new_paddr); // ov_paddr at offset 8 within omap_val
        return true;
    }
    false
}

/// One catalog leaf touched by an incremental rename, plus the record-level
/// edits to apply to it. Several logical edits (remove old drec, insert new
/// drec, replace inode) may resolve to the SAME physical leaf; they are merged
/// here so each leaf is parsed, edited, and COW'd exactly once.
struct LeafWork {
    oid: u64,
    old_paddr: u64,
    node: Vec<u8>,
    removes: Vec<Vec<u8>>,
    inserts: Vec<(Vec<u8>, Vec<u8>)>,
}

/// Return the index of the `LeafWork` for `loc.oid`, pushing a fresh entry if
/// this leaf has not been seen yet.
fn leafwork_idx(work: &mut Vec<LeafWork>, loc: &LeafLocation) -> usize {
    if let Some(i) = work.iter().position(|w| w.oid == loc.oid) {
        return i;
    }
    work.push(LeafWork {
        oid: loc.oid,
        old_paddr: loc.paddr,
        node: loc.node.clone(),
        removes: Vec::new(),
        inserts: Vec::new(),
    });
    work.len() - 1
}

/// Apply `removes` + `inserts` to one catalog leaf and re-pack it with the SAME
/// proven packers the full-rebuild path uses, so the result is byte-identical to
/// `rewrite_fstree`'s output (fsck-valid by construction - this is the #140
/// "parse + repack" approach that replaces the earlier byte-surgery leaf ops).
///
/// Returns:
///   - `Ok(Some(node))` - the re-packed, re-checksummed leaf (`o_xid = new_xid`);
///   - `Ok(None)` - caller must fall back to `rewrite_fstree` because the edit
///     would (a) overflow a single node, (b) empty a NON-root leaf, or (c) change
///     a NON-root leaf's minimum key.
///
/// fstree is a virtual-oid B-tree: internal nodes reference children by OID and
/// the omap resolves OID→paddr, so COWing a leaf needs only a new paddr + an omap
/// patch - the parent stays valid AS LONG AS the leaf's minimum key (the parent's
/// separator) is unchanged. Case (c) enforces that invariant, which the original
/// (gated-off) fast path lacked. [#140]
fn repack_cat_leaf_edited(
    node: &[u8],
    removes: &[Vec<u8>],
    inserts: &[(Vec<u8>, Vec<u8>)],
    new_xid: u64,
    bsz: usize,
) -> Result<Option<Vec<u8>>, TxnError> {
    let is_root = rd_u16(node, 32) & BTNODE_ROOT != 0;
    let oid = rd_u64(node, 8);
    let mut recs = parse_cat_leaf(node).map_err(|m| TxnError::SpacemanParse(m.into()))?;
    let old_min = recs.first().map(|r| r.key.clone());
    recs.retain(|r| !removes.iter().any(|k| k.as_slice() == r.key.as_slice()));
    for (k, v) in inserts {
        recs.push(CatRecord {
            key: k.clone(),
            val: v.clone(),
        });
    }
    recs.sort_by(|a, b| cat_key_cmp(&a.key, &b.key));

    // (b)/(c): a non-root leaf must stay non-empty AND keep its minimum key, or
    // the internal nodes above would need rewriting (outside the fast path).
    if !is_root {
        match recs.first() {
            None => return Ok(None),
            Some(first) => {
                if old_min.as_deref() != Some(first.key.as_slice()) {
                    return Ok(None);
                }
            }
        }
    }

    // (a): build_fstree_node does NOT range-check; pack_cat_leaf reports overflow
    // via Err. Unify on one explicit capacity test up front.
    let used: usize = recs.iter().map(|r| 8 + r.key.len() + r.val.len()).sum();
    if used > node_capacity(bsz, is_root) {
        return Ok(None);
    }

    let mut out = if is_root {
        // Single-node tree: the leaf IS the root (BTNODE_ROOT + btree_info
        // footer). pack_cat_leaf preserves the footer and recomputes longest_*.
        match pack_cat_leaf(node, recs) {
            Ok(n) => n,
            Err(_) => return Ok(None),
        }
    } else {
        // Non-root leaf: rebuild via the proven node packer (no footer,
        // OBJ_VIRTUAL | BTREE_NODE), byte-identical to rewrite_fstree's leaves.
        let kv: Vec<(Vec<u8>, Vec<u8>)> = recs.into_iter().map(|r| (r.key, r.val)).collect();
        build_fstree_node(oid, new_xid, false, 0, &kv, None, bsz)
    };
    // pack_cat_leaf copies the source header verbatim (old o_xid); build_fstree_node
    // already stamped new_xid. Normalise o_xid + (re)checksum on both paths.
    wr_u64(&mut out, 16, new_xid);
    update_checksum_in_place(&mut out);
    Ok(Some(out))
}

/// Incremental rename: modify only the leaf node(s) that contain the affected
/// drec + inode records, COW each touched leaf, patch the volume omap, return
/// the new omap paddr.
///
/// Returns `Ok(None)` when any touched leaf lacks space for the insert (split
/// needed) - the caller must fall back to `rewrite_fstree`.
/// Returns `Ok(Some(new_omap_paddr, node_delta))` on success.
///
/// Operations performed:
///   1. Find the leaf containing `old_drec_key`, remove it, insert
///      `new_drec_key`/`new_drec_val`.  If old and new drec land in different
///      leaves (cross-leaf rename - split boundary fell between them) we
///      perform two separate leaf COWs.
///   2. Find the leaf containing `inode_key`, replace its value with
///      `new_inode_val`.
///   3. COW each touched leaf (alloc_block + stage_raw), patch omap entry.
///   4. Build a fresh omap node from the updated triples and return its paddr.
///
/// [CERTAIN: algorithm derived from APFS PDF B-tree spec §B-Trees +
///  linux-apfs-rw btree.c apfs_btree_insert/remove/replace pattern]
#[allow(clippy::too_many_arguments)]
fn incremental_rename_drec<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    omap_node: &[u8],
    new_xid: u64,
    old_drec_key: &[u8],
    new_drec_key: &[u8],
    new_drec_val: &[u8],
    inode_key: &[u8],
    new_inode_val: &[u8],
    bsz: usize,
) -> Result<Option<(u64, i64)>, TxnError> {
    let root_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let has_snapshot = rd_u64(vsb_raw, VSBI_NUM_SNAPSHOTS) > 0;

    // Incremental path is only safe on single-node omaps (root = leaf) - multi-
    // node omaps require a full omap rebuild which we don't implement here.
    // Detect by checking that the omap node itself is the root *and* leaf.
    let omap_level = rd_u16(omap_node, 34);
    if omap_level != 0 {
        // Multi-level omap: fall back to full rebuild.
        return Ok(None);
    }

    // Snapshots require careful omap retention - fall back to rewrite_fstree
    // which has the full snapshot-pinning logic.
    if has_snapshot {
        return Ok(None);
    }

    // 1. Resolve the (up to three) leaves involved and merge edits that land on
    //    the same physical leaf, so each leaf is parsed/edited/COW'd exactly once.
    //    - old_drec_key  : removed from its leaf
    //    - new_drec_key  : inserted into the leaf where it sorts (may differ)
    //    - inode_key     : value replaced in its leaf (key is stable)
    let drec_old_leaf = match find_leaf_with_key(txn, omap_node, root_oid, old_drec_key, bsz)? {
        Some(l) => l,
        None => return Ok(None),
    };
    let drec_new_leaf = match find_leaf_with_key(txn, omap_node, root_oid, new_drec_key, bsz)? {
        Some(l) => l,
        None => return Ok(None),
    };
    let inode_leaf = match find_leaf_with_key(txn, omap_node, root_oid, inode_key, bsz)? {
        Some(l) => l,
        None => return Ok(None),
    };

    let mut work: Vec<LeafWork> = Vec::with_capacity(3);
    let i = leafwork_idx(&mut work, &drec_old_leaf);
    work[i].removes.push(old_drec_key.to_vec());
    let i = leafwork_idx(&mut work, &drec_new_leaf);
    work[i]
        .inserts
        .push((new_drec_key.to_vec(), new_drec_val.to_vec()));
    let i = leafwork_idx(&mut work, &inode_leaf);
    work[i].removes.push(inode_key.to_vec());
    work[i]
        .inserts
        .push((inode_key.to_vec(), new_inode_val.to_vec()));

    // 2. Re-pack every touched leaf with the proven packers as a PURE pass -
    //    NO allocation yet. repack_cat_leaf_edited returns None (→ fall back to
    //    rewrite_fstree) on overflow, an emptied non-root leaf, or a changed
    //    non-root min key. Allocating before we know ALL leaves repack would
    //    strand the already-allocated blocks when a later leaf bails (the
    //    fallback never references them), leaking them as fsck "overallocation".
    //    So gather all new node bytes first; allocate only past the commit point.
    //    [#140 - an audit pass root cause 2: the original fast path allocated as it went]
    let mut repacked: Vec<(u64, u64, Vec<u8>)> = Vec::with_capacity(work.len()); // (oid, old_paddr, node)
    for w in &work {
        match repack_cat_leaf_edited(&w.node, &w.removes, &w.inserts, new_xid, bsz)? {
            Some(n) => repacked.push((w.oid, w.old_paddr, n)),
            None => return Ok(None),
        }
    }

    // Prepare (still PURE) the multi-node root btree_info footer COW. The root
    // carries bt_longest_key / bt_longest_val high-water marks over the whole
    // tree ("the longest that has ever been stored"). A rename can introduce a
    // drec key or inode value longer than any current record; the untouched root
    // footer would then understate the longest and fsck rejects the tree
    // ("invalid bt_longest_key … fsroot tree is invalid"). Single-node trees are
    // the root-leaf already handled above by pack_cat_leaf (recomputes the
    // footer). [#140 an audit pass root cause 1]
    let root_paddr = match omap_lookup(omap_node, root_oid, bsz) {
        Some(p) => p,
        None => return Ok(None),
    };
    let mut root_node = vec![0u8; bsz];
    txn.read_block(root_paddr, &mut root_node)?;
    let root_cow: Option<Vec<u8>> = if rd_u16(&root_node, 34) > 0 {
        let bti = bsz - BTREE_INFO_SIZE;
        let cur_lk = rd_u32(&root_node, bti + 16);
        let cur_lv = rd_u32(&root_node, bti + 20);
        let new_lk = cur_lk.max(new_drec_key.len() as u32);
        // Cover EVERY value this rename inserts into the tree: the inode value
        // and the drec value. (drec values are a fixed-size struct already at or
        // below the existing high-water in any multi-node tree, but guard both so
        // the invariant holds structurally, not by coincidence.) [#140 review]
        let new_lv = cur_lv
            .max(new_inode_val.len() as u32)
            .max(new_drec_val.len() as u32);
        if new_lk != cur_lk || new_lv != cur_lv {
            wr_u32(&mut root_node, bti + 16, new_lk);
            wr_u32(&mut root_node, bti + 20, new_lv);
            wr_u64(&mut root_node, 16, new_xid);
            update_checksum_in_place(&mut root_node);
            Some(root_node)
        } else {
            None
        }
    } else {
        None
    };

    // 3. COMMIT POINT: every leaf re-packed and the root footer prepared, so no
    //    further bail is possible. NOW allocate + stage each new block and record
    //    its omap patch (oid → new paddr) and the old paddr to free.
    let mut omap_patches: Vec<(u64, u64)> = Vec::with_capacity(repacked.len() + 1);
    let mut old_paddrs: Vec<u64> = Vec::with_capacity(repacked.len() + 1);
    for (oid, old_paddr, new_node) in repacked {
        let p = txn.alloc_block()?;
        txn.stage_raw(p, new_node);
        omap_patches.push((oid, p));
        old_paddrs.push(old_paddr);
    }
    if let Some(rn) = root_cow {
        let rp = txn.alloc_block()?;
        txn.stage_raw(rp, rn);
        omap_patches.push((root_oid, rp));
        old_paddrs.push(root_paddr);
    }

    // 4. Incremental omap update: patch the single-node volume omap in place for
    //    every COW'd object, then COW the omap node itself. Only changed entries
    //    are touched - no full B-tree rebuild. Falls back to a full omap rebuild
    //    if a patch target is missing (should not happen for live objects, but we
    //    never panic). [CERTAIN: APFS PDF §Object Maps + linux-apfs-rw object_map.c]
    let new_omap_paddr = txn.alloc_block()?;
    let mut patched_omap = omap_node.to_vec();
    let mut all_patched = true;
    for (oid, new_paddr) in &omap_patches {
        if !patch_omap_entry(&mut patched_omap, *oid, *new_paddr, new_xid, bsz) {
            all_patched = false;
            break;
        }
    }
    if all_patched {
        // Update the omap node's own o_oid + o_xid and re-checksum.
        wr_u64(&mut patched_omap, 8, new_omap_paddr);
        wr_u64(&mut patched_omap, 16, new_xid);
        update_checksum_in_place(&mut patched_omap);
        txn.stage_raw(new_omap_paddr, patched_omap);
    } else {
        let mut triples = parse_omap_entries(omap_node, bsz);
        for (patch_oid, patch_paddr) in &omap_patches {
            for t in triples.iter_mut() {
                if t.0 == *patch_oid {
                    t.1 = new_xid;
                    t.2 = *patch_paddr;
                }
            }
        }
        let fallback_omap = build_omap_node(omap_node, &triples, new_omap_paddr, new_xid, bsz);
        txn.stage_raw(new_omap_paddr, fallback_omap);
    }

    // 5. Free the superseded blocks LAST - after every allocation above - so a
    //    deferred-or-not free can never hand a just-freed block back to an
    //    alloc_block in this same transaction (the #138 block-reuse class).
    for p in &old_paddrs {
        txn.free_block(*p)?;
    }

    Ok(Some((new_omap_paddr, 0i64))) // node_delta = 0 (same node count)
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

    let toc_len = nkeys * 8;
    let key_area = DATA_BASE + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
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

/// Maximum record bytes that fit in a node body.
pub fn node_capacity(bsz: usize, is_root: bool) -> usize {
    bsz - DATA_BASE - if is_root { BTREE_INFO_SIZE } else { 0 }
}

/// Greedily split `recs` into chunks each packing within `cap` bytes
/// (TOC + key + value per record). Order is preserved.
fn partition_records(recs: &[CatRec], cap: usize) -> Vec<Vec<CatRec>> {
    let mut out: Vec<Vec<CatRec>> = Vec::new();
    let mut cur: Vec<CatRec> = Vec::new();
    let mut sz = 0usize;
    for (k, v) in recs {
        let rsz = 8 + k.len() + v.len();
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
    out
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

    let toc_len = nkeys * 8;
    let key_area = DATA_BASE + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
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
fn collect_extref_records<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    root_paddr: u64,
    bsz: usize,
) -> Result<(Vec<CatRec>, Vec<u64>), TxnError> {
    let mut leaf_recs: Vec<CatRec> = Vec::new();
    let mut all_padrs: Vec<u64> = Vec::new();
    collect_extref_node(txn, root_paddr, bsz, &mut leaf_recs, &mut all_padrs)?;
    Ok((leaf_recs, all_padrs))
}

fn collect_extref_node<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    paddr: u64,
    bsz: usize,
    leaf_recs: &mut Vec<CatRec>,
    all_padrs: &mut Vec<u64>,
) -> Result<(), TxnError> {
    all_padrs.push(paddr);
    let mut node = vec![0u8; bsz];
    txn.read_block(paddr, &mut node)?;
    let level = rd_u16(&node, 34);
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
            collect_extref_node(txn, child_paddr, bsz, leaf_recs, all_padrs)?;
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

        for chunk in partition_records(&recs, cap) {
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
            for chunk in partition_records(&lvl_recs, cap) {
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
fn rewrite_extref_tree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    old_root_paddr: u64,
    new_xid: u64,
    insert_recs: Vec<(Vec<u8>, Vec<u8>)>,
    remove_keys: &[Vec<u8>],
    upsert_recs: &[(Vec<u8>, Vec<u8>)],
    bsz: usize,
) -> Result<(u64, Vec<u64>, i64), TxnError> {
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

    // Apply inserts (reject duplicate keys - same policy as insert_catalog_records).
    for (ik, iv) in insert_recs {
        recs.push((ik, iv));
    }

    // 3. Sort by key (same comparator as catalog/fstree).
    recs.sort_by(|a, b| cat_key_cmp(&a.0, &b.0));

    // 4. Rebuild and return.
    let (new_root_paddr, node_delta) = build_extref_tree(txn, recs, new_xid, old_node_count, bsz)?;
    Ok((new_root_paddr, old_padrs, node_delta))
}

/// Recursively collect every (key, value) leaf record of the FSTREE rooted at
/// virtual `oid`, resolving child node oids through the volume omap node.
fn collect_fstree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    oid: u64,
    bsz: usize,
) -> Result<Vec<CatRec>, TxnError> {
    // Resolve through the (possibly multi-node) volume omap for the live view.
    // omap_lookup only reads one node and misreads an index root once the omap
    // splits (#157); omap_resolve descends. u64::MAX = latest version.
    let paddr = omap_resolve(txn, omap_node, oid, u64::MAX, bsz)?
        .ok_or_else(|| TxnError::SpacemanParse("fstree oid not in volume omap".into()))?;
    let mut node = vec![0u8; bsz];
    txn.read_block(paddr, &mut node)?;
    let recs = parse_cat_leaf(&node).map_err(|e| TxnError::SpacemanParse(e.into()))?;
    if rd_u16(&node, 34) == 0 {
        return Ok(recs.into_iter().map(|r| (r.key, r.val)).collect());
    }
    let mut out = Vec::new();
    for r in recs {
        out.extend(collect_fstree(txn, omap_node, rd_u64(&r.val, 0), bsz)?);
    }
    Ok(out)
}

/// Return true if `drec_key` (a parent+name directory record) already exists in
/// the catalog. Used to reject duplicate-name creates/writes/renames BEFORE any
/// block is allocated, so a rejected op leaves no trace (no leaked blocks) and
/// never produces a duplicate DREC (which corrupts the fsroot tree).
fn drec_exists<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    root_tree_oid: u64,
    drec_key: &[u8],
    bsz: usize,
) -> Result<bool, TxnError> {
    let all = collect_fstree(txn, omap_node, root_tree_oid, bsz)?;
    Ok(all.iter().any(|(k, _)| k.as_slice() == drec_key))
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
fn free_replaced_metadata<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    vsb_raw: &[u8],
    vol_omap_raw: &[u8],
    old_omap_node: &[u8],
    old_extref_padrs: &[u64],
    skip_catalog_reclaim: bool,
    bsz: usize,
) -> Result<i64, TxnError> {
    // Newest snapshot xid (0 = no snapshots). Pinned iff a block's xid <= this.
    let newest_snap = rd_u64(vol_omap_raw, 64); // omap_phys.om_most_recent_snap

    // Non-catalog COW-pairs (omap header, omap b-tree, old extentref nodes):
    // old freed / sm_fq'd, new allocated - net 0, never subtracted by callers.
    // old_extref_padrs: every block that was part of the old extref tree
    // (may be multiple nodes when the tree was already multi-node). [#151]
    // Walk the OLD (possibly multi-node) omap b-tree once: every tree node must
    // be freed (a split omap has more than the root node, else the non-root
    // nodes leak → fsck "overallocation"), and every LEAF mapping gives the old
    // catalog node paddrs to reclaim. [#157]
    let old_omap_root_paddr = rd_u64(vol_omap_raw, 48);
    let mut old_omap_triples: Vec<(u64, u64, u64)> = Vec::new();
    let mut old_omap_nodes: Vec<u64> = Vec::new();
    walk_omap_tree(
        txn,
        old_omap_node,
        old_omap_root_paddr,
        bsz,
        &mut old_omap_triples,
        &mut old_omap_nodes,
    )?;

    let non_catalog: Vec<u64> = {
        let mut v = vec![rd_u64(vsb_raw, VSBI_OMAP_OID)]; // old omap_phys header
        v.extend_from_slice(&old_omap_nodes); // every old omap b-tree node
        v.extend_from_slice(old_extref_padrs);
        v
    };

    // Catalog nodes: only process entries with xid > newest_snap (the
    // "intermediate" ones that were pruned from the new omap by rewrite_fstree).
    // Snapshot-pinned entries (xid <= newest_snap) are RETAINED verbatim in
    // the new omap - their backing blocks are neither freed nor sm_fq'd here.
    // Processing them would cause duplicate sm_fq enqueues on every commit
    // because the same paddr keeps appearing in old_omap_node. [#149 fix]
    //
    // #140: the INCREMENTAL rename fast path keeps almost every catalog leaf at
    // its existing paddr (only the touched leaves are COW'd, and the fast path
    // freed exactly those itself). The full-rebuild's assumption - that EVERY
    // old catalog node is superseded - is false there, so freeing all of them
    // would drop blocks the new omap still references (fsck "underallocation").
    // When the fast path ran, skip catalog reclaim entirely; the non-catalog
    // COW-pairs (omap header + omap b-tree + extentref) are still superseded and
    // are reclaimed above regardless.
    let catalog_padrs: Vec<u64> = if skip_catalog_reclaim {
        Vec::new()
    } else {
        old_omap_triples
            .iter()
            .filter(|&&(_oid, xid, _paddr)| xid > newest_snap)
            .map(|&(_oid, _xid, paddr)| paddr)
            .collect()
    };

    // Net fs_alloc_count correction to return to callers.
    //
    // Per apfsprogs Q5 model: blocks sent to sm_fq are decremented from
    // fs_alloc_count at enqueue time (they are no longer live-owned).
    // Blocks freed immediately via free_block are also decremented.
    //
    // For non-catalog COW-pairs (omap hdr, omap btree, extentref):
    //   - immediate free: caller's alloc_delta already nets to 0 (new alloc
    //     cancels old free); no fs_alloc_count adjustment needed here.
    //   - sm_fq: NEW block was allocated (+1 via alloc_block inside
    //     rewrite_fstree / COW path), OLD block goes to sm_fq. Per Q5,
    //     old block must be decremented NOW. So correction = -1.
    //
    // For catalog nodes:
    //   - immediate free (xid > newest_snap): pruned intermediate entry;
    //     node_delta = new_count - old_replaceable_nodes already accounts
    //     for it. No additional correction.
    //   - sm_fq (xid <= newest_snap, after filtering): only snapshot-pinned
    //     catalog nodes reach here, and they were filtered out above.
    //     In practice catalog_padrs only contains xid > newest_snap entries,
    //     so this branch is dead. But if it fires, same logic: sm_fq = -1
    //     correction (old goes to sm_fq, decrement from fs_alloc_count).
    //
    // Summary: correction += -1 for every sm_fq enqueue, 0 for free_block.
    let mut correction: i64 = 0;

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
                // sm_fq: old COW-pair block sent to deferred reclaim.
                // Per Q5: decrement fs_alloc_count at enqueue time.
                txn.enqueue_sm_fq(newest_snap, paddr)?;
                correction -= 1;
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
                txn.enqueue_sm_fq(newest_snap, paddr)?;
                correction -= 1;
            }
        }
    }
    Ok(correction)
}

/// Parse all entries of a fixed-kv volume omap node into (oid, xid, paddr)
/// triples. Mirrors `omap_lookup`'s layout decoding. [the APFS specification]
pub(crate) fn parse_omap_entries(node: &[u8], bsz: usize) -> Vec<(u64, u64, u64)> {
    let nkeys = rd_u32(node, 36) as usize;
    let toc_off = rd_u16(node, 40) as usize;
    let toc_len = rd_u16(node, 42) as usize;
    let is_root = rd_u16(node, 32) & 0x1 != 0;
    let key_area = DATA_BASE + toc_off + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    let mut out = Vec::with_capacity(nkeys);
    for i in 0..nkeys {
        let te = DATA_BASE + toc_off + i * 4;
        let k_off = rd_u16(node, te) as usize;
        let v_off = rd_u16(node, te + 2) as usize;
        let k = key_area + k_off;
        let oid = rd_u64(node, k);
        let xid = rd_u64(node, k + 8);
        let v = val_area_end - v_off;
        let paddr = rd_u64(node, v + 8);
        out.push((oid, xid, paddr));
    }
    out
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
    triples: &mut Vec<(u64, u64, u64)>,
    nodes: &mut Vec<u64>,
) -> Result<(), TxnError> {
    nodes.push(node_paddr);
    if rd_u16(node, 34) == 0 {
        // Leaf: decode its {oid, xid} -> paddr mappings.
        triples.extend(parse_omap_entries(node, bsz));
        return Ok(());
    }
    // Index node: each value is an 8-byte PHYSICAL child paddr - recurse.
    let nkeys = rd_u32(node, 36) as usize;
    let toc_off = rd_u16(node, 40) as usize;
    let is_root = rd_u16(node, 32) & BTNODE_ROOT != 0;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    for i in 0..nkeys {
        let te = DATA_BASE + toc_off + i * 4;
        let v_off = rd_u16(node, te + 2) as usize;
        let v = val_area_end - v_off;
        let child = rd_u64(node, v);
        let mut buf = vec![0u8; bsz];
        txn.read_block(child, &mut buf)?;
        walk_omap_tree(txn, &buf, child, bsz, triples, nodes)?;
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
) -> Result<Vec<(u64, u64, u64)>, TxnError> {
    let mut triples = Vec::new();
    let mut nodes = Vec::new();
    walk_omap_tree(txn, root_node, root_paddr, bsz, &mut triples, &mut nodes)?;
    Ok(triples)
}

/// Rebuild the volume omap b-tree (fixed-kv, key {oid,xid} 16B, val
/// {flags,size,paddr} 16B) from `entries` (oid, xid, paddr) triples - each
/// keyed by its OWN xid so that snapshot-time versions can coexist with the
/// live version. The node's own o_xid is set to `xid` (the current txn).
/// Header/flags/footer skeleton copied from `template`. [the APFS specification]
pub(crate) fn build_omap_node(
    template: &[u8],
    entries: &[(u64, u64, u64)],
    paddr: u64,
    xid: u64,
    bsz: usize,
) -> Vec<u8> {
    let mut sorted = entries.to_vec();
    // omap b-tree key order: oid ascending, then xid ascending.
    sorted.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let nkeys = sorted.len();
    let mut out = vec![0u8; bsz];
    // obj_phys + btn_flags/level from template (fixed-kv root omap node).
    wr_bytes(&mut out, 0, template.get(..36).unwrap_or(&[]));
    wr_u64(&mut out, 8, paddr); // physical o_oid = paddr
    wr_u64(&mut out, 16, xid);
    wr_u32(&mut out, 36, nkeys as u32);
    wr_u16(&mut out, 48, BTOFF_INVALID);
    wr_u16(&mut out, 52, BTOFF_INVALID);
    // Preserve the template's TOC reserve (e.g. 448) so layout matches the kernel.
    let toc_len = rd_u16(template, 42) as usize;
    let key_area = DATA_BASE + toc_len;
    let val_area_end = bsz - BTREE_INFO_SIZE;
    for (i, &(oid, kxid, p)) in sorted.iter().enumerate() {
        let k = key_area + i * 16;
        wr_u64(&mut out, k, oid);
        wr_u64(&mut out, k + 8, kxid);
        let v = val_area_end - (i + 1) * 16;
        wr_u32(&mut out, v, 0); // ov_flags
        wr_u32(&mut out, v + 4, bsz as u32); // ov_size
        wr_u64(&mut out, v + 8, p); // ov_paddr
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
                                                                    // Footer: copy template, patch key_count.
    let bti = bsz - BTREE_INFO_SIZE;
    wr_bytes(&mut out, bti, template.get(bti..).unwrap_or(&[]));
    wr_u64(&mut out, bti + 24, nkeys as u64); // bt_key_count
    update_checksum_in_place(&mut out);
    out
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
fn build_omap_tree<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    template: &[u8],
    triples: &[(u64, u64, u64)],
    new_xid: u64,
    bsz: usize,
) -> Result<(u64, u64), TxnError> {
    let mut sorted = triples.to_vec();
    sorted.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
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
        .map(|&(oid, xid, p)| {
            let mut k = [0u8; 16];
            k[..8].copy_from_slice(&oid.to_le_bytes());
            k[8..].copy_from_slice(&xid.to_le_bytes());
            let mut v = vec![0u8; 16];
            wr_u32(&mut v, 0, 0); // ov_flags
            wr_u32(&mut v, 4, bsz as u32); // ov_size
            wr_u64(&mut v, 8, p); // ov_paddr
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

/// Rebuild a fixed-kv volume omap node from `template`, dropping the single
/// entry that matches `(drop_oid, drop_xid)`. Used by snapshot revert (COW-7):
/// the live-xid `root_tree_oid` mapping must be REMOVED (not zeroed) so that
/// `Omap::resolve` - which selects the highest xid <= query and only skips
/// `OMAP_VAL_DELETED`, never a `paddr == 0` - falls back to the snapshot's
/// fsroot instead of resolving to block 0 (the NX superblock). Reuses
/// `parse_omap_entries` + `build_omap_node` so the layout matches what the
/// kernel/fsck expect (no hand-rolled TOC surgery). [COW-7;
/// docs/refs/di-tier-verification-2026-05.md]
pub(crate) fn rebuild_omap_node_without(
    template: &[u8],
    drop_oid: u64,
    drop_xid: u64,
    new_paddr: u64,
    new_xid: u64,
    bsz: usize,
) -> Vec<u8> {
    let surviving: Vec<(u64, u64, u64)> = parse_omap_entries(template, bsz)
        .into_iter()
        .filter(|&(oid, xid, _)| !(oid == drop_oid && xid == drop_xid))
        .collect();
    build_omap_node(template, &surviving, new_paddr, new_xid, bsz)
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
) -> Result<(u64, i64), TxnError> {
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

/// Like [`rewrite_fstree`] but additionally REPLACES the value of already-present
/// records listed in `update_records` (matched by exact key). Used by clone_file
/// to bump the source inode flags + the source dstream_id refcnt without the
/// duplicate-key rejection that `new_records` enforces. [#141]
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
) -> Result<(u64, i64), TxnError> {
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let mut recs = collect_fstree(txn, omap_node, root_tree_oid, bsz)?;

    if let Some((pino, delta)) = parent_patch {
        let want = encode_jkey(pino, APFS_TYPE_INODE);
        for (k, v) in recs.iter_mut() {
            if rd_u64(k, 0) == want {
                let nch = rd_u32(v, INODE_NCHILDREN) as i64;
                wr_u32(v, INODE_NCHILDREN, (nch + delta).max(0) as u32);
                wr_u64(v, INODE_MOD_TIME, now);
                wr_u64(v, INODE_CHANGE_TIME, now);
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
        for chunk in partition_records(&recs, cap) {
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
            for chunk in partition_records(&lvl_recs, cap) {
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
    let mut omap_triples: Vec<(u64, u64, u64)> = omap_entries
        .iter()
        .map(|&(oid, p)| (oid, new_xid, p))
        .collect();
    // Read EVERY old omap leaf mapping by walking the (possibly multi-node) old
    // omap tree - `parse_omap_entries` alone would see only the root node and so
    // miss every mapping below a split (#157). The omap node's o_oid (@8) is its
    // own physical paddr (BTREE_PHYSICAL). Used for snapshot retention AND the
    // node_delta accounting below.
    let old_omap_root_paddr = rd_u64(omap_node, 8);
    let old_omap_triples = collect_omap_triples(txn, omap_node, old_omap_root_paddr, bsz)?;
    let has_snapshot = rd_u64(vsb_raw, VSBI_NUM_SNAPSHOTS) > 0;
    if has_snapshot {
        // Read om_most_recent_snap from the live volume omap header to set
        // the retention boundary (matches `free_replaced_metadata`'s rule).
        let vol_omap_paddr = rd_u64(vsb_raw, VSBI_OMAP_OID);
        let mut vom = vec![0u8; bsz];
        txn.read_block(vol_omap_paddr, &mut vom)?;
        let newest_snap = rd_u64(&vom, 64); // omap_phys.om_most_recent_snap
        let new_pairs: std::collections::HashSet<(u64, u64)> =
            omap_triples.iter().map(|&(o, x, _)| (o, x)).collect();
        for &(oid, xid, p) in &old_omap_triples {
            if !new_pairs.contains(&(oid, xid)) && xid <= newest_snap {
                omap_triples.push((oid, xid, p));
            }
        }
    }

    // Build the new volume omap as a (possibly MULTI-NODE) b-tree (#157):
    // the single-node `build_omap_node` overflowed once the mappings no longer
    // fit one node. Returns the root paddr.
    let (new_omap_tree_paddr, _new_omap_node_count) =
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
        .filter(|&&(_, xid, _)| xid > newest_snap_for_delta)
        .count() as i64;
    let new_catalog_nodes = omap_entries.len() as i64;
    let node_delta = new_catalog_nodes - old_replaceable_nodes;
    Ok((new_omap_tree_paddr, node_delta))
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
fn parse_cat_leaf(node: &[u8]) -> Result<Vec<CatRecord>, &'static str> {
    let bsz = node.len();
    let is_root = rd_u16(node, 32) & 0x1 != 0;
    let nkeys = rd_u32(node, 36) as usize;
    let toc_off = rd_u16(node, 40) as usize;
    let toc_len = rd_u16(node, 42) as usize;
    let key_area = DATA_BASE + toc_off + toc_len;
    let val_area_end = bsz - if is_root { BTREE_INFO_SIZE } else { 0 };
    let toc_base = DATA_BASE + toc_off;
    let mut out = Vec::with_capacity(nkeys);
    for i in 0..nkeys {
        let te = toc_base + i * 8;
        let k_off = rd_u16(node, te) as usize;
        let k_len = rd_u16(node, te + 2) as usize;
        let v_off = rd_u16(node, te + 4) as usize;
        let v_len = rd_u16(node, te + 6) as usize;
        let ks = key_area + k_off;
        let vs = val_area_end - v_off;
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

    let mut stats = VerifyStats::default();
    let mut longest_key = 0u32;
    let mut longest_val = 0u32;
    verify_cat_node(
        txn,
        &omap_node,
        &root,
        None,
        bsz,
        &mut stats,
        &mut longest_key,
        &mut longest_val,
    )?;

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

/// Recursive worker for [`verify_fsroot`]. Validates one catalog node and (for an
/// internal node) descends into every child, threading the parent-separator key
/// and the running longest-key/val + node/record counters.
#[allow(clippy::too_many_arguments)]
fn verify_cat_node<D: WritableBlockDevice>(
    txn: &mut Transaction<D>,
    omap_node: &[u8],
    node: &[u8],
    expect_min_key: Option<&[u8]>,
    bsz: usize,
    stats: &mut VerifyStats,
    longest_key: &mut u32,
    longest_val: &mut u32,
) -> Result<(), String> {
    let level = rd_u16(node, 34);
    stats.nodes += 1;
    let recs =
        parse_cat_leaf(node).map_err(|e| format!("parse catalog node (level {level}): {e}"))?;
    if recs.is_empty() {
        return Err(format!("empty catalog node (level {level})"));
    }
    // Separator invariant: this node's minimum key must equal the key the parent
    // stored as the separator for it. (The root has no parent → skip.)
    if let Some(min) = expect_min_key {
        if recs[0].key.as_slice() != min {
            return Err(format!(
                "catalog node min key != parent separator (level {level})"
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
        for r in &recs {
            if r.val.len() < 8 {
                return Err(format!(
                    "internal catalog value shorter than 8 bytes (level {level})"
                ));
            }
            let child_oid = rd_u64(&r.val, 0);
            let child_paddr = omap_resolve(txn, omap_node, child_oid, u64::MAX, bsz)
                .map_err(|e| format!("resolve child oid {child_oid}: {e}"))?
                .ok_or_else(|| format!("child oid {child_oid} not in volume omap"))?;
            let mut child = vec![0u8; bsz];
            txn.read_block(child_paddr, &mut child)
                .map_err(|e| format!("read child node paddr {child_paddr}: {e}"))?;
            verify_cat_node(
                txn,
                omap_node,
                &child,
                Some(&r.key),
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

    let mut out = vec![0u8; bsz];
    // Copy obj_phys header + node fields verbatim, then overwrite layout fields.
    wr_bytes(&mut out, 0, node.get(..DATA_BASE).ok_or("short node")?);
    // btree_info_t footer copied verbatim (root node); we patch longest_*.
    wr_bytes(
        &mut out,
        bsz - BTREE_INFO_SIZE,
        node.get(bsz - BTREE_INFO_SIZE..).ok_or("short footer")?,
    );

    // TOC capacity. A non-empty node packs the table of contents tightly
    // (nkeys * 8 bytes per kvloc entry). An EMPTY node must still reserve a
    // non-zero TOC area: the kernel's canonical empty b-tree root reserves
    // 64 bytes (8 kvloc slots) with table_space.len == 64. A node packed with
    // table_space.len == 0 is rejected by fsck_apfs ("tree is invalid"). This
    // only triggers when a single-node tree is emptied by deletion (e.g. the
    // extentref tree after unlinking the last extent-bearing file).
    // [CERTAIN: empirical - kernel fresh-format empty extentref node carries
    //  table_space(off=0,len=64), free_space(off=0,len=3936); reproducing it
    //  byte-for-byte makes fsck CLEAN. To be confirmed via the APFS specification (the APFS spec).]
    const EMPTY_TOC_CAP: usize = 64;
    let toc_cap = if nkeys == 0 { EMPTY_TOC_CAP } else { nkeys * 8 };
    let key_area = DATA_BASE + toc_cap;
    let val_area_end = bsz - BTREE_INFO_SIZE; // root node

    let mut key_cursor = key_area;
    let mut val_cursor = val_area_end;
    let mut longest_k = 0u32;
    let mut longest_v = 0u32;
    for (i, r) in recs.iter().enumerate() {
        let k_off = key_cursor - key_area;
        wr_bytes(&mut out, key_cursor, &r.key);
        key_cursor += r.key.len();
        val_cursor -= r.val.len();
        wr_bytes(&mut out, val_cursor, &r.val);
        let v_off = val_area_end - val_cursor;
        let te = DATA_BASE + i * 8;
        wr_u16(&mut out, te, k_off as u16);
        wr_u16(&mut out, te + 2, r.key.len() as u16);
        wr_u16(&mut out, te + 4, v_off as u16);
        wr_u16(&mut out, te + 6, r.val.len() as u16);
        longest_k = longest_k.max(r.key.len() as u32);
        longest_v = longest_v.max(r.val.len() as u32);
        if key_cursor > val_cursor {
            return Err("node overflow");
        }
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
    let bti = bsz - BTREE_INFO_SIZE;
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
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let all = collect_fstree(txn, &omap_node, root_tree_oid, bsz)?;

    // Locate old DREC -> file_id, date_added, dt_type.
    let old_drec_key = build_drec_key(old_parent_ino, old_name, case_fold, normalize);
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
                let ancestor_key = build_inode_key(cur);
                match all
                    .iter()
                    .find(|(k, _)| *k == ancestor_key)
                    .map(|(_, v)| v.as_slice())
                {
                    Some(v) => cur = rd_u64(v, 0),
                    None => break,
                }
            }
        }
    }

    // Rebuild inode: update parent_id + NAME xfield, bump mtime + ctime.
    let mut new_inode_val = rebuild_inode_with_name(&old_inode_val, new_name, now);
    // Overwrite parent_id (offset 0 in j_inode_val fixed prefix).
    wr_u64(&mut new_inode_val, 0, new_parent_ino);
    // Also bump mod_time (rebuild_inode_with_name only bumps change_time).
    wr_u64(&mut new_inode_val, INODE_MOD_TIME, now);

    // New DREC under new_parent_ino with new_name (preserve date_added + dt_type).
    let new_drec_key = build_drec_key(new_parent_ino, new_name, case_fold, normalize);
    let new_drec = (
        new_drec_key.clone(),
        build_drec_val(file_id, date_added, dt_type),
    );

    // Detect an existing target with `new_name` under `new_parent_ino`. If
    // present and `replace_if_exists`, collect its records + owned blocks
    // for the same transaction; otherwise return AlreadyExists.
    let target_replace =
        collect_replace_target(&all, &new_drec_key, file_id, replace_if_exists, bsz)?;

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

    // Apply nchildren deltas directly to the collected records.
    // We pass parent_patch=None to rewrite_fstree and do this ourselves so
    // we can handle two distinct parent inodes in one pass.
    let same_parent = old_parent_ino == new_parent_ino;
    let (new_omap_tree_paddr, node_delta) = {
        // We need a version of rewrite_fstree that can patch two parents.
        // Strategy: pre-patch the collected records, then call with parent_patch=None.
        // collect_fstree was already called above; replay inline.
        let root_oid = root_tree_oid;
        let mut recs = collect_fstree(txn, &omap_node, root_oid, bsz)?;

        // Apply parent nchildren patches. Deltas depend on whether a target
        // was replaced and whether the move is same-parent or cross-parent:
        //
        //   cross-parent, no replace: old -1, new +1  (one entry moves between dirs)
        //   cross-parent, with replace: old -1, new  0  (target slot filled by src)
        //   same-parent,  no replace: same  0          (in-place rename)
        //   same-parent,  with replace: same -1        (two entries collapse into one)
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
        let new_parent_delta: i64 = if same_parent {
            0
        } else if replacing {
            0
        } else {
            1
        };

        for (k, v) in recs.iter_mut() {
            let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
            let ty = rd_u64(k, 0) >> 60;
            if ty != APFS_TYPE_INODE {
                continue;
            }
            if oid == old_parent_ino {
                if old_parent_delta != 0 {
                    let nch = rd_u32(v, INODE_NCHILDREN) as i64;
                    wr_u32(v, INODE_NCHILDREN, (nch + old_parent_delta).max(0) as u32);
                }
                // Always bump mtime/ctime on old parent.
                wr_u64(v, INODE_MOD_TIME, now);
                wr_u64(v, INODE_CHANGE_TIME, now);
            }
            if oid == new_parent_ino && !same_parent {
                if new_parent_delta != 0 {
                    let nch = rd_u32(v, INODE_NCHILDREN) as i64;
                    wr_u32(v, INODE_NCHILDREN, (nch + new_parent_delta).max(0) as u32);
                }
                wr_u64(v, INODE_MOD_TIME, now);
                wr_u64(v, INODE_CHANGE_TIME, now);
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
            for chunk in partition_records(&recs, cap) {
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
                for chunk in partition_records(&lvl_recs, cap) {
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

        let mut omap_triples: Vec<(u64, u64, u64)> = omap_entries
            .iter()
            .map(|&(oid, p)| (oid, new_xid, p))
            .collect();
        let has_snapshot = rd_u64(vsb_raw, VSBI_NUM_SNAPSHOTS) > 0;
        if has_snapshot {
            let vol_omap_paddr = rd_u64(vsb_raw, VSBI_OMAP_OID);
            let mut vom = vec![0u8; bsz];
            txn.read_block(vol_omap_paddr, &mut vom)?;
            let newest_snap = rd_u64(&vom, 64);
            let new_pairs: std::collections::HashSet<(u64, u64)> =
                omap_triples.iter().map(|&(o, x, _)| (o, x)).collect();
            for (oid, xid, p) in parse_omap_entries(&omap_node, bsz) {
                if !new_pairs.contains(&(oid, xid)) && xid <= newest_snap {
                    omap_triples.push((oid, xid, p));
                }
            }
        }

        let new_omap_tree_paddr = txn.alloc_block()?;
        let node = build_omap_node(&omap_node, &omap_triples, new_omap_tree_paddr, new_xid, bsz);
        txn.stage_raw(new_omap_tree_paddr, node);

        let old_catalog_nodes = parse_omap_entries(&omap_node, bsz).len() as i64;
        let new_catalog_nodes = omap_entries.len() as i64;
        let node_delta = new_catalog_nodes - old_catalog_nodes;
        (new_omap_tree_paddr, node_delta)
    };

    // COW the volume omap header.
    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = vol_omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

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
    // move_entry always uses the full rebuild (no incremental fast path), so the
    // old catalog nodes are genuinely superseded → reclaim them (skip = false).
    let frm_correction = free_replaced_metadata(
        txn,
        vsb_raw,
        vol_omap_raw,
        &omap_node,
        &extref_old_padrs,
        false,
        bsz,
    )?;

    if target_replace.is_some() {
        let num_files = rd_u64(&new_vsb, VSBI_NUM_FILES);
        wr_u64(&mut new_vsb, VSBI_NUM_FILES, num_files.saturating_sub(1));
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
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(vol_omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let all = collect_fstree(txn, &omap_node, root_tree_oid, bsz)?;

    // Locate DREC -> file_id.
    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
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
    const INODE_CREATE_TIME: usize = 16; // btime (birth time)
    const INODE_ACCESS_TIME: usize = 40;
    const INODE_MODE: usize = 80;

    if let Some(t) = btime_ns {
        wr_u64(&mut new_inode_val, INODE_CREATE_TIME, t);
    }
    if let Some(t) = mtime_ns {
        wr_u64(&mut new_inode_val, INODE_MOD_TIME, t);
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
    let (new_omap_tree_paddr, node_delta) = rewrite_fstree(
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
    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = vol_omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

    // Reclaim COW-replaced metadata (set_inode_attrs never touches extentref).
    let frm_correction =
        free_replaced_metadata(txn, vsb_raw, vol_omap_raw, &omap_node, &[], false, bsz)?;

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

/// Metadata-only file resize - patches `dstream.size` and the inode mtime,
/// without reading or writing any data blocks. Covers two cases on the fast
/// path: any extend (`new_size >= current_size`) and any shrink that drops
/// only whole past-EOF extents (no partial-extent boundary cut needed).
///
/// For an extend, the resulting file is sparse: read past `alloced_size`
/// returns zeros, and a subsequent write into the extended range materialises
/// the blocks lazily through `write_file`.
///
/// For an aligned shrink, the dropped extents' blocks are freed via the
/// spaceman-free-queue / live-extref-tree pattern; the boundary extent (if
/// any) is left intact, leaving at most `bsz - 1` bytes of internal slack
/// (POSIX-legal, future M8 #5b can reclaim).
///
/// Returns:
/// - `Ok(TruncateOutcome::Applied)` - done.
/// - `Ok(TruncateOutcome::FastPathDeclined)` - out-of-scope (e.g. partial-
///   extent boundary cut). The caller should fall back.
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
    let bsz = txn.nx.block_size as usize;
    let new_xid = txn.xid;
    let now = now_ns();
    let root_tree_oid = rd_u64(vsb_raw, VSBI_ROOT_TREE_OID);
    let incompat = rd_u64(vsb_raw, VSBI_INCOMPAT_FEATURES);
    let case_fold = incompat & APFS_INCOMPAT_CASE_INSENSITIVE != 0;
    let normalize = true;

    let omap_tree_paddr = rd_u64(omap_raw, 48);
    let mut omap_node = vec![0u8; bsz];
    txn.read_block(omap_tree_paddr, &mut omap_node)?;
    let all = collect_fstree(txn, &omap_node, root_tree_oid, bsz)?;

    let drec_key = build_drec_key(parent_ino, name, case_fold, normalize);
    let file_id = all
        .iter()
        .find(|(k, _)| *k == drec_key)
        .map(|(_, v)| rd_u64(v, 0) & 0x0FFF_FFFF_FFFF_FFFF)
        .ok_or_else(|| TxnError::NotFound(format!("'{name}' not in parent {parent_ino}")))?;

    // Walk extents for this file; classify each as keep / drop on shrink.
    // For extend, every extent is kept (no walk needed) - but we still need
    // total alloced size to write into the new inode's dstream xfield.
    let mut old_inode_val: Option<Vec<u8>> = None;
    let mut old_alloced: u64 = 0;
    let mut extents: Vec<(u64, u64, u64, u64)> = Vec::new(); // (logical_addr, length_bytes, phys_start, block_count)
    for (k, v) in &all {
        let oid = rd_u64(k, 0) & 0x0FFF_FFFF_FFFF_FFFF;
        let ty = rd_u64(k, 0) >> 60;
        if oid != file_id {
            continue;
        }
        if ty == APFS_TYPE_INODE {
            old_inode_val = Some(v.clone());
        }
        if ty == APFS_TYPE_FILE_EXTENT {
            let logical_addr = rd_u64(k, 8);
            let len_bytes = rd_u64(v, 0) & 0x00FF_FFFF_FFFF_FFFF;
            let phys = rd_u64(v, 8);
            let block_count = len_bytes / bsz as u64;
            old_alloced += len_bytes;
            extents.push((logical_addr, len_bytes, phys, block_count));
        }
    }
    let old_inode_val =
        old_inode_val.ok_or_else(|| TxnError::NotFound(format!("inode {file_id}")))?;

    // The full 92-byte fixed prefix and all xfields are forwarded via
    // rebuild_inode_preserving_unknown - only the name is extracted separately.
    let old_name_str = extract_inode_name(&old_inode_val).unwrap_or(name);

    // ---- No-op: same size. ----
    if new_size == current_size {
        return Ok(TruncateOutcome::Applied);
    }

    // ---- Extend (new_size > current_size): no extent change. ----
    if new_size > current_size {
        // Preserve unknown xfields from the original inode. [W4-FRAG-1]
        let new_inode_val = rebuild_inode_preserving_unknown(
            &old_inode_val,
            old_name_str,
            Some(DstreamArgs {
                size: new_size,
                // alloced_size unchanged - sparse extension. Reading past the
                // last extent returns zeros via the reader's extent-walk.
                alloced_size: old_alloced,
            }),
            now,
        );
        let inode_key = build_inode_key(file_id);
        return finish_metadata_only_rewrite(
            txn,
            vsb_raw,
            omap_raw,
            &omap_node,
            new_xid,
            now,
            vec![(inode_key.clone(), new_inode_val)],
            vec![inode_key],
            None,
            bsz,
        )
        .map(|()| TruncateOutcome::Applied);
    }

    // ---- Shrink: classify extents. Decline on boundary cut. ----
    let new_alloced_eof = new_size; // file logical EOF; alloced is bsz-rounded
    let mut keep_alloced: u64 = 0;
    let mut drop_keys: Vec<Vec<u8>> = Vec::new();
    let mut drop_runs: Vec<(u64, u64)> = Vec::new();
    for &(logical_addr, len_bytes, phys, blk_count) in &extents {
        let ext_end = logical_addr + len_bytes;
        if ext_end <= new_alloced_eof {
            keep_alloced = keep_alloced.max(ext_end);
        } else if logical_addr >= new_alloced_eof {
            drop_keys.push(build_file_extent_key(file_id, logical_addr));
            drop_runs.push((phys, blk_count));
        } else {
            // Boundary: extent crosses the new EOF - needs partial-extent
            // cut (M8 #5b). Decline so caller can fall back.
            return Ok(TruncateOutcome::FastPathDeclined);
        }
    }

    // Free dropped blocks against the live extref tree. [#151: multi-node aware]
    let old_extref_paddr = rd_u64(vsb_raw, VSBI_EXTENTREF_TREE_OID);
    // extref_update: Some((new_paddr, old_padrs, node_delta, freed_count)) or None.
    let extref_update_opt = if drop_runs.is_empty() {
        None
    } else {
        let (live_recs_pairs, _) = collect_extref_records(txn, old_extref_paddr, bsz)?;
        let live_keys: Vec<Vec<u8>> = live_recs_pairs.into_iter().map(|(k, _)| k).collect();
        let mut owned_keys: Vec<Vec<u8>> = Vec::new();
        let mut owned_runs: Vec<(u64, u64)> = Vec::new();
        for &(phys, count) in &drop_runs {
            let key = build_phys_ext_key(phys);
            if live_keys.contains(&key) {
                owned_keys.push(key);
                owned_runs.push((phys, count));
            }
        }
        if owned_keys.is_empty() {
            None
        } else {
            let (new_paddr, old_padrs, nd) = rewrite_extref_tree(
                txn,
                old_extref_paddr,
                new_xid,
                vec![],
                &owned_keys,
                &[],
                bsz,
            )?;
            let mut total: u64 = 0;
            for &(phys, count) in &owned_runs {
                for b in phys..phys + count {
                    txn.free_block(b)?;
                }
                total += count;
            }
            Some((new_paddr, old_padrs, nd, total))
        }
    };

    // Preserve unknown xfields from the original inode. [W4-FRAG-1]
    let new_inode_val = rebuild_inode_preserving_unknown(
        &old_inode_val,
        old_name_str,
        Some(DstreamArgs {
            size: new_size,
            alloced_size: keep_alloced,
        }),
        now,
    );
    let inode_key = build_inode_key(file_id);
    let mut new_records: Vec<(Vec<u8>, Vec<u8>)> = vec![(inode_key.clone(), new_inode_val)];
    let mut remove_keys: Vec<Vec<u8>> = vec![inode_key];
    remove_keys.append(&mut drop_keys);
    let _ = &mut new_records; // no extra inserts beyond inode replacement

    finish_metadata_only_rewrite(
        txn,
        vsb_raw,
        omap_raw,
        &omap_node,
        new_xid,
        now,
        new_records,
        remove_keys,
        extref_update_opt,
        bsz,
    )
    .map(|()| TruncateOutcome::Applied)
}

/// Shared tail of [`truncate_file_fast`]: rewrite the fstree with the inode
/// replacement (+ optional extent drops), COW the volume omap, stage the
/// volume superblock with adjusted accounting, free COW-replaced metadata.
///
/// `extref_update` is `Some((new_extref_paddr, old_padrs, extref_node_delta,
/// freed_block_count))` when the shrink path freed past-EOF extents; `None`
/// for extend. [#151: multi-node capable]
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
    bsz: usize,
) -> Result<(), TxnError> {
    let (new_omap_tree_paddr, node_delta) = rewrite_fstree(
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

    let new_vomap_paddr = txn.alloc_block()?;
    let mut new_vomap = omap_raw.to_vec();
    new_vomap.resize(bsz, 0);
    wr_u64(&mut new_vomap, 8, new_vomap_paddr);
    wr_u64(&mut new_vomap, 16, new_xid);
    wr_u64(&mut new_vomap, 48, new_omap_tree_paddr);
    update_checksum_in_place(&mut new_vomap);
    txn.stage_raw(new_vomap_paddr, new_vomap);

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
        false,
        bsz,
    )?;

    let fs_alloc = rd_u64(&new_vsb, VSBI_FS_ALLOC_COUNT) as i64;
    let alloc_delta = -(freed_count as i64) + node_delta + extref_node_delta_opt + frm_correction;
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

    /// COW-7: `rebuild_omap_node_without` removes exactly the live-xid
    /// root_tree mapping, leaves the snapshot's lower-xid mapping (and unrelated
    /// entries) intact, and never leaves a paddr==0 entry. After removal the
    /// highest xid for root_tree_oid is the snapshot's, so `Omap::resolve`
    /// returns the snapshot fsroot instead of block 0 (the NX superblock).
    #[test]
    fn rebuild_omap_node_without_removes_live_xid_entry() {
        let bsz = 4096usize;
        // Minimal fixed-kv root+leaf omap template (BTNODE_ROOT|LEAF|FIXED_KV).
        let mut template = vec![0u8; bsz];
        template[32] = 0x07;
        wr_u16(&mut template, 42, 64); // table_space.len reserve

        let root_oid = 0x402u64;
        let snap_xid = 5u64;
        let live_xid = 6u64;
        let revert_xid = 7u64;
        let entries = vec![
            (root_oid, snap_xid, 0x100u64), // snapshot fsroot
            (root_oid, live_xid, 0x200u64), // live fsroot (to be dropped)
            (0x500u64, 4u64, 0x300u64),     // unrelated mapping
        ];
        let node = build_omap_node(&template, &entries, 0x111, revert_xid - 1, bsz);

        let rebuilt = rebuild_omap_node_without(&node, root_oid, live_xid, 0x222, revert_xid, bsz);
        let got = parse_omap_entries(&rebuilt, bsz);

        assert!(
            !got.iter().any(|&(o, x, _)| o == root_oid && x == live_xid),
            "live_xid root mapping must be removed: {got:?}"
        );
        assert!(
            got.iter()
                .any(|&(o, x, p)| o == root_oid && x == snap_xid && p == 0x100),
            "snap_xid root mapping must survive: {got:?}"
        );
        assert!(
            got.iter()
                .any(|&(o, x, p)| o == 0x500 && x == 4 && p == 0x300),
            "unrelated mapping must survive: {got:?}"
        );
        assert!(
            !got.iter().any(|&(_, _, p)| p == 0),
            "no surviving entry may point at block 0 (the NXSB): {got:?}"
        );
        assert_eq!(got.len(), 2, "exactly one entry removed: {got:?}");
        let max_root = got
            .iter()
            .filter(|&&(o, _, _)| o == root_oid)
            .map(|&(_, x, _)| x)
            .max();
        assert_eq!(
            max_root,
            Some(snap_xid),
            "after revert the highest root_tree xid must be the snapshot's"
        );
        assert_eq!(rd_u64(&rebuilt, 16), revert_xid, "node o_xid = revert_xid");
        assert_eq!(rd_u64(&rebuilt, 8), 0x222, "node o_oid = new paddr");
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
    fn xattr_embedded_threshold_below_xattr_max_embedded_size() {
        // Our threshold must be strictly below the spec max so we never
        // accidentally try to embed something too large.
        assert!(
            XATTR_EMBEDDED_THRESHOLD < apfs_core::xattr::XATTR_MAX_EMBEDDED_SIZE,
            "XATTR_EMBEDDED_THRESHOLD ({}) must be < XATTR_MAX_EMBEDDED_SIZE ({})",
            XATTR_EMBEDDED_THRESHOLD,
            apfs_core::xattr::XATTR_MAX_EMBEDDED_SIZE,
        );
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
}
