//! LZBITMAP decoder (decmpfs methods 13 and 14, also produced by Apple's
//! libcompression as `COMPRESSION_LZBITMAP`).
//!
//! Stream layout, as reverse-engineered by the open-source readers
//! (`linux-apfs-rw` and `apfs-fuse` agree) and confirmed here against files
//! Apple itself wrote: a 4-byte signature, then chunks. Every chunk opens with
//! two 24-bit little-endian lengths: the chunk's total length (header
//! included) and its decompressed length, at most 32 KiB. A chunk whose total
//! length is exactly six bytes more than its decompressed length stores the
//! bytes as they are. Any other chunk carries nine more header bytes: three
//! 24-bit offsets, relative to the chunk start, of the period area, the
//! inline-bitmap area and the token area. Literal bytes begin right after
//! that 15-byte header, and a table of twelve 10-bit entries (an 8-bit bitmap
//! plus a 2-bit period byte count, packed low bit first) occupies the chunk's
//! last 17 bytes. The stream ends with a chunk whose decompressed length is
//! zero.
//!
//! Decoding a packed chunk emits up to eight bytes per step. A 4-bit token
//! selects the step: 3 to 14 pick a table entry; 0 to 2 take the next byte
//! of the inline-bitmap area as the bitmap and the token itself as the period
//! byte count. A non-zero period byte count reads a new period (one or two
//! bytes, little-endian) from the period area; the period starts at 8 in
//! every packed chunk and can never be zero. Then, low bit first, a set bit
//! copies the next literal and a clear bit repeats the byte `period`
//! positions back in the output, which may reach into an earlier chunk. A
//! token followed by the nibble 0xF is applied `4 + n1 + n2 + ...` times,
//! the sum running over the following nibbles up to and including the first
//! one that is not 0xF. No repeat marker is looked for when eight bytes or
//! fewer remain, because the token area may run straight into the table.
//!
//! No `unsafe`, no indexing without a bounds check, no panics: every cursor
//! is validated against the chunk it belongs to.
use crate::endian::ParseError;

const SIGNATURE: [u8; 4] = [b'Z', b'B', b'M', 0x09];
const MAX_CHUNK_OUT: usize = 0x8000;
const PLAIN_HEADER: usize = 6;
const PACKED_HEADER: usize = 15;
const TABLE_BYTES: usize = 17;
const TABLE_ENTRIES: usize = 12;
const ENTRY_BITS: usize = 10;
/// Tokens below this carry their bitmap in the inline area.
const INLINE_TOKENS: u8 = 3;
const REPEAT_MARK: u8 = 0xF;
const REPEAT_BASE: usize = 4;
const INITIAL_PERIOD: usize = 8;
const MAX_PERIOD_BYTES: u8 = 2;
const BYTES_PER_STEP: usize = 8;

fn short(at: usize, need: usize, len: usize) -> ParseError {
    ParseError::Short { at, need, len }
}

fn u24_le(src: &[u8], at: usize) -> Result<usize, ParseError> {
    let bytes: [u8; 3] = src
        .get(at..at.saturating_add(3))
        .and_then(|s| s.try_into().ok())
        .ok_or(short(at, 3, src.len()))?;
    Ok(usize::from(bytes[0]) | usize::from(bytes[1]) << 8 | usize::from(bytes[2]) << 16)
}

#[derive(Clone, Copy, Default)]
struct Step {
    bitmap: u8,
    period_bytes: u8,
}

/// One packed chunk: the chunk bytes plus a cursor into each area.
struct Packed<'a> {
    chunk: &'a [u8],
    literal: usize,
    period_area: usize,
    inline_area: usize,
    /// Cursor into the token area, counted in nibbles from the chunk start.
    token: usize,
    table: [Step; TABLE_ENTRIES],
}

impl<'a> Packed<'a> {
    fn open(chunk: &'a [u8]) -> Result<Self, ParseError> {
        let len = chunk.len();
        if len < PACKED_HEADER + TABLE_BYTES {
            return Err(short(0, PACKED_HEADER + TABLE_BYTES, len));
        }
        let period_area = u24_le(chunk, 6)?;
        let inline_area = u24_le(chunk, 9)?;
        let token_area = u24_le(chunk, 12)?;
        if period_area >= len || inline_area >= len || token_area >= len {
            return Err(short(6, 9, len));
        }
        let table = read_table(chunk)?;
        Ok(Self {
            chunk,
            literal: PACKED_HEADER,
            period_area,
            inline_area,
            token: token_area.saturating_mul(2),
            table,
        })
    }

    fn byte(&self, at: usize) -> Result<u8, ParseError> {
        self.chunk
            .get(at)
            .copied()
            .ok_or(short(at, 1, self.chunk.len()))
    }

    fn next_literal(&mut self) -> Result<u8, ParseError> {
        let b = self.byte(self.literal)?;
        self.literal += 1;
        Ok(b)
    }

    fn next_inline_bitmap(&mut self) -> Result<u8, ParseError> {
        let b = self.byte(self.inline_area)?;
        self.inline_area += 1;
        Ok(b)
    }

    fn next_period(&mut self, bytes: u8) -> Result<usize, ParseError> {
        let mut period = 0usize;
        for i in 0..usize::from(bytes) {
            let b = self.byte(self.period_area)?;
            self.period_area += 1;
            period |= usize::from(b) << (8 * i);
        }
        Ok(period)
    }

    fn next_token(&mut self) -> Result<u8, ParseError> {
        let b = self.byte(self.token / 2)?;
        let nibble = if self.token.is_multiple_of(2) {
            b & 0xF
        } else {
            b >> 4
        };
        self.token += 1;
        Ok(nibble)
    }

    fn unread_token(&mut self) {
        self.token = self.token.saturating_sub(1);
    }

    fn step_for(&mut self, token: u8) -> Result<Step, ParseError> {
        if token >= INLINE_TOKENS {
            let index = usize::from(token - INLINE_TOKENS);
            return self.table.get(index).copied().ok_or(short(
                self.token / 2,
                1,
                self.chunk.len(),
            ));
        }
        Ok(Step {
            bitmap: self.next_inline_bitmap()?,
            period_bytes: token,
        })
    }
}

/// The twelve packed table entries in the chunk's last 17 bytes.
fn read_table(chunk: &[u8]) -> Result<[Step; TABLE_ENTRIES], ParseError> {
    let start = chunk
        .len()
        .checked_sub(TABLE_BYTES)
        .ok_or(short(0, TABLE_BYTES, chunk.len()))?;
    let area = chunk
        .get(start..)
        .ok_or(short(start, TABLE_BYTES, chunk.len()))?;
    let bit = |i: usize| -> u8 { area.get(i / 8).map(|b| (b >> (i % 8)) & 1).unwrap_or(0) };
    let mut table = [Step::default(); TABLE_ENTRIES];
    for (n, entry) in table.iter_mut().enumerate() {
        let base = n * ENTRY_BITS;
        let mut bitmap = 0u8;
        for i in 0..8 {
            bitmap |= bit(base + i) << i;
        }
        let period_bytes = bit(base + 8) | bit(base + 9) << 1;
        if period_bytes > MAX_PERIOD_BYTES {
            return Err(short(start, TABLE_BYTES, chunk.len()));
        }
        *entry = Step {
            bitmap,
            period_bytes,
        };
    }
    Ok(table)
}

/// Decode one packed chunk, appending exactly `want` bytes to `out`.
fn decode_packed(chunk: &[u8], want: usize, out: &mut Vec<u8>) -> Result<(), ParseError> {
    let mut p = Packed::open(chunk)?;
    let mut period = INITIAL_PERIOD;
    let mut written = 0usize;
    while written < want {
        let token = p.next_token()?;
        if token == REPEAT_MARK {
            return Err(short(p.token / 2, 1, chunk.len()));
        }
        let mut repeat = 1usize;
        if want - written > BYTES_PER_STEP {
            let next = p.next_token()?;
            if next == REPEAT_MARK {
                repeat = REPEAT_BASE;
                loop {
                    let n = p.next_token()?;
                    repeat = repeat.saturating_add(usize::from(n));
                    if n != REPEAT_MARK {
                        break;
                    }
                }
            } else {
                p.unread_token();
            }
        }
        for _ in 0..repeat {
            if written >= want {
                break;
            }
            let step = p.step_for(token)?;
            if step.period_bytes > 0 {
                period = p.next_period(step.period_bytes)?;
            }
            if period == 0 {
                return Err(short(p.period_area, 1, chunk.len()));
            }
            for i in 0..BYTES_PER_STEP {
                if written >= want {
                    break;
                }
                let b = if (step.bitmap >> i) & 1 == 1 {
                    p.next_literal()?
                } else {
                    let back =
                        out.len()
                            .checked_sub(period)
                            .ok_or(short(p.literal, period, out.len()))?;
                    out.get(back).copied().ok_or(short(back, 1, out.len()))?
                };
                out.push(b);
                written += 1;
            }
        }
    }
    Ok(())
}

/// Decode an LZBITMAP stream. `expected_len` bounds the output: the decoder
/// refuses to produce more, so an on-disk size cannot drive an oversized
/// allocation, and returns an error if the stream ends before reaching it.
pub fn lzbitmap_decode(src: &[u8], expected_len: usize) -> Result<Vec<u8>, ParseError> {
    if src.get(..SIGNATURE.len()) != Some(&SIGNATURE[..]) {
        return Err(ParseError::Short {
            at: 0,
            need: SIGNATURE.len(),
            len: src.len(),
        });
    }
    let mut out = Vec::with_capacity(expected_len.min(crate::lzvn::MAX_DECODE_RESERVE));
    let mut at = SIGNATURE.len();
    loop {
        let len = u24_le(src, at)?;
        let want = u24_le(src, at + 3)?;
        if want == 0 {
            break;
        }
        if want > MAX_CHUNK_OUT || len < PLAIN_HEADER {
            return Err(short(at, PLAIN_HEADER, src.len()));
        }
        let end = at.checked_add(len).ok_or(short(at, len, src.len()))?;
        let chunk = src.get(at..end).ok_or(short(at, len, src.len()))?;
        if out.len().saturating_add(want) > expected_len {
            return Err(short(at, want, expected_len));
        }
        if len == want + PLAIN_HEADER {
            let plain = chunk.get(PLAIN_HEADER..).ok_or(short(at, len, src.len()))?;
            out.extend_from_slice(plain);
        } else {
            decode_packed(chunk, want, &mut out)?;
        }
        at = end;
    }
    if out.len() != expected_len {
        return Err(short(at, expected_len, out.len()));
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn stream(chunks: &[&[u8]]) -> Vec<u8> {
        let mut s = SIGNATURE.to_vec();
        for c in chunks {
            s.extend_from_slice(c);
        }
        s.extend_from_slice(&[6, 0, 0, 0, 0, 0]);
        s
    }

    fn plain_chunk(data: &[u8]) -> Vec<u8> {
        let len = data.len() + PLAIN_HEADER;
        let mut c = vec![len as u8, (len >> 8) as u8, (len >> 16) as u8];
        c.extend_from_slice(&[
            data.len() as u8,
            (data.len() >> 8) as u8,
            (data.len() >> 16) as u8,
        ]);
        c.extend_from_slice(data);
        c
    }

    #[test]
    fn plain_chunks_are_copied() {
        let s = stream(&[&plain_chunk(b"hello "), &plain_chunk(b"world")]);
        assert_eq!(lzbitmap_decode(&s, 11).unwrap(), b"hello world");
    }

    #[test]
    fn wrong_signature_and_short_stream_error() {
        assert!(lzbitmap_decode(b"ZBM\x08", 0).is_err());
        assert!(lzbitmap_decode(b"ZBM\x09", 0).is_err());
        let s = stream(&[&plain_chunk(b"abc")]);
        assert!(lzbitmap_decode(&s[..s.len() - 8], 3).is_err());
    }

    #[test]
    fn output_is_bounded_by_the_expected_length() {
        let s = stream(&[&plain_chunk(b"hello")]);
        assert!(lzbitmap_decode(&s, 4).is_err());
        assert!(lzbitmap_decode(&s, 6).is_err());
        assert_eq!(lzbitmap_decode(&s, 5).unwrap(), b"hello");
    }

    #[test]
    fn packed_chunk_with_a_period_copy() {
        // Table entry 0 (token 3): bitmap 0b0000_0011, period byte count 1.
        // One step: two literals, then six copies at period 1 -> "abbbbbbb".
        let want = 8usize;
        let literals = b"ab";
        let periods = [1u8];
        let inline: [u8; 0] = [];
        let tokens = [0x03u8];
        let mut table = [0u8; TABLE_BYTES];
        // entry 0: bits 0..8 = 0b0000_0011, bits 8..10 = 0b01
        table[0] = 0b0000_0011;
        table[1] = 0b01;
        let body_len = PACKED_HEADER
            + literals.len()
            + periods.len()
            + inline.len()
            + tokens.len()
            + TABLE_BYTES;
        let mut c = vec![body_len as u8, (body_len >> 8) as u8, 0];
        c.extend_from_slice(&[want as u8, 0, 0]);
        let period_off = PACKED_HEADER + literals.len();
        let inline_off = period_off + periods.len();
        let token_off = inline_off + inline.len();
        for off in [period_off, inline_off, token_off] {
            c.extend_from_slice(&[off as u8, (off >> 8) as u8, 0]);
        }
        c.extend_from_slice(literals);
        c.extend_from_slice(&periods);
        c.extend_from_slice(&inline);
        c.extend_from_slice(&tokens);
        c.extend_from_slice(&table);
        assert_eq!(c.len(), body_len);
        let s = stream(&[&c]);
        assert_eq!(lzbitmap_decode(&s, want).unwrap(), b"abbbbbbb");
    }
}
