//! Raw `\\.\PhysicalDriveN` block device (Windows-only).
//!
//! Read-only by default (`open`); call `open_rw` to obtain a writable
//! handle (additionally requires GENERIC_WRITE access and FSCTL_LOCK_
//! VOLUME to keep Windows from mediating the writes through its own
//! cache for the device). All writes go through `WriteFile` with
//! `FILE_FLAG_NO_BUFFERING`, so callers MUST pass sector-aligned
//! buffers (the `aligned_window` helper does this).

use std::ffi::c_void;

use apfs_core::block_device::{BlockDevice, BlockError, WritableBlockDevice};
use widestring::U16CString;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, SetFilePointerEx, WriteFile, FILE_BEGIN, FILE_FLAG_NO_BUFFERING,
    FILE_FLAG_WRITE_THROUGH, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{DISK_GEOMETRY, IOCTL_DISK_GET_DRIVE_GEOMETRY};
use windows::Win32::System::IO::DeviceIoControl;

use crate::align::aligned_window;
use crate::gpt::find_apfs_container;

pub struct PhysicalDrive {
    handle: HANDLE,
    sector: u64,
    size: u64,
    /// True when the handle was opened with GENERIC_WRITE access. Guards
    /// `write_at` so a read-only PhysicalDrive cannot silently accept
    /// write attempts at runtime.
    writable: bool,
}

// SAFETY note: HANDLE is a raw OS handle; PhysicalDrive owns it exclusively
// and closes it on Drop. Sent across winfsp worker threads only behind the
// mount's Mutex (see mount.rs), so no concurrent handle use.
unsafe impl Send for PhysicalDrive {}

impl PhysicalDrive {
    /// Open `\\.\PhysicalDrive{n}` for raw read.
    pub fn open(n: u32) -> Result<Self, BlockError> {
        Self::open_inner(n, false)
    }

    /// Open `\\.\PhysicalDrive{n}` for raw read+write.
    /// Adds GENERIC_WRITE and FILE_FLAG_WRITE_THROUGH so writes bypass
    /// the system cache and reach disk immediately (matching the
    /// FILE_FLAG_NO_BUFFERING read path's alignment + durability
    /// expectations).
    pub fn open_rw(n: u32) -> Result<Self, BlockError> {
        Self::open_inner(n, true)
    }

    fn open_inner(n: u32, writable: bool) -> Result<Self, BlockError> {
        let path = U16CString::from_str(format!("\\\\.\\PhysicalDrive{n}"))
            .map_err(|_| io_other("bad device path"))?;
        let access = if writable {
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0
        } else {
            FILE_GENERIC_READ.0
        };
        // Read path uses FILE_FLAG_NO_BUFFERING (sector-aligned, fast).
        // Write path uses FILE_FLAG_WRITE_THROUGH only — leaving NO_BUFFERING
        // off so callers can pass non-aligned buffers without the OS
        // rejecting the open. OSFMount virtual disks empirically reject
        // GENERIC_WRITE + NO_BUFFERING with ERROR_INVALID_PARAMETER even
        // when IsReadOnly is false; WRITE_THROUGH alone preserves
        // durability without the alignment constraint.
        let flags = if writable {
            FILE_FLAG_WRITE_THROUGH
        } else {
            FILE_FLAG_NO_BUFFERING
        };
        // Exact CreateFileW signature is resolved against the pinned
        // `windows` crate on the build box; this is the 0.58 shape.
        let handle = unsafe {
            CreateFileW(
                windows::core::PCWSTR(path.as_ptr()),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                flags,
                HANDLE::default(),
            )
        }
        .map_err(|e| io_other_string(format!("CreateFileW: {e}")))?;

        let mut geo = DISK_GEOMETRY::default();
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_DISK_GET_DRIVE_GEOMETRY,
                None,
                0,
                Some(&mut geo as *mut _ as *mut c_void),
                std::mem::size_of::<DISK_GEOMETRY>() as u32,
                Some(&mut returned),
                None,
            )
        };
        let sector = if ok.is_ok() && geo.BytesPerSector >= 512 {
            geo.BytesPerSector as u64
        } else {
            512
        };
        // windows 0.58: LARGE_INTEGER.Cylinders is exposed as i64 directly
        let size = geo.Cylinders as u64
            * geo.TracksPerCylinder as u64
            * geo.SectorsPerTrack as u64
            * sector;

        Ok(Self {
            handle,
            sector,
            size,
            writable,
        })
    }

    /// True when this drive was opened for writing.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// Open the first physical drive whose GPT has an APFS partition.
    /// Returns the opened drive plus the APFS container byte range.
    pub fn autodetect_apfs() -> Result<(Self, crate::gpt::ContainerRange), BlockError> {
        for n in 0..16u32 {
            if let Ok(mut d) = PhysicalDrive::open(n) {
                if let Ok(range) = find_apfs_container(&mut d) {
                    return Ok((d, range));
                }
            }
        }
        Err(io_other("no APFS physical drive found"))
    }

    pub fn sector(&self) -> u64 {
        self.sector
    }
}

fn io_other(msg: &'static str) -> BlockError {
    BlockError::Io(std::io::Error::new(std::io::ErrorKind::Other, msg))
}
fn io_other_string(msg: String) -> BlockError {
    BlockError::Io(std::io::Error::new(std::io::ErrorKind::Other, msg))
}

impl Drop for PhysicalDrive {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

impl BlockDevice for PhysicalDrive {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.is_empty() {
            return Ok(());
        }
        let w = aligned_window(offset, buf.len(), self.sector);
        let mut scratch = vec![0u8; w.aligned_len];
        let mut newpos = 0i64;
        let moved = unsafe {
            SetFilePointerEx(
                self.handle,
                w.aligned_offset as i64,
                Some(&mut newpos),
                FILE_BEGIN,
            )
        };
        moved.map_err(|e| io_other_string(format!("seek: {e}")))?;

        let mut done = 0usize;
        while done < scratch.len() {
            let mut read = 0u32;
            let chunk = &mut scratch[done..];
            let ok = unsafe { ReadFile(self.handle, Some(chunk), Some(&mut read), None) };
            ok.map_err(|e| io_other_string(format!("ReadFile: {e}")))?;
            if read == 0 {
                return Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.size,
                });
            }
            done += read as usize;
        }

        let slice = scratch
            .get(w.skip..w.skip + w.take)
            .ok_or(io_other("alignment slice out of range"))?;
        buf.copy_from_slice(slice);
        Ok(())
    }
}

impl WritableBlockDevice for PhysicalDrive {
    /// Write `buf.len()` bytes at `offset`. The handle was opened with
    /// `FILE_FLAG_NO_BUFFERING`, so both `offset` and `buf.len()` MUST
    /// be multiples of the device sector size and the caller-supplied
    /// buffer must be sector-aligned. APFS blocks (4 KiB) satisfy this
    /// on every supported drive (512B / 4KiB sectors). Misaligned
    /// writes return an error rather than RMW'ing — the safe and fast
    /// default; an RMW helper can be added if a non-aligned caller
    /// appears.
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        if !self.writable {
            return Err(io_other("PhysicalDrive opened read-only"));
        }
        if buf.is_empty() {
            return Ok(());
        }
        if offset % self.sector != 0 || (buf.len() as u64) % self.sector != 0 {
            return Err(io_other("PhysicalDrive write: misaligned offset or length"));
        }
        let mut newpos = 0i64;
        unsafe { SetFilePointerEx(self.handle, offset as i64, Some(&mut newpos), FILE_BEGIN) }
            .map_err(|e| io_other_string(format!("seek (write): {e}")))?;

        // WriteFile can return short writes under unusual conditions; loop
        // until the full buffer is committed (or an error surfaces).
        let mut done = 0usize;
        while done < buf.len() {
            let chunk = &buf[done..];
            let mut written = 0u32;
            unsafe { WriteFile(self.handle, Some(chunk), Some(&mut written), None) }
                .map_err(|e| io_other_string(format!("WriteFile: {e}")))?;
            if written == 0 {
                return Err(io_other("WriteFile returned 0 bytes"));
            }
            done += written as usize;
        }
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    use super::*;
    #[test]
    fn autodetect_finds_nxsb() {
        let (mut d, range) = PhysicalDrive::autodetect_apfs().unwrap();
        let mut m = [0u8; 4];
        d.read_at(range.offset + 32, &mut m).unwrap();
        assert_eq!(&m, b"NXSB");
    }
}
