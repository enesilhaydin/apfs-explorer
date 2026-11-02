//! com.apple.decmpfs transparent decompression: header, method dispatch and
//! the two resource-fork layouts.
//!
//! Ten methods exist, each pairing an encoding with a place. Odd numbers keep
//! the content inline in the attribute after the 16-byte header; even numbers
//! keep it in the resource fork, cut into 64 KiB chunks. 3/4 zlib, 7/8 LZVN,
//! 9/10 stored as is, 11/12 LZFSE, 13/14 LZBITMAP. Every chunk, inline
//! payloads included, is either encoded or stored raw behind a one-byte
//! marker, and which byte marks a raw chunk depends on the encoding.
//!
//! The layouts and markers here were measured against macOS itself: files
//! stored with each method were handed to macOS to read back before this
//! reader saw them (`scripts/decmpfs-samples.py`), and files macOS wrote on
//! its own were decoded from their attributes. Two details differ from the
//! open-source readers: plain resource-fork chunks (method 10) sit in the
//! flat offset table, not the Resource Manager layout, and macOS insists on
//! the 0xCC marker in front of them.
use crate::endian::{u32_le, u64_le, ParseError};
use crate::lzbitmap::lzbitmap_decode;
use crate::lzvn::{lzvn_decode, MAX_DECODE_RESERVE};
use std::io::Read;

const DECMPFS_MAGIC: u32 = 0x636d_7066; // bytes 'f','p','m','c' as LE u32 ("cmpf")
const HEADER_LEN: usize = 16;
const CHUNK: usize = 65536;

/// Content stored as is, inline, with no marker byte (older writers).
pub const METHOD_INLINE_LEGACY: u32 = 1;
pub const METHOD_ZLIB_ATTR: u32 = 3;
pub const METHOD_ZLIB_RSRC: u32 = 4;
pub const METHOD_LZVN_ATTR: u32 = 7;
pub const METHOD_LZVN_RSRC: u32 = 8;
pub const METHOD_PLAIN_ATTR: u32 = 9;
pub const METHOD_PLAIN_RSRC: u32 = 10;
pub const METHOD_LZFSE_ATTR: u32 = 11;
pub const METHOD_LZFSE_RSRC: u32 = 12;
pub const METHOD_LZBITMAP_ATTR: u32 = 13;
pub const METHOD_LZBITMAP_RSRC: u32 = 14;
/// The content is not on the volume at all; macOS fetches it on first access.
pub const METHOD_DATALESS_ATTR: u32 = 0x8000_0001;
pub const METHOD_DATALESS_RSRC: u32 = 0x8000_0002;

/// First byte of a zlib stream with the default window size.
const ZLIB_FIRST_BYTE: u8 = 0x78;
/// The zlib and LZBITMAP methods mark a raw chunk with a byte whose low
/// nibble is all ones (0xFF in practice).
const RAW_LOW_NIBBLE: u8 = 0x0F;
/// LZVN's end-of-stream opcode, which no real stream starts with.
const RAW_LZVN: u8 = 0x06;
const RAW_LZFSE: u8 = 0xFF;
const LZFSE_FIRST_BYTE: u8 = b'b';
const LZBITMAP_FIRST_BYTE: u8 = b'Z';

/// Resource Manager fork (zlib): the data section starts with a big-endian
/// length, then a little-endian chunk count and (offset, size) pairs whose
/// offsets are relative to the chunk-count field.
const RSRC_LENGTH_FIELD: usize = 4;
const RSRC_TABLE_HEADER: usize = 8;
const RSRC_ENTRY_LEN: usize = 8;

#[derive(Debug, PartialEq, Eq)]
pub enum DecmpfsError {
    Parse(ParseError),
    BadMagic(u32),
    UnsupportedMethod(u32),
    /// The content is not on this volume (a macOS dataless file).
    Dataless(u32),
    /// The inode is flagged compressed but carries no com.apple.decmpfs
    /// attribute.
    MissingAttribute,
    MissingResourceFork,
    /// A chunk, or the inline payload, is not in the form its method requires.
    BadChunk {
        method: u32,
        chunk: usize,
    },
    /// A chunk decoded to a length other than the one the header calls for.
    ChunkLength {
        chunk: usize,
        got: usize,
        want: usize,
    },
    LengthMismatch {
        got: usize,
        want: usize,
    },
}
impl From<ParseError> for DecmpfsError {
    fn from(e: ParseError) -> Self {
        DecmpfsError::Parse(e)
    }
}

fn short(at: usize, need: usize, len: usize) -> ParseError {
    ParseError::Short { at, need, len }
}

fn u32_be(buf: &[u8], at: usize) -> Result<u32, ParseError> {
    let bytes: [u8; 4] = buf
        .get(at..at.saturating_add(4))
        .and_then(|s| s.try_into().ok())
        .ok_or(short(at, 4, buf.len()))?;
    Ok(u32::from_be_bytes(bytes))
}

/// Parsed 16-byte com.apple.decmpfs header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecmpfsHeader {
    pub method: u32,
    pub uncompressed_size: u64,
}

impl DecmpfsHeader {
    pub fn parse(x: &[u8]) -> Result<Self, DecmpfsError> {
        let magic = u32_le(x, 0)?;
        if magic != DECMPFS_MAGIC {
            return Err(DecmpfsError::BadMagic(magic));
        }
        Ok(Self {
            method: u32_le(x, 4)?,
            uncompressed_size: u64_le(x, 8)?,
        })
    }

    /// Whether the content lives in the resource fork rather than inline.
    pub fn uses_resource_fork(&self) -> bool {
        matches!(
            self.method,
            METHOD_ZLIB_RSRC
                | METHOD_LZVN_RSRC
                | METHOD_PLAIN_RSRC
                | METHOD_LZFSE_RSRC
                | METHOD_LZBITMAP_RSRC
        )
    }
}

/// Decompress a decmpfs-compressed file. `decmpfs_xattr` = the full
/// com.apple.decmpfs value (incl. 16-byte header). `resource_fork` = the
/// com.apple.ResourceFork value if present (required for the even methods).
pub fn decompress(
    decmpfs_xattr: &[u8],
    resource_fork: Option<&[u8]>,
) -> Result<Vec<u8>, DecmpfsError> {
    let h = DecmpfsHeader::parse(decmpfs_xattr)?;
    let total =
        usize::try_from(h.uncompressed_size).map_err(|_| short(8, 8, decmpfs_xattr.len()))?;
    let payload =
        decmpfs_xattr
            .get(HEADER_LEN..)
            .ok_or(short(HEADER_LEN, 0, decmpfs_xattr.len()))?;
    let out = match h.method {
        METHOD_INLINE_LEGACY => payload
            .get(..total)
            .ok_or(short(0, total, payload.len()))?
            .to_vec(),
        METHOD_ZLIB_ATTR | METHOD_LZVN_ATTR | METHOD_PLAIN_ATTR | METHOD_LZFSE_ATTR
        | METHOD_LZBITMAP_ATTR => {
            let mut out = Vec::with_capacity(total.min(MAX_DECODE_RESERVE));
            decode_chunk(h.method, 0, payload, total, &mut out)?;
            out
        }
        METHOD_ZLIB_RSRC | METHOD_LZVN_RSRC | METHOD_PLAIN_RSRC | METHOD_LZFSE_RSRC
        | METHOD_LZBITMAP_RSRC => {
            let rf = resource_fork.ok_or(DecmpfsError::MissingResourceFork)?;
            let mut source: &[u8] = rf;
            let chunks = total.div_ceil(CHUNK);
            let mut out = Vec::with_capacity(total.min(MAX_DECODE_RESERVE));
            if chunks > 0 {
                decode_chunks(
                    h.method,
                    rf.len(),
                    &mut source,
                    total,
                    0,
                    chunks - 1,
                    &mut out,
                )?;
            }
            out
        }
        METHOD_DATALESS_ATTR | METHOD_DATALESS_RSRC => {
            return Err(DecmpfsError::Dataless(h.method))
        }
        m => return Err(DecmpfsError::UnsupportedMethod(m)),
    };
    if out.len() != total {
        return Err(DecmpfsError::LengthMismatch {
            got: out.len(),
            want: total,
        });
    }
    Ok(out)
}

/// Where resource-fork bytes come from. A fork small enough to sit inline in
/// its attribute record is a slice; one stored as a data stream (any fork of
/// consequence) is read from the device a range at a time, so a large
/// compressed file costs only the chunks a read touches.
pub trait ForkBytes {
    type Error: From<DecmpfsError>;
    /// Exactly `len` bytes at `offset`, or an error.
    fn read(&mut self, offset: usize, len: usize) -> Result<Vec<u8>, Self::Error>;
}

impl ForkBytes for &[u8] {
    type Error = DecmpfsError;
    fn read(&mut self, offset: usize, len: usize) -> Result<Vec<u8>, DecmpfsError> {
        self.get(offset..offset.saturating_add(len))
            .map(|s| s.to_vec())
            .ok_or(DecmpfsError::Parse(short(offset, len, self.len())))
    }
}

/// Byte ranges of the chunks inside a resource fork of `fork_len` bytes,
/// validated up front so a crafted table cannot point outside the fork or
/// run backwards. Only the table is read.
fn chunk_ranges<F: ForkBytes>(
    method: u32,
    fork_len: usize,
    fork: &mut F,
    chunks: usize,
) -> Result<Vec<(usize, usize)>, F::Error> {
    let bad = |chunk: usize| F::Error::from(DecmpfsError::BadChunk { method, chunk });
    let mut ranges = Vec::with_capacity(chunks.min(fork_len / 4 + 1));
    if method == METHOD_ZLIB_RSRC {
        let head = fork.read(0, 4)?;
        let data =
            usize::try_from(u32_be(&head, 0).map_err(DecmpfsError::from)?).map_err(|_| bad(0))?;
        let count_field = fork.read(data.saturating_add(RSRC_LENGTH_FIELD), 4)?;
        let count = u32_le(&count_field, 0).map_err(DecmpfsError::from)?;
        if usize::try_from(count).map_or(true, |c| c < chunks) {
            return Err(bad(0));
        }
        let base = data.saturating_add(RSRC_LENGTH_FIELD);
        let entries = fork.read(
            data.saturating_add(RSRC_TABLE_HEADER),
            chunks.saturating_mul(RSRC_ENTRY_LEN),
        )?;
        for i in 0..chunks {
            let entry = i.saturating_mul(RSRC_ENTRY_LEN);
            let off = u32_le(&entries, entry).map_err(DecmpfsError::from)? as usize;
            let size = u32_le(&entries, entry + 4).map_err(DecmpfsError::from)? as usize;
            let start = base.saturating_add(off);
            let end = start.saturating_add(size);
            if end > fork_len {
                return Err(bad(i));
            }
            ranges.push((start, end));
        }
        return Ok(ranges);
    }
    // Flat table: one little-endian offset per chunk plus the end of the last.
    let table_len = 4usize.saturating_mul(chunks.saturating_add(1));
    let table = fork.read(0, table_len)?;
    let mut previous = table_len;
    let mut start = u32_le(&table, 0).map_err(DecmpfsError::from)? as usize;
    if start < table_len {
        return Err(bad(0));
    }
    for i in 0..chunks {
        let end = u32_le(&table, 4 * (i + 1)).map_err(DecmpfsError::from)? as usize;
        if end < start || end > fork_len || start < previous {
            return Err(bad(i));
        }
        ranges.push((start, end));
        previous = end;
        start = end;
    }
    Ok(ranges)
}

/// Decode chunks `first..=last` of a `total`-byte file from its fork,
/// appending them to `out`.
fn decode_chunks<F: ForkBytes>(
    method: u32,
    fork_len: usize,
    fork: &mut F,
    total: usize,
    first: usize,
    last: usize,
    out: &mut Vec<u8>,
) -> Result<(), F::Error> {
    let chunks = total.div_ceil(CHUNK);
    let ranges = chunk_ranges(method, fork_len, fork, chunks)?;
    for i in first..=last {
        let (start, end) = ranges
            .get(i)
            .copied()
            .ok_or(DecmpfsError::BadChunk { method, chunk: i })?;
        let blob = fork.read(start, end.saturating_sub(start))?;
        let want = CHUNK.min(total - i * CHUNK);
        decode_chunk(method, i, &blob, want, out)?;
    }
    Ok(())
}

/// Bytes `offset..offset+len` of a compressed file (clamped to its size),
/// decoding only the chunks that cover them. `fork_len` is the resource
/// fork's length and `fork` reads ranges of it; inline methods have no fork
/// and pass an empty slice.
pub fn decompress_range<F: ForkBytes>(
    decmpfs_xattr: &[u8],
    fork_len: usize,
    fork: &mut F,
    offset: usize,
    len: usize,
) -> Result<Vec<u8>, F::Error> {
    let h = DecmpfsHeader::parse(decmpfs_xattr)?;
    let total = usize::try_from(h.uncompressed_size)
        .map_err(|_| DecmpfsError::Parse(short(8, 8, decmpfs_xattr.len())))?;
    if offset >= total || len == 0 {
        return Ok(Vec::new());
    }
    let end = offset.saturating_add(len).min(total);
    if !h.uses_resource_fork() {
        // Inline content is at most a few KiB: decode it whole and cut.
        let whole = decompress(decmpfs_xattr, None)?;
        return Ok(whole
            .get(offset..end)
            .map(|s| s.to_vec())
            .unwrap_or_default());
    }
    let first = offset / CHUNK;
    let last = (end - 1) / CHUNK;
    let mut out = Vec::with_capacity(((last - first + 1) * CHUNK).min(MAX_DECODE_RESERVE));
    decode_chunks(h.method, fork_len, fork, total, first, last, &mut out)?;
    let skip = offset - first * CHUNK;
    out.get(skip..skip + (end - offset))
        .map(|s| s.to_vec())
        .ok_or_else(|| {
            DecmpfsError::LengthMismatch {
                got: out.len(),
                want: end - offset,
            }
            .into()
        })
}

/// A raw chunk: one marker byte, then exactly `want` bytes of content.
fn raw(blob: &[u8], want: usize, out: &mut Vec<u8>) -> Option<()> {
    let content = blob.get(1..)?;
    if content.len() != want {
        return None;
    }
    out.extend_from_slice(content);
    Some(())
}

fn inflate(blob: &[u8], want: usize, out: &mut Vec<u8>) -> Option<()> {
    // The stream carries the two-byte zlib header; the deflate data follows.
    let deflate = blob.get(2..)?;
    let v = miniz_oxide::inflate::decompress_to_vec_with_limit(deflate, want).ok()?;
    out.extend_from_slice(&v);
    Some(())
}

fn lzfse(blob: &[u8], want: usize, out: &mut Vec<u8>) -> Option<()> {
    let limit = u64::try_from(want).ok()?.checked_add(1)?;
    let mut decoder = lzfse_rust::LzfseRingDecoder::default();
    let mut reader = decoder.reader_bytes(blob).take(limit);
    reader.read_to_end(out).ok()?;
    Some(())
}

/// Decode one chunk (or the inline payload) of `method`, appending exactly
/// `want` bytes to `out`.
fn decode_chunk(
    method: u32,
    index: usize,
    blob: &[u8],
    want: usize,
    out: &mut Vec<u8>,
) -> Result<(), DecmpfsError> {
    let bad = || DecmpfsError::BadChunk {
        method,
        chunk: index,
    };
    let first = blob.first().copied().ok_or_else(bad)?;
    let start = out.len();
    match method {
        METHOD_PLAIN_ATTR | METHOD_PLAIN_RSRC => raw(blob, want, out).ok_or_else(bad)?,
        METHOD_ZLIB_ATTR | METHOD_ZLIB_RSRC => {
            if first == ZLIB_FIRST_BYTE {
                inflate(blob, want, out).ok_or_else(bad)?;
            } else if first & RAW_LOW_NIBBLE == RAW_LOW_NIBBLE {
                raw(blob, want, out).ok_or_else(bad)?;
            } else {
                return Err(bad());
            }
        }
        METHOD_LZVN_ATTR | METHOD_LZVN_RSRC => {
            if first == RAW_LZVN {
                raw(blob, want, out).ok_or_else(bad)?;
            } else {
                out.extend_from_slice(&lzvn_decode(blob, want)?);
            }
        }
        METHOD_LZFSE_ATTR | METHOD_LZFSE_RSRC => {
            if first == LZFSE_FIRST_BYTE {
                lzfse(blob, want, out).ok_or_else(bad)?;
            } else if first == RAW_LZFSE {
                raw(blob, want, out).ok_or_else(bad)?;
            } else {
                return Err(bad());
            }
        }
        METHOD_LZBITMAP_ATTR | METHOD_LZBITMAP_RSRC => {
            if first == LZBITMAP_FIRST_BYTE {
                out.extend_from_slice(&lzbitmap_decode(blob, want)?);
            } else if first & RAW_LOW_NIBBLE == RAW_LOW_NIBBLE {
                raw(blob, want, out).ok_or_else(bad)?;
            } else {
                return Err(bad());
            }
        }
        m => return Err(DecmpfsError::UnsupportedMethod(m)),
    }
    let got = out.len().saturating_sub(start);
    if got != want {
        return Err(DecmpfsError::ChunkLength {
            chunk: index,
            got,
            want,
        });
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Helper: build a valid 16-byte decmpfs header + payload.
    fn make_xattr(method: u32, uncompressed_size: u64, payload: &[u8]) -> Vec<u8> {
        let mut x = vec![0x66, 0x70, 0x6d, 0x63]; // magic "cmpf" LE
        x.extend_from_slice(&method.to_le_bytes());
        x.extend_from_slice(&uncompressed_size.to_le_bytes());
        x.extend_from_slice(payload);
        x
    }

    /// Flat resource fork: offset table, then the chunk blobs.
    fn flat_fork(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut off = 4 * (chunks.len() + 1);
        let mut rf = Vec::new();
        for c in chunks {
            rf.extend_from_slice(&(off as u32).to_le_bytes());
            off += c.len();
        }
        rf.extend_from_slice(&(off as u32).to_le_bytes());
        for c in chunks {
            rf.extend_from_slice(c);
        }
        rf
    }

    /// Resource Manager fork as the zlib method stores it.
    fn resource_fork(chunks: &[Vec<u8>]) -> Vec<u8> {
        let mut body = (chunks.len() as u32).to_le_bytes().to_vec();
        let mut off = 4 + 8 * chunks.len();
        for c in chunks {
            body.extend_from_slice(&(off as u32).to_le_bytes());
            body.extend_from_slice(&(c.len() as u32).to_le_bytes());
            off += c.len();
        }
        for c in chunks {
            body.extend_from_slice(c);
        }
        let mut rf = Vec::new();
        rf.extend_from_slice(&0x100u32.to_be_bytes());
        rf.extend_from_slice(&((0x100 + 4 + body.len()) as u32).to_be_bytes());
        rf.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
        rf.extend_from_slice(&50u32.to_be_bytes());
        rf.resize(0x100, 0);
        rf.extend_from_slice(&(body.len() as u32).to_be_bytes());
        rf.extend_from_slice(&body);
        rf.extend_from_slice(&[0u8; 50]);
        rf
    }

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|i| b"chunky text "[i % 12]).collect()
    }

    #[test]
    fn method1_raw_roundtrip() {
        let x = make_xattr(1, 5, b"hello");
        assert_eq!(decompress(&x, None).unwrap(), b"hello");
    }

    // Malformed-image hardening: a crafted uncompressed_size must not drive an
    // up-front allocation. Method 7 routes the size to lzvn_decode as the output
    // quota; a truncated payload must error cleanly, never reserve ~16 EiB.
    #[test]
    fn method7_huge_uncompressed_size_does_not_oom() {
        let x = make_xattr(7, u64::MAX, &[0x00]);
        assert!(decompress(&x, None).is_err());
    }

    // Method 8 sizes its offset table and output buffer from uncompressed_size;
    // an absurd size with a tiny resource fork must error, not OOM.
    #[test]
    fn method8_huge_uncompressed_size_does_not_oom() {
        let x = make_xattr(8, u64::MAX, &[]);
        let rf = vec![0u8; 8];
        assert!(decompress(&x, Some(&rf)).is_err());
    }

    #[test]
    fn bad_magic_and_unsupported_error_not_panic() {
        assert!(matches!(
            decompress(&[0u8; 16], None),
            Err(DecmpfsError::BadMagic(_))
        ));
        let x = make_xattr(5, 0, &[]);
        assert!(matches!(
            decompress(&x, None),
            Err(DecmpfsError::UnsupportedMethod(5))
        ));
    }

    #[test]
    fn dataless_files_are_reported_as_such() {
        for m in [METHOD_DATALESS_ATTR, METHOD_DATALESS_RSRC] {
            let x = make_xattr(m, 100, &[]);
            assert_eq!(decompress(&x, None), Err(DecmpfsError::Dataless(m)));
        }
    }

    #[test]
    fn method8_missing_rsrc_errors() {
        let x = make_xattr(8, 100, &[]);
        assert!(matches!(
            decompress(&x, None),
            Err(DecmpfsError::MissingResourceFork)
        ));
    }

    #[test]
    fn method9_uncompressed_inline_with_marker_prefix() {
        // fpmc + method=9 + usize=5 ; payload = 0xCC marker + b"hello"
        let mut payload = vec![0xCC];
        payload.extend_from_slice(b"hello");
        let x = make_xattr(9, 5, &payload);
        assert_eq!(decompress(&x, None).unwrap(), b"hello");

        // truncated method-9 payload -> error (no panic)
        let y = make_xattr(9, 10, &[0xCC, b'h', b'i']);
        assert!(decompress(&y, None).is_err());
    }

    // Trailing bytes after the content are as wrong as missing ones: fsck
    // rejects an inline payload whose length does not match the header.
    #[test]
    fn inline_payload_with_slack_errors() {
        let mut payload = vec![0xCC];
        payload.extend_from_slice(b"hello!");
        let x = make_xattr(9, 5, &payload);
        assert!(decompress(&x, None).is_err());
        let mut payload = vec![RAW_LZVN];
        payload.extend_from_slice(b"hello!");
        let x = make_xattr(7, 5, &payload);
        assert!(decompress(&x, None).is_err());
    }

    // Method 7 (LZVN inline): a payload whose first byte is 0x06 is stored raw
    // rather than LZVN-encoded. Both reference readers branch on 0x06 here;
    // 0xFF is the raw marker for the zlib and LZBITMAP methods, not this one.
    #[test]
    fn method7_raw_marker_is_0x06() {
        let data = b"raw inline content, never LZVN encoded";
        let mut payload = vec![0x06];
        payload.extend_from_slice(data);
        let x = make_xattr(7, data.len() as u64, &payload);
        let out = decompress(&x, None).expect("method 7 raw payload");
        assert_eq!(out.as_slice(), data.as_ref(), "0x06 payload is verbatim");
    }

    #[test]
    fn method7_raw_marker_truncated_errors_not_panic() {
        let x = make_xattr(7, 10, &[0x06, b'x', b'y']);
        assert!(decompress(&x, None).is_err());
    }

    #[test]
    fn method7_ff_is_not_a_raw_marker() {
        let data = b"12345678";
        let mut payload = vec![0xFF];
        payload.extend_from_slice(data);
        let x = make_xattr(7, data.len() as u64, &payload);
        if let Ok(out) = decompress(&x, None) {
            assert_ne!(out.as_slice(), data.as_ref());
        }
    }

    #[test]
    fn method8_single_uncompressed_chunk_with_0x06_marker() {
        let data = b"hello";
        let mut chunk = vec![0x06];
        chunk.extend_from_slice(data);
        let rf = flat_fork(&[chunk]);
        let x = make_xattr(8, data.len() as u64, &[]);
        assert_eq!(decompress(&x, Some(&rf)).unwrap(), data);
    }

    #[test]
    fn method1_size_mismatch_errors() {
        let x = make_xattr(1, 5, b"abc");
        assert!(decompress(&x, None).is_err());
    }

    #[test]
    fn header_parse_too_short_errors() {
        assert!(DecmpfsHeader::parse(&[0u8; 8]).is_err());
        let mut s = vec![0x66, 0x70, 0x6d, 0x63];
        s.extend_from_slice(&[0u8; 4]);
        assert!(DecmpfsHeader::parse(&s).is_err());
    }

    // --- zlib ---------------------------------------------------------------

    #[test]
    fn zlib_inline_encoded_and_raw() {
        let data = sample(3000);
        let z = miniz_oxide::deflate::compress_to_vec_zlib(&data, 6);
        assert_eq!(z[0], ZLIB_FIRST_BYTE);
        let x = make_xattr(METHOD_ZLIB_ATTR, data.len() as u64, &z);
        assert_eq!(decompress(&x, None).unwrap(), data);

        let mut payload = vec![0xFF];
        payload.extend_from_slice(&data);
        let x = make_xattr(METHOD_ZLIB_ATTR, data.len() as u64, &payload);
        assert_eq!(decompress(&x, None).unwrap(), data);

        let mut payload = vec![0x00];
        payload.extend_from_slice(&data);
        let x = make_xattr(METHOD_ZLIB_ATTR, data.len() as u64, &payload);
        assert!(matches!(
            decompress(&x, None),
            Err(DecmpfsError::BadChunk {
                method: 3,
                chunk: 0
            })
        ));
    }

    #[test]
    fn zlib_resource_fork_mixes_encoded_and_raw_chunks() {
        let mut data = sample(CHUNK);
        data.extend_from_slice(&[0x5A; 7]);
        let first = miniz_oxide::deflate::compress_to_vec_zlib(&data[..CHUNK], 6);
        let mut second = vec![0xFF];
        second.extend_from_slice(&data[CHUNK..]);
        let rf = resource_fork(&[first, second]);
        let x = make_xattr(METHOD_ZLIB_RSRC, data.len() as u64, &[]);
        assert_eq!(decompress(&x, Some(&rf)).unwrap(), data);
    }

    #[test]
    fn zlib_resource_fork_with_too_few_entries_errors() {
        let data = sample(CHUNK + 1);
        let only = miniz_oxide::deflate::compress_to_vec_zlib(&data[..CHUNK], 6);
        let rf = resource_fork(&[only]);
        let x = make_xattr(METHOD_ZLIB_RSRC, data.len() as u64, &[]);
        assert!(decompress(&x, Some(&rf)).is_err());
    }

    // --- plain resource fork: flat table and the 0xCC marker ----------------

    #[test]
    fn plain_resource_fork_uses_the_flat_table() {
        let mut data = sample(CHUNK);
        data.extend_from_slice(b"tail");
        let mut a = vec![0xCC];
        a.extend_from_slice(&data[..CHUNK]);
        let mut b = vec![0xCC];
        b.extend_from_slice(&data[CHUNK..]);
        let rf = flat_fork(&[a, b]);
        let x = make_xattr(METHOD_PLAIN_RSRC, data.len() as u64, &[]);
        assert_eq!(decompress(&x, Some(&rf)).unwrap(), data);
    }

    // --- LZFSE --------------------------------------------------------------

    #[test]
    fn lzfse_inline_encoded_and_raw() {
        let data = sample(3000);
        let mut z = Vec::new();
        lzfse_rust::encode_bytes(&data, &mut z).unwrap();
        assert_eq!(z[0], LZFSE_FIRST_BYTE);
        let x = make_xattr(METHOD_LZFSE_ATTR, data.len() as u64, &z);
        assert_eq!(decompress(&x, None).unwrap(), data);

        let mut payload = vec![RAW_LZFSE];
        payload.extend_from_slice(&data);
        let x = make_xattr(METHOD_LZFSE_ATTR, data.len() as u64, &payload);
        assert_eq!(decompress(&x, None).unwrap(), data);

        // A stream longer than the header says is refused, not truncated.
        let x = make_xattr(METHOD_LZFSE_ATTR, data.len() as u64 - 1, &z);
        assert!(decompress(&x, None).is_err());
    }

    #[test]
    fn lzfse_resource_fork_chunks() {
        let mut data = sample(CHUNK);
        data.extend_from_slice(&sample(10));
        let mut a = Vec::new();
        lzfse_rust::encode_bytes(&data[..CHUNK], &mut a).unwrap();
        let mut b = Vec::new();
        lzfse_rust::encode_bytes(&data[CHUNK..], &mut b).unwrap();
        let rf = flat_fork(&[a, b]);
        let x = make_xattr(METHOD_LZFSE_RSRC, data.len() as u64, &[]);
        assert_eq!(decompress(&x, Some(&rf)).unwrap(), data);
    }

    // --- LZBITMAP -----------------------------------------------------------

    #[test]
    fn lzbitmap_inline_stored_chunk_and_raw() {
        let data = b"stored inside a ZBM stream".to_vec();
        let len = data.len() + 6;
        let mut z = b"ZBM\x09".to_vec();
        z.extend_from_slice(&[len as u8, 0, 0, data.len() as u8, 0, 0]);
        z.extend_from_slice(&data);
        z.extend_from_slice(&[6, 0, 0, 0, 0, 0]);
        let x = make_xattr(METHOD_LZBITMAP_ATTR, data.len() as u64, &z);
        assert_eq!(decompress(&x, None).unwrap(), data);

        let mut payload = vec![0xFF];
        payload.extend_from_slice(&data);
        let x = make_xattr(METHOD_LZBITMAP_ATTR, data.len() as u64, &payload);
        assert_eq!(decompress(&x, None).unwrap(), data);
    }

    // --- ranged reads --------------------------------------------------------

    /// A fork source that records every range it is asked for.
    struct Counting<'a> {
        bytes: &'a [u8],
        reads: Vec<(usize, usize)>,
    }
    impl ForkBytes for Counting<'_> {
        type Error = DecmpfsError;
        fn read(&mut self, offset: usize, len: usize) -> Result<Vec<u8>, DecmpfsError> {
            self.reads.push((offset, len));
            let mut s = self.bytes;
            ForkBytes::read(&mut s, offset, len)
        }
    }

    #[test]
    fn decompress_range_decodes_only_the_chunks_it_needs() {
        let data = sample(2 * CHUNK + 100);
        let mut chunks = Vec::new();
        for c in data.chunks(CHUNK) {
            let mut z = Vec::new();
            lzfse_rust::encode_bytes(c, &mut z).unwrap();
            chunks.push(z);
        }
        let rf = flat_fork(&chunks);
        let x = make_xattr(METHOD_LZFSE_RSRC, data.len() as u64, &[]);
        let table_len = 4 * (chunks.len() + 1);
        let chunk1_start = table_len + chunks[0].len();

        // Inside one chunk: the table and that chunk only.
        let mut src = Counting {
            bytes: &rf,
            reads: Vec::new(),
        };
        let got = decompress_range(&x, rf.len(), &mut src, CHUNK + 10, 50).unwrap();
        assert_eq!(got, &data[CHUNK + 10..CHUNK + 60]);
        assert_eq!(
            src.reads,
            vec![(0, table_len), (chunk1_start, chunks[1].len())]
        );

        // Across a chunk boundary.
        let mut src: &[u8] = &rf;
        let got = decompress_range(&x, rf.len(), &mut src, CHUNK - 5, 10).unwrap();
        assert_eq!(got, &data[CHUNK - 5..CHUNK + 5]);

        // Past the end is clamped; at or beyond the size is empty.
        let mut src: &[u8] = &rf;
        let got = decompress_range(&x, rf.len(), &mut src, 2 * CHUNK, 1000).unwrap();
        assert_eq!(got, &data[2 * CHUNK..]);
        let mut src: &[u8] = &rf;
        assert!(decompress_range(&x, rf.len(), &mut src, data.len(), 5)
            .unwrap()
            .is_empty());
        let mut src: &[u8] = &rf;
        assert!(decompress_range(&x, rf.len(), &mut src, 7, 0)
            .unwrap()
            .is_empty());

        // The whole file, in one go.
        let mut src: &[u8] = &rf;
        assert_eq!(
            decompress_range(&x, rf.len(), &mut src, 0, usize::MAX).unwrap(),
            data
        );
    }

    #[test]
    fn decompress_range_on_inline_and_resource_manager_forks() {
        let data = sample(3000);
        let mut z = Vec::new();
        lzfse_rust::encode_bytes(&data, &mut z).unwrap();
        let x = make_xattr(METHOD_LZFSE_ATTR, data.len() as u64, &z);
        let mut none: &[u8] = &[];
        assert_eq!(
            decompress_range(&x, 0, &mut none, 100, 20).unwrap(),
            &data[100..120]
        );

        let mut data = sample(CHUNK);
        data.extend_from_slice(&[0x5A; 7]);
        let first = miniz_oxide::deflate::compress_to_vec_zlib(&data[..CHUNK], 6);
        let mut second = vec![0xFF];
        second.extend_from_slice(&data[CHUNK..]);
        let rf = resource_fork(&[first, second]);
        let x = make_xattr(METHOD_ZLIB_RSRC, data.len() as u64, &[]);
        let mut src: &[u8] = &rf;
        assert_eq!(
            decompress_range(&x, rf.len(), &mut src, CHUNK - 2, 9).unwrap(),
            &data[CHUNK - 2..]
        );
    }

    // --- flat table validation ----------------------------------------------

    #[test]
    fn flat_table_running_backwards_or_past_the_fork_errors() {
        let mut chunk = vec![0x06];
        chunk.extend_from_slice(b"hello");
        let mut rf = flat_fork(&[chunk]);
        let x = make_xattr(8, 5, &[]);
        assert!(decompress(&x, Some(&rf)).is_ok());
        // terminal offset past the end of the fork
        let n = rf.len();
        rf[4..8].copy_from_slice(&((n + 1) as u32).to_le_bytes());
        assert!(matches!(
            decompress(&x, Some(&rf)),
            Err(DecmpfsError::BadChunk {
                method: 8,
                chunk: 0
            })
        ));
        // first chunk starting inside the table
        rf[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        rf[0..4].copy_from_slice(&4u32.to_le_bytes());
        assert!(decompress(&x, Some(&rf)).is_err());
    }
}
