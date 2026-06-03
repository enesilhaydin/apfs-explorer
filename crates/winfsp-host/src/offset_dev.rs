//! `OffsetBlockDevice` - present a sub-range of a device as if it began
//! at byte 0, so `apfs_core::Container::open` sees the APFS container
//! directly regardless of the GPT partition offset.

use apfs_core::block_device::{BlockDevice, BlockError, WritableBlockDevice};

pub struct OffsetBlockDevice<D: BlockDevice> {
    inner: D,
    base: u64,
    len: u64,
}

impl<D: BlockDevice> OffsetBlockDevice<D> {
    /// Wrap `inner` exposing `[base, base+len)` as `[0, len)`.
    pub fn new(inner: D, base: u64, len: u64) -> Self {
        Self { inner, base, len }
    }
}

impl<D: BlockDevice> BlockDevice for OffsetBlockDevice<D> {
    fn size(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let blen = buf.len() as u64;
        let end = offset.checked_add(blen).ok_or(BlockError::OutOfRange {
            offset,
            len: blen,
            size: self.len,
        })?;
        if end > self.len {
            return Err(BlockError::OutOfRange {
                offset,
                len: blen,
                size: self.len,
            });
        }
        let abs = self
            .base
            .checked_add(offset)
            .ok_or(BlockError::OutOfRange {
                offset,
                len: blen,
                size: self.len,
            })?;
        self.inner.read_at(abs, buf)
    }
}

impl<D: WritableBlockDevice> WritableBlockDevice for OffsetBlockDevice<D> {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        let blen = buf.len() as u64;
        let end = offset.checked_add(blen).ok_or(BlockError::OutOfRange {
            offset,
            len: blen,
            size: self.len,
        })?;
        if end > self.len {
            return Err(BlockError::OutOfRange {
                offset,
                len: blen,
                size: self.len,
            });
        }
        let abs = self
            .base
            .checked_add(offset)
            .ok_or(BlockError::OutOfRange {
                offset,
                len: blen,
                size: self.len,
            })?;
        self.inner.write_at(abs, buf)
    }

    /// Delegate the COW-1 durability barrier to the inner device. The window
    /// does not buffer, so durability is the inner device's responsibility -
    /// without this delegation the trait no-op default would swallow the
    /// barrier before it reached `PhysicalDrive::flush_data`.
    fn flush_data(&mut self) -> Result<(), BlockError> {
        self.inner.flush_data()
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
pub mod tests {
    use super::*;

    pub struct MemDevice {
        pub data: Vec<u8>,
    }
    impl BlockDevice for MemDevice {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
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

    #[test]
    fn rebases_reads_to_base() {
        let mut data = vec![0u8; 4096];
        data[1000..1004].copy_from_slice(b"APFS");
        let mut dev = OffsetBlockDevice::new(MemDevice { data }, 1000, 100);
        assert_eq!(dev.size(), 100);
        let mut b = [0u8; 4];
        dev.read_at(0, &mut b).unwrap();
        assert_eq!(&b, b"APFS");
    }

    #[test]
    fn read_past_logical_end_errors() {
        let dev_data = MemDevice {
            data: vec![0u8; 4096],
        };
        let mut dev = OffsetBlockDevice::new(dev_data, 1000, 16);
        let mut b = [0u8; 32];
        assert!(matches!(
            dev.read_at(0, &mut b),
            Err(BlockError::OutOfRange { .. })
        ));
    }
}
