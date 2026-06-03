//! Per-chunk free-run index mirroring the allocation bitmap.
//!
//! Build lazily when a chunk's bitmap is first loaded into the dirty cache,
//! then keep it in sync with each alloc/free hot-path edit. Allocation
//! becomes `range((n, 0)..)` on a `BTreeSet` keyed by (run_length, start_bit)
//! - O(log R) where R is the number of contiguous free runs in the chunk -
//! instead of a linear bitmap scan.
//!
//! The on-disk bitmap remains authoritative; this is a parallel hot-path
//! structure. If a chunk's runs cache is missing (e.g. never built), the
//! caller can fall back to the bitmap scan.
//!
//! Adjacency merge on free is O(log R) via a parallel `BTreeMap<start_bit,
//! run_length>` for predecessor/successor lookup. The pattern is the
//! standard two-index free-runs cache documented in
//! `docs/refs/apple-apfs-kext-symbols.md` (Tier 2).

use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default, Clone)]
pub(crate) struct ChunkFreeRuns {
    by_size: BTreeSet<(u32, u32)>,
    by_addr: BTreeMap<u32, u32>,
}

impl ChunkFreeRuns {
    pub fn from_bitmap(bm: &[u8], total_bits: usize) -> Self {
        // APFS chunks are at most blocks_per_chunk bits wide (≤ 65536 for
        // 4096-byte blocks). Guard against a caller supplying a value that
        // would silently truncate when cast to u32.
        let total_bits = total_bits.min(u32::MAX as usize);
        let mut s = Self::default();
        let mut bit = 0usize;
        while bit < total_bits {
            while bit < total_bits && bit_is_set(bm, bit) {
                bit += 1;
            }
            if bit >= total_bits {
                break;
            }
            let start = bit;
            while bit < total_bits && !bit_is_set(bm, bit) {
                bit += 1;
            }
            let len = bit - start;
            if len > 0 {
                // Safety: total_bits clamped to u32::MAX above; start < total_bits
                // and len ≤ total_bits, so both values fit in u32.
                s.by_size.insert((len as u32, start as u32));
                s.by_addr.insert(start as u32, len as u32);
            }
        }
        s
    }

    /// Take a contiguous run of EXACTLY `n` bits if any free run of length
    /// ≥ n exists. Returns the start bit and length actually allocated
    /// (always == n on success). Splits the remainder back into the cache.
    pub fn alloc_exact(&mut self, n: u32) -> Option<(u32, u32)> {
        if n == 0 {
            return None;
        }
        let probe = (n, 0u32);
        let &(c, p) = self.by_size.range(probe..).next()?;
        self.by_size.remove(&(c, p));
        self.by_addr.remove(&p);
        if c > n {
            let rem_start = p + n;
            let rem_len = c - n;
            self.by_size.insert((rem_len, rem_start));
            self.by_addr.insert(rem_start, rem_len);
        }
        Some((p, n))
    }

    /// Largest available run, capped at `cap`. Used when no run of length
    /// ≥ requested exists; caller may loop for the rest.
    pub fn alloc_largest(&mut self, cap: u32) -> Option<(u32, u32)> {
        if cap == 0 {
            return None;
        }
        let &(c, p) = self.by_size.iter().next_back()?;
        let take = c.min(cap);
        self.by_size.remove(&(c, p));
        self.by_addr.remove(&p);
        if c > take {
            let rem_start = p + take;
            let rem_len = c - take;
            self.by_size.insert((rem_len, rem_start));
            self.by_addr.insert(rem_start, rem_len);
        }
        Some((p, take))
    }

    /// Return `(start_bit, len)` to the free pool, merging with adjacent
    /// predecessor and successor runs if present.
    pub fn free(&mut self, start_bit: u32, run_len: u32) {
        if run_len == 0 {
            return;
        }
        let mut p = start_bit;
        let mut n = run_len;

        if let Some((&pp, &pc)) = self.by_addr.range(..p).next_back() {
            if pp.checked_add(pc) == Some(p) {
                self.by_size.remove(&(pc, pp));
                self.by_addr.remove(&pp);
                p = pp;
                n = n.saturating_add(pc);
            }
        }

        if let Some(&sc) = self.by_addr.get(&(p.saturating_add(n))) {
            let succ_start = p + n;
            self.by_size.remove(&(sc, succ_start));
            self.by_addr.remove(&succ_start);
            n = n.saturating_add(sc);
        }

        self.by_size.insert((n, p));
        self.by_addr.insert(p, n);
    }

    #[cfg(test)]
    pub fn total_free(&self) -> u32 {
        self.by_addr.values().copied().sum()
    }

    #[cfg(test)]
    pub fn run_count(&self) -> usize {
        self.by_addr.len()
    }
}

fn bit_is_set(bm: &[u8], bit: usize) -> bool {
    bm.get(bit / 8)
        .map_or(true, |&b| b & (1u8 << (bit % 8)) != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_bit(bm: &mut [u8], bit: usize) {
        bm[bit / 8] |= 1u8 << (bit % 8);
    }

    #[test]
    fn empty_bitmap_one_big_run() {
        let bm = vec![0u8; 16];
        let r = ChunkFreeRuns::from_bitmap(&bm, 128);
        assert_eq!(r.run_count(), 1);
        assert_eq!(r.total_free(), 128);
    }

    #[test]
    fn full_bitmap_no_runs() {
        let bm = vec![0xFFu8; 16];
        let r = ChunkFreeRuns::from_bitmap(&bm, 128);
        assert_eq!(r.run_count(), 0);
        assert_eq!(r.total_free(), 0);
    }

    #[test]
    fn alloc_exact_splits_remainder() {
        let bm = vec![0u8; 16];
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 128);
        let (p, n) = r.alloc_exact(10).unwrap();
        assert_eq!((p, n), (0, 10));
        assert_eq!(r.total_free(), 118);
        assert_eq!(r.run_count(), 1);
    }

    #[test]
    fn alloc_picks_first_fit_by_size() {
        // Two runs: [0..3) and [10..30). alloc(5) must pick the [10..30) one
        // because the size-ordered scan starts at length 5.
        let mut bm = vec![0u8; 16];
        for b in 3..10 {
            set_bit(&mut bm, b);
        }
        // Mark [30..128) as used too so we leave only the two free runs.
        for b in 30..128 {
            set_bit(&mut bm, b);
        }
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 128);
        assert_eq!(r.run_count(), 2);
        let (p, n) = r.alloc_exact(5).unwrap();
        assert_eq!((p, n), (10, 5));
        // 15 left in the big run; small one untouched.
        assert_eq!(r.total_free(), 3 + 15);
    }

    #[test]
    fn alloc_largest_partial() {
        let mut bm = vec![0xFFu8; 16];
        // Free only [4..10) - 6 bits.
        for b in 4..10 {
            bm[b / 8] &= !(1u8 << (b % 8));
        }
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 128);
        let (p, n) = r.alloc_largest(20).unwrap();
        assert_eq!((p, n), (4, 6));
        assert_eq!(r.total_free(), 0);
    }

    #[test]
    fn free_merges_both_neighbors() {
        let mut bm = vec![0xFFu8; 16];
        for b in 0..10 {
            bm[b / 8] &= !(1u8 << (b % 8));
        }
        for b in 20..30 {
            bm[b / 8] &= !(1u8 << (b % 8));
        }
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 128);
        assert_eq!(r.run_count(), 2);
        // Free [10..20) - should merge with both neighbors into [0..30).
        r.free(10, 10);
        assert_eq!(r.run_count(), 1);
        assert_eq!(r.total_free(), 30);
    }

    #[test]
    fn free_no_merge_isolated() {
        let mut bm = vec![0xFFu8; 16];
        for b in 0..3 {
            bm[b / 8] &= !(1u8 << (b % 8));
        }
        for b in 50..60 {
            bm[b / 8] &= !(1u8 << (b % 8));
        }
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 128);
        r.free(20, 5);
        assert_eq!(r.run_count(), 3);
        assert_eq!(r.total_free(), 3 + 5 + 10);
    }

    #[test]
    fn alloc_exact_zero_returns_none() {
        let bm = vec![0u8; 8];
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 64);
        // n=0 is a no-op: must return None without modifying the pool.
        assert!(
            r.alloc_exact(0).is_none(),
            "alloc_exact(0) must return None"
        );
        assert_eq!(r.total_free(), 64, "pool unchanged after alloc_exact(0)");
    }

    #[test]
    fn alloc_largest_zero_cap_returns_none() {
        let bm = vec![0u8; 8];
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 64);
        // cap=0 must return None without modifying the pool.
        assert!(
            r.alloc_largest(0).is_none(),
            "alloc_largest(0) must return None"
        );
        assert_eq!(r.total_free(), 64, "pool unchanged after alloc_largest(0)");
    }

    #[test]
    fn alloc_largest_cap_smaller_than_run_splits_remainder() {
        // One run of 20 bits. alloc_largest(5) takes 5 and leaves 15.
        let mut bm = vec![0xFFu8; 16];
        for b in 0..20 {
            bm[b / 8] &= !(1u8 << (b % 8));
        }
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 128);
        let (p, n) = r
            .alloc_largest(5)
            .expect("alloc_largest(5) with run=20 must succeed");
        assert_eq!(n, 5, "must allocate exactly cap=5 bits");
        assert_eq!(p, 0, "must start at beginning of run");
        assert_eq!(r.total_free(), 15, "15 bits must remain");
        assert_eq!(r.run_count(), 1, "remainder forms one run");
    }

    #[test]
    fn alloc_exact_empty_pool_returns_none() {
        let bm = vec![0xFFu8; 8]; // all used
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 64);
        assert!(r.alloc_exact(1).is_none(), "empty pool must return None");
    }

    #[test]
    fn alloc_largest_empty_pool_returns_none() {
        let bm = vec![0xFFu8; 8]; // all used
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 64);
        assert!(r.alloc_largest(10).is_none(), "empty pool must return None");
    }

    #[test]
    fn free_zero_len_is_noop() {
        let bm = vec![0xFFu8; 8]; // all used - empty pool
        let mut r = ChunkFreeRuns::from_bitmap(&bm, 64);
        r.free(0, 0); // must not panic or add spurious entries
        assert_eq!(r.run_count(), 0, "free(0,0) must be a no-op");
    }
}
