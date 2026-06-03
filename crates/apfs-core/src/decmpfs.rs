//! com.apple.decmpfs transparent decompression. Header + method dispatch.
//! Spec: ctx_search(source:"the APFS specification"). Method-8 resource-fork
//! offset-array layout was reverse-engineered from real macOS (libfsapfs
//! leaves it "TODO"); the decisive arbiter is the real-image test.
use crate::endian::{u32_le, u64_le, ParseError};
use crate::lzvn::lzvn_decode;

const DECMPFS_MAGIC: u32 = 0x636d_7066; // bytes 'f','p','m','c' as LE u32 ("cmpf")
const CHUNK: usize = 65536;

#[derive(Debug, PartialEq, Eq)]
pub enum DecmpfsError {
    Parse(ParseError),
    BadMagic(u32),
    UnsupportedMethod(u32),
    MissingResourceFork,
    LengthMismatch { got: usize, want: usize },
}
impl From<ParseError> for DecmpfsError {
    fn from(e: ParseError) -> Self {
        DecmpfsError::Parse(e)
    }
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
}

/// Decompress a decmpfs-compressed file. `decmpfs_xattr` = the full
/// com.apple.decmpfs value (incl. 16-byte header). `resource_fork` = the
/// com.apple.ResourceFork value if present (required for method 8).
pub fn decompress(
    decmpfs_xattr: &[u8],
    resource_fork: Option<&[u8]>,
) -> Result<Vec<u8>, DecmpfsError> {
    let h = DecmpfsHeader::parse(decmpfs_xattr)?;
    let usize_total = h.uncompressed_size as usize;
    let payload = decmpfs_xattr.get(16..).ok_or(ParseError::Short {
        at: 16,
        need: 0,
        len: decmpfs_xattr.len(),
    })?;
    let out = match h.method {
        1 => payload
            .get(..usize_total)
            .ok_or(ParseError::Short {
                at: 0,
                need: usize_total,
                len: payload.len(),
            })?
            .to_vec(),
        7 => {
            if payload.first() == Some(&0xFF) {
                payload
                    .get(1..1 + usize_total)
                    .ok_or(ParseError::Short {
                        at: 1,
                        need: usize_total,
                        len: payload.len(),
                    })?
                    .to_vec()
            } else {
                lzvn_decode(payload, usize_total)?
            }
        }
        9 => {
            // Uncompressed, inline in the decmpfs xattr, with a 1-byte marker
            // prefix (real-macOS reverse-engineered; libfsapfs "Unknown").
            // content = payload[1 .. 1+usize]. Spec:
            // ctx_search(source:"the APFS specification") METHOD 9.
            payload
                .get(1..1 + usize_total)
                .ok_or(ParseError::Short {
                    at: 1,
                    need: usize_total,
                    len: payload.len(),
                })?
                .to_vec()
        }
        8 => {
            let rf = resource_fork.ok_or(DecmpfsError::MissingResourceFork)?;
            let n = usize_total.div_ceil(CHUNK);
            let mut offs = Vec::with_capacity(n + 1);
            for i in 0..=n {
                offs.push(u32_le(rf, 4 * i)? as usize);
            }
            let mut out = Vec::with_capacity(usize_total);
            for i in 0..n {
                let a = offs.get(i).copied().ok_or(ParseError::Short {
                    at: i,
                    need: 1,
                    len: offs.len(),
                })?;
                let b = offs.get(i + 1).copied().ok_or(ParseError::Short {
                    at: i + 1,
                    need: 1,
                    len: offs.len(),
                })?;
                let blk = rf.get(a..b).ok_or(ParseError::Short {
                    at: a,
                    need: b.saturating_sub(a),
                    len: rf.len(),
                })?;
                let want = core::cmp::min(CHUNK, usize_total - i * CHUNK);
                if blk.first() == Some(&0x06) && blk.len() == want + 1 {
                    out.extend_from_slice(blk.get(1..).unwrap_or(&[]));
                } else {
                    out.extend_from_slice(&lzvn_decode(blk, want)?);
                }
            }
            out
        }
        m => return Err(DecmpfsError::UnsupportedMethod(m)),
    };
    if out.len() != usize_total {
        return Err(DecmpfsError::LengthMismatch {
            got: out.len(),
            want: usize_total,
        });
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn method1_raw_roundtrip() {
        let mut x = vec![0x66, 0x70, 0x6d, 0x63];
        x.extend_from_slice(&1u32.to_le_bytes());
        x.extend_from_slice(&5u64.to_le_bytes());
        x.extend_from_slice(b"hello");
        assert_eq!(decompress(&x, None).unwrap(), b"hello");
    }

    #[test]
    fn bad_magic_and_unsupported_error_not_panic() {
        assert!(matches!(
            decompress(&[0u8; 16], None),
            Err(DecmpfsError::BadMagic(_))
        ));
        let mut x = vec![0x66, 0x70, 0x6d, 0x63];
        x.extend_from_slice(&11u32.to_le_bytes());
        x.extend_from_slice(&0u64.to_le_bytes());
        assert!(matches!(
            decompress(&x, None),
            Err(DecmpfsError::UnsupportedMethod(11))
        ));
    }

    #[test]
    fn method8_missing_rsrc_errors() {
        let mut x = vec![0x66, 0x70, 0x6d, 0x63];
        x.extend_from_slice(&8u32.to_le_bytes());
        x.extend_from_slice(&100u64.to_le_bytes());
        assert!(matches!(
            decompress(&x, None),
            Err(DecmpfsError::MissingResourceFork)
        ));
    }

    #[test]
    fn method9_uncompressed_inline_with_marker_prefix() {
        // fpmc + method=9 + usize=5 ; payload = 0xCC marker + b"hello"
        let mut x = vec![0x66, 0x70, 0x6d, 0x63];
        x.extend_from_slice(&9u32.to_le_bytes());
        x.extend_from_slice(&5u64.to_le_bytes());
        x.push(0xCC);
        x.extend_from_slice(b"hello");
        assert_eq!(decompress(&x, None).unwrap(), b"hello");

        // truncated method-9 payload -> error (no panic)
        let mut y = vec![0x66, 0x70, 0x6d, 0x63];
        y.extend_from_slice(&9u32.to_le_bytes());
        y.extend_from_slice(&10u64.to_le_bytes());
        y.push(0xCC);
        y.extend_from_slice(b"hi");
        assert!(decompress(&y, None).is_err());
    }

    // an audit pass - method 7 and method 8 branches

    /// Helper: build a valid 16-byte decmpfs header + payload.
    fn make_xattr(method: u32, uncompressed_size: u64, payload: &[u8]) -> Vec<u8> {
        let mut x = vec![0x66, 0x70, 0x6d, 0x63]; // magic "cmpf" LE
        x.extend_from_slice(&method.to_le_bytes());
        x.extend_from_slice(&uncompressed_size.to_le_bytes());
        x.extend_from_slice(payload);
        x
    }

    #[test]
    fn method7_ff_prefix_means_uncompressed_inline() {
        // Method 7: if payload[0] == 0xFF the remaining bytes are raw uncompressed.
        // payload = 0xFF + data (no LZVN encoding).
        let data = b"Wave9test";
        let mut payload = vec![0xFF];
        payload.extend_from_slice(data);
        let x = make_xattr(7, data.len() as u64, &payload);
        let out = decompress(&x, None).expect("method7 0xFF uncompressed");
        assert_eq!(out.as_slice(), data.as_ref());
    }

    #[test]
    fn method7_ff_prefix_truncated_errors_not_panic() {
        // 0xFF prefix but uncompressed_size claims more bytes than are present.
        let payload = vec![0xFF, b'x', b'y']; // only 2 bytes after prefix
        let x = make_xattr(7, 10, &payload); // claims 10 bytes
        assert!(
            decompress(&x, None).is_err(),
            "truncated method-7 0xFF must error"
        );
    }

    #[test]
    fn method8_single_uncompressed_chunk_with_0x06_marker() {
        // Method 8, resource fork layout:
        //   rf[0..4*(n+1)] = offset table  (n = ceil(usize/65536) chunks)
        //   rf[offsets[i]..offsets[i+1]]   = chunk blob
        //   chunk blob for uncompressed: 0x06 + raw_bytes (len == want+1)
        //
        // For usize=5 (one chunk, want=5):
        //   n=1, offset table = [8, 14] (2 u32s = 8 bytes)
        //   chunk at [8..14] = [0x06, h, e, l, l, o]
        let data = b"hello";
        let usize_total: u64 = data.len() as u64;
        let n: usize = 1; // ceil(5/65536) = 1
        let offsets: [u32; 2] = [
            ((n + 1) * 4) as u32,                  // 8: start of chunk 0
            ((n + 1) * 4 + 1 + data.len()) as u32, // 14: end of chunk 0
        ];
        let mut rf = Vec::new();
        for o in &offsets {
            rf.extend_from_slice(&o.to_le_bytes());
        }
        rf.push(0x06); // uncompressed marker
        rf.extend_from_slice(data);

        let x = make_xattr(8, usize_total, &[]); // payload empty - all in rf
        let out = decompress(&x, Some(&rf)).expect("method8 uncompressed chunk");
        assert_eq!(out.as_slice(), data.as_ref());
    }

    #[test]
    fn method8_missing_resource_fork_errors() {
        // method=8 with rf=None must return MissingResourceFork immediately.
        let x = make_xattr(8, 5, &[]);
        assert!(matches!(
            decompress(&x, None),
            Err(DecmpfsError::MissingResourceFork)
        ));
    }

    #[test]
    fn method1_size_mismatch_errors() {
        // method=1 inline: payload has 3 bytes but uncompressed_size=5 → error.
        let x = make_xattr(1, 5, b"abc");
        assert!(
            decompress(&x, None).is_err(),
            "method-1 short payload must error"
        );
    }

    #[test]
    fn header_parse_too_short_errors() {
        // A buffer shorter than 16 bytes must return a Parse error.
        assert!(DecmpfsHeader::parse(&[0u8; 8]).is_err());
        // Correct magic but too short for method/size fields - still fails gracefully.
        let mut short = vec![0x66, 0x70, 0x6d, 0x63];
        short.extend_from_slice(&[0u8; 4]); // only 8 bytes total
        assert!(DecmpfsHeader::parse(&short).is_err());
    }
}
