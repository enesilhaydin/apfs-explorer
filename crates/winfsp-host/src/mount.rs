//! WinFsp read-only host delegating to `apfs::FsView` (Windows-only).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use apfs::FsView;
use apfs_core::block_device::WritableBlockDevice;
use apfs_write::file as apfs_write_file;
use apfs_write::txn::{Transaction, TxnError};

use crate::batch::{BatchQueue, DeferredOp};
use widestring::U16CStr;
use winfsp::filesystem::{
    DirInfo,
    FileInfo,
    FileSecurity,
    FileSystemContext,
    OpenFileInfo,
    VolumeInfo,
    // winfsp 0.13: WideNameInfo trait gates set_name/append_to_buffer/finalize_buffer on DirInfo
    WideNameInfo,
};
// winfsp 0.13: FileSystemHost<T, S: OperationGuardStrategy = FineGuard>.
// FileSystemHost::new() returns FileSystemHost<T, FineGuard> (the default).
// Our Mutex<FsView> provides our own serialisation; FineGuard is fine here.
use winfsp::host::{FileSystemHost, FineGuard, VolumeParams};

use crate::cache::CachingBlockDevice;
use crate::offset_dev::OffsetBlockDevice;
use crate::winraw::PhysicalDrive;

// Device stack: raw drive -> rebase to container -> automatic read-ahead/LRU
// cache -> FsView. The cache turns the APFS traversal's many tiny random
// reads into few large sequential ones (essential on slow USB, neutral on
// fast SSD); fully automatic, no tuning knobs.
type Dev = CachingBlockDevice<OffsetBlockDevice<PhysicalDrive>>;

const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034u32 as i32;
const STATUS_OBJECT_NAME_INVALID: i32 = 0xC0000033u32 as i32;
const STATUS_NOT_IMPLEMENTED: i32 = 0xC0000002u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185u32 as i32;

/// Map a fallible mount step to a WinFsp IO error while surfacing the real
/// underlying cause to stderr. WinFsp only carries an opaque NTSTATUS, so
/// without this the genuine failure (bad device, no APFS, mount denied,
/// ...) would be swallowed — a diagnostic dead end and an error-swallowing
/// anti-pattern. `step` names the stage; `e` is the concrete error.
pub(crate) fn io_err<E: std::fmt::Debug>(step: &str) -> impl FnOnce(E) -> winfsp::FspError + '_ {
    move |e| {
        eprintln!("mount_apfs: {step} failed: {e:?}");
        winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR)
    }
}
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x02;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// Microsoft-assigned reparse tag for POSIX-style symbolic links
/// (`IO_REPARSE_TAG_SYMLINK`, winnt.h).
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// `SYMLINK_FLAG_RELATIVE` (winnt.h) — set on the reparse buffer when the
/// substitute name is a relative path rather than an absolute / NT object
/// path. APFS targets are POSIX strings and may be either; we infer by
/// inspecting the leading character.
const SYMLINK_FLAG_RELATIVE: u32 = 0x0000_0001;
/// POSIX S_IFLNK / S_IFMT bit pattern used to detect symbolic links in
/// the inode mode field surfaced by `apfs::Attr`.
const S_IFMT: u16 = 0o170000;
const S_IFLNK: u16 = 0o120000;
/// BSD UF_HIDDEN (`<sys/stat.h>`). APFS j_inode_val.bsd_flags carries this
/// flag when Finder hides the file; we round-trip it to Windows
/// FILE_ATTRIBUTE_HIDDEN.
const UF_HIDDEN: u32 = 0x0000_8000;
/// FILE_ATTRIBUTE_NORMAL — set on regular files in writable mounts so
/// Windows does NOT block DELETE access. Previously we set READONLY
/// unconditionally (legacy from the read-only mount era), which made
/// Windows reject set_delete on every Remove-Item before reaching our
/// FS. With NORMAL, Windows dispatches the delete protocol through.
#[allow(dead_code)]
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// Generic over the underlying device type so the same ApfsFs can host
/// either a `PhysicalDrive`-backed mount (real disk) or — once #45 lands
/// — a `FileBlockDeviceRw`-backed mount (--image). The trait bound is
/// `WritableBlockDevice` so the write callbacks (Step D) can call
/// `write_at` directly; a read-only mount still uses a writable-trait
/// device whose `write_at` returns an error at runtime.
///
/// The FsView is stored inside `Option` so a write callback can take the
/// device for one Transaction commit cycle and then reopen a fresh FsView
/// from the returned device (`FsView::into_dev` + `FsView::open`). The
/// option is `None` only briefly during the transaction; read callbacks
/// outside that window can safely `as_mut().expect(...)`.
pub struct ApfsFs<D: WritableBlockDevice + Send + 'static> {
    fs: Mutex<Option<FsView<D>>>,
    /// Deferred-op batch queue for amortising per-op commit cost (M8 #2).
    /// rename / set_basic_info / truncate_fast ops are enqueued here and
    /// flushed as one Transaction when the threshold is reached or on an
    /// explicit flush call (cleanup, unmount, reads that need consistency).
    batch: BatchQueue,
    /// Shared dirty flag: `true` when there are uncommitted batched ops.
    /// Updated after every enqueue and flush. The tray process reads the
    /// corresponding status file (written by the mount loop) instead of
    /// accessing this directly; this field drives that file.
    dirty: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Per-open handle: the normalized `/`-path plus a lazily-filled cache of
/// the file's fully-decompressed content. The volume is read-only, so the
/// content is immutable for the handle's lifetime; caching it makes a
/// sequential file read O(size) ONCE instead of O(size) per WinFsp read
/// chunk (Windows reads a file in many small chunks, each of which would
/// otherwise re-read+re-decompress the whole file from the device).
pub struct Handle {
    path: String,
    /// Read-side content cache (read-only field-of-use; M4b). Populated
    /// lazily on first Read so subsequent Read chunks for the same handle
    /// don't re-traverse extents + decompress.
    content: std::sync::Mutex<Option<std::sync::Arc<Vec<u8>>>>,
    /// Write-side dirty buffer. Step D: WinFsp Write callbacks are
    /// chunked (offset + buf), but apfs_write::write_file takes the
    /// whole content at once. We accumulate writes into this buffer and
    /// flush on Cleanup-non-delete via unlink + write_file (since
    /// apfs_write::write_file rejects existing names). `None` means the
    /// handle hasn't been written; `Some` (even empty) means dirty.
    write_buf: std::sync::Mutex<Option<Vec<u8>>>,
    /// FILE_DELETE_ON_CLOSE (create_options bit 0x1000) handles delete
    /// without an explicit set_delete dispatch. PowerShell Remove-Item
    /// -Recurse and DeleteFile from cmd both take this path. Captured at
    /// open time so cleanup can route to unlink even when the FspCleanupDelete
    /// flag is absent.
    delete_on_close: std::sync::atomic::AtomicBool,
}

fn win_to_unix(name: &U16CStr) -> String {
    let s = name.to_string_lossy();
    let s = s.replace('\\', "/");
    if s.is_empty() {
        "/".to_string()
    } else {
        s
    }
}

fn guard<T>(f: impl FnOnce() -> winfsp::Result<T>) -> winfsp::Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR)),
    }
}

impl<D: WritableBlockDevice + Send + 'static> ApfsFs<D> {
    pub fn new(fs: FsView<D>) -> Self {
        Self {
            fs: Mutex::new(Some(fs)),
            batch: BatchQueue::new(),
            dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Returns `true` when there are pending batched writes that have not yet
    /// been committed to disk. The tray supervisor polls this to drive the
    /// dirty/clean indicator in the tray menu header.
    pub fn is_dirty(&self) -> bool {
        self.batch.has_pending()
    }

    /// Returns a clone of the shared dirty flag. The mount loop in `main.rs`
    /// holds this arc and writes it to the status file on each poll tick so
    /// the tray process can display a dirty/clean indicator.
    pub fn dirty_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.dirty.clone()
    }

    /// Sync the dirty flag from the batch queue state. Called after every
    /// enqueue and after every flush so the arc stays accurate.
    fn sync_dirty_flag(&self) {
        self.dirty
            .store(self.batch.has_pending(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Resolve `(parent_ino, canonical_on_disk_name)` from a WinFsp-supplied
    /// path. `file_name` is preferred (typically uppercase from the OS);
    /// `fallback_path` is used when WinFsp supplies None (cleanup callback).
    /// Returns None on any lookup failure — callers treat as "nothing to do".
    fn resolve_parent_and_name(
        &self,
        file_name: Option<&U16CStr>,
        fallback_path: &str,
    ) -> Option<(u64, String)> {
        use unicode_normalization::UnicodeNormalization;
        let path = match file_name {
            Some(p) => win_to_unix(p),
            None => fallback_path.to_string(),
        };
        let (parent_path, supplied_name) = match path.rsplit_once('/') {
            Some((p, n)) if !n.is_empty() => (if p.is_empty() { "/" } else { p }, n.to_string()),
            _ => return None,
        };
        let mut g = self.fs.lock().ok()?;
        let fs = g.as_mut()?;
        // Windows volumes are advertised case-insensitive (VolumeParams
        // case_sensitive_search=false) so the kernel hands us paths in
        // whichever case the user typed (often UPPERCASE). The on-disk
        // APFS catalog stores names verbatim — typically the lowercase
        // form create_file/mkdir wrote. Try the path as-given first, then
        // fall back to a lowercase canonical form so case-insensitive
        // resolution actually works through every callback.
        let p_ino = fs
            .lookup(parent_path)
            .ok()
            .flatten()
            .or_else(|| fs.lookup(&parent_path.to_lowercase()).ok().flatten())?;
        let parent_canonical = if fs.lookup(parent_path).ok().flatten().is_some() {
            parent_path.to_string()
        } else {
            parent_path.to_lowercase()
        };
        let entries = fs.read_dir(&parent_canonical).ok()?;
        // Unicode-aware canonical match: NFD-decompose both sides before
        // case-folding. ASCII-only eq would miss "Yeni klasör" because the
        // 'ö' may differ between Windows pre-composed (U+00F6) and APFS
        // stored decomposed (U+006F U+0308), and even when bytes match the
        // ascii fold doesn't lowercase the non-ASCII byte for case
        // insensitivity. Falls back to NFD-lowercase if no entry matches.
        let supplied_nfd_lower: String = supplied_name.nfd().collect::<String>().to_lowercase();
        let canonical = entries
            .iter()
            .find(|e| e.name.nfd().collect::<String>().to_lowercase() == supplied_nfd_lower)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| supplied_name.to_lowercase());
        Some((p_ino, canonical))
    }

    /// Read the volume name from the live VSB (apfs_volname, offset 0x2C0).
    /// Returns None if the device is read-only or the VSB cannot be read.
    fn read_volume_label(&self) -> Option<String> {
        const VSBI_VOLNAME: usize = 0x2C0;
        const APFS_VOLNAME_LEN: usize = 256;
        let mut g = self.fs.lock().ok()?;
        let fs = g.as_mut()?;
        let (vsb_raw, _) = fs.read_vsb_omap_raw().ok()?;
        let name_bytes = vsb_raw.get(VSBI_VOLNAME..VSBI_VOLNAME + APFS_VOLNAME_LEN)?;
        let nul_pos = name_bytes
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(APFS_VOLNAME_LEN);
        std::str::from_utf8(&name_bytes[..nul_pos])
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    }

    /// Run a closure inside an apfs-write Transaction, reopening the FsView
    /// after a successful commit or aborting (returning the device) on
    /// error. The closure receives the staged transaction plus fresh
    /// vsb_raw / vol_omap_raw byte slices read from the device.
    /// Failures from `op` are propagated; the mount stays alive (the
    /// device is returned via `Transaction::abort` and a fresh FsView is
    /// opened from it).
    fn with_writable<R, F>(&self, op: F) -> winfsp::Result<R>
    where
        F: FnOnce(&mut Transaction<D>, &[u8], &[u8]) -> Result<R, TxnError>,
    {
        // Acquire the lock and take the FsView out (None state).
        let mut guard = self
            .fs
            .lock()
            .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
        let mut view = guard
            .take()
            .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
        // Snapshot vsb/omap raw bytes from the latest on-disk state.
        let (vsb_raw, omap_raw) = match view.read_vsb_omap_raw() {
            Ok(p) => p,
            Err(e) => {
                // Restore the FsView and propagate as I/O error.
                *guard = Some(view);
                eprintln!("with_writable: read_vsb_omap_raw failed: {e:?}");
                return Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR));
            }
        };
        let dev = view.into_dev();
        let mut txn = match Transaction::begin(dev) {
            Ok(t) => t,
            Err(e) => {
                // Begin failed before consuming the device; cannot recover
                // because Transaction::begin consumes the device by value
                // when it succeeds, but the failure path here means the
                // device was already dropped inside begin. Mount is now
                // broken; return DEVICE_NOT_READY so callers see a clean
                // error rather than a panic. This is a documented edge
                // case; commit-side errors are recoverable via abort().
                eprintln!("with_writable: Transaction::begin failed: {e:?}");
                return Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR));
            }
        };
        let result = op(&mut txn, &vsb_raw, &omap_raw);
        match result {
            Ok(r) => {
                match txn.commit() {
                    Ok(mut dev_back) => {
                        // Defense-in-depth: drop any read-ahead cache before
                        // reopening so the next container open re-reads the
                        // post-commit nx_superblock/checkpoint state from the
                        // durable inner device. write-through invalidates the
                        // written ranges already; this guards against any
                        // residual aliasing across the FS reopen boundary.
                        dev_back.invalidate_read_cache();
                        match FsView::open(dev_back) {
                            Ok(new_view) => {
                                *guard = Some(new_view);
                                Ok(r)
                            }
                            Err(e) => {
                                eprintln!(
                                    "with_writable: FsView reopen after commit failed: {e:?}"
                                );
                                Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("with_writable: commit failed: {e:?}");
                        Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))
                    }
                }
            }
            Err(e) => {
                // Discard staged changes, recover the device, reopen FsView.
                let dev_back = txn.abort();
                match FsView::open(dev_back) {
                    Ok(new_view) => {
                        *guard = Some(new_view);
                    }
                    Err(e2) => {
                        eprintln!("with_writable: FsView reopen after abort failed: {e2:?}");
                    }
                }
                eprintln!("with_writable: op failed: {e:?}");
                Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))
            }
        }
    }

    /// Flush all pending batched ops into a single Transaction commit.
    ///
    /// Called on: explicit threshold exceeded (from enqueue), cleanup callback,
    /// read callbacks that need consistency (get_volume_info, read_directory),
    /// and clean shutdown. Skips `with_writable` if the queue is empty.
    ///
    /// Each op is replayed in enqueue order inside one transaction; if any op
    /// returns `TxnError`, the transaction is aborted and the error is returned
    /// as `STATUS_IO_DEVICE_ERROR`. The remaining ops are discarded to avoid
    /// replaying against a potentially inconsistent on-disk state.
    ///
    /// `TruncateFast` ops that return `FastPathDeclined` (boundary-cut shrinks)
    /// are collected and replayed after the batch transaction commits via the
    /// read+rewrite slow path. This allows shrink ops to be queued in the batch
    /// without special-casing them at enqueue time (M8 #3).
    fn flush_batch(&self) -> winfsp::Result<()> {
        let ops = self.batch.drain();
        if ops.is_empty() {
            return Ok(());
        }
        // Collect shrink ops that the fast path declines so we can fall back
        // after the batch commits. Tuple: (parent_ino, name, path, new_size).
        let mut shrink_fallbacks: Vec<(u64, String, String, u64)> = Vec::new();
        self.with_writable(|txn, vsb_raw, omap_raw| {
            for op in &ops {
                match crate::batch::replay_op(op, txn, vsb_raw, omap_raw)? {
                    crate::batch::OpOutcome::Ok => {}
                    crate::batch::OpOutcome::Truncate(
                        apfs_write_file::TruncateOutcome::Applied,
                    ) => {}
                    crate::batch::OpOutcome::Truncate(
                        apfs_write_file::TruncateOutcome::FastPathDeclined,
                    ) => {
                        // Boundary-cut shrink: mark for slow-path fallback
                        // after this transaction commits. The fast path left
                        // on-disk size unchanged, so the fallback sees the
                        // original bytes and can safely read+rewrite.
                        if let crate::batch::DeferredOp::TruncateFast {
                            parent_ino,
                            name,
                            path,
                            new_size,
                            ..
                        } = op
                        {
                            shrink_fallbacks.push((*parent_ino, name.clone(), path.clone(), *new_size));
                        }
                    }
                }
            }
            Ok(())
        })?;
        // Slow fallback for boundary-cut shrinks declined by truncate_file_fast.
        // Each op runs in its own transaction (rare path; typical whole-extent
        // shrinks are handled by the fast path above).
        for (parent_ino, name, path, new_size) in shrink_fallbacks {
            // Read the current on-disk content via the path stored in the op.
            // A read failure is propagated rather than silently zeroing the
            // file (mirrors the same guard in set_file_size slow path).
            let current_bytes = {
                let mut fs_guard = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = fs_guard
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let size = fs
                    .getattr(&path)
                    .ok()
                    .flatten()
                    .map(|a| a.size)
                    .unwrap_or(0);
                if size > 0 {
                    fs.read(&path, 0, size as usize)
                        .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                } else {
                    Vec::new()
                }
            };
            let mut new_content = current_bytes;
            new_content.resize(new_size as usize, 0u8);
            self.with_writable(|txn, vsb_raw, omap_raw| {
                apfs_write_file::overwrite_existing_file(
                    txn,
                    vsb_raw,
                    omap_raw,
                    parent_ino,
                    &name,
                    &new_content,
                )
            })?;
        }
        // Queue is now drained: mark clean.
        self.sync_dirty_flag();
        Ok(())
    }

    /// Encode an APFS symlink target into a Windows `REPARSE_DATA_BUFFER`
    /// laid out for `IO_REPARSE_TAG_SYMLINK`. POSIX forward slashes are
    /// converted to backslashes so Explorer can resolve the link; an
    /// absolute target (POSIX `/foo` or NT `\\?\...`) clears the
    /// `SYMLINK_FLAG_RELATIVE` bit, anything else sets it.
    ///
    /// Layout (winnt.h `REPARSE_DATA_BUFFER` + `SymbolicLinkReparseBuffer`):
    /// ```text
    /// off  size  field
    ///   0     4  ReparseTag = 0xA000_000C
    ///   4     2  ReparseDataLength = 12 + path_bytes
    ///   6     2  Reserved = 0
    ///   8     2  SubstituteNameOffset = 0
    ///  10     2  SubstituteNameLength = path_bytes
    ///  12     2  PrintNameOffset = 0
    ///  14     2  PrintNameLength = path_bytes
    ///  16     4  Flags (SYMLINK_FLAG_RELATIVE for non-absolute)
    ///  20    ..  PathBuffer (UTF-16, no NUL)
    /// ```
    /// Returns the number of bytes written to `out` on success, or
    /// `STATUS_BUFFER_OVERFLOW` if the buffer is too small.
    fn encode_symlink_reparse(target: &[u8], out: &mut [u8]) -> winfsp::Result<u64> {
        const STATUS_BUFFER_OVERFLOW: i32 = i32::from_ne_bytes((0x8000_0005u32).to_ne_bytes());
        let target_str = std::str::from_utf8(target)
            .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
        let win_path: String = target_str.replace('/', "\\");
        let utf16: Vec<u16> = win_path.encode_utf16().collect();
        let path_bytes = utf16.len() * 2;
        let total = 20usize.saturating_add(path_bytes);
        if out.len() < total {
            return Err(winfsp::FspError::NTSTATUS(STATUS_BUFFER_OVERFLOW));
        }
        out[0..4].copy_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
        out[4..6].copy_from_slice(&((12 + path_bytes) as u16).to_le_bytes());
        out[6..8].copy_from_slice(&0u16.to_le_bytes());
        out[8..10].copy_from_slice(&0u16.to_le_bytes());
        out[10..12].copy_from_slice(&(path_bytes as u16).to_le_bytes());
        out[12..14].copy_from_slice(&0u16.to_le_bytes());
        out[14..16].copy_from_slice(&(path_bytes as u16).to_le_bytes());
        let absolute = target_str.starts_with('/') || win_path.starts_with('\\');
        let flags = if absolute {
            0u32
        } else {
            SYMLINK_FLAG_RELATIVE
        };
        out[16..20].copy_from_slice(&flags.to_le_bytes());
        for (i, &c) in utf16.iter().enumerate() {
            let p = 20 + i * 2;
            out[p..p + 2].copy_from_slice(&c.to_le_bytes());
        }
        Ok(total as u64)
    }

    /// Convert APFS nanoseconds-since-UNIX-epoch into a Windows FILETIME
    /// value (100-ns ticks since 1601-01-01). Saturating arithmetic so any
    /// future-far values (e.g. unset/zero, or year > 30000) clamp safely
    /// rather than overflow.
    fn apfs_ns_to_filetime(ns: u64) -> u64 {
        const WIN_TO_UNIX_TICKS: u64 = 116_444_736_000_000_000;
        (ns / 100).saturating_add(WIN_TO_UNIX_TICKS)
    }

    fn fill_info(attr: &apfs::Attr, info: &mut FileInfo) {
        info.file_size = attr.size;
        info.allocation_size = (attr.size + 4095) & !4095;
        info.creation_time = Self::apfs_ns_to_filetime(attr.create_time);
        info.last_write_time = Self::apfs_ns_to_filetime(attr.mod_time);
        info.change_time = Self::apfs_ns_to_filetime(attr.change_time);
        info.last_access_time = Self::apfs_ns_to_filetime(attr.access_time);
        // POSIX u-write (S_IWUSR = 0o200) absence → FILE_ATTRIBUTE_READONLY
        // so Set-ItemProperty / Get-ItemProperty IsReadOnly round-trips
        // correctly. Directories never get the READONLY bit (Explorer
        // treats it as "system folder" which surprises users).
        let is_symlink = attr.mode & S_IFMT == S_IFLNK;
        let base_attr = if attr.is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else if attr.mode & 0o200 == 0 {
            FILE_ATTRIBUTE_READONLY
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
        // UF_HIDDEN (BSD flag set by Finder) → FILE_ATTRIBUTE_HIDDEN.
        let hidden = if attr.bsd_flags & UF_HIDDEN != 0 {
            FILE_ATTRIBUTE_HIDDEN
        } else {
            0
        };
        // S_IFLNK → FILE_ATTRIBUTE_REPARSE_POINT + IO_REPARSE_TAG_SYMLINK.
        // The target string lives in the j_xattr 'com.apple.fs.symlink'
        // record and is materialised by the `get_reparse_point` callback
        // on demand.
        let reparse = if is_symlink {
            info.reparse_tag = IO_REPARSE_TAG_SYMLINK;
            FILE_ATTRIBUTE_REPARSE_POINT
        } else {
            0
        };
        info.file_attributes = base_attr | hidden | reparse;
        info.index_number = attr.inode;
    }

    /// Decode a Windows `REPARSE_DATA_BUFFER` tagged
    /// `IO_REPARSE_TAG_SYMLINK` into a POSIX target string. Prefers the
    /// PrintName field (the display path the user typed) over the
    /// SubstituteName (NT internal `\??\C:\...`) because APFS stores plain
    /// POSIX targets. Backslashes are converted to forward slashes.
    fn decode_symlink_reparse(buf: &[u8]) -> winfsp::Result<Vec<u8>> {
        const STATUS_INVALID_PARAMETER: i32 = i32::from_ne_bytes((0xC000_000Du32).to_ne_bytes());
        const STATUS_NOT_SUPPORTED: i32 = i32::from_ne_bytes((0xC000_00BBu32).to_ne_bytes());
        if buf.len() < 20 {
            return Err(winfsp::FspError::NTSTATUS(STATUS_INVALID_PARAMETER));
        }
        let mut tag_bytes = [0u8; 4];
        tag_bytes.copy_from_slice(buf.get(0..4).unwrap_or(&[0; 4]));
        let tag = u32::from_le_bytes(tag_bytes);
        if tag != IO_REPARSE_TAG_SYMLINK {
            return Err(winfsp::FspError::NTSTATUS(STATUS_NOT_SUPPORTED));
        }
        let read_u16 = |off: usize| -> u16 {
            let mut b = [0u8; 2];
            if let Some(s) = buf.get(off..off + 2) {
                b.copy_from_slice(s);
            }
            u16::from_le_bytes(b)
        };
        let print_off = read_u16(12) as usize;
        let print_len = read_u16(14) as usize;
        let sub_off = read_u16(8) as usize;
        let sub_len = read_u16(10) as usize;
        // PrintName preferred; fall back to SubstituteName for callers that
        // only set the substitute side (some NT-only clients).
        let (off, len) = if print_len > 0 {
            (print_off, print_len)
        } else {
            (sub_off, sub_len)
        };
        let start = 20usize
            .checked_add(off)
            .ok_or_else(|| winfsp::FspError::NTSTATUS(STATUS_INVALID_PARAMETER))?;
        let end = start
            .checked_add(len)
            .ok_or_else(|| winfsp::FspError::NTSTATUS(STATUS_INVALID_PARAMETER))?;
        let bytes = buf
            .get(start..end)
            .ok_or_else(|| winfsp::FspError::NTSTATUS(STATUS_INVALID_PARAMETER))?;
        let utf16: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let s = String::from_utf16_lossy(&utf16);
        let posix = s.replace('\\', "/");
        Ok(posix.into_bytes())
    }
}

impl<D: WritableBlockDevice + Send + 'static> FileSystemContext for ApfsFs<D> {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _sd: Option<&mut [std::ffi::c_void]>,
        _resolve: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        guard(|| {
            let path = win_to_unix(file_name);
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            match fs
                .getattr(&path)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
            {
                None => Err(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND)),
                Some(a) => {
                    let is_symlink = a.mode & S_IFMT == S_IFLNK;
                    let base = if a.is_dir {
                        FILE_ATTRIBUTE_DIRECTORY
                    } else if a.mode & 0o200 == 0 {
                        FILE_ATTRIBUTE_READONLY
                    } else {
                        FILE_ATTRIBUTE_NORMAL
                    };
                    let hidden = if a.bsd_flags & UF_HIDDEN != 0 {
                        FILE_ATTRIBUTE_HIDDEN
                    } else {
                        0
                    };
                    let reparse_bit = if is_symlink {
                        FILE_ATTRIBUTE_REPARSE_POINT
                    } else {
                        0
                    };
                    Ok(FileSecurity {
                        reparse: is_symlink,
                        sz_security_descriptor: 0,
                        attributes: base | hidden | reparse_bit,
                    })
                }
            }
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        // winfsp 0.13: granted_access is u32, not FILE_ACCESS_RIGHTS
        granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        guard(|| {
            let path = win_to_unix(file_name);
            eprintln!(
                "open: path={} create_options=0x{:x} granted_access=0x{:x}",
                path, create_options, granted_access
            );
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let attr = fs
                .getattr(&path)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            Self::fill_info(&attr, file_info.as_mut());
            Ok(Handle {
                path,
                content: std::sync::Mutex::new(None),
                write_buf: std::sync::Mutex::new(None),
                delete_on_close: std::sync::atomic::AtomicBool::new(create_options & 0x1000 != 0),
            })
        })
    }

    fn close(&self, _context: Self::FileContext) {}

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        guard(|| {
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let attr = fs
                .getattr(&context.path)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            Self::fill_info(&attr, file_info);
            Ok(())
        })
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        guard(|| {
            // Fill the per-handle content cache exactly once; subsequent
            // chunked reads are served from RAM (no per-chunk whole-file
            // re-read/re-decompress from the slow device).
            let data = {
                let mut slot = context
                    .content
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                match slot.as_ref() {
                    Some(d) => std::sync::Arc::clone(d),
                    None => {
                        let mut fs_guard = self
                            .fs
                            .lock()
                            .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                        let fs = fs_guard
                            .as_mut()
                            .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                        let full = fs
                            .read(&context.path, 0, usize::MAX)
                            .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                        let arc = std::sync::Arc::new(full);
                        *slot = Some(std::sync::Arc::clone(&arc));
                        arc
                    }
                }
            };
            let off = offset as usize;
            if off >= data.len() {
                return Ok(0);
            }
            let end = off.saturating_add(buffer.len()).min(data.len());
            let n = end - off;
            let src = data
                .get(off..end)
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            buffer
                .get_mut(..n)
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                .copy_from_slice(src);
            Ok(n as u32)
        })
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: winfsp::filesystem::DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        guard(|| {
            // Flush pending batch ops so directory contents reflect all
            // recent renames / setattr operations (M8 #2 consistency).
            self.flush_batch()?;
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let entries = fs
                .read_dir(&context.path)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            // A directory larger than one transfer buffer is enumerated
            // across MULTIPLE read_directory calls: WinFsp re-calls with
            // `marker` = the last name we returned. We MUST resume strictly
            // AFTER it — otherwise every call re-emits the same prefix and
            // the listing never terminates (Explorer/dir hangs forever).
            // winfsp 0.13: DirMarker::inner() -> Option<&[u16]> (UTF-16
            // units). None = no marker (first call); Some = resume after it.
            let resume: Option<String> = marker.inner().map(String::from_utf16_lossy);
            let mut cursor = 0u32;
            for e in entries {
                // Order-based skip: emit any entry whose name sorts strictly
                // AFTER the marker. Previous logic required marker == e.name
                // to flip an "emitting" flag, which silently dropped the
                // remainder when the marker entry had been deleted between
                // the prior dispatch and this one (cmd rmdir /s removes
                // children one-by-one and the kernel re-enumerates with the
                // last-returned name; the marker is already gone by then).
                // APFS DRECs are stored in name-hash order; for ASCII this
                // also matches lexicographic byte order, which is what
                // Windows comparison uses to consume the listing.
                if let Some(m) = resume.as_deref() {
                    if e.name.as_str() <= m {
                        continue;
                    }
                }
                let mut di = DirInfo::<255>::new();
                di.set_name(e.name.as_str())
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let child = if context.path == "/" {
                    format!("/{}", e.name)
                } else {
                    format!("{}/{}", context.path, e.name)
                };
                if let Some(a) = fs.getattr(&child).ok().flatten() {
                    Self::fill_info(&a, di.file_info_mut());
                }
                if !di.append_to_buffer(buffer, &mut cursor) {
                    break;
                }
            }
            DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
            Ok(cursor)
        })
    }

    /// FlushFileBuffers from user-space (Notepad++ save-as, atomic save
    /// patterns). Windows expects this to succeed even when there is no
    /// per-file flush work to do — our writes are buffered into the
    /// handle and committed on Cleanup, and the inner block device is
    /// opened FILE_FLAG_WRITE_THROUGH so any data we have already
    /// written-through is already durable. Returning the default
    /// STATUS_INVALID_DEVICE_REQUEST surfaces as "Yanlış işlev" /
    /// "Incorrect function" on every save dialog. Refresh the session
    /// FileInfo from the live FS so the caller sees the latest size.
    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        guard(|| {
            if let Some(ctx) = context {
                let mut fs_guard = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = fs_guard
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                if let Ok(Some(attr)) = fs.getattr(&ctx.path) {
                    Self::fill_info(&attr, file_info);
                }
            }
            Ok(())
        })
    }

    /// Volume info — REQUIRED override. WinFsp's default returns
    /// STATUS_INVALID_DEVICE_REQUEST which surfaces to user-space as
    /// "Incorrect function" the moment `dir`, Explorer or any tool
    /// queries volume metadata (GetVolumeInformationW). The device's
    /// total size is the underlying block device size; free size we
    /// approximate from FsView (full APFS spaceman accounting is
    /// future work — for now the reported free is a safe lower bound
    /// so Explorer doesn't claim the volume is full).
    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        guard(|| {
            // Flush any pending batch ops so volume stats reflect the latest state.
            self.flush_batch()?;
            let (total, free) = {
                let mut fs_guard = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = fs_guard
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                fs.volume_size().unwrap_or((1u64 << 30, 1u64 << 29))
            };
            out_volume_info.total_size = total;
            out_volume_info.free_size = free;
            // Read actual volume name from VSB if the device is writable; fall back to "APFS".
            let label = self
                .read_volume_label()
                .unwrap_or_else(|| "APFS".to_string());
            out_volume_info.set_volume_label(&label);
            Ok(())
        })
    }

    /// Persist a new volume label into the APFS VSB via `set_volume_label`.
    fn set_volume_label(
        &self,
        volume_label: &U16CStr,
        volume_info: &mut VolumeInfo,
    ) -> winfsp::Result<()> {
        guard(|| {
            let label = volume_label.to_string_lossy();
            eprintln!("set_volume_label: {label:?}");
            self.with_writable(|txn, vsb_raw, _omap_raw| {
                apfs_write_file::set_volume_label(txn, vsb_raw, &label)
            })?;
            // Reflect back: update volume_info with the new label.
            volume_info.set_volume_label(&label);
            Ok(())
        })
    }

    /// Cleanup is called by WinFsp when a file handle closes. If the
    /// FspCleanupDelete bit is set in `flags`, the file (or directory)
    /// must be removed from the filesystem. We translate that into an
    /// apfs-write `unlink` call under a fresh Transaction.
    /// The default cleanup is a no-op; here we override only to honour
    /// the delete bit. Non-delete cleanups (e.g. SetLastWriteTime via
    /// the cleanup path) currently do nothing — handled by SetBasicInfo.
    /// Write a chunk into the handle's dirty buffer. Flushed on Cleanup
    /// (non-delete) by unlink + apfs_write::write_file.
    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        guard(|| {
            let mut wb = context
                .write_buf
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            // First write: seed with existing on-disk content (if any) so
            // partial-overwrite-by-offset doesn't lose data we're not about
            // to overwrite.
            if wb.is_none() {
                let initial: Vec<u8> = {
                    let mut g = self
                        .fs
                        .lock()
                        .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                    let fs = g
                        .as_mut()
                        .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                    // getattr is best-effort: a missing path implies a
                    // freshly created handle with no on-disk content yet, so
                    // start from an empty buffer. A non-zero size, however,
                    // means the catalog claims real bytes — if the read
                    // then fails we MUST propagate the error rather than
                    // silently seed an empty buffer, otherwise the partial-
                    // overwrite about to happen would clobber the existing
                    // content with zeros + the user's write.
                    let size = fs
                        .getattr(&context.path)
                        .ok()
                        .flatten()
                        .map(|a| a.size)
                        .unwrap_or(0);
                    if size > 0 {
                        fs.read(&context.path, 0, size as usize)
                            .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                    } else {
                        Vec::new()
                    }
                };
                *wb = Some(initial);
            }
            let buf = wb.as_mut().expect("seeded above");
            let start = if write_to_eof {
                buf.len() as u64
            } else {
                offset
            };
            let end = start.saturating_add(buffer.len() as u64);
            if (buf.len() as u64) < end {
                buf.resize(end as usize, 0);
            }
            let s = start as usize;
            let e = end as usize;
            buf[s..e].copy_from_slice(buffer);
            // Refresh file_info.file_size so Windows sees the new EOF.
            let info = file_info;
            info.file_size = buf.len() as u64;
            info.allocation_size = ((buf.len() as u64) + 4095) & !4095;
            Ok(buffer.len() as u32)
        })
    }

    /// Mark a file for deletion on close. Windows issues `set_delete` via
    /// NtSetInformationFile(FileDispositionInformation) BEFORE issuing
    /// the final close. We accept the mark unconditionally for any
    /// existing path (the actual unlink happens in cleanup with the
    /// FspCleanupDelete flag). If delete_file is false, the caller is
    /// CLEARING the disposition (Windows allows toggling) — also OK.
    /// Create a new file or directory. `create_options & 0x01` is
    /// FILE_DIRECTORY_FILE → mkdir; else create_file. Returns the new
    /// Handle. file_info is populated from the post-commit FsView so
    /// Windows sees correct attributes immediately.
    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        // winfsp 0.13 trait uses winfsp_sys::FILE_ACCESS_RIGHTS /
        // FILE_FLAGS_AND_ATTRIBUTES type aliases for these — both are
        // raw u32 under the hood and winfsp doesn't re-export
        // winfsp_sys publicly, so we override with u32 directly
        // (matches open() in this file, which winfsp also exposes as u32).
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[std::ffi::c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        guard(|| {
            let path = win_to_unix(file_name);
            // FILE_DIRECTORY_FILE = 0x00000001 (winfsp_sys / NT API).
            let is_dir = (create_options & 0x01) != 0;

            // Split into parent + name.
            let (parent_path, name) = match path.rsplit_once('/') {
                Some((p, n)) if !n.is_empty() => (
                    if p.is_empty() {
                        "/".to_string()
                    } else {
                        p.to_string()
                    },
                    n.to_string(),
                ),
                _ => {
                    return Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR));
                }
            };

            // Resolve parent inode under a short lock.
            let parent_ino = {
                let mut g = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = g
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                fs.lookup(&parent_path)
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?
            };

            // Issue the create or mkdir under a fresh Transaction.
            self.with_writable(|txn, vsb_raw, omap_raw| {
                if is_dir {
                    apfs_write_file::mkdir(txn, vsb_raw, omap_raw, parent_ino, &name).map(|_| ())
                } else {
                    apfs_write_file::create_file(txn, vsb_raw, omap_raw, parent_ino, &name)
                        .map(|_| ())
                }
            })?;

            // After commit, FsView was reopened — populate file_info from
            // the live state so Windows has accurate attributes.
            {
                let mut g = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = g
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                if let Ok(Some(attr)) = fs.getattr(&path) {
                    Self::fill_info(&attr, file_info.as_mut());
                }
            }
            Ok(Handle {
                path,
                content: std::sync::Mutex::new(None),
                write_buf: std::sync::Mutex::new(None),
                delete_on_close: std::sync::atomic::AtomicBool::new(create_options & 0x1000 != 0),
            })
        })
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        // Persist the disposition into the handle so cleanup honours it
        // even if WinFsp doesn't pass FspCleanupDelete in flags
        // (PowerShell Remove-Item / Explorer "Delete" go through this
        // path; the recursive variant uses FILE_DELETE_ON_CLOSE only).
        context
            .delete_on_close
            .store(delete_file, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn cleanup(&self, context: &Self::FileContext, file_name: Option<&U16CStr>, flags: u32) {
        // Flush any pending batch ops before this file's cleanup runs.
        // Required so that a rename-then-delete sequence sees the renamed
        // name, and so that write-file sees post-truncate sizes (M8 #2).
        let _ = self.flush_batch();
        let dispatch_delete = (flags & 0x01) != 0;
        let handle_doc = context
            .delete_on_close
            .load(std::sync::atomic::Ordering::SeqCst);
        // Honour BOTH the FspCleanupDelete flag and the handle's
        // delete-on-close (set by either FILE_DELETE_ON_CLOSE create
        // option or a prior set_delete). PowerShell Remove-Item
        // -Recurse takes the FILE_DELETE_ON_CLOSE path with no
        // FspCleanupDelete bit, which is why a non-recursive delete
        // works but a recursive one didn't.
        let is_delete = dispatch_delete || handle_doc;
        // Step D — write flush path. When the handle's dirty buffer is
        // populated (Some) and this is a non-delete cleanup, the chunked
        // writes accumulated through write() are flushed to disk via
        // unlink + apfs_write::write_file (apfs_write::write_file rejects
        // existing names, so we delete the empty placeholder first).
        if !is_delete {
            let dirty = context.write_buf.lock().ok().and_then(|mut g| g.take()); // take so we don't re-flush
            if let Some(content) = dirty {
                // Canonical linux-apfs-rw pattern (apfs_setsize + apfs_truncate
                // + apfs_get_new_block): a SINGLE transaction frees the old
                // dstream extents, allocates new blocks for the replacement
                // content, and updates the inode in place. The inode number
                // is preserved; no unlink/relink is needed. This replaces an
                // earlier two-transaction unlink+write_file flow whose second
                // commit corrupted the container on reopen.
                let resolved = self.resolve_parent_and_name(file_name, &context.path);
                if let Some((parent_ino, name)) = resolved {
                    let clen = content.len();
                    let r = self.with_writable(|txn, vsb_raw, omap_raw| {
                        apfs_write_file::overwrite_existing_file(
                            txn, vsb_raw, omap_raw, parent_ino, &name, &content,
                        )
                    });
                } else {
                }
            }
            return;
        }
        // delete branch (FspCleanupDelete = 0x01).
        if let Some((parent_ino, canonical_name)) =
            self.resolve_parent_and_name(file_name, &context.path)
        {
            let r = self.with_writable(|txn, vsb_raw, omap_raw| {
                apfs_write_file::unlink(txn, vsb_raw, omap_raw, parent_ino, &canonical_name)
            });
        } else {
        }
        return;
    }

    /// Rename (same-parent) or move (cross-directory).
    ///
    /// Same-parent rename is routed to `apfs_write::rename`.
    /// Cross-directory move is routed to `apfs_write::move_entry` (Phase 1.3).
    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        guard(|| {
            // Resolve the OLD name canonically — Windows often sends uppercase
            // paths through the FS even when the on-disk name is lowercase
            // (e.g. \\FILE_A.TXT for an on-disk file_a.txt), so we must match
            // case-insensitively against the live directory listing exactly
            // like the cleanup-delete path does.
            let (old_parent_ino, old_name) = self
                .resolve_parent_and_name(Some(file_name), "")
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            let new_path = win_to_unix(new_file_name);
            let (new_parent_path, new_name_raw) = match new_path.rsplit_once('/') {
                Some((p, n)) if !n.is_empty() => (
                    if p.is_empty() {
                        "/".to_string()
                    } else {
                        p.to_string()
                    },
                    n.to_string(),
                ),
                _ => return Err(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_INVALID)),
            };
            // Always materialise the new name in its Unicode NFD case-folded
            // form: the on-disk catalog hashes the folded form, and
            // create_file already writes it folded. Diverging here would
            // yield a record the catalog cannot find on subsequent lookups.
            // Unicode fold handles Turkish İ/ı, German ß, Greek Σ/σ, etc.
            // correctly where to_ascii_lowercase would silently truncate.
            let new_name = apfs::case_fold::apfs_case_fold(&new_name_raw);
            let new_parent_ino = {
                let mut g = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = g
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                fs.lookup(&new_parent_path)
                    .ok()
                    .flatten()
                    .or_else(|| fs.lookup(&new_parent_path.to_lowercase()).ok().flatten())
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?
            };
            // Batch path (M8 #2): enqueue and only flush when threshold is
            // reached. On a 1000-rename workload this reduces Transaction
            // commits from 1000 to ~16 (1000/64), amortising the B-tree
            // rebuild and NXSB ring write cost.
            let op = if old_parent_ino == new_parent_ino {
                eprintln!(
                    "rename (same-parent): parent={old_parent_ino} {old_name:?} -> {new_name:?}"
                );
                DeferredOp::Rename {
                    parent_ino: old_parent_ino,
                    old_name,
                    new_name,
                    replace_if_exists,
                }
            } else {
                eprintln!(
                    "move_entry: {old_parent_ino}/{old_name:?} -> {new_parent_ino}/{new_name:?}"
                );
                DeferredOp::MoveEntry {
                    old_parent_ino,
                    old_name,
                    new_parent_ino,
                    new_name,
                    replace_if_exists,
                }
            };
            let flush_now = self.batch.enqueue(op);
            if flush_now {
                self.flush_batch()?;
            } else {
                self.sync_dirty_flag();
            }
            Ok(())
        })
    }

    /// Touch timestamps + attributes. Maps FILE_ATTRIBUTE_READONLY to POSIX
    /// mode-bit changes and persists mtime/atime/ctime/btime via
    /// `apfs_write::set_inode_attrs` (Phase 1.4).
    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        file_attributes: u32,
        creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
        last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        guard(|| {
            // FILE_ATTRIBUTE_READONLY = 0x1 (Windows SDK winnt.h)
            const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
            // 100-ns ticks between Windows epoch (1601-01-01) and Unix epoch (1970-01-01).
            const WIN_TO_UNIX_TICKS: u64 = 116_444_736_000_000_000;

            /// Convert a WinFsp FILETIME (100-ns ticks since 1601) to APFS ns since 1970.
            /// Returns None if the value is 0 (WinFsp uses 0 to mean "no change").
            fn ft_to_ns(ft: u64) -> Option<u64> {
                if ft == 0 {
                    return None;
                }
                ft.checked_sub(WIN_TO_UNIX_TICKS)
                    .map(|ticks| ticks.saturating_mul(100))
            }

            let mtime_ns = ft_to_ns(last_write_time);
            let ctime_ns = ft_to_ns(last_change_time);
            let atime_ns = ft_to_ns(last_access_time);
            let btime_ns = ft_to_ns(creation_time);

            // Map FILE_ATTRIBUTE_READONLY → POSIX mode bits, and
            // FILE_ATTRIBUTE_HIDDEN → APFS UF_HIDDEN BSD flag. 0xFFFFFFFF
            // means "no change to attributes" in WinFsp.
            let attrs_supplied = file_attributes != 0 && file_attributes != 0xFFFF_FFFF;
            let (mode_set, mode_clear): (Option<u16>, Option<u16>) = if attrs_supplied {
                if file_attributes & FILE_ATTRIBUTE_READONLY != 0 {
                    (None, Some(0o222))
                } else {
                    (Some(0o200), None)
                }
            } else {
                (None, None)
            };
            let (bsd_flags_set, bsd_flags_clear): (Option<u32>, Option<u32>) = if attrs_supplied {
                if file_attributes & FILE_ATTRIBUTE_HIDDEN != 0 {
                    (Some(UF_HIDDEN), None)
                } else {
                    (None, Some(UF_HIDDEN))
                }
            } else {
                (None, None)
            };

            let anything_to_persist = mtime_ns.is_some()
                || ctime_ns.is_some()
                || atime_ns.is_some()
                || btime_ns.is_some()
                || mode_set.is_some()
                || mode_clear.is_some()
                || bsd_flags_set.is_some()
                || bsd_flags_clear.is_some();

            if anything_to_persist {
                if let Some((parent_ino, name)) = self.resolve_parent_and_name(None, &context.path)
                {
                    // Batch path (M8 #2): enqueue and flush on threshold.
                    let op = DeferredOp::SetInodeAttrs {
                        parent_ino,
                        name,
                        mode_set,
                        mode_clear,
                        mtime_ns,
                        ctime_ns,
                        atime_ns,
                        btime_ns,
                        bsd_flags_set,
                        bsd_flags_clear,
                    };
                    let flush_now = self.batch.enqueue(op);
                    if flush_now {
                        if let Err(e) = self.flush_batch() {
                            eprintln!("set_basic_info: batch flush failed: {e:?}");
                            return Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR));
                        }
                    } else {
                        self.sync_dirty_flag();
                    }
                }
            }

            // Reflect the requested values into file_info for this session.
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let attr = fs
                .getattr(&context.path)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            Self::fill_info(&attr, file_info);
            if creation_time != 0 {
                file_info.creation_time = creation_time;
            }
            if last_access_time != 0 {
                file_info.last_access_time = last_access_time;
            }
            if last_write_time != 0 {
                file_info.last_write_time = last_write_time;
            }
            if last_change_time != 0 {
                file_info.change_time = last_change_time;
            }
            Ok(())
        })
    }

    /// Resize a file. Truncate-to-zero is fully supported by
    /// `overwrite_existing_file(&[])`; arbitrary truncate / extend is M8
    /// work (needs partial-extent truncate semantics) — accept the call
    /// and reflect the size into file_info so Windows progress dialogs
    /// don't stall, but do not persist a partial new size yet.
    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        guard(|| {
            // Windows splits a .NET FileStream.SetLength into two WinFsp
            // dispatches: SetEndOfFile (set_allocation_size=false, new_size
            // is the logical EOF) THEN SetAllocationSize (set_allocation_size
            // =true, new_size is the on-disk allocation rounded up to a
            // cluster). Honouring both with the same shrink/extend would
            // overwrite the user's freshly-set EOF with the larger
            // allocation, growing the file to a multiple of 4 KiB.
            // For us the allocation is implicit (one extent per ~bsz);
            // accept the allocation call without touching dstream.size.
            if set_allocation_size {
                let mut fs_guard = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = fs_guard
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                if let Ok(Some(attr)) = fs.getattr(&context.path) {
                    Self::fill_info(&attr, file_info);
                }
                return Ok(());
            }
            let (parent_ino, name) = self
                .resolve_parent_and_name(None, &context.path)
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            // Get current size cheaply (one getattr) so we can take the
            // metadata-only fast path for extends and aligned shrinks.
            let current_size = {
                let mut fs_guard = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = fs_guard
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                fs.getattr(&context.path)
                    .ok()
                    .flatten()
                    .map(|a| a.size)
                    .unwrap_or(0)
            };
            // Batch path (M8 #3): ALL truncate ops — extends, zero-truncates,
            // AND shrinks — are enqueued. flush_batch handles FastPathDeclined
            // outcomes (boundary-cut shrinks) via the read+rewrite slow path
            // after the batch commits. This removes the per-shrink forced flush
            // that was causing 50 truncates to generate 50 commits (W4: 8.5x).
            let op = DeferredOp::TruncateFast {
                parent_ino,
                name: name.clone(),
                path: context.path.clone(),
                new_size,
                current_size,
            };
            let flush_now = self.batch.enqueue(op);
            if flush_now {
                self.flush_batch()?;
            } else {
                self.sync_dirty_flag();
            }
            // Reflect size into file_info immediately (the op is staged).
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            if let Ok(Some(attr)) = fs.getattr(&context.path) {
                Self::fill_info(&attr, file_info);
            }
            // Report the desired new_size even before flush so Windows
            // does not stall progress dialogs.
            file_info.file_size = new_size;
            file_info.allocation_size = (new_size + 4095) & !4095;
            Ok(())
        })
    }

    /// CREATE_ALWAYS / supersede semantic: Windows opens an existing file
    /// with TRUNCATE intent. Treat as truncate-to-zero + drop any pending
    /// dirty buffer; the subsequent Write/Cleanup will rewrite via
    /// overwrite_existing_file just like a fresh create.
    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        guard(|| {
            if let Ok(mut wb) = context.write_buf.lock() {
                *wb = None;
            }
            if let Some((parent_ino, name)) = self.resolve_parent_and_name(None, &context.path) {
                self.with_writable(|txn, vsb_raw, omap_raw| {
                    apfs_write_file::overwrite_existing_file(
                        txn,
                        vsb_raw,
                        omap_raw,
                        parent_ino,
                        &name,
                        &[],
                    )
                })?;
            }
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            if let Ok(Some(attr)) = fs.getattr(&context.path) {
                Self::fill_info(&attr, file_info);
            }
            Ok(())
        })
    }

    /// Materialise the APFS symlink target stored in the
    /// `com.apple.fs.symlink` xattr into a Windows `REPARSE_DATA_BUFFER`
    /// tagged `IO_REPARSE_TAG_SYMLINK`. Returns `STATUS_NOT_A_REPARSE_POINT`
    /// for any file that is not S_IFLNK. WinFsp passes a context-bound
    /// path so we reuse the cached `context.path` rather than re-converting
    /// `file_name`.
    fn get_reparse_point(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        guard(|| {
            const STATUS_NOT_A_REPARSE_POINT: i32 =
                i32::from_ne_bytes((0xC000_0275u32).to_ne_bytes());
            let target = {
                let mut g = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = g
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                fs.read_symlink(&context.path)
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
            };
            let target = target.ok_or(winfsp::FspError::NTSTATUS(STATUS_NOT_A_REPARSE_POINT))?;
            // Strip a single trailing NUL if the xattr value carries one
            // (apfs-write `build_xattr_val_embedded` does not, but Apple's
            // own writers sometimes do).
            let slice: &[u8] = if target.last() == Some(&0) {
                &target[..target.len() - 1]
            } else {
                &target[..]
            };
            Self::encode_symlink_reparse(slice, buffer)
        })
    }

    /// Path-based reparse lookup used by WinFsp's automatic resolver during
    /// open. Same encoding as `get_reparse_point`; resolves the path through
    /// `FsView::read_symlink` directly.
    fn get_reparse_point_by_name(
        &self,
        file_name: &U16CStr,
        _is_directory: bool,
        buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        guard(|| {
            const STATUS_NOT_A_REPARSE_POINT: i32 =
                i32::from_ne_bytes((0xC000_0275u32).to_ne_bytes());
            let path = win_to_unix(file_name);
            let target = {
                let mut g = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = g
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                fs.read_symlink(&path)
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
            };
            let target = target.ok_or(winfsp::FspError::NTSTATUS(STATUS_NOT_A_REPARSE_POINT))?;
            let slice: &[u8] = if target.last() == Some(&0) {
                &target[..target.len() - 1]
            } else {
                &target[..]
            };
            Self::encode_symlink_reparse(slice, buffer)
        })
    }

    /// Convert an existing file (or freshly-created empty placeholder) into
    /// an APFS symbolic link with the target encoded in `buffer`. WinFsp
    /// dispatches this on `CreateSymbolicLinkW` after the Create callback
    /// has produced a zero-byte regular file; we unlink that placeholder
    /// and immediately recreate the entry as `S_IFLNK | 0o777` carrying
    /// the `com.apple.fs.symlink` xattr.
    ///
    /// The two operations span two transactions — a brief ENOENT window
    /// exists between them. For typical `New-Item -ItemType SymbolicLink`
    /// or `mklink` flows this is invisible because no concurrent reader
    /// is racing the create.
    fn set_reparse_point(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        buffer: &[u8],
    ) -> winfsp::Result<()> {
        guard(|| {
            let target = Self::decode_symlink_reparse(buffer)?;
            let path = win_to_unix(file_name);
            let (parent_ino, name) = self
                .resolve_parent_and_name(None, &path)
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            // Txn 1: drop the empty placeholder created by the Create
            // callback. If the path doesn't exist (rare), unlink returns
            // NotFound and we still proceed to create the symlink.
            let _ = self.with_writable(|txn, vsb_raw, omap_raw| {
                apfs_write_file::unlink(txn, vsb_raw, omap_raw, parent_ino, &name)
            });
            // Txn 2: create the symlink record set.
            self.with_writable(|txn, vsb_raw, omap_raw| {
                apfs_write_file::create_symlink(txn, vsb_raw, omap_raw, parent_ino, &name, &target)
                    .map(|_| ())
            })?;
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // Extended attributes (Windows EA ↔ APFS xattr bridge)
    // -----------------------------------------------------------------------
    //
    // Windows EA wire format: a packed sequence of FILE_FULL_EA_INFORMATION
    // structs (little-endian):
    //   u32  NextEntryOffset   (0 = last entry)
    //   u8   Flags
    //   u8   EaNameLength      (byte count, NOT including NUL)
    //   u16  EaValueLength
    //   char EaName[EaNameLength + 1]   (NUL-terminated)
    //   u8   EaValue[EaValueLength]
    //   <padding to u32 alignment for next entry>
    //
    // Each EA maps 1-to-1 onto an APFS xattr by the same name.
    // XATTR_FILE_SYSTEM_OWNED (0x0004) attributes are excluded from
    // get_extended_attributes so Windows consumers only see user xattrs.
    //
    // Reference: WinFsp memfs.cpp + Microsoft FILE_FULL_EA_INFORMATION docs.

    /// Serialize all user-visible APFS xattrs for `context.path` into the
    /// `FILE_FULL_EA_INFORMATION` wire format. Returns the total byte count
    /// written into `buffer`.
    fn get_extended_attributes(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        guard(|| {
            // Collect names under a short lock.
            let names: Vec<String> = {
                let mut g = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = g
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                fs.list_xattrs(&context.path)
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
            };

            let mut pos: usize = 0;
            let mut last_offset_pos: Option<usize> = None;

            for name in &names {
                let name_len = name.len();
                if name_len > 255 {
                    continue;
                }

                // Fetch value under a short lock.
                let value: Vec<u8> = {
                    let mut g = self
                        .fs
                        .lock()
                        .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                    let fs = g
                        .as_mut()
                        .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                    fs.get_xattr(&context.path, name)
                        .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                        .unwrap_or_default()
                };

                let val_len = value.len();
                if val_len > u16::MAX as usize {
                    continue;
                }

                // Header: NextEntryOffset(4) + Flags(1) + EaNameLength(1) +
                //         EaValueLength(2) = 8 bytes, then name+NUL, then value.
                let entry_base = 8 + name_len + 1 + val_len;
                // Align to 4-byte boundary for NextEntryOffset.
                let entry_aligned = (entry_base + 3) & !3;

                if pos + entry_base > buffer.len() {
                    // Buffer too small — stop. WinFsp will call again with a
                    // larger buffer if needed; return what we have so far.
                    break;
                }

                // Patch the previous entry's NextEntryOffset to point here.
                if let Some(prev) = last_offset_pos {
                    let delta = (pos - prev) as u32;
                    buffer[prev..prev + 4].copy_from_slice(&delta.to_le_bytes());
                }

                // Write this entry (NextEntryOffset = 0 for the final entry).
                buffer[pos..pos + 4].copy_from_slice(&0u32.to_le_bytes());
                buffer[pos + 4] = 0u8; // Flags
                buffer[pos + 5] = name_len as u8; // EaNameLength
                buffer[pos + 6..pos + 8]
                    .copy_from_slice(&(val_len as u16).to_le_bytes());
                buffer[pos + 8..pos + 8 + name_len].copy_from_slice(name.as_bytes());
                buffer[pos + 8 + name_len] = 0u8; // NUL terminator
                let val_start = pos + 8 + name_len + 1;
                buffer[val_start..val_start + val_len].copy_from_slice(&value);
                // Zero alignment padding.
                for b in buffer[val_start + val_len..pos + entry_aligned].iter_mut() {
                    *b = 0;
                }

                last_offset_pos = Some(pos);
                pos += entry_aligned;
            }

            Ok(pos as u32)
        })
    }

    /// Parse a `FILE_FULL_EA_INFORMATION` buffer from Windows and persist
    /// each entry as an APFS xattr via `set_xattr` / `delete_xattr`.
    /// A zero-length value means "delete this attribute". `file_info` is
    /// refreshed from the live FS so Windows sees correct attributes.
    fn set_extended_attributes(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        guard(|| {
            // Resolve inode once before entering any transaction.
            let ino: u64 = {
                let mut g = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = g
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                fs.lookup(&context.path)
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?
            };

            // Walk the packed FILE_FULL_EA_INFORMATION list.
            let mut offset: usize = 0;
            loop {
                // Minimum header: 4+1+1+2 = 8 bytes.
                if offset + 8 > buffer.len() {
                    break;
                }
                let next_offset = u32::from_le_bytes(
                    buffer[offset..offset + 4].try_into().unwrap_or([0; 4]),
                ) as usize;
                let name_len = buffer[offset + 5] as usize;
                let val_len = u16::from_le_bytes(
                    buffer[offset + 6..offset + 8].try_into().unwrap_or([0; 2]),
                ) as usize;

                let name_start = offset + 8;
                let name_end = name_start + name_len;
                let val_start = name_end + 1; // skip NUL terminator
                let val_end = val_start + val_len;

                if val_end > buffer.len() {
                    break;
                }

                let name_bytes = &buffer[name_start..name_end];
                let name = match std::str::from_utf8(name_bytes) {
                    Ok(s) => s.to_string(),
                    Err(_) => {
                        // Non-UTF-8 EA name — skip (APFS names are UTF-8).
                        if next_offset == 0 {
                            break;
                        }
                        offset += next_offset;
                        continue;
                    }
                };
                let value = &buffer[val_start..val_end];

                if value.is_empty() {
                    // Zero-length value = delete attribute (idempotent).
                    self.with_writable(|txn, vsb_raw, omap_raw| {
                        apfs_write_file::delete_xattr(txn, vsb_raw, omap_raw, ino, &name)
                    })?;
                } else {
                    self.with_writable(|txn, vsb_raw, omap_raw| {
                        apfs_write_file::set_xattr(txn, vsb_raw, omap_raw, ino, &name, value)
                    })?;
                }

                if next_offset == 0 {
                    break;
                }
                offset += next_offset;
            }

            // Refresh file_info from live FS.
            let mut g = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = g
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            if let Ok(Some(attr)) = fs.getattr(&context.path) {
                Self::fill_info(&attr, file_info);
            }
            Ok(())
        })
    }

    // -----------------------------------------------------------------------
    // DeviceIoControl / FSCTL dispatch
    // -----------------------------------------------------------------------
    //
    // FSCTL_DUPLICATE_EXTENTS_TO_FILE (0x000940CF) — Windows 11+ server-side
    // copy: `cp --reflink` / `Copy-Item -Reflink` emits this FSCTL.
    //
    // Input layout: DUPLICATE_EXTENTS_DATA {
    //   HANDLE FileHandle;               8 bytes (source file handle)
    //   LARGE_INTEGER SourceFileOffset;  8 bytes
    //   LARGE_INTEGER TargetFileOffset;  8 bytes
    //   LARGE_INTEGER ByteCount;         8 bytes (0 = whole file)
    // }
    //
    // UPSTREAM-BLOCKED (W6-GAP-2, WinFsp 0.13):
    // WinFsp does not resolve the source HANDLE to a path before dispatching
    // the control callback — the raw kernel HANDLE value arrives in `input`.
    // Resolving it requires NtQueryInformationFile / GetFinalPathNameByHandle
    // in-process on Windows, but from a WinFsp user-mode callback there is no
    // platform helper for this. Until WinFsp exposes a handle→path API the
    // clone path cannot be completed; STATUS_NOT_SUPPORTED is returned so the
    // caller (robocopy, Copy-Item -Reflink) falls back to a regular byte copy
    // transparently — no data loss, just no CoW sharing.
    //
    // When WinFsp gains handle resolution support, the path here becomes:
    //   1. Parse FileHandle from input[0..8].
    //   2. Resolve to source path via WinFsp helper.
    //   3. resolve_parent_and_name for src + dst.
    //   4. with_writable → apfs_write_file::clone_file(txn, vsb, omap,
    //        src_parent, src_name, dst_parent, dst_name).
    //
    // See: docs/refs/winfsp-wiring-status.md

    /// DeviceIoControl / FSCTL dispatch. Handles FSCTL_DUPLICATE_EXTENTS_TO_FILE
    /// (0x000940CF) with an upstream-blocked diagnostic; all other codes return
    /// STATUS_INVALID_DEVICE_REQUEST (the winfsp-rs trait default).
    fn control(
        &self,
        _context: &Self::FileContext,
        control_code: u32,
        _input: &[u8],
        _output: &mut [u8],
    ) -> winfsp::Result<u32> {
        // FSCTL_DUPLICATE_EXTENTS_TO_FILE = 0x000940CF
        // Upstream-blocked: WinFsp 0.13 does not resolve the source HANDLE to
        // a path inside the callback. Return STATUS_NOT_SUPPORTED so callers
        // fall back to a normal byte copy transparently.
        const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 0x000940CF;
        const STATUS_NOT_SUPPORTED: i32 = i32::from_ne_bytes((0xC000_00BBu32).to_ne_bytes());
        if control_code == FSCTL_DUPLICATE_EXTENTS_TO_FILE {
            eprintln!(
                "control: FSCTL_DUPLICATE_EXTENTS_TO_FILE received; \
                 upstream-blocked (WinFsp 0.13 handle-to-path gap) — \
                 returning STATUS_NOT_SUPPORTED, caller will use byte copy"
            );
            return Err(winfsp::FspError::NTSTATUS(STATUS_NOT_SUPPORTED));
        }
        Err(winfsp::FspError::NTSTATUS(STATUS_NOT_IMPLEMENTED))
    }
}

/// Guarantee `winfsp-x64.dll` resolves for the delay-loaded WinFsp import
/// BEFORE the first winfsp call. WinFsp installs the DLL under its own
/// program directory, which is not on any DLL search path the winfsp-rs
/// loader honors (`SetDllDirectory` is ignored by its restricted
/// `LoadLibraryEx` search), so the delay-loaded import fails with
/// ERROR_DELAY_LOAD_FAILED (Win32 1285).
///
/// The executable's own directory is ALWAYS searched first, in every
/// `LoadLibrary` mode. So we self-provision: locate the installed
/// `winfsp-x64.dll` (via `$WINFSP_BIN` or the standard install dirs) and
/// copy it next to the running executable if absent. Best-effort and fully
/// automatic — no PATH, no admin, no manual setup. (A packaged build ships
/// the DLL beside the exe anyway; this also self-heals a dev/run tree.)
pub(crate) fn ensure_winfsp_dll() {
    let exe_dir = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
    {
        Some(d) => d,
        None => return,
    };
    let dest = exe_dir.join("winfsp-x64.dll");
    if dest.is_file() {
        return;
    }
    let mut sources: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("WINFSP_BIN") {
        sources.push(std::path::PathBuf::from(p).join("winfsp-x64.dll"));
    }
    for var in ["ProgramFiles(x86)", "ProgramW6432", "ProgramFiles"] {
        if let Ok(pf) = std::env::var(var) {
            sources.push(
                std::path::Path::new(&pf)
                    .join("WinFsp")
                    .join("bin")
                    .join("winfsp-x64.dll"),
            );
        }
    }
    for src in sources {
        if src.is_file() {
            let _ = std::fs::copy(&src, &dest);
            return;
        }
    }
}

/// Build the device stack and mount the APFS container at `mountpoint`
/// (e.g. `"Z:"`). If `drive` is `None`, auto-detect the APFS USB.
/// When `writable` is true, the underlying PhysicalDrive is opened with
/// GENERIC_WRITE and the VolumeParams advertises a writable volume.
/// (Read-only by default — safe default for production mounts.)
///
/// Returns `(host, dirty_arc)`. The caller can poll `dirty_arc` to detect
/// uncommitted batched writes for the tray dirty indicator.
pub fn mount_apfs(
    drive: Option<u32>,
    mountpoint: &str,
    writable: bool,
    // winfsp 0.13: FileSystemHost::new returns FileSystemHost<T, FineGuard> (default S)
) -> winfsp::Result<(FileSystemHost<ApfsFs<Dev>, FineGuard>, std::sync::Arc<std::sync::atomic::AtomicBool>)> {
    // Self-provision the WinFsp DLL beside the exe before ANY winfsp call,
    // so the delay-loaded import resolves (else Win32 1285).
    ensure_winfsp_dll();
    let init = winfsp::winfsp_init().map_err(io_err("winfsp_init"))?;
    let _ = init;
    let (pd, range) = match drive {
        Some(n) => {
            let mut pd = if writable {
                PhysicalDrive::open_rw(n).map_err(io_err("PhysicalDrive::open_rw"))?
            } else {
                PhysicalDrive::open(n).map_err(io_err("PhysicalDrive::open"))?
            };
            let r =
                crate::gpt::find_apfs_container(&mut pd).map_err(io_err("find_apfs_container"))?;
            (pd, r)
        }
        None => {
            // Autodetect always opens read-only first. If writable was
            // requested, reopen the matching drive number in RW mode.
            let (pd_ro, r) = PhysicalDrive::autodetect_apfs().map_err(io_err("autodetect_apfs"))?;
            if writable {
                // Re-open the same device id with write access. The
                // autodetect helper currently does not expose its drive
                // index; we re-scan to find it (cheap — only a few drives).
                drop(pd_ro);
                let mut found = None;
                for n in 0..16u32 {
                    if let Ok(mut p) = PhysicalDrive::open_rw(n) {
                        if let Ok(rr) = crate::gpt::find_apfs_container(&mut p) {
                            found = Some((p, rr));
                            break;
                        }
                    }
                }
                found.ok_or_else(|| {
                    io_err::<&str>("autodetect_apfs (rw rescan)")("no writable APFS drive found")
                })?
            } else {
                (pd_ro, r)
            }
        }
    };
    let dev = CachingBlockDevice::new(OffsetBlockDevice::new(pd, range.offset, range.length));
    let fsview = FsView::open(dev).map_err(io_err("FsView::open"))?;

    let mut vp = VolumeParams::new();
    vp.filesystem_name("APFS")
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .read_only_volume(!writable)
        // Flags mirroring the WinFsp memfs reference so the kernel
        // driver post-dispatches Cleanup / SetDelete the same way it
        // does for memfs — without these, our Cleanup callback only
        // receives flags=0x20 (SetLastAccessTime) on every read and
        // never sees FspCleanupDelete (0x01), so Remove-Item silently
        // fails. memfs.cpp sets these together:
        //   PostCleanupWhenModifiedOnly = 1   -> Cleanup only on modify
        //   PassQueryDirectoryFileName  = 1   -> name in ReadDirectory queries
        // (See WinFsp memfs.cpp VolumeParams init block.)
        .post_cleanup_when_modified_only(true)
        .pass_query_directory_filename(true)
        // memfs sets persistent_acls = 1. Empirically required for the
        // Windows kernel to route CreateFile through our Create callback;
        // without it, CreateFile(CREATE_ALWAYS) returns ERROR_PATH_NOT_FOUND
        // before our filesystem is consulted.
        .persistent_acls(true)
        // Drop OS caches on cleanup so the next CreateFile reaches our
        // open/set_delete callbacks instead of being satisfied from
        // a stale dir cache.
        .flush_and_purge_on_cleanup(true);
    // NOTE: WinFsp 2.1 SDK (winfsp-sys 0.12 / winfsp 0.13) does not expose a
    // RemovableMedia bitfield in FSP_FSCTL_VOLUME_PARAMS — the field was present
    // in earlier WinFsp versions but is absent from the 2.1 bindgen output.
    // Removable-media reporting (DriveType=2, "Safely Remove Hardware" tray)
    // is a deferred enhancement tracked in the M13 roadmap.

    // winfsp 0.13: turbofish pins S=FineGuard so start() call is unambiguous
    let fs = ApfsFs::new(fsview);
    let dirty_arc = fs.dirty_arc();
    let mut host = FileSystemHost::<ApfsFs<Dev>, FineGuard>::new(vp, fs)
        .map_err(io_err("FileSystemHost::new"))?;
    host.mount(mountpoint).map_err(io_err("host.mount"))?;
    host.start().map_err(io_err("host.start"))?;
    Ok((host, dirty_arc))
}

// ---------------------------------------------------------------------------
// Status-file helpers: the mount process writes a tiny JSON status file to
// %TEMP% so the tray process can poll dirty/clean state without a full IPC
// channel. Filename: `apfs-status-<drive>.json`.
// ---------------------------------------------------------------------------

/// Returns the path of the status file for a given physical-drive number.
/// Uses `%TEMP%` (falls back to the system temp dir).
pub fn status_file_path(drive: u32) -> std::path::PathBuf {
    let dir = std::env::var("TEMP")
        .or_else(|_| std::env::var("TMP"))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    dir.join(format!("apfs-status-{drive}.json"))
}

/// Write `{"drive":<n>,"mount":"<letter>","dirty":<bool>}` to the status file.
/// Errors are silently ignored — status display is best-effort.
pub fn write_status_file(
    path: &std::path::Path,
    drive: u32,
    mount: &str,
    dirty: bool,
) -> std::io::Result<()> {
    let json = format!(
        "{{\"drive\":{drive},\"mount\":{mount:?},\"dirty\":{dirty}}}\n"
    );
    std::fs::write(path, json)
}
