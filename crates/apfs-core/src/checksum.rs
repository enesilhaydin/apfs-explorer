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
    for w in data.chunks_exact(4) {
        let mut a = [0u8; 4];
        a.copy_from_slice(w);
        let v = u64::from(u32::from_le_bytes(a));
        s1 = (s1 + v) % MOD;
        s2 = (s2 + s1) % MOD;
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
