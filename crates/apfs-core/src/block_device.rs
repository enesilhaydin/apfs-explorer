use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BlockError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("read out of range: offset {offset} len {len} exceeds device size {size}")]
    OutOfRange { offset: u64, len: u64, size: u64 },
}

/// Block device abstraction. Prod = raw `\\.\PhysicalDriveN`; test = file image.
/// The parser does not know the device kind (testability).
pub trait BlockDevice {
    fn size(&self) -> u64;
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError>;
}

/// Extension of `BlockDevice` for writable devices.
///
/// Implementors MUST NOT corrupt the device on partial writes - callers are
/// responsible for writing complete, checksum-valid blocks. `write_at` writes
/// exactly `buf.len()` bytes starting at `offset`; offset must be block-aligned
/// (enforcement is caller's responsibility for performance).
///
/// `#![forbid(unsafe_code)]` - no unsafe in this crate.
pub trait WritableBlockDevice: BlockDevice {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError>;

    /// Drop any internal read caches. A read-only cache layer must override
    /// this; raw devices keep the default no-op. Callers invoke it between
    /// a committed transaction and a fresh container open to guarantee the
    /// next read observes durable on-disk state (defense-in-depth: per-write
    /// invalidation in `write_at` should already suffice, but a full drop
    /// removes any residual aliasing across an FS reopen).
    fn invalidate_read_cache(&mut self) {}

    /// Force all previously-written blocks to durable media. The COW
    /// transaction commit calls this as a write barrier: once *before* writing
    /// the NX superblock (so every block the new checkpoint references is
    /// durable first) and once *after* (so the commit point itself is durable
    /// before any later step reuses freed space). This mirrors linux-apfs-rw
    /// `apfs_checkpoint_end`, which brackets the superblock write with
    /// `filemap_write_and_wait`.
    ///
    /// Default is a no-op: in-memory test devices need no flush. File- and
    /// raw-device implementors MUST override - neither the OS page cache nor a
    /// disk's write cache guarantees ordering or durability without an explicit
    /// flush, so without this a torn write can leave the container referencing
    /// blocks whose data never reached the platter.
    fn flush_data(&mut self) -> Result<(), BlockError> {
        Ok(())
    }
}

/// File-backed `BlockDevice` - for test fixtures and file images (read-only).
pub struct FileBlockDevice {
    file: File,
    size: u64,
}

impl FileBlockDevice {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, BlockError> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }

    /// Open with an explicitly supplied size. Raw block-device nodes report
    /// `metadata().len() == 0` (macOS `/dev/diskNsM`, Windows
    /// `\\.\PhysicalDriveN`), so the caller obtains the true size from the
    /// platform-appropriate source (e.g. macOS `diskutil`, Windows IOCTL)
    /// and passes it here. No unsafe - the size is just stored.
    pub fn open_with_size<P: AsRef<Path>>(path: P, size: u64) -> Result<Self, BlockError> {
        let file = File::open(path)?;
        Ok(Self { file, size })
    }
}

/// File-backed writable `BlockDevice` - for scratch images and write tests.
pub struct FileBlockDeviceRw {
    file: File,
    size: u64,
}

impl FileBlockDeviceRw {
    /// Open an existing image file for read-write access.
    pub fn open_rw<P: AsRef<Path>>(path: P) -> Result<Self, BlockError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { file, size })
    }

    /// Open read-write with an explicitly supplied size - for raw
    /// block-device nodes whose `metadata().len()` is 0 (see
    /// [`FileBlockDevice::open_with_size`]). No unsafe.
    pub fn open_rw_with_size<P: AsRef<Path>>(path: P, size: u64) -> Result<Self, BlockError> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { file, size })
    }
}

impl BlockDevice for FileBlockDeviceRw {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let len = buf.len() as u64;
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
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)?;
        Ok(())
    }
}

impl WritableBlockDevice for FileBlockDeviceRw {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        let len = buf.len() as u64;
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
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(buf)?;
        Ok(())
    }

    fn flush_data(&mut self) -> Result<(), BlockError> {
        // `sync_data` = fdatasync (Linux/macOS) / FlushFileBuffers (Windows).
        // Data-only sync is sufficient: the APFS container size is fixed, so
        // the host file's metadata (length/mtime) is never read back.
        self.file.sync_data().map_err(BlockError::Io)
    }
}

/// Writable offset block device - presents a sub-range `[base, base+len)` as `[0, len)`.
///
/// Wraps an inner `WritableBlockDevice`. Used by the scratch integration test to
/// address the APFS container inside a GPT image.
pub struct WritableOffsetBlockDevice<D: WritableBlockDevice> {
    inner: D,
    base: u64,
    len: u64,
}

impl<D: WritableBlockDevice> WritableOffsetBlockDevice<D> {
    /// Wrap `inner`, exposing `[base, base+len)` as `[0, len)`.
    pub const fn new(inner: D, base: u64, len: u64) -> Self {
        Self { inner, base, len }
    }
}

impl<D: WritableBlockDevice> BlockDevice for WritableOffsetBlockDevice<D> {
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

impl<D: WritableBlockDevice> WritableBlockDevice for WritableOffsetBlockDevice<D> {
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

    fn flush_data(&mut self) -> Result<(), BlockError> {
        // Delegate to the wrapped device - the offset window does not buffer,
        // so durability is entirely the inner device's responsibility.
        self.inner.flush_data()
    }
}

impl BlockDevice for FileBlockDevice {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let len = buf.len() as u64;
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
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(buf)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_bytes_at_offset() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"HELLO-APFS-WORLD").unwrap();
        let mut dev = FileBlockDevice::open(tmp.path()).unwrap();
        assert_eq!(dev.size(), 16);
        let mut buf = [0u8; 4];
        dev.read_at(6, &mut buf).unwrap();
        assert_eq!(&buf, b"APFS");
    }

    #[test]
    fn read_past_end_errors() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"123").unwrap();
        let mut dev = FileBlockDevice::open(tmp.path()).unwrap();
        let mut buf = [0u8; 8];
        let err = dev.read_at(0, &mut buf).unwrap_err();
        assert!(matches!(err, BlockError::OutOfRange { .. }));
    }

    // --- WritableOffsetBlockDevice tests ---

    /// Minimal in-memory writable block device for unit testing.
    struct VecDev(Vec<u8>);

    impl VecDev {
        fn new(size: usize) -> Self {
            Self(vec![0u8; size])
        }
    }

    impl BlockDevice for VecDev {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let end = offset as usize + buf.len();
            if end > self.0.len() {
                return Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.0.len() as u64,
                });
            }
            buf.copy_from_slice(&self.0[offset as usize..end]);
            Ok(())
        }
    }

    impl WritableBlockDevice for VecDev {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
            let end = offset as usize + buf.len();
            if end > self.0.len() {
                return Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.0.len() as u64,
                });
            }
            self.0[offset as usize..end].copy_from_slice(buf);
            Ok(())
        }
    }

    #[test]
    fn offset_dev_translates_reads() {
        // Inner device: 32 bytes of known data.
        let mut inner = VecDev::new(32);
        inner.0[8..12].copy_from_slice(b"APFS");
        // Expose [8, 8+16) as [0, 16).
        let mut dev = WritableOffsetBlockDevice::new(inner, 8, 16);
        assert_eq!(dev.size(), 16);
        // Reading offset 0 in the window reads inner[8..12].
        let mut buf = [0u8; 4];
        dev.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf, b"APFS");
    }

    #[test]
    fn offset_dev_translates_writes() {
        let inner = VecDev::new(32);
        let mut dev = WritableOffsetBlockDevice::new(inner, 8, 16);
        dev.write_at(4, b"TEST").unwrap();
        // inner[8+4..8+8] == b"TEST"
        let mut buf = [0u8; 4];
        dev.read_at(4, &mut buf).unwrap();
        assert_eq!(&buf, b"TEST");
    }

    #[test]
    fn offset_dev_read_oob_errors() {
        let inner = VecDev::new(32);
        let mut dev = WritableOffsetBlockDevice::new(inner, 8, 16);
        // Attempt to read past the window end (size=16, read 4 bytes at offset 14).
        let mut buf = [0u8; 4];
        let err = dev.read_at(14, &mut buf).unwrap_err();
        assert!(
            matches!(err, BlockError::OutOfRange { .. }),
            "read past window must be OutOfRange"
        );
    }

    #[test]
    fn offset_dev_write_oob_errors() {
        let inner = VecDev::new(32);
        let mut dev = WritableOffsetBlockDevice::new(inner, 8, 16);
        // Attempt to write past the window end.
        let err = dev.write_at(15, b"XX").unwrap_err();
        assert!(
            matches!(err, BlockError::OutOfRange { .. }),
            "write past window must be OutOfRange"
        );
    }

    #[test]
    fn offset_dev_size_reports_window() {
        let inner = VecDev::new(64);
        let dev = WritableOffsetBlockDevice::new(inner, 16, 24);
        assert_eq!(
            dev.size(),
            24,
            "size() must report window length not inner size"
        );
    }
}
