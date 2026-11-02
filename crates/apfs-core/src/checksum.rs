//! APFS Fletcher-64. The Apple spec leaves the algorithm unspecified
//! (confirmed: PDF stub, the APFS specification); this is the community-canonical
//! algorithm used by libfsapfs/apfs-fuse/linux-apfs, proven here against a real
//! macOS-written checksum (tests/fixtures/apfs-tiny.img block 0).
use crate::endian::{u64_le, ParseError};

const MOD: u64 = 0xFFFF_FFFF;

/// Fletcher-64 over `data` as little-endian u32 words. `data` MUST be the
/// object bytes AFTER the 8-byte `o_cksum` field; trailing bytes < 4 are ignored.
fn fletcher64_words(data: &[u8]) -> u64 {
    let mut s1: u64 = 0;
    let mut s2: u64 = 0;
    // The sums are reduced once per REDUCE_EVERY words instead of after
    // every word: the same residues, without two divisions per word (the
    // checksum was the costliest step of a small write). Starting below MOD,
    // after k words s1 < (k + 1) * 2^32 and s2 < (k + 1)^2 * 2^31, inside a
    // u64 while k < 90 000.
    const REDUCE_EVERY: usize = 4096;
    for group in data.chunks(REDUCE_EVERY * 4) {
        for w in group.chunks_exact(4) {
            let mut a = [0u8; 4];
            a.copy_from_slice(w);
            s1 += u64::from(u32::from_le_bytes(a));
            s2 += s1;
        }
        s1 %= MOD;
        s2 %= MOD;
    }
    let c1 = MOD - ((s1 + s2) % MOD);
    let c2 = MOD - ((s1 + c1) % MOD);
    (c2 << 32) | c1
}

/// Compute the APFS Fletcher-64 for a full object block. The stored o_cksum
/// occupies block[0..8] and is EXCLUDED from the input.
pub fn fletcher64(block: &[u8]) -> u64 {
    let body = block.get(8..).unwrap_or(&[]);
    fletcher64_words(body)
}

/// Verify a block's stored Fletcher-64 (block[0..8], little-endian u64).
pub fn verify_block(block: &[u8]) -> Result<(), ParseError> {
    let stored = u64_le(block, 0)?;
    let computed = fletcher64(block);
    if stored == computed {
        Ok(())
    } else {
        Err(ParseError::BadChecksum { stored, computed })
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use std::path::Path;

    /// The checksum reducing after every word, as first written.
    fn reference(data: &[u8]) -> u64 {
        let (mut s1, mut s2) = (0u64, 0u64);
        for w in data.chunks_exact(4) {
            let v = u64::from(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
            s1 = (s1 + v) % MOD;
            s2 = (s2 + s1) % MOD;
        }
        let c1 = MOD - ((s1 + s2) % MOD);
        let c2 = MOD - ((s1 + c1) % MOD);
        (c2 << 32) | c1
    }

    /// Reducing once per group of words gives the same checksum, for words
    /// at the top of the range, over several groups, and for a length that
    /// is not a whole number of words.
    #[test]
    fn deferred_reduction_matches_per_word() {
        let mut x: u32 = 0x1234_5678;
        let mut noise = || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        let mut cases: Vec<Vec<u8>> = vec![
            vec![0xFF; 4096],
            vec![0xFF; 64 * 4096 + 3],
            vec![0; 4096],
            Vec::new(),
        ];
        for len in [4088usize, 4096, 16_384 + 7, 100_000] {
            cases.push((0..len).map(|_| noise() as u8).collect());
        }
        for c in &cases {
            assert_eq!(fletcher64_words(c), reference(c), "length {}", c.len());
        }
    }

    #[test]
    fn verifies_real_apfs_block_zero() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-tiny.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing ({img}) - run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let data = std::fs::read(img).unwrap();
        let block = &data[0..4096];
        verify_block(block).expect("real APFS block 0 must pass Fletcher-64");
        let mut bad = block.to_vec();
        bad[100] ^= 0xFF;
        assert!(verify_block(&bad).is_err());
    }
}
