//! Sector-alignment window math for raw physical-device reads.
//!
//! A raw `\\.\PhysicalDriveN` handle only accepts reads whose byte offset
//! and length are multiples of the device sector size. [`aligned_window`]
//! widens an arbitrary `(offset, len)` request to the enclosing aligned
//! window and reports where the requested bytes sit inside that window.

/// The aligned read window enclosing a requested byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Sector-aligned start offset to read from.
    pub aligned_offset: u64,
    /// Sector-multiple number of bytes to read.
    pub aligned_len: usize,
    /// Bytes to skip at the front of the window buffer.
    pub skip: usize,
    /// Bytes to copy out after `skip` (== requested len).
    pub take: usize,
}

/// Compute the aligned window for reading `len` bytes at `offset` from a
/// device with the given `sector` size. `sector` must be a power of two.
/// `len == 0` yields an empty zero-length window at the aligned offset.
pub fn aligned_window(offset: u64, len: usize, sector: u64) -> Window {
    let mask = sector - 1;
    let aligned_offset = offset & !mask;
    let skip = (offset - aligned_offset) as usize;
    let end = offset + len as u64;
    let aligned_end = (end + mask) & !mask;
    let aligned_len = (aligned_end - aligned_offset) as usize;
    Window {
        aligned_offset,
        aligned_len,
        skip,
        take: len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_aligned_full_sector() {
        let w = aligned_window(0, 512, 512);
        assert_eq!(
            w,
            Window {
                aligned_offset: 0,
                aligned_len: 512,
                skip: 0,
                take: 512
            }
        );
    }

    #[test]
    fn unaligned_offset_and_len_widens_both_ends() {
        // Request 100 bytes at offset 600 with 512-byte sectors:
        // window must cover [512, 1024) -> 512 bytes, skip 88, take 100.
        let w = aligned_window(600, 100, 512);
        assert_eq!(
            w,
            Window {
                aligned_offset: 512,
                aligned_len: 512,
                skip: 88,
                take: 100
            }
        );
    }

    #[test]
    fn spans_multiple_sectors() {
        // 5000 bytes at offset 600 -> end 5600 -> aligned_end 5632,
        // aligned_offset 512 -> aligned_len = 5632 - 512 = 5120.
        let w = aligned_window(600, 5000, 512);
        assert_eq!(w.aligned_offset, 512);
        assert_eq!(w.aligned_len, 5120);
        assert_eq!(w.skip, 88);
        assert_eq!(w.take, 5000);
        assert_eq!(w.aligned_len % 512, 0);
        assert!(w.aligned_offset + w.aligned_len as u64 >= 600 + 5000);
    }

    #[test]
    fn zero_len_is_empty_window() {
        // offset 700 -> aligned_offset 512; end 700 -> aligned_end 1024;
        // aligned_len = 1024 - 512 = 512. take=0, skip=188.
        let w = aligned_window(700, 0, 512);
        assert_eq!(w.aligned_offset, 512);
        assert_eq!(w.aligned_len, 512);
        assert_eq!(w.skip, 188);
        assert_eq!(w.take, 0);
    }

    #[test]
    fn large_real_world_offset_4k_sector() {
        // Container at 209_735_680, read 4096 at container+1000.
        let w = aligned_window(209_735_680 + 1000, 4096, 4096);
        assert_eq!(w.aligned_offset % 4096, 0);
        assert_eq!(w.aligned_len % 4096, 0);
        assert!(w.aligned_offset <= 209_735_680 + 1000);
        assert!(w.aligned_offset + w.aligned_len as u64 >= 209_735_680 + 1000 + 4096);
        assert_eq!(w.take, 4096);
    }
}
