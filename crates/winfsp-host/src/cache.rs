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
//! defaults. There are no user-facing knobs - the product "does all the
//! optimal settings automatically".

use std::collections::{BTreeMap, HashMap};

use apfs_core::block_device::{BlockDevice, BlockError, WritableBlockDevice};

/// Read-ahead granularity: 64 KiB. A multiple of every APFS/disk sector
/// size (512/4096) so block-aligned reads are always device-aligned.
///
/// Sizing is dominated by the *mount* requirement: opening the volume
/// touches a small set of SCATTERED metadata (container/volume superblocks,
/// checkpoint, object map, catalog B-tree root) - a few dozen 4 KiB reads
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
/// and the next read has to re-fetch the whole window - reverted; the
/// 64 KiB sweet spot is right for our mixed read/write workload.)
const MAX_BLOCKS: usize = 1024;

/// Largest read the cache serves: one APFS block, the size of every node
/// and object the metadata is read as. Anything larger is file content.
const METADATA_READ_MAX: u64 = 4096;

/// Read-ahead for file content read in order. Windows caches content
/// itself and asks for it in pieces of a few KiB; one device read per
/// piece doubled the time of a sequential read.
const CONTENT_WINDOW: usize = 1 << 20;

/// Windows kept at once. A file edited in places has its edits elsewhere on
/// the disk (copy-on-write), and a reader going through it in order dips
/// there and comes back; with one window each dip moved it and the way back
/// refilled it, two 1 MiB reads per edit.
const CONTENT_WINDOWS: usize = 4;

/// How much a reader must have read in order before a window is read ahead
/// for it. A dip into a file's edits is a block or two read in order; a
/// window of 1 MiB for each read 1 MiB to use 8 KiB.
const READ_AHEAD_AFTER: u64 = 16 << 10;

/// Granularity of the content cache: one APFS block. A block's content
/// never changes in place (copy-on-write), and a write through this device
/// drops what it overlaps, so a cached block holds what the device holds,
/// whichever files share it.
const CONTENT_BLOCK: u64 = 4096;

/// Content blocks kept: 64 MiB, a budget of its own beside the metadata's.
/// Windows does not cache file data on the mount (its data caching hung
/// copies of large files), so without it a file read twice was read from
/// the drive twice.
const CONTENT_BLOCKS: usize = (64 << 20) / CONTENT_BLOCK as usize;

/// Content read ahead from the device offset `at`, last used at `used`.
struct Window {
    at: u64,
    data: Vec<u8>,
    used: u64,
}

impl Window {
    fn holds(&self, offset: u64, end: u64) -> bool {
        offset >= self.at && end <= self.at + self.data.len() as u64
    }
}

/// Wraps any [`BlockDevice`] with transparent read-ahead + LRU caching.
pub struct CachingBlockDevice<D: BlockDevice> {
    inner: D,
    size: u64,
    /// Resident blocks, each with the stamp of its last use.
    blocks: HashMap<u64, (Vec<u8>, u64)>,
    /// Resident blocks by the stamp of their last use, oldest first. A scan
    /// of a list for every hit cost up to MAX_BLOCKS comparisons per read.
    lru: BTreeMap<u64, u64>,
    stamp: u64,
    /// Content read ahead of readers going in order.
    windows: Vec<Window>,
    /// Where the last content read ended; a read starting here is in order.
    content_end: u64,
    /// Bytes read in order up to and including the last content read.
    streak: u64,
    /// Content blocks read, each with the stamp of its last use.
    content: HashMap<u64, (Vec<u8>, u64)>,
    /// Content blocks by the stamp of their last use, oldest first.
    content_lru: BTreeMap<u64, u64>,
}

impl<D: BlockDevice> CachingBlockDevice<D> {
    pub fn new(inner: D) -> Self {
        let size = inner.size();
        Self {
            inner,
            size,
            blocks: HashMap::new(),
            lru: BTreeMap::new(),
            stamp: 0,
            windows: Vec::new(),
            content_end: u64::MAX,
            streak: 0,
            content: HashMap::new(),
            content_lru: BTreeMap::new(),
        }
    }

    /// A content read (`offset..end`): from the content cache when it holds
    /// every block of it; otherwise read as below, and the whole blocks it
    /// covers kept.
    fn read_content(&mut self, offset: u64, end: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let in_order = offset == self.content_end;
        self.content_end = end;
        let len = buf.len() as u64;
        self.streak = if in_order {
            self.streak.saturating_add(len)
        } else {
            len
        };
        if self.content_hit(offset, end, buf) {
            return Ok(());
        }
        let stamp = self.next_stamp();
        self.fetch_content(offset, end, buf, in_order, stamp)?;
        self.keep_content(offset, buf);
        Ok(())
    }

    /// Copy `offset..end` out of the content cache if it holds all of it.
    /// Each block gets a stamp of its own: the eviction order is keyed by
    /// stamp, and blocks sharing one left all but one out of it.
    fn content_hit(&mut self, offset: u64, end: u64, buf: &mut [u8]) -> bool {
        let (first, last) = (offset / CONTENT_BLOCK, (end - 1) / CONTENT_BLOCK);
        if !(first..=last).all(|idx| self.content.contains_key(&idx)) {
            return false;
        }
        for idx in first..=last {
            let stamp = self.next_stamp();
            let Some((block, used)) = self.content.get_mut(&idx) else {
                return false;
            };
            self.content_lru.remove(used);
            *used = stamp;
            self.content_lru.insert(stamp, idx);
            let bstart = idx * CONTENT_BLOCK;
            let (from, to) = (bstart.max(offset), (bstart + CONTENT_BLOCK).min(end));
            let (Some(src), Some(dst)) = (
                block.get((from - bstart) as usize..(to - bstart) as usize),
                buf.get_mut((from - offset) as usize..(to - offset) as usize),
            ) else {
                return false;
            };
            dst.copy_from_slice(src);
        }
        true
    }

    /// Keep every whole block `buf` (read at `offset`) covers, each under a
    /// stamp of its own.
    fn keep_content(&mut self, offset: u64, buf: &[u8]) {
        let end = offset + buf.len() as u64;
        let first = offset.div_ceil(CONTENT_BLOCK);
        let last = end / CONTENT_BLOCK;
        for idx in first..last {
            let bstart = idx * CONTENT_BLOCK;
            let from = (bstart - offset) as usize;
            let Some(src) = buf.get(from..from + CONTENT_BLOCK as usize) else {
                continue;
            };
            if let Some((_, used)) = self.content.remove(&idx) {
                self.content_lru.remove(&used);
            }
            if self.content.len() >= CONTENT_BLOCKS {
                if let Some((_, evict)) = self.content_lru.pop_first() {
                    self.content.remove(&evict);
                }
            }
            let stamp = self.next_stamp();
            self.content.insert(idx, (src.to_vec(), stamp));
            self.content_lru.insert(stamp, idx);
        }
    }

    /// Drop the cached content blocks holding any of `offset..end`.
    fn forget_content(&mut self, offset: u64, end: u64) {
        let (first, last) = (offset / CONTENT_BLOCK, (end - 1) / CONTENT_BLOCK);
        if last - first >= self.content.len() as u64 {
            let lru = &mut self.content_lru;
            self.content.retain(|&idx, (_, used)| {
                let keep = idx < first || idx > last;
                if !keep {
                    lru.remove(used);
                }
                keep
            });
        } else {
            for idx in first..=last {
                if let Some((_, used)) = self.content.remove(&idx) {
                    self.content_lru.remove(&used);
                }
            }
        }
    }

    /// Read content the cache does not hold: from the window when it holds
    /// it; a read that continues the previous one refills the window from
    /// where it starts; any other goes to the device as asked.
    fn fetch_content(
        &mut self,
        offset: u64,
        end: u64,
        buf: &mut [u8],
        in_order: bool,
        stamp: u64,
    ) -> Result<(), BlockError> {
        let (len, size) = (buf.len() as u64, self.size);
        let oor = || BlockError::OutOfRange { offset, len, size };
        if let Some(w) = self.windows.iter_mut().find(|w| w.holds(offset, end)) {
            w.used = stamp;
            let from = (offset - w.at) as usize;
            buf.copy_from_slice(w.data.get(from..from + buf.len()).ok_or_else(oor)?);
            return Ok(());
        }
        if !in_order || self.streak < READ_AHEAD_AFTER {
            return self.inner.read_at(offset, buf);
        }
        // A new window, or the one used longest ago.
        let slot = if self.windows.len() < CONTENT_WINDOWS {
            self.windows.push(Window {
                at: offset,
                data: Vec::new(),
                used: stamp,
            });
            self.windows.len() - 1
        } else {
            self.windows
                .iter()
                .enumerate()
                .min_by_key(|(_, w)| w.used)
                .map_or(0, |(i, _)| i)
        };
        let n = (buf.len().max(CONTENT_WINDOW) as u64).min(self.size - offset) as usize;
        let Some(w) = self.windows.get_mut(slot) else {
            return self.inner.read_at(offset, buf);
        };
        w.data.resize(n, 0);
        if let Err(e) = self.inner.read_at(offset, &mut w.data) {
            self.windows.swap_remove(slot);
            return Err(e);
        }
        w.at = offset;
        w.used = stamp;
        buf.copy_from_slice(w.data.get(..buf.len()).ok_or_else(oor)?);
        Ok(())
    }

    /// Drop every window holding any of `offset..end`.
    fn forget_window(&mut self, offset: u64, end: u64) {
        self.windows
            .retain(|w| offset >= w.at + w.data.len() as u64 || end <= w.at);
    }

    fn next_stamp(&mut self) -> u64 {
        self.stamp += 1;
        self.stamp
    }

    /// Ensure block `idx` is resident, fetching it (and only it - a single
    /// large aligned read) from the inner device on a miss.
    fn ensure(&mut self, idx: u64) -> Result<(), BlockError> {
        let now = self.next_stamp();
        if let Some((_, used)) = self.blocks.get_mut(&idx) {
            self.lru.remove(used);
            *used = now;
            self.lru.insert(now, idx);
            return Ok(());
        }
        let start = idx * BLOCK;
        let want = BLOCK.min(self.size - start) as usize;
        let mut buf = vec![0u8; want];
        self.inner.read_at(start, &mut buf)?;
        if self.blocks.len() >= MAX_BLOCKS {
            if let Some((_, evict)) = self.lru.pop_first() {
                self.blocks.remove(&evict);
            }
        }
        self.blocks.insert(idx, (buf, now));
        self.lru.insert(now, idx);
        Ok(())
    }

    /// Drop block `idx` if it is resident.
    fn forget(&mut self, idx: u64) {
        if let Some((_, used)) = self.blocks.remove(&idx) {
            self.lru.remove(&used);
        }
    }
}

impl<D: BlockDevice> BlockDevice for CachingBlockDevice<D> {
    fn size(&self) -> u64 {
        self.size
    }

    /// File content, whatever its size: Windows passes a program's 4 KiB
    /// reads through as they come, and kept as metadata they filled the
    /// whole cache (97 MB of mount memory after reading large files). It is
    /// kept in the content cache instead, within that cache's own budget.
    fn read_content_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let len = buf.len() as u64;
        if len == 0 {
            return Ok(());
        }
        let end =
            offset
                .checked_add(len)
                .filter(|&e| e <= self.size)
                .ok_or(BlockError::OutOfRange {
                    offset,
                    len,
                    size: self.size,
                })?;
        self.read_content(offset, end, buf)
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
        // Metadata is read a node at a time, one APFS block; file content
        // comes in larger pieces, and Windows keeps its own copy of it, so
        // here it would only push the metadata out: reading a 64 MiB file
        // filled the whole cache with it. A resident block never differs
        // from the device (a write drops it), so reading past them is safe.
        if len > METADATA_READ_MAX {
            return self.read_content(offset, end, buf);
        }
        let first = offset / BLOCK;
        let last = (end - 1) / BLOCK;
        let mut done = 0usize;
        for idx in first..=last {
            self.ensure(idx)?;
            let (block, _) = self.blocks.get(&idx).ok_or(BlockError::OutOfRange {
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
    /// Drop the entire read-ahead set. Used after a commit that failed part
    /// way, so the reopened container re-reads metadata from the inner
    /// device. A commit that succeeded keeps the cache: its writes went
    /// through [`Self::write_at`], which drops what they overlap.
    fn invalidate_read_cache(&mut self) {
        self.blocks.clear();
        self.lru.clear();
        self.windows.clear();
        self.content.clear();
        self.content_lru.clear();
        self.inner.invalidate_read_cache();
    }

    /// Write-through: forward to the inner device, then invalidate any
    /// cached metadata, read-ahead and content that overlaps the written
    /// range. The caches hold reads only; we do not buffer writes (the inner
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
            self.forget(idx);
        }
        self.forget_window(offset, end);
        self.forget_content(offset, end);
        result
    }

    /// Forward the COW-1 durability barrier to the inner device. The cache is
    /// read-ahead only (writes are write-through), so it holds no dirty data -
    /// but it MUST delegate, or the trait's no-op default would silently
    /// swallow the barrier and `PhysicalDrive::flush_data` (FlushFileBuffers)
    /// would never run on the real mount stack.
    fn flush_data(&mut self) -> Result<(), BlockError> {
        self.inner.flush_data()
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
        bytes: u64,
    }
    impl BlockDevice for CountedMem {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            self.reads += 1;
            self.bytes += buf.len() as u64;
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
            bytes: 0,
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
        let mut dev = CachingBlockDevice::new(CountedMem {
            data,
            reads: 0,
            bytes: 0,
        });
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
            bytes: 0,
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
    fn a_read_larger_than_a_block_passes_the_cache_by() {
        let data = pattern(4 << 20);
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: data.clone(),
            reads: 0,
            bytes: 0,
        });
        let mut b = vec![0u8; 1 << 20];
        dev.read_at(12345, &mut b).unwrap();
        assert_eq!(b, &data[12345..12345 + (1 << 20)]);
        assert!(dev.blocks.is_empty(), "content read in bulk is not kept");
        assert_eq!(dev.inner.reads, 1, "one read for the whole range");
        let mut small = [0u8; 16];
        dev.read_at(12345, &mut small).unwrap();
        assert_eq!(&small, &data[12345..12361]);
        assert_eq!(dev.blocks.len(), 1, "small reads still fill the cache");
    }

    #[test]
    fn content_read_in_order_comes_through_one_window() {
        let data = pattern(8 << 20);
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: data.clone(),
            reads: 0,
            bytes: 0,
        });
        let piece = 8192usize;
        let mut off = 1 << 20;
        for _ in 0..256 {
            let mut b = vec![0u8; piece];
            dev.read_at(off as u64, &mut b).unwrap();
            assert_eq!(b, &data[off..off + piece], "off={off}");
            off += piece;
        }
        // 2 MiB in order: the first piece alone, then whole windows.
        assert!(dev.inner.reads <= 4, "{} device reads", dev.inner.reads);
        assert!(
            dev.blocks.is_empty(),
            "content stays out of the block cache"
        );
    }

    /// A file edited in places, read in order: its blocks come from its own
    /// region, and every 64 KiB two come from where the edits were written.
    /// Each jump to the edits and back used to move the one window, and
    /// refill it there: two 1 MiB reads per edit.
    #[test]
    fn a_read_that_dips_into_another_region_keeps_its_window() {
        let data = pattern(16 << 20);
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: data.clone(),
            reads: 0,
            bytes: 0,
        });
        let main = 1usize << 20;
        let edits = 8usize << 20;
        // The edits were written in another order than they are read: each
        // pair of blocks lies somewhere in 4 MiB of edits.
        let edit_at = |chunk: usize| edits + (chunk * 37 % 512) * 8192;
        for chunk in 0..64usize {
            for blk in 0..16usize {
                let off = if blk == 6 || blk == 7 {
                    edit_at(chunk) + (blk - 6) * 4096
                } else {
                    main + chunk * 65536 + blk * 4096
                };
                let mut b = vec![0u8; 4096];
                dev.read_content_at(off as u64, &mut b).unwrap();
                assert_eq!(b, &data[off..off + 4096], "off={off}");
            }
        }
        // Four windows over the file's own blocks and two direct reads per
        // edit; not a window refilled for every pair.
        assert!(
            dev.inner.reads <= 4 + 2 * 64 + 4,
            "{} device reads for 4 MiB read in order",
            dev.inner.reads
        );
        assert!(
            dev.inner.bytes <= (6 << 20) as u64,
            "{} bytes read from the device for 4 MiB",
            dev.inner.bytes
        );
    }

    #[test]
    fn content_read_in_blocks_stays_out_of_the_block_cache() {
        let data = pattern(4 << 20);
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: data.clone(),
            reads: 0,
            bytes: 0,
        });
        // A reader asking 4 KiB at a time, as Windows passes them through.
        for k in 0..512usize {
            let mut b = vec![0u8; 4096];
            dev.read_content_at((k * 4096) as u64, &mut b).unwrap();
            assert_eq!(b, &data[k * 4096..(k + 1) * 4096]);
        }
        assert!(dev.blocks.is_empty(), "content is not kept as metadata");
        // The first 12 KiB come as asked (no read-ahead before 16 KiB in
        // order), the rest through windows.
        assert!(
            dev.inner.reads <= 6,
            "{} device reads for 2 MiB",
            dev.inner.reads
        );
    }

    /// Scattered 4 KiB reads, as Windows passes a program's reads through:
    /// a block read once comes from memory the next time.
    #[test]
    fn content_read_again_comes_from_memory() {
        let data = pattern(8 << 20);
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: data.clone(),
            reads: 0,
            bytes: 0,
        });
        let blocks: Vec<usize> = (0..300).map(|i| (i * 677) % 2048).collect();
        let read_all = |dev: &mut CachingBlockDevice<CountedMem>| {
            for &k in &blocks {
                let mut b = vec![0u8; 4096];
                dev.read_content_at((k * 4096) as u64, &mut b).unwrap();
                assert_eq!(b, &data[k * 4096..(k + 1) * 4096], "block {k}");
            }
        };
        read_all(&mut dev);
        let first = dev.inner.reads;
        assert!(first > 0);
        read_all(&mut dev);
        assert_eq!(dev.inner.reads, first, "the second pass reads the device");
        assert!(
            dev.blocks.is_empty(),
            "content stays out of the block cache"
        );
    }

    #[test]
    fn a_write_drops_the_content_it_overlaps() {
        let data = pattern(4 << 20);
        let mut dev = CachingBlockDevice::new(MemRw { data: data.clone() });
        let mut b = vec![0u8; 4096];
        dev.read_content_at(65536, &mut b).unwrap();
        dev.read_content_at(3 << 20, &mut b).unwrap();
        // A write inside the block, then one over the whole of another.
        dev.write_at(65536 + 1024, &[0xCD; 512]).unwrap();
        dev.write_at(3 << 20, &[0xEF; 4096]).unwrap();
        dev.read_content_at(65536, &mut b).unwrap();
        let mut want = data[65536..65536 + 4096].to_vec();
        want[1024..1536].fill(0xCD);
        assert_eq!(b, want, "the read sees the write inside the block");
        dev.read_content_at(3 << 20, &mut b).unwrap();
        assert_eq!(
            b,
            vec![0xEF; 4096],
            "the read sees the write over the block"
        );
        dev.invalidate_read_cache();
        assert!(dev.content.is_empty(), "dropping the cache drops content");
    }

    #[test]
    fn the_content_cache_holds_its_budget() {
        let n = CONTENT_BLOCKS + 10;
        let data = pattern(n * 4096);
        let mut dev = CachingBlockDevice::new(CountedMem {
            data,
            reads: 0,
            bytes: 0,
        });
        let mut b = vec![0u8; 4096];
        // Last block first, so no read is in order and none is read ahead.
        for k in (0..n).rev() {
            dev.read_content_at((k * 4096) as u64, &mut b).unwrap();
        }
        assert_eq!(dev.content.len(), CONTENT_BLOCKS);
        let before = dev.inner.reads;
        dev.read_content_at(0, &mut b).unwrap();
        assert_eq!(dev.inner.reads, before, "the newest block stays");
        dev.read_content_at(((n - 1) * 4096) as u64, &mut b)
            .unwrap();
        assert_eq!(dev.inner.reads, before + 1, "the oldest block went");
        assert_eq!(dev.content.len(), CONTENT_BLOCKS);
    }

    /// Reads of many blocks each, as a copy makes: every block gets its own
    /// place in the eviction order, so the budget holds. Sharing one place
    /// per read let the cache grow by all but one block of every read.
    #[test]
    fn large_reads_keep_the_content_budget() {
        let data = pattern(CONTENT_BLOCKS * 4096 + (8 << 20));
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: data.clone(),
            reads: 0,
            bytes: 0,
        });
        for (k, off) in (0..data.len()).step_by(1 << 20).enumerate() {
            let len = (1 << 20).min(data.len() - off);
            let mut b = vec![0u8; len];
            dev.read_content_at(off as u64, &mut b).unwrap();
            assert_eq!(b, &data[off..off + len], "read {k}");
            assert!(
                dev.content.len() <= CONTENT_BLOCKS,
                "read {k}: {}",
                dev.content.len()
            );
            assert_eq!(dev.content.len(), dev.content_lru.len(), "read {k}");
        }
        // Again, read back from the middle: hits refresh blocks one by one.
        let mut b = vec![0u8; 1 << 20];
        dev.read_content_at((CONTENT_BLOCKS * 4096) as u64, &mut b)
            .unwrap();
        assert_eq!(dev.content.len(), dev.content_lru.len());
    }

    #[test]
    fn unaligned_content_reads_are_exact() {
        let data = pattern(2 << 20);
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: data.clone(),
            reads: 0,
            bytes: 0,
        });
        let reads: Vec<(usize, usize)> = (0..400)
            .map(|i| {
                (
                    (i * 7919 + i * i * 13) % ((2 << 20) - 70_000),
                    1 + (i * 4099) % 66_000,
                )
            })
            .collect();
        for _ in 0..2 {
            for &(off, len) in &reads {
                let mut b = vec![0u8; len];
                dev.read_content_at(off as u64, &mut b).unwrap();
                assert_eq!(b, &data[off..off + len], "off={off} len={len}");
            }
        }
    }

    #[test]
    fn content_read_out_of_order_reads_only_what_is_asked() {
        let data = pattern(8 << 20);
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: data.clone(),
            reads: 0,
            bytes: 0,
        });
        for &off in &[5usize << 20, 1 << 20, 3 << 20] {
            let mut b = vec![0u8; 8192];
            dev.read_at(off as u64, &mut b).unwrap();
            assert_eq!(b, &data[off..off + 8192]);
        }
        assert_eq!(dev.inner.reads, 3);
        assert!(dev.windows.is_empty(), "no read-ahead for scattered reads");
    }

    #[test]
    fn a_write_drops_the_window_it_overlaps() {
        let data = pattern(4 << 20);
        let mut dev = CachingBlockDevice::new(MemRw { data: data.clone() });
        let mut b = vec![0u8; 8192];
        dev.read_at(0, &mut b).unwrap();
        dev.read_at(8192, &mut b).unwrap();
        assert!(!dev.windows.is_empty());
        dev.write_at(65536, &[0xAB; 8192]).unwrap();
        dev.read_at(65536, &mut b).unwrap();
        assert_eq!(b, vec![0xAB; 8192], "the read sees the write");
    }

    /// Writable in-memory device.
    struct MemRw {
        data: Vec<u8>,
    }
    impl BlockDevice for MemRw {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let o = offset as usize;
            buf.copy_from_slice(&self.data[o..o + buf.len()]);
            Ok(())
        }
    }
    impl WritableBlockDevice for MemRw {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
            let o = offset as usize;
            self.data[o..o + buf.len()].copy_from_slice(buf);
            Ok(())
        }
    }

    #[test]
    fn the_least_recently_used_block_goes_first() {
        let data = pattern((MAX_BLOCKS + 1) * (1 << 16));
        let mut dev = CachingBlockDevice::new(CountedMem {
            data,
            reads: 0,
            bytes: 0,
        });
        let mut b = [0u8; 8];
        for idx in 0..MAX_BLOCKS as u64 {
            dev.read_at(idx << 16, &mut b).unwrap();
        }
        // Block 0 is used again, so block 1 is now the oldest.
        dev.read_at(0, &mut b).unwrap();
        dev.read_at((MAX_BLOCKS as u64) << 16, &mut b).unwrap();
        assert!(dev.blocks.contains_key(&0), "the block used again stays");
        assert!(!dev.blocks.contains_key(&1), "the oldest block goes");
        assert_eq!(dev.blocks.len(), MAX_BLOCKS);
    }

    #[test]
    fn rejects_out_of_range() {
        let mut dev = CachingBlockDevice::new(CountedMem {
            data: pattern(4096),
            reads: 0,
            bytes: 0,
        });
        let mut b = [0u8; 64];
        assert!(matches!(
            dev.read_at(4090, &mut b),
            Err(BlockError::OutOfRange { .. })
        ));
    }
}
