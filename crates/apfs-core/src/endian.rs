//! Bounds-checked little-endian readers over byte slices. No panics.
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("buffer too short: need {need} bytes at offset {at}, have {len}")]
    Short { at: usize, need: usize, len: usize },
    #[error("bad magic: expected {expected:#010x}, found {found:#010x}")]
    BadMagic { expected: u32, found: u32 },
    #[error("checksum mismatch: stored {stored:#018x}, computed {computed:#018x}")]
    BadChecksum { stored: u64, computed: u64 },
}

fn slice(buf: &[u8], at: usize, need: usize) -> Result<&[u8], ParseError> {
    buf.get(at..at + need).ok_or(ParseError::Short {
        at,
        need,
        len: buf.len(),
    })
}

pub fn u16_le(buf: &[u8], at: usize) -> Result<u16, ParseError> {
    let s = slice(buf, at, 2)?;
    let mut a = [0u8; 2];
    a.copy_from_slice(s);
    Ok(u16::from_le_bytes(a))
}

pub fn u32_le(buf: &[u8], at: usize) -> Result<u32, ParseError> {
    let s = slice(buf, at, 4)?;
    let mut a = [0u8; 4];
    a.copy_from_slice(s);
    Ok(u32::from_le_bytes(a))
}

pub fn u64_le(buf: &[u8], at: usize) -> Result<u64, ParseError> {
    let s = slice(buf, at, 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(u64::from_le_bytes(a))
}

pub fn i64_le(buf: &[u8], at: usize) -> Result<i64, ParseError> {
    Ok(u64_le(buf, at)? as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_le_values() {
        let b = [0x4e, 0x58, 0x53, 0x42, 1, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(u32_le(&b, 0).unwrap(), 0x4253_584e);
        assert_eq!(u64_le(&b, 4).unwrap(), 1);
        assert_eq!(i64_le(&b, 4).unwrap(), 1);
    }

    #[test]
    fn short_buffer_errors_not_panics() {
        let b = [0u8; 3];
        assert_eq!(
            u32_le(&b, 0),
            Err(ParseError::Short {
                at: 0,
                need: 4,
                len: 3
            })
        );
        assert!(u64_le(&b, 0).is_err());
    }
}
