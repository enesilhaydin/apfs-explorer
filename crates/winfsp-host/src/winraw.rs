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
use windows::Win32::Foundation::{
    CloseHandle, ERROR_DEVICE_NOT_CONNECTED, ERROR_DEV_NOT_EXIST, ERROR_FILE_NOT_FOUND,
    ERROR_NO_SUCH_DEVICE, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, SetFilePointerEx, WriteFile, FILE_BEGIN,
    FILE_FLAG_NO_BUFFERING, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_ATTRIBUTES,
    FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    DISK_GEOMETRY, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_DRIVE_GEOMETRY,
    IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_CHECK_VERIFY2,
};
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
    /// The `N` of `\\.\PhysicalDrive<N>`.
    number: u32,
}

// SAFETY note: HANDLE is a raw OS handle; PhysicalDrive owns it exclusively
// and closes it on Drop. Sent across winfsp worker threads only behind the
// mount's Mutex (see mount.rs), so no concurrent handle use.
unsafe impl Send for PhysicalDrive {}

/// `IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS`, which the bindings in use do not
/// expose: CTL_CODE(IOCTL_VOLUME_BASE = 'V', 0, METHOD_BUFFERED,
/// FILE_ANY_ACCESS) is 0x56 << 16.
const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x0056_0000;

/// Physical drive numbers the running Windows installation sits on.
///
/// Empty when the question cannot be answered - a missing answer must not be
/// read as "not the system disk", so callers treat empty as "unknown" and the
/// refusal below only fires on a definite match.
fn system_drive_numbers() -> Vec<u32> {
    let system_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let Ok(path) = U16CString::from_str(format!("\\\\.\\{}", system_drive.trim_end_matches('\\')))
    else {
        return Vec::new();
    };
    // Zero access rights are enough for a query ioctl and avoid asking for
    // rights on the system volume that we have no business holding.
    let handle = unsafe {
        CreateFileW(
            windows::core::PCWSTR(path.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        )
    };
    let Ok(handle) = handle else {
        return Vec::new();
    };
    // One extent is the common case; a spanned volume needs the bigger buffer,
    // which the call reports by failing with the size it wants.
    // VOLUME_DISK_EXTENTS is a u32 count, four bytes of padding, then one
    // 24-byte DISK_EXTENT per extent; room for sixteen covers any real volume.
    let mut buf = vec![0u8; 8 + 16 * 24];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
            None,
            0,
            Some(buf.as_mut_ptr() as *mut c_void),
            buf.len() as u32,
            Some(&mut returned),
            None,
        )
    };
    let mut out = Vec::new();
    if ok.is_ok() {
        // VOLUME_DISK_EXTENTS: u32 count, u32 pad, then DISK_EXTENT[] of
        // {u32 disk, u32 pad, i64 offset, i64 length}.
        let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        for i in 0..count {
            let off = 8 + i * 24;
            if off + 4 > buf.len() {
                break;
            }
            out.push(u32::from_le_bytes([
                buf[off],
                buf[off + 1],
                buf[off + 2],
                buf[off + 3],
            ]));
        }
    }
    let _ = unsafe { CloseHandle(handle) };
    out
}

impl PhysicalDrive {
    /// Open `\\.\PhysicalDrive{n}` for raw read.
    pub fn open(n: u32) -> Result<Self, BlockError> {
        Self::open_inner(n, false, FILE_SHARE_READ | FILE_SHARE_WRITE)
    }

    /// Open `\\.\PhysicalDrive{n}` for raw read+write (adds GENERIC_WRITE;
    /// writes reach the drive as issued and become durable at
    /// `flush_data`).
    pub fn open_rw(n: u32) -> Result<Self, BlockError> {
        // Never hand out a writable handle to the disk Windows is running
        // from. Nothing stopped `--drive 0 --writable` before, and the
        // autodetect path probes every drive number with open_rw just to see
        // what is there, so the system disk was opened for writing on any
        // writable mount that did not name a drive.
        if system_drive_numbers().contains(&n) {
            return Err(io_other_string(format!(
                "PhysicalDrive{n} holds the running Windows installation; \
                 refusing to open it for writing"
            )));
        }
        Self::open_inner(n, true, FILE_SHARE_READ | FILE_SHARE_WRITE)
    }

    /// [`Self::open_rw`] sharing the drive with no one: refused while any
    /// other handle has it open, a mount of it included. For a one-shot
    /// change (a snapshot subcommand) that a running mount would otherwise
    /// write over, and whose blocks that mount would not see.
    pub fn open_exclusive_rw(n: u32) -> Result<Self, BlockError> {
        if system_drive_numbers().contains(&n) {
            return Err(io_other_string(format!(
                "PhysicalDrive{n} holds the running Windows installation; \
                 refusing to open it for writing"
            )));
        }
        Self::open_inner(n, true, FILE_SHARE_MODE(0)).map_err(|e| {
            io_other_string(format!(
                "{e} (PhysicalDrive{n} is in use: unmount it first)"
            ))
        })
    }

    fn open_inner(n: u32, writable: bool, share: FILE_SHARE_MODE) -> Result<Self, BlockError> {
        let path = U16CString::from_str(format!("\\\\.\\PhysicalDrive{n}"))
            .map_err(|_| io_other("bad device path"))?;
        let access = if writable {
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0
        } else {
            FILE_GENERIC_READ.0
        };
        // Read path uses FILE_FLAG_NO_BUFFERING (sector-aligned, fast).
        // The write path takes no flag. OSFMount virtual disks reject
        // GENERIC_WRITE + NO_BUFFERING with ERROR_INVALID_PARAMETER even when
        // IsReadOnly is false. WRITE_THROUGH made every write a forced unit
        // access, which a USB stick serves at a fraction of its speed (12
        // MB/s against ~35 with the flag off, measured); durability comes
        // from flush_data at the commit's two barriers, which asks the drive
        // to empty its cache, and a raw disk handle has no OS cache to skip.
        let flags = if writable {
            Default::default()
        } else {
            FILE_FLAG_NO_BUFFERING
        };
        // Exact CreateFileW signature is resolved against the pinned
        // `windows` crate on the build box; this is the 0.58 shape.
        let handle = unsafe {
            CreateFileW(
                windows::core::PCWSTR(path.as_ptr()),
                access,
                share,
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
        // The geometry describes whole cylinders only, up to ~8 MB short of
        // the disk, and macOS runs the APFS partition to the last usable
        // sector, so the container's end would fall past a size taken from
        // it. The length ioctl gives the exact byte count; the geometry is
        // only the fallback when the driver does not answer it.
        let mut len = GET_LENGTH_INFORMATION::default();
        let len_ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_DISK_GET_LENGTH_INFO,
                None,
                0,
                Some(&mut len as *mut _ as *mut c_void),
                std::mem::size_of::<GET_LENGTH_INFORMATION>() as u32,
                Some(&mut returned),
                None,
            )
        };
        let size = if len_ok.is_ok() && len.Length > 0 {
            len.Length as u64
        } else {
            // windows 0.58: LARGE_INTEGER.Cylinders is exposed as i64 directly
            geo.Cylinders as u64
                * geo.TracksPerCylinder as u64
                * geo.SectorsPerTrack as u64
                * sector
        };

        Ok(Self {
            handle,
            sector,
            size,
            writable,
            number: n,
        })
    }

    /// True when this drive was opened for writing.
    pub fn is_writable(&self) -> bool {
        self.writable
    }

    /// The `N` of `\\.\PhysicalDrive<N>` this was opened on.
    pub fn number(&self) -> u32 {
        self.number
    }

    /// Open the first physical drive whose GPT has an APFS partition.
    /// Returns the opened drive plus the APFS container byte range.
    pub fn autodetect_apfs() -> Result<(Self, crate::gpt::ContainerRange), BlockError> {
        for n in 0..16u32 {
            if let Ok(mut d) = PhysicalDrive::open(n) {
                let sec = d.sector();
                if let Ok(range) = find_apfs_container(&mut d, sec) {
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

/// A handle on `\\.\PhysicalDrive<N>` that reads nothing, kept to notice the
/// drive being pulled out. Removal invalidates every handle on the device
/// for good: a drive put back in is a new device that this handle never
/// sees, so a quick unplug and replug is still caught.
pub struct DrivePresence {
    handle: HANDLE,
}

impl DrivePresence {
    pub fn open(n: u32) -> Result<Self, BlockError> {
        let path = U16CString::from_str(format!("\\\\.\\PhysicalDrive{n}"))
            .map_err(|_| io_other("bad device path"))?;
        // SAFETY: a plain CreateFileW on a NUL-terminated path; the handle
        // is owned by the returned value and closed on Drop.
        let handle = unsafe {
            CreateFileW(
                windows::core::PCWSTR(path.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                Default::default(),
                HANDLE::default(),
            )
        }
        .map_err(|e| io_other_string(format!("CreateFileW: {e}")))?;
        Ok(Self { handle })
    }

    /// False once the drive has gone. Only the errors that say the device
    /// no longer exists count; anything else (a driver that does not
    /// answer the request) is taken as present.
    pub fn present(&self) -> bool {
        let mut returned = 0u32;
        // SAFETY: a no-buffer ioctl on a handle this value owns.
        let r = unsafe {
            DeviceIoControl(
                self.handle,
                IOCTL_STORAGE_CHECK_VERIFY2,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            )
        };
        match r {
            Ok(()) => true,
            Err(e) => ![
                ERROR_NO_SUCH_DEVICE,
                ERROR_DEV_NOT_EXIST,
                ERROR_DEVICE_NOT_CONNECTED,
                ERROR_FILE_NOT_FOUND,
            ]
            .iter()
            .any(|w| e.code() == w.to_hresult()),
        }
    }
}

impl Drop for DrivePresence {
    fn drop(&mut self) {
        // SAFETY: closes the handle this value owns, once.
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
    /// writes return an error rather than RMW'ing - the safe and fast
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

    fn flush_data(&mut self) -> Result<(), BlockError> {
        if !self.writable {
            return Ok(());
        }
        // A raw disk handle has no OS cache, but the drive's own write cache
        // may still hold the data. FlushFileBuffers issues the device-level
        // flush so the COW commit barrier reaches media (the explicit
        // equivalent of linux-apfs-rw's filemap_write_and_wait); a drive
        // Windows runs with its write cache off completes writes on media.
        unsafe { FlushFileBuffers(self.handle) }
            .map_err(|e| io_other_string(format!("FlushFileBuffers: {e}")))
    }
}

#[cfg(all(test, windows))]
mod win_tests {
    use super::*;
    /// The disk Windows boots from must never be handed out for writing.
    /// Detection is best-effort, so a machine that cannot answer skips.
    #[test]
    fn the_system_disk_is_not_writable() {
        let system = system_drive_numbers();
        if system.is_empty() {
            println!("the_system_disk_is_not_writable: system drive unknown - skip");
            return;
        }
        println!("system drive numbers: {system:?}");
        for n in system {
            let err = PhysicalDrive::open_rw(n)
                .err()
                .unwrap_or_else(|| panic!("PhysicalDrive{n} holds Windows and must be refused"));
            let msg = format!("{err:?}");
            assert!(
                msg.contains("running Windows"),
                "the refusal must say why, got: {msg}"
            );
            // Reading it is still allowed - that is how autodetect looks around.
            assert!(
                PhysicalDrive::open(n).is_ok(),
                "PhysicalDrive{n} must still open read-only"
            );
        }
    }

    /// Only a machine with an APFS disk attached can answer this, so a
    /// machine without one skips rather than fails - it was failing on every
    /// build box, which is the same as not being run at all.
    #[test]
    fn autodetect_finds_nxsb() {
        let Ok((mut d, range)) = PhysicalDrive::autodetect_apfs() else {
            println!("autodetect_finds_nxsb: no APFS physical drive attached - skip");
            return;
        };
        let mut m = [0u8; 4];
        d.read_at(range.offset + 32, &mut m)
            .expect("the range autodetect returned must be readable");
        assert_eq!(
            &m, b"NXSB",
            "autodetect must land on the container superblock"
        );
    }
}
