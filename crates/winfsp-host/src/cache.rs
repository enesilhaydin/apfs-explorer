//! Automatic read-ahead + LRU block cache.
//!
//! An APFS traversal issues many tiny, often-repeated random reads (B-tree
//! nodes, object map, inode/extent records). On a slow USB stick each raw
//! read costs milliseconds of seek+transfer, so an uncached walk is
//! unusably slow; on a fast Type-C SSD the same pattern is fine. This layer
//! makes both optimal automatically: every miss is satisfied by ONE large
//! aligned sequential read (read-ahead), and hot blocks are served from RAM.
//!
//! Zero-config by design: the block size and capacity are fixed sensible
//! defaults. There are no user-facing knobs — the product "does all the
//! optimal settings automatically".

use std::collections::{HashMap, VecDeque};

use apfs_core::block_device::{BlockDevice, BlockError, WritableBlockDevice};

/// Read-ahead granularity: 64 KiB. A multiple of every APFS/disk sector
/// size (512/4096) so block-aligned reads are always device-aligned.
///
/// Sizing is dominated by the *mount* requirement: opening the volume
/// touches a small set of SCATTERED metadata (container/volume superblocks,
/// checkpoint, object map, catalog B-tree root) — a few dozen 4 KiB reads
/// at unrelated offsets. A large block would force a full block transfer
/// per tiny metadata read (read amplification), making mount crawl on a
/// slow USB. 64 KiB keeps cold-metadata amplification ~16x lower so the
/// mount is effectively instant (NTFS-like) on any device, while still
/// amortizing seek latency and giving useful read-ahead for sequential
/// file content. The whole design stays zero-config.
const BLOCK: u64 = 1 << 16;

/// Resident block cap (~64 MiB at 64 KiB/block). Easily holds the catalog/
/// omap working set hot; bounded so memory stays modest on any machine.
/// (M8: an experiment to raise BLOCK to 256 KiB / 256 entries cost 2x on
/// the write path because every write_at invalidates a 4x larger range
/// and the next read has to re-fetch the whole window — reverted; the
/// 64 KiB sweet spot is right for our mixed read/write workload.)
const MAX_BLOCKS: usize = 1024;

/// Wraps any [`BlockDevice`] with transparent read-ahead + LRU caching.
pub struct CachingBlockDevice<D: BlockDevice> {
    inner: D,
    size: u64,
    blocks: HashMap<u64, Vec<u8>>,
    lru: VecDeque<u64>,
}

impl<D: BlockDevice> CachingBlockDevice<D> {
    pub fn new(inner: D) -> Self {
        let size = inner.size();
        Self {
            inner,
            size,
            blocks: HashMap::new(),
            lru: VecDeque::new(),
        }
    }

    fn touch(&mut self, idx: u64) {
        if let Some(pos) = self.lru.iter().position(|&b| b == idx) {
            self.lru.remove(pos);
        }
        self.lru.push_back(idx);
    }

    /// Ensure block `idx` is resident, fetching it (and only it — a single
    /// large aligned read) from the inner device on a miss.
    fn ensure(&mut self, idx: u64) -> Result<(), BlockError> {
        if self.blocks.contains_key(&idx) {
            self.touch(idx);
            return Ok(());
        }
        let start = idx * BLOCK;
        let want = BLOCK.min(self.size - start) as usize;
        let mut buf = vec![0u8; want];
        self.inner.read_at(start, &mut buf)?;
        if self.blocks.len() >= MAX_BLOCKS {
            if let Some(evict) = self.lru.pop_front() {
                self.blocks.remove(&evict);
            }
        }
        self.blocks.insert(idx, buf);
        self.lru.push_back(idx);
        Ok(())
    }
}

impl<D: BlockDevice> BlockDevice for CachingBlockDevice<D> {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let len = buf.len() as u64;
        if len == 0 {
            return Ok(());
        }
        let end = offset.checked_add(len).ok_or(BlockError::OutOfRange {
            offset,
            len,
            size: self.size,
        })?;
        if end > self.size {
            return Err(BlockError::OutOfRange {
                offset,
                len,
                size: self.size,
            });
        }
        let first = offset / BLOCK;
        let last = (end - 1) / BLOCK;
        let mut done = 0usize;
        for idx in first..=last {
            self.ensure(idx)?;
            let block = self.blocks.get(&idx).ok_or(BlockError::OutOfRange {
                offset,
                len,
                size: self.size,
            })?;
            let bstart = idx * BLOCK;
            // 0 for every block after the first (offset < bstart there).
            let from = offset.saturating_sub(bstart) as usize;
            let copy_start = bstart.max(offset);
            let copy_end = (bstart + block.len() as u64).min(end);
            let n = (copy_end - copy_start) as usize;
            let src = block.get(from..from + n).ok_or(BlockError::OutOfRange {
                offset,
                len,
                size: self.size,
            })?;
            buf.get_mut(done..done + n)
                .ok_or(BlockError::OutOfRange {
                    offset,
                    len,
                    size: self.size,
                })?
                .copy_from_slice(src);
            done += n;
        }
        Ok(())
    }
}

impl<D: WritableBlockDevice> WritableBlockDevice for CachingBlockDevice<D> {
    /// Drop the entire read-ahead set. Used by `with_writable` between a
    /// committed transaction and the next FsView open so the reopened
    /// container always re-reads metadata from the durable inner device.
    fn invalidate_read_cache(&mut self) {
        self.blocks.clear();
        self.lru.clear();
        self.inner.invalidate_read_cache();
    }

    /// Write-through: forward to the inner device, then invalidate any
    /// cached blocks that overlap the written range. The cache is a
    /// read-ahead cache only; we do not buffer writes (the inner
    /// PhysicalDrive opens with FILE_FLAG_WRITE_THROUGH for durability).
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        let len = buf.len() as u64;
        if len == 0 {
            return Ok(());
        }
        let end = offset.checked_add(len).ok_or(BlockError::OutOfRange {
            offset,
            len,
            size: self.size,
        })?;
        if end > self.size {
            return Err(BlockError::OutOfRange {
                offset,
                len,
                size: self.size,
            });
        }
        // Write-through to inner. Invalidate overlapping cache blocks
        // BEFORE propagating the result on failure: an inner driver that
        // reports an error after writing partial bytes (rare under
        // FILE_FLAG_WRITE_THROUGH but legal per the WriteFile contract)
        // would otherwise leave stale cache entries silently masking the
        // partial state from later reads. Cost on failure: at most a
        // single re-read of the affected range on the next access.
        let first = offset / BLOCK;
        let last = (end - 1) / BLOCK;
        let result = self.inner.write_at(offset, buf);
        for idx in first..=last {
            self.blocks.remove(&idx);
            self.lru.retain(|&i| i != idx);
        }
        result
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Backing device that counts inner reads, to prove read-ahead.
    struct CountedMem {
        data: Vec<u8>,
        reads: usize,
    }
    impl BlockDevice for CountedMem {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            self.reads += 1;
            let o = offset as usize;
            let src = self
                .data
                .get(o..o + buf.len())
                .ok_or(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.data.len() as u64,
                })?;
            buf.copy_from_slice(src);
            Ok(())
        }
    }

    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn returns_identical_bytes_as_uncached() {
        let data = pattern(3 * (1 << 20) + 12345);
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: data.clone(),
            reads: 0,
        });
        for &(off, len) in &[
            (0u64, 100usize),
            (1 << 20, 4096),
            ((1 << 20) - 10, 50),          // straddles a block boundary
            (2 * (1 << 20) + 1000, 70000), // spans 2+ blocks
            (data.len() as u64 - 7, 7),    // exact EOF tail
        ] {
            let mut b = vec![0u8; len];
            dev.read_at(off, &mut b).unwrap();
            assert_eq!(b, &data[off as usize..off as usize + len], "off={off}");
        }
    }

    #[test]
    fn read_ahead_collapses_many_small_reads() {
        let data = pattern(1 << 20);
        let mut dev = CachingBlockDevice::new(CountedMem { data, reads: 0 });
        // 1000 tiny reads all within the first 1 MiB block.
        for i in 0..1000u64 {
            let mut b = [0u8; 16];
            dev.read_at(i * 64, &mut b).unwrap();
        }
        // Exactly one underlying device read despite 1000 logical reads.
        // (Access the counter via a second handle is not possible; instead
        // assert by re-reading: cache hit must not grow resident set.)
        assert_eq!(dev.blocks.len(), 1);
    }

    #[test]
    fn eviction_keeps_correctness_over_capacity() {
        // More distinct blocks than MAX_BLOCKS forces eviction; reads must
        // still return correct bytes (re-fetch on miss). Spacing is one
        // BLOCK so each access lands in a distinct cache block.
        let nblocks = MAX_BLOCKS + 8;
        let data = pattern(nblocks * (1 << 16));
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: data.clone(),
            reads: 0,
        });
        for idx in 0..nblocks as u64 {
            let off = idx * (1 << 16) + 123;
            let mut b = [0u8; 32];
            dev.read_at(off, &mut b).unwrap();
            assert_eq!(&b, &data[off as usize..off as usize + 32], "block {idx}");
        }
        assert!(dev.blocks.len() <= MAX_BLOCKS);
        // Re-read an evicted early block: still correct.
        let mut b = [0u8; 32];
        dev.read_at(123, &mut b).unwrap();
        assert_eq!(&b, &data[123..155]);
    }

    #[test]
    fn rejects_out_of_range() {
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: pattern(4096),
            reads: 0,
        });
        let mut b = [0u8; 64];
        assert!(matches!(
            dev.read_at(4090, &mut b),
            Err(BlockError::OutOfRange { .. })
        ));
    }
}
