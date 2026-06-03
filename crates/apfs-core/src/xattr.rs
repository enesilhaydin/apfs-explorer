//! j_xattr records - extended-attribute key/value parsing. Spec:
//! ctx_search(source:"the APFS specification"). For XATTR_DATA_STREAM the
//! xdata holds the FULL 48-byte j_xattr_dstream_t inline (linux-apfs /
//! apfs-fuse cross-confirmed; Apple p82 prose is misleading).
use crate::endian::{u16_le, u64_le, ParseError};

pub const XATTR_DATA_STREAM: u16 = 0x0001;
pub const XATTR_DATA_EMBEDDED: u16 = 0x0002;
pub const XATTR_FILE_SYSTEM_OWNED: u16 = 0x0004;
/// Largest size, in bytes, of an embedded extended-attribute value.
pub const XATTR_MAX_EMBEDDED_SIZE: usize = 3804;

/// One extended attribute of a file: its name and j_xattr_flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XattrEntry {
    pub name: String,
    pub flags: u16,
}

/// The inline j_xattr_dstream_t (48 bytes) carried in a XATTR_DATA_STREAM
/// record's xdata: { u64 xattr_obj_id; j_dstream_t dstream; }. Only the
/// fields M3c needs are surfaced. `xattr_obj_id` keys the FILE_EXTENT
/// records; `size` is the authoritative attribute byte length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XattrDstream {
    pub xattr_obj_id: u64,
    pub size: u64,
}

impl XattrDstream {
    /// Parse from xdata: xattr_obj_id u64@0; j_dstream_t@8 whose first
    /// field `size` is u64@8.
    pub fn parse(xdata: &[u8]) -> Result<Self, ParseError> {
        Ok(Self {
            xattr_obj_id: u64_le(xdata, 0)?,
            size: u64_le(xdata, 8)?,
        })
    }
}

/// Parse the attribute name from a j_xattr_key. After the 8-byte j_key_t:
/// u16 name_len @8 (includes the trailing NUL), then name bytes @10.
pub fn xattr_key_name(key: &[u8]) -> Result<String, ParseError> {
    let name_len = u16_le(key, 8)? as usize;
    let raw = key.get(10..10 + name_len).ok_or(ParseError::Short {
        at: 10,
        need: name_len,
        len: key.len(),
    })?;
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Ok(String::from_utf8_lossy(raw.get(..end).unwrap_or(&[])).into_owned())
}

/// Parse a j_xattr_val: returns (flags, xdata). flags u16@0, xdata_len
/// u16@2, xdata @4 (xdata_len bytes).
pub fn xattr_val(val: &[u8]) -> Result<(u16, &[u8]), ParseError> {
    let flags = u16_le(val, 0)?;
    let xdata_len = u16_le(val, 2)? as usize;
    let xdata = val.get(4..4 + xdata_len).ok_or(ParseError::Short {
        at: 4,
        need: xdata_len,
        len: val.len(),
    })?;
    Ok((flags, xdata))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn parses_key_name_strips_trailing_nul() {
        // j_key_t (8 bytes) + name_len(u16) + "user.x\0"
        let mut k = vec![0u8; 8];
        let name = b"user.x\0";
        k.extend_from_slice(&(name.len() as u16).to_le_bytes());
        k.extend_from_slice(name);
        assert_eq!(xattr_key_name(&k).unwrap(), "user.x");
    }

    #[test]
    fn parses_embedded_val() {
        // flags=XATTR_DATA_EMBEDDED, xdata_len=5, xdata="hello"
        let mut v = Vec::new();
        v.extend_from_slice(&XATTR_DATA_EMBEDDED.to_le_bytes());
        v.extend_from_slice(&5u16.to_le_bytes());
        v.extend_from_slice(b"hello");
        let (flags, xdata) = xattr_val(&v).unwrap();
        assert_eq!(flags, XATTR_DATA_EMBEDDED);
        assert_eq!(xdata, b"hello");
    }

    #[test]
    fn parses_inline_xattr_dstream_48_bytes() {
        // xdata of a STREAM record: xattr_obj_id=99 @0, j_dstream_t @8
        // with size=5000 @8. Total 48 bytes.
        let mut xd = vec![0u8; 48];
        xd[0..8].copy_from_slice(&99u64.to_le_bytes());
        xd[8..16].copy_from_slice(&5000u64.to_le_bytes());
        let ds = XattrDstream::parse(&xd).unwrap();
        assert_eq!(ds.xattr_obj_id, 99);
        assert_eq!(ds.size, 5000);
    }

    #[test]
    fn short_buffers_error_not_panic() {
        assert!(xattr_key_name(&[0u8; 4]).is_err());
        assert!(xattr_val(&[0u8; 2]).is_err());
        assert!(XattrDstream::parse(&[0u8; 8]).is_err());
    }
}
