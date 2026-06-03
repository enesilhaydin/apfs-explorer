//! j_inode_val_t (packed) + xfields walk. Offsets/rule:
//! ctx_search(source:"the APFS specification"). xfields[] starts at byte 92;
//! each extended field's data slot is round_up(x_size, 8) bytes.
use crate::endian::{u16_le, u32_le, u64_le, ParseError};

/// Extended-field type: the data stream descriptor (j_dstream_t). Apple p110.
pub const INO_EXT_TYPE_DSTREAM: u8 = 8;
/// Extended-field type: the filename string (NUL-terminated).
pub const INO_EXT_TYPE_NAME: u8 = 4;
/// Byte offset of xfields[] inside the packed j_inode_val_t.
pub const INODE_XFIELDS_OFFSET: usize = 92;

/// An xfield entry whose type is not recognized by this implementation.
/// Preserved verbatim on inode rewrite to avoid silent data loss when
/// operating on inodes created by macOS (e.g. DOCUMENT_ID, FINDER_INFO).
/// Layout: (x_type, x_flags, raw_data_bytes). The data bytes do NOT include
/// alignment padding; callers must re-pad to 8-byte boundaries on serialization.
///
/// Forward-compatible: unknown types round-trip unchanged through any write path
/// that uses [`parse_unknown_xfields`] + the apfs-write rebuild helpers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownXfield {
    pub x_type: u8,
    pub x_flags: u8,
    /// Raw payload bytes (without alignment padding).
    pub data: Vec<u8>,
}

/// Parse xfields from `inode_val`, returning only entries whose type is not
/// `INO_EXT_TYPE_NAME` (4) or `INO_EXT_TYPE_DSTREAM` (8). These are the
/// "recognized" types that our write path handles explicitly; everything else
/// is returned here for verbatim round-trip preservation.
///
/// Returns an empty `Vec` when the inode has no xfields or when all xfields
/// are recognized. Returns `Err` only on a structurally corrupt blob (truncated
/// header or negative-overflow offsets).
pub fn parse_unknown_xfields(inode_val: &[u8]) -> Result<Vec<UnknownXfield>, ParseError> {
    if inode_val.len() <= INODE_XFIELDS_OFFSET {
        return Ok(Vec::new());
    }
    // Guarded by the length check above; `.get` keeps it panic-free for clippy.
    let blob = inode_val.get(INODE_XFIELDS_OFFSET..).unwrap_or(&[]);
    let num = u16_le(blob, 0)? as usize;
    // xf_used_data at blob[2..4] - not needed for the walk.
    // Array of x_field_t starts at blob[4]; each entry is 4 bytes.
    let arr_off = 4usize;
    let data0 = arr_off
        .checked_add(num.checked_mul(4).ok_or(ParseError::Short {
            at: INODE_XFIELDS_OFFSET + arr_off,
            need: 0,
            len: inode_val.len(),
        })?)
        .ok_or(ParseError::Short {
            at: INODE_XFIELDS_OFFSET + arr_off,
            need: 0,
            len: inode_val.len(),
        })?;
    let mut cur = 0usize; // byte offset into data area
    let mut out = Vec::new();
    for i in 0..num {
        let ent = arr_off + i * 4;
        let x_type = *blob.get(ent).ok_or(ParseError::Short {
            at: INODE_XFIELDS_OFFSET + ent,
            need: 1,
            len: inode_val.len(),
        })?;
        let x_flags = *blob.get(ent + 1).ok_or(ParseError::Short {
            at: INODE_XFIELDS_OFFSET + ent + 1,
            need: 1,
            len: inode_val.len(),
        })?;
        let x_size = u16_le(blob, ent + 2)? as usize;
        if x_type != INO_EXT_TYPE_NAME && x_type != INO_EXT_TYPE_DSTREAM {
            let start = data0.checked_add(cur).ok_or(ParseError::Short {
                at: INODE_XFIELDS_OFFSET + data0,
                need: cur,
                len: inode_val.len(),
            })?;
            let abs_start = INODE_XFIELDS_OFFSET + start;
            let data = inode_val
                .get(abs_start..abs_start + x_size)
                .ok_or(ParseError::Short {
                    at: abs_start,
                    need: x_size,
                    len: inode_val.len(),
                })?
                .to_vec();
            out.push(UnknownXfield {
                x_type,
                x_flags,
                data,
            });
        }
        cur = cur
            .checked_add(round_up8(x_size))
            .ok_or(ParseError::Short {
                at: INODE_XFIELDS_OFFSET + data0 + cur,
                need: x_size,
                len: inode_val.len(),
            })?;
    }
    Ok(out)
}

/// j_dstream_t - a file's data stream record (all u64, 40 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JDstream {
    pub size: u64,
    pub alloced_size: u64,
    pub default_crypto_id: u64,
    pub total_bytes_written: u64,
    pub total_bytes_read: u64,
}

impl JDstream {
    pub fn parse(b: &[u8]) -> Result<Self, ParseError> {
        Ok(Self {
            size: u64_le(b, 0)?,
            alloced_size: u64_le(b, 8)?,
            default_crypto_id: u64_le(b, 16)?,
            total_bytes_written: u64_le(b, 24)?,
            total_bytes_read: u64_le(b, 32)?,
        })
    }
}

/// Parsed inode value: the fields M3b needs plus the data stream (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inode {
    pub parent_id: u64,
    pub private_id: u64,
    pub internal_flags: u64,
    /// mode_t (u16) @ j_inode_val_t byte offset 80 (the APFS specification).
    pub mode: u16,
    /// BSD file flags (u32) @ offset 68. Carries UF_HIDDEN (0x8000),
    /// UF_IMMUTABLE (0x2), UF_APPEND (0x4), SF_RESTRICTED (0x80000), etc.
    /// Round-trips Windows FILE_ATTRIBUTE_HIDDEN ↔ UF_HIDDEN.
    pub bsd_flags: u32,
    /// File creation time (btime) - APFS ns since UNIX epoch, offset 16.
    pub create_time: u64,
    /// File modification time (mtime) - APFS ns since UNIX epoch, offset 24.
    pub mod_time: u64,
    /// Inode change time (ctime) - APFS ns since UNIX epoch, offset 32.
    pub change_time: u64,
    /// Last access time (atime) - APFS ns since UNIX epoch, offset 40.
    pub access_time: u64,
    pub dstream: Option<JDstream>,
}

#[inline]
fn round_up8(n: usize) -> usize {
    (n + 7) & !7
}

impl Inode {
    /// Parse a j_inode_val_t value buffer. The xfields blob (if present)
    /// begins at INODE_XFIELDS_OFFSET (92): xf_blob_t { u16 xf_num_exts;
    /// u16 xf_used_data; } then xf_num_exts x_field_t {u8 x_type; u8 x_flags;
    /// u16 x_size} entries, then the data area; each field's data occupies
    /// round_up(x_size, 8) bytes in x_field order.
    pub fn parse(val: &[u8]) -> Result<Self, ParseError> {
        let parent_id = u64_le(val, 0)?;
        let private_id = u64_le(val, 8)?;
        let create_time = u64_le(val, 16)?;
        let mod_time = u64_le(val, 24)?;
        let change_time = u64_le(val, 32)?;
        let access_time = u64_le(val, 40)?;
        let internal_flags = u64_le(val, 48)?;
        let bsd_flags = u32_le(val, 68)?;
        let mode = u16_le(val, 80)?;
        let mut dstream = None;
        if val.len() > INODE_XFIELDS_OFFSET {
            let num = u16_le(val, INODE_XFIELDS_OFFSET)? as usize;
            // xf_used_data is val[94..96]; not needed for the walk.
            let arr = INODE_XFIELDS_OFFSET + 4; // start of x_field_t array
            let data0 = arr
                .checked_add(num.checked_mul(4).ok_or(ParseError::Short {
                    at: arr,
                    need: 0,
                    len: val.len(),
                })?)
                .ok_or(ParseError::Short {
                    at: arr,
                    need: 0,
                    len: val.len(),
                })?;
            let mut cur = 0usize;
            for i in 0..num {
                let ent = arr + i * 4;
                let x_type = *val.get(ent).ok_or(ParseError::Short {
                    at: ent,
                    need: 1,
                    len: val.len(),
                })?;
                let x_size = u16_le(val, ent + 2)? as usize;
                if x_type == INO_EXT_TYPE_DSTREAM {
                    let start = data0.checked_add(cur).ok_or(ParseError::Short {
                        at: data0,
                        need: cur,
                        len: val.len(),
                    })?;
                    let field = val.get(start..start + x_size).ok_or(ParseError::Short {
                        at: start,
                        need: x_size,
                        len: val.len(),
                    })?;
                    dstream = Some(JDstream::parse(field)?);
                    break;
                }
                cur = cur
                    .checked_add(round_up8(x_size))
                    .ok_or(ParseError::Short {
                        at: cur,
                        need: x_size,
                        len: val.len(),
                    })?;
            }
        }
        Ok(Self {
            parent_id,
            private_id,
            internal_flags,
            bsd_flags,
            create_time,
            mod_time,
            change_time,
            access_time,
            mode,
            dstream,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn base_prefix(private_id: u64) -> Vec<u8> {
        let mut v = vec![0u8; INODE_XFIELDS_OFFSET];
        v[0..8].copy_from_slice(&7u64.to_le_bytes()); // parent_id = 7
        v[8..16].copy_from_slice(&private_id.to_le_bytes());
        v[48..56].copy_from_slice(&0u64.to_le_bytes()); // internal_flags
        v
    }

    #[test]
    fn no_xfields_means_no_dstream() {
        let v = base_prefix(0x1234);
        let ino = Inode::parse(&v).unwrap();
        assert_eq!(ino.parent_id, 7);
        assert_eq!(ino.private_id, 0x1234);
        assert!(ino.dstream.is_none());
    }

    #[test]
    fn single_dstream_xfield() {
        let mut v = base_prefix(0xABCD);
        // xf_blob: num=1, used=4+round_up(40,8)=44
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&44u16.to_le_bytes());
        // x_field[0] = { x_type=8 (DSTREAM), x_flags=0, x_size=40 }
        v.push(INO_EXT_TYPE_DSTREAM);
        v.push(0);
        v.extend_from_slice(&40u16.to_le_bytes());
        // data area (starts at 96 + 1*4 = offset 100): j_dstream_t, size=4096
        let mut ds = vec![0u8; 40];
        ds[0..8].copy_from_slice(&4096u64.to_le_bytes());
        v.extend_from_slice(&ds);
        let ino = Inode::parse(&v).unwrap();
        assert_eq!(ino.private_id, 0xABCD);
        let d = ino.dstream.expect("dstream present");
        assert_eq!(d.size, 4096);
    }

    #[test]
    fn dstream_after_unaligned_first_field_uses_round_up_8() {
        // Two xfields: [0]=NAME type 4 x_size=3 (slot round_up(3,8)=8),
        // [1]=DSTREAM type 8 x_size=40. DSTREAM data must be read at
        // data0 + 8 (NOT data0 + 3) - proves the round_up(x_size,8) rule.
        let mut v = base_prefix(0x55);
        v.extend_from_slice(&2u16.to_le_bytes()); // xf_num_exts = 2
        v.extend_from_slice(&(8u16 + 4 + 40).to_le_bytes()); // xf_used_data (cosmetic)
        v.push(4); // x_field[0].x_type = INO_EXT_TYPE_NAME
        v.push(0);
        v.extend_from_slice(&3u16.to_le_bytes()); // x_size = 3
        v.push(INO_EXT_TYPE_DSTREAM); // x_field[1].x_type = 8
        v.push(0);
        v.extend_from_slice(&40u16.to_le_bytes()); // x_size = 40
                                                   // data area starts at 96 + 2*4 = offset 104.
        v.extend_from_slice(&[b'a', b'b', 0]); // field0 raw value (3 bytes)
        v.extend_from_slice(&[0u8; 5]); // padding so field0 slot = round_up(3,8)=8
        let mut ds = vec![0u8; 40];
        ds[0..8].copy_from_slice(&4096u64.to_le_bytes());
        v.extend_from_slice(&ds); // field1 (DSTREAM) at data0 + 8
        let ino = Inode::parse(&v).unwrap();
        assert_eq!(ino.dstream.expect("dstream").size, 4096);
    }

    #[test]
    fn parses_mode_field_at_offset_80() {
        let mut v = base_prefix(0x1234);
        // mode_t mode is u16 @ offset 80 (the APFS specification). 0o040755 = dir.
        v[80..82].copy_from_slice(&0o040755u16.to_le_bytes());
        let ino = Inode::parse(&v).unwrap();
        assert_eq!(ino.mode, 0o040755);
        assert_eq!(ino.mode & 0o170000, 0o040000); // S_IFDIR
    }

    #[test]
    fn short_buffer_errors_not_panics() {
        assert!(Inode::parse(&[0u8; 16]).is_err());
        assert!(JDstream::parse(&[0u8; 8]).is_err());
    }

    #[test]
    fn parse_unknown_xfields_empty_when_no_xfields() {
        let v = base_prefix(0x1);
        let unk = parse_unknown_xfields(&v).unwrap();
        assert!(unk.is_empty());
    }

    #[test]
    fn parse_unknown_xfields_skips_recognized_name_and_dstream() {
        // Two xfields: NAME (4) and DSTREAM (8) - both recognized; result empty.
        let mut v = base_prefix(0xABCD);
        // xf_blob: num=2, used = round_up(3,8) + round_up(40,8) = 8 + 40 = 48
        v.extend_from_slice(&2u16.to_le_bytes()); // xf_num_exts
        v.extend_from_slice(&48u16.to_le_bytes()); // xf_used_data
                                                   // x_field[0] = NAME, x_size=3
        v.push(INO_EXT_TYPE_NAME);
        v.push(0);
        v.extend_from_slice(&3u16.to_le_bytes());
        // x_field[1] = DSTREAM, x_size=40
        v.push(INO_EXT_TYPE_DSTREAM);
        v.push(0);
        v.extend_from_slice(&40u16.to_le_bytes());
        // data area: NAME slot (8 bytes padded), then DSTREAM (40 bytes)
        v.extend_from_slice(&[b'a', b'b', 0, 0, 0, 0, 0, 0]); // NAME slot, round_up(3,8)=8
        v.extend_from_slice(&[0u8; 40]); // DSTREAM slot
        let unk = parse_unknown_xfields(&v).unwrap();
        assert!(unk.is_empty(), "expected no unknowns, got: {unk:?}");
    }

    #[test]
    fn parse_unknown_xfields_returns_unknown_types() {
        // Three xfields: NAME(4), UNKNOWN(0x99, data=[1,2,3]), DSTREAM(8).
        // Only 0x99 should appear in the output.
        let mut v = base_prefix(0x55);
        let unk_data: &[u8] = &[1u8, 2, 3];
        let unk_slot = round_up8(unk_data.len()); // 8
        let ds_slot = 40usize;
        let name_bytes: &[u8] = &[b'x', 0];
        let name_slot = round_up8(name_bytes.len()); // 8
        let used = name_slot + unk_slot + ds_slot; // 8+8+40=56
        v.extend_from_slice(&3u16.to_le_bytes()); // xf_num_exts = 3
        v.extend_from_slice(&(used as u16).to_le_bytes()); // xf_used_data
                                                           // descriptors (sorted by type? - ascending order for fsck, but parse
                                                           // must handle any order): emit NAME=4, UNK=0x99, DSTREAM=8.
        v.push(INO_EXT_TYPE_NAME);
        v.push(0);
        v.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        v.push(0x99u8);
        v.push(0x07);
        v.extend_from_slice(&(unk_data.len() as u16).to_le_bytes());
        v.push(INO_EXT_TYPE_DSTREAM);
        v.push(0);
        v.extend_from_slice(&(ds_slot as u16).to_le_bytes());
        // data area: NAME slot, UNK slot, DSTREAM slot
        let mut name_pad = [0u8; 8];
        name_pad[..name_bytes.len()].copy_from_slice(name_bytes);
        v.extend_from_slice(&name_pad);
        let mut unk_pad = [0u8; 8];
        unk_pad[..unk_data.len()].copy_from_slice(unk_data);
        v.extend_from_slice(&unk_pad);
        v.extend_from_slice(&[0u8; 40]); // DSTREAM slot
        let result = parse_unknown_xfields(&v).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].x_type, 0x99);
        assert_eq!(result[0].x_flags, 0x07);
        assert_eq!(result[0].data, &[1u8, 2, 3]);
    }

    #[test]
    fn parse_unknown_xfields_truncated_descriptor_array_errors() {
        // xf_num_exts=5 but the blob only contains room for 1 descriptor.
        // The overflow in arr_off + num*4 must produce Err, not panic.
        let mut v = base_prefix(0xDEAD);
        v.extend_from_slice(&5u16.to_le_bytes()); // xf_num_exts=5 (too large)
        v.extend_from_slice(&0u16.to_le_bytes()); // xf_used_data (irrelevant)
                                                  // Only 4 bytes of descriptor data (1 entry worth), not 20 (5 entries).
        v.push(INO_EXT_TYPE_DSTREAM);
        v.push(0);
        v.extend_from_slice(&40u16.to_le_bytes());
        // No data area at all - truncated.
        let result = parse_unknown_xfields(&v);
        assert!(
            result.is_err(),
            "truncated descriptor array must return Err, not panic"
        );
    }

    #[test]
    fn parse_unknown_xfields_xsize_exceeds_remaining_data_errors() {
        // One xfield with x_size=200 but data area is only 8 bytes.
        // Must return Err (Short), not panic.
        let mut v = base_prefix(0xBEEF);
        v.extend_from_slice(&1u16.to_le_bytes()); // xf_num_exts=1
        v.extend_from_slice(&200u16.to_le_bytes()); // xf_used_data=200 (larger than actual)
        v.push(0x42u8); // x_type (unknown)
        v.push(0u8); // x_flags
        v.extend_from_slice(&200u16.to_le_bytes()); // x_size=200
                                                    // Provide only 8 bytes of data - far too short for 200.
        v.extend_from_slice(&[0u8; 8]);
        let result = parse_unknown_xfields(&v);
        assert!(
            result.is_err(),
            "x_size exceeding available data must return Err"
        );
    }

    #[test]
    fn jdstream_parse_all_fields_round_trip() {
        // Build a 40-byte j_dstream_t and verify all five u64 fields parse correctly.
        let mut ds = [0u8; 40];
        ds[0..8].copy_from_slice(&0x1111_2222_3333_4444u64.to_le_bytes()); // size
        ds[8..16].copy_from_slice(&0xAAAA_BBBB_CCCC_DDDDu64.to_le_bytes()); // alloced_size
        ds[16..24].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes()); // default_crypto_id
        ds[24..32].copy_from_slice(&1_000_000u64.to_le_bytes()); // total_bytes_written
        ds[32..40].copy_from_slice(&500_000u64.to_le_bytes()); // total_bytes_read
        let d = JDstream::parse(&ds).unwrap();
        assert_eq!(d.size, 0x1111_2222_3333_4444);
        assert_eq!(d.alloced_size, 0xAAAA_BBBB_CCCC_DDDD);
        assert_eq!(d.default_crypto_id, 0x0102_0304_0506_0708);
        assert_eq!(d.total_bytes_written, 1_000_000);
        assert_eq!(d.total_bytes_read, 500_000);
    }
}
