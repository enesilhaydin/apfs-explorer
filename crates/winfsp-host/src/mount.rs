//! WinFsp read-only host delegating to `apfs::FsView` (Windows-only).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use apfs::FsView;
use apfs_core::block_device::WritableBlockDevice;
use apfs_write::file as apfs_write_file;
use apfs_write::txn::{Transaction, TxnError};

use crate::group::{
    commit_group, write_in_group_as, GroupClock, GroupDev, GroupError, Names, Policy,
};
use crate::ops::{apply_op, HostOp, OpOutcome};
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
use winfsp::host::{FileSystemHost, FileSystemParams, FineGuard, VolumeParams};

use crate::cache::CachingBlockDevice;
use crate::offset_dev::OffsetBlockDevice;
use crate::winraw::PhysicalDrive;

// Device stack: raw drive -> rebase to container -> automatic read-ahead/LRU
// cache -> FsView. The cache turns the APFS traversal's many tiny random
// reads into few large sequential ones (essential on slow USB, neutral on
// fast SSD); fully automatic, no tuning knobs.
type Dev = CachingBlockDevice<OffsetBlockDevice<PhysicalDrive>>;

const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034u32 as i32;
const STATUS_MEDIA_WRITE_PROTECTED: i32 = 0xC00000A2u32 as i32;
const STATUS_OBJECT_NAME_INVALID: i32 = 0xC0000033u32 as i32;
const STATUS_NOT_IMPLEMENTED: i32 = 0xC0000002u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185u32 as i32;
const STATUS_DISK_FULL: i32 = 0xC000007Fu32 as i32;
const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC0000035u32 as i32;
const STATUS_DIRECTORY_NOT_EMPTY: i32 = 0xC0000101u32 as i32;
const STATUS_NOT_SUPPORTED_OP: i32 = 0xC00000BBu32 as i32;
const STATUS_INVALID_PARAMETER_OP: i32 = 0xC000000Du32 as i32;
const STATUS_FILE_CORRUPT_ERROR: i32 = 0xC0000102u32 as i32;
const STATUS_FILE_IS_OFFLINE: i32 = 0xC0000267u32 as i32;

/// What a write operation's failure means to Windows.
///
/// Everything used to come back as a device error, so a full volume, a name
/// that already exists, or an operation the engine refuses all looked to the
/// caller like failing hardware - and applications report it that way. The
/// cases that have an established meaning get it; anything genuinely unknown
/// still says the device failed.
fn ntstatus_for(err: &apfs_write::txn::TxnError) -> i32 {
    use apfs_write::txn::TxnError as E;
    match err {
        E::NoFreeBlocks => STATUS_DISK_FULL,
        E::AlreadyExists(_) => STATUS_OBJECT_NAME_COLLISION,
        E::DirectoryNotEmpty(_) => STATUS_DIRECTORY_NOT_EMPTY,
        E::NotFound(_) | E::SnapshotNotFound(_) => STATUS_OBJECT_NAME_NOT_FOUND,
        E::Unsupported(_) => STATUS_NOT_SUPPORTED_OP,
        E::InvalidArgument(_) => STATUS_INVALID_PARAMETER_OP,
        _ => STATUS_IO_DEVICE_ERROR,
    }
}

/// What a failed stat or read of a file means to Windows.
///
/// A compressed file whose content is not on the volume (macOS keeps such
/// dataless files for cloud eviction) is offline, not broken; an encoding
/// this build cannot decode is not supported; a damaged decmpfs record is a
/// corrupt file. Everything else still says the device failed.
fn ntstatus_for_fs(err: &apfs::FsError) -> i32 {
    use apfs_core::container::ContainerError as C;
    use apfs_core::decmpfs::DecmpfsError as D;
    match err {
        apfs::FsError::Container(C::Decmpfs(D::Dataless(_))) => STATUS_FILE_IS_OFFLINE,
        apfs::FsError::Container(C::Decmpfs(D::UnsupportedMethod(_))) => STATUS_NOT_SUPPORTED_OP,
        apfs::FsError::Container(C::Decmpfs(_)) => STATUS_FILE_CORRUPT_ERROR,
        _ => STATUS_IO_DEVICE_ERROR,
    }
}

/// Map a failed stat or read to its Windows meaning, keeping the real cause
/// on stderr; `step` names the operation.
fn fs_err(step: &str) -> impl FnOnce(apfs::FsError) -> winfsp::FspError + '_ {
    move |e| {
        eprintln!("mount_apfs: {step} failed: {e:?}");
        winfsp::FspError::NTSTATUS(ntstatus_for_fs(&e))
    }
}

/// Map a fallible mount step to a WinFsp IO error while surfacing the real
/// underlying cause to stderr. WinFsp only carries an opaque NTSTATUS, so
/// without this the genuine failure (bad device, no APFS, mount denied,
/// ...) would be swallowed - a diagnostic dead end and an error-swallowing
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
/// `SYMLINK_FLAG_RELATIVE` (winnt.h) - set on the reparse buffer when the
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
/// FILE_ATTRIBUTE_NORMAL - set on regular files in writable mounts so
/// Windows does NOT block DELETE access. Previously we set READONLY
/// unconditionally (legacy from the read-only mount era), which made
/// Windows reject set_delete on every Remove-Item before reaching our
/// FS. With NORMAL, Windows dispatches the delete protocol through.
#[allow(dead_code)]
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// Generic over the underlying device type so the same ApfsFs can host
/// either a `PhysicalDrive`-backed mount (real disk) or - once #45 lands
/// - a `FileBlockDeviceRw`-backed mount (--image). The trait bound is
/// `WritableBlockDevice` so the write callbacks (Step D) can call
/// `write_at` directly; a read-only mount still uses a writable-trait
/// device whose `write_at` returns an error at runtime.
///
/// Write operations are grouped (see `crate::group`): each runs at once in
/// the open write transaction and reports its own outcome, and the reader
/// reads through that transaction, so every lookup sees every operation
/// already reported done. The group is committed when it has been idle for
/// a moment, has been open for a few seconds or holds many operations, on an
/// explicit flush, and at unmount - the volume on disk is always one
/// checkpoint or the next. The FsView is stored inside `Option` so a commit
/// can take it, commit, and reopen it on the plain device.
pub struct ApfsFs<D: WritableBlockDevice + Send + 'static> {
    fs: Arc<Mutex<Option<View<D>>>>,
    /// The open group's operation count and times.
    clock: Arc<Mutex<GroupClock>>,
    policy: Policy,
    /// How many open handles hold written content that has not reached the
    /// disk yet: a file's bytes wait in its handle until it is cleaned up.
    dirty_handles: Arc<AtomicUsize>,
    /// Whether a group holds operations not yet committed.
    group_open: Arc<AtomicBool>,
    /// Shared dirty flag: `true` while a handle holds written content or a
    /// group is open. The tray process reads the corresponding status file
    /// (written by the mount loop) instead of accessing this directly; this
    /// field drives that file.
    dirty: Arc<AtomicBool>,
    /// #137/BUG-02 fallback: set `true` only when a begin/commit failure left
    /// us unable to even reopen a read-only FsView from the recovered device
    /// (so the mount has no live view). While `true`, `with_writable` fails
    /// fast instead of touching a half-dead mount. The common write-error case
    /// stays recoverable (device handed back via begin/commit_recoverable, a
    /// fresh FsView is reopened, mount keeps serving read-only).
    broken: Arc<AtomicBool>,
    /// Tells the flusher thread to stop.
    stop: Arc<AtomicBool>,
    /// Commits a group that has gone idle or old.
    flusher: Option<std::thread::JoinHandle<()>>,
    /// The mount was not opened for writing, or was turned read-only (a
    /// volume with snapshots). WinFsp's read-only volume flag does not stop
    /// the write callbacks, so every one checks this itself: without it a
    /// read-only mount took writes it could never commit, and the snapshot
    /// guard's mount wrote to the disk.
    read_only: bool,
}

/// The reader of a mounted volume: on the plain device, or through the
/// open write transaction.
pub type View<D> = FsView<GroupDev<D>>;

/// Directory entries read_directory takes from the listing at a time; a
/// buffer holds a few hundred.
const DIR_PAGE: usize = 256;

/// How often the flusher looks at the open group.
const FLUSH_TICK: Duration = Duration::from_millis(50);

/// The kernel's info and data caching window when `APFS_INFO_TIMEOUT_MS` is
/// not set. Measured through WinFsp: renaming 300 files 364 -> 258 ms and
/// removing 150 473 -> 135 ms (NTFS 265 and 125).
const DEFAULT_INFO_TIMEOUT_MS: u32 = 1000;

/// The longest window `APFS_INFO_TIMEOUT_MS` may ask for. WinFsp turns
/// on data caching when the window never expires (`u32::MAX`), and with it
/// on, CopyFile of 4 GiB or more hung in the kernel: the stuck copies could
/// not be ended and held their unwritten cache pages (20 GB of memory)
/// until Windows restarted. No setting may get there.
const MAX_INFO_TIMEOUT_MS: u32 = 60_000;

/// The window to use for `APFS_INFO_TIMEOUT_MS` = `setting`: the default
/// when unset or unreadable, never more than [`MAX_INFO_TIMEOUT_MS`].
fn info_timeout_ms(setting: Option<&str>) -> u32 {
    setting
        .and_then(|v| v.trim().parse::<u32>().ok())
        .unwrap_or(DEFAULT_INFO_TIMEOUT_MS)
        .min(MAX_INFO_TIMEOUT_MS)
}

/// Flusher ticks between writes of the `APFS_STATS` counts (one second).
const STATS_DUMP_TICKS: u32 = 20;

/// Commits the open group; run before the process is stopped from outside
/// (the launcher stops a mount by ending its process, which runs no
/// destructors). One mount per process, so one hook.
static ON_STOP: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();

/// How long the kernel may answer file, directory and security queries from
/// what it was last told, in milliseconds (`APFS_INFO_TIMEOUT_MS`; 0 asks
/// the mount every time). Every change goes through the mount, so what the
/// kernel keeps stays current.
///
/// File information is kept for this window, not without limit. WinFsp
/// caches file data only when it never expires, and that read faster (a
/// second read of a file came from memory), but with it on, a copy of a
/// file of 4 GiB or more made with CopyFile (Copy-Item, Explorer) waited
/// forever in the kernel and never reached the mount, while the same size
/// written through a FileStream went through. Without data caching such
/// copies complete.
pub fn apply_info_timeouts(vp: &mut VolumeParams) {
    let ms = info_timeout_ms(std::env::var("APFS_INFO_TIMEOUT_MS").ok().as_deref());
    if ms > 0 {
        vp.file_info_timeout(ms)
            .dir_info_timeout(ms)
            .security_timeout(ms)
            .volume_info_timeout(ms)
            .flush_and_purge_on_cleanup(true);
    }
}

/// Host options shared by both mounts: register get_dir_info_by_name, which
/// PassQueryDirectoryFileName sends single-name queries to.
pub fn host_params(vp: VolumeParams) -> FileSystemParams {
    let mut params = FileSystemParams::default_params(vp);
    params.use_dir_info_by_name = true;
    params
}

/// Commit the open write group before the process goes away.
pub fn commit_before_exit() {
    if let Some(f) = ON_STOP.get() {
        f();
    }
    crate::stats::dump();
}

/// Recompute the shared dirty flag.
fn refresh_dirty(dirty: &AtomicBool, handles: &AtomicUsize, group_open: &AtomicBool) {
    dirty.store(
        handles.load(Ordering::SeqCst) > 0 || group_open.load(Ordering::SeqCst),
        Ordering::Relaxed,
    );
}

/// Commit the open group when `when` says so, and reopen the reader on the
/// plain device. Locks the view, then the clock - the order every path
/// takes. A commit that fails loses the group, as a power cut would; the
/// volume stays at the checkpoint before it, and the reader is reopened
/// from there.
fn commit_group_if<D: WritableBlockDevice>(
    fs: &Mutex<Option<View<D>>>,
    clock: &Mutex<GroupClock>,
    broken: &AtomicBool,
    group_open: &AtomicBool,
    when: impl FnOnce(&GroupClock) -> bool,
) -> Result<(), i32> {
    let mut g = fs.lock().map_err(|_| STATUS_IO_DEVICE_ERROR)?;
    let mut c = clock.lock().map_err(|_| STATUS_IO_DEVICE_ERROR)?;
    if !when(&c) {
        return Ok(());
    }
    let _t = crate::stats::timed("commit");
    let view = g.take().ok_or(STATUS_IO_DEVICE_ERROR)?;
    let sel = view.selector();
    let (committed, reopened) = commit_group(view, sel, &mut c);
    group_open.store(false, Ordering::SeqCst);
    match reopened {
        Ok(v) => *g = Some(v),
        Err(e) => {
            broken.store(true, Ordering::SeqCst);
            eprintln!("commit: reopening the volume after the commit failed: {e:?}");
        }
    }
    committed.map_err(|e| {
        eprintln!("commit: the write group was lost: {e:?}");
        STATUS_IO_DEVICE_ERROR
    })
}

/// Per-open handle: the normalized `/`-path, the content written through it
/// that has not reached the disk yet, and its delete-on-close disposition.
/// Reads go to the device for the range asked; nothing is cached here.
pub struct Handle {
    path: String,
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

/// One-line label for a catalog op, used by the #156 self-verify forensic log.
fn describe_op(op: &HostOp) -> String {
    use HostOp::*;
    match op {
        Rename {
            parent_ino,
            old_name,
            new_name,
            ..
        } => {
            format!("rename(parent={parent_ino}, {old_name} -> {new_name})")
        }
        MoveEntry {
            old_parent_ino,
            old_name,
            new_parent_ino,
            new_name,
            ..
        } => {
            format!("move({old_parent_ino}/{old_name} -> {new_parent_ino}/{new_name})")
        }
        SetInodeAttrs {
            parent_ino, name, ..
        } => {
            format!("setattr(parent={parent_ino}, {name})")
        }
        TruncateFast {
            parent_ino,
            name,
            new_size,
            ..
        } => {
            format!("truncate(parent={parent_ino}, {name}, size={new_size})")
        }
    }
}

impl<D: WritableBlockDevice + Send + 'static> ApfsFs<D> {
    pub fn new(fs: View<D>) -> Self {
        let fs = Arc::new(Mutex::new(Some(fs)));
        let clock = Arc::new(Mutex::new(GroupClock::default()));
        let policy = Policy::default();
        let dirty_handles = Arc::new(AtomicUsize::new(0));
        let group_open = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicBool::new(false));
        let broken = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let flusher = {
            let (fs, clock, broken, stop) =
                (fs.clone(), clock.clone(), broken.clone(), stop.clone());
            let (dirty, dirty_handles, group_open) =
                (dirty.clone(), dirty_handles.clone(), group_open.clone());
            std::thread::Builder::new()
                .name("apfs-flusher".into())
                .spawn(move || {
                    let mut ticks: u32 = 0;
                    while !stop.load(Ordering::SeqCst) {
                        std::thread::sleep(FLUSH_TICK);
                        // The counts also reach their file while the mount
                        // runs, so a mount that is killed or stuck still
                        // shows what it was doing.
                        ticks = ticks.wrapping_add(1);
                        if ticks % STATS_DUMP_TICKS == 0 {
                            crate::stats::dump();
                        }
                        let due = |c: &GroupClock| c.due(Instant::now(), &policy);
                        if let Err(status) = commit_group_if(&fs, &clock, &broken, &group_open, due)
                        {
                            eprintln!("flusher: commit failed ({status:#x})");
                        }
                        refresh_dirty(&dirty, &dirty_handles, &group_open);
                    }
                })
                .map_err(|e| eprintln!("flusher thread not started: {e}"))
                .ok()
        };
        {
            let (fs, clock, broken, group_open) = (
                fs.clone(),
                clock.clone(),
                broken.clone(),
                group_open.clone(),
            );
            let hook: Box<dyn Fn() + Send + Sync> = Box::new(move || {
                if let Err(status) =
                    commit_group_if(&fs, &clock, &broken, &group_open, GroupClock::is_open)
                {
                    eprintln!("stop: the last write group was not committed ({status:#x})");
                }
            });
            if ON_STOP.set(hook).is_err() {
                eprintln!("stop: a second mount in this process has no commit-on-stop hook");
            }
        }
        Self {
            fs,
            clock,
            policy,
            dirty_handles,
            group_open,
            dirty,
            broken,
            stop,
            flusher,
            read_only: false,
        }
    }

    /// Serve the volume read-only: every write callback answers that the
    /// medium is write-protected.
    pub fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// The answer of every write callback on a read-only mount.
    fn refuse_if_read_only(&self) -> winfsp::Result<()> {
        if self.read_only {
            return Err(winfsp::FspError::NTSTATUS(STATUS_MEDIA_WRITE_PROTECTED));
        }
        Ok(())
    }

    /// Commit the open group now, if there is one.
    fn commit_now(&self) -> winfsp::Result<()> {
        let r = commit_group_if(
            &self.fs,
            &self.clock,
            &self.broken,
            &self.group_open,
            GroupClock::is_open,
        );
        refresh_dirty(&self.dirty, &self.dirty_handles, &self.group_open);
        r.map_err(winfsp::FspError::NTSTATUS)
    }

    /// Returns `true` while some open handle holds written content that has
    /// not reached the disk, or a write group is not committed yet. The tray
    /// supervisor polls this to drive the dirty/clean indicator.
    pub fn is_dirty(&self) -> bool {
        self.dirty_handles.load(Ordering::SeqCst) > 0 || self.group_open.load(Ordering::SeqCst)
    }

    /// Returns a clone of the shared dirty flag. The mount loop in `main.rs`
    /// holds this arc and writes it to the status file on each poll tick so
    /// the tray process can display a dirty/clean indicator.
    pub fn dirty_arc(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.dirty.clone()
    }

    /// Write `buffer` into the file of `context` on the volume, as one
    /// operation of the open write group. `write_to_eof` writes at the end;
    /// `constrained_io` (paging I/O) never makes the file longer. Returns
    /// the bytes written, or `None` when the range writer declines the file
    /// and nothing changed.
    fn write_in_place(
        &self,
        context: &Handle,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<Option<u32>> {
        let (parent_ino, name) = self
            .resolve_parent_and_name(None, &context.path)
            .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
        let size = self.size_of(&context.path)?;
        let start = if write_to_eof { size } else { offset };
        let data = if constrained_io {
            let room = usize::try_from(size.saturating_sub(start)).unwrap_or(usize::MAX);
            buffer.get(..buffer.len().min(room)).unwrap_or(&[])
        } else {
            buffer
        };
        let write = || {
            self.with_writable_keeping_names(|txn, vsb_raw, omap_raw| {
                apfs_write_file::write_range(txn, vsb_raw, omap_raw, parent_ino, &name, start, data)
            })
        };
        let mut outcome = write()?;
        if outcome == apfs_write_file::RangeWrite::Declined
            && self.own_plain_stream(&context.path, parent_ino, &name)?
        {
            outcome = write()?;
        }
        if outcome == apfs_write_file::RangeWrite::Declined {
            return Ok(None);
        }
        self.self_verify(&format!("write_range({name})"));
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
        Ok(Some(data.len() as u32))
    }

    /// Give the file at `path` a plain data stream of its own, the state
    /// `write_range` writes into, and say whether anything changed. A
    /// stream a macOS clone shares is given by reference (nothing copied);
    /// a compressed file is read back and written plain, as macOS does on
    /// the first write to one.
    fn own_plain_stream(&self, path: &str, parent_ino: u64, name: &str) -> winfsp::Result<bool> {
        if self.with_writable_keeping_names(|txn, vsb_raw, omap_raw| {
            apfs_write_file::unshare_stream(txn, vsb_raw, omap_raw, parent_ino, name)
        })? {
            return Ok(true);
        }
        let content = {
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let attr = fs
                .getattr(path)
                .map_err(fs_err("getattr"))?
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            if attr.bsd_flags & apfs_core::inode::UF_COMPRESSED == 0 {
                return Ok(false);
            }
            let size = usize::try_from(attr.size)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            fs.read(path, 0, size).map_err(fs_err("read"))?
        };
        self.with_writable_keeping_names(|txn, vsb_raw, omap_raw| {
            apfs_write_file::overwrite_existing_file(
                txn, vsb_raw, omap_raw, parent_ino, name, &content,
            )
        })?;
        Ok(true)
    }

    /// The size of the file at `path` as the volume has it.
    fn size_of(&self, path: &str) -> winfsp::Result<u64> {
        let mut fs_guard = self
            .fs
            .lock()
            .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
        let fs = fs_guard
            .as_mut()
            .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
        Ok(fs
            .getattr(path)
            .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
            .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?
            .size)
    }

    /// A handle's write buffer came into being (`true`) or was taken out of
    /// it (`false`); keep the count and the shared flag in step.
    fn note_write_buffer(&self, created: bool) {
        use std::sync::atomic::Ordering::SeqCst;
        if created {
            self.dirty_handles.fetch_add(1, SeqCst);
        } else {
            let _ = self
                .dirty_handles
                .fetch_update(SeqCst, SeqCst, |n| Some(n.saturating_sub(1)));
        }
        refresh_dirty(&self.dirty, &self.dirty_handles, &self.group_open);
    }

    /// Resolve `(parent_ino, canonical_on_disk_name)` from a WinFsp-supplied
    /// path. `file_name` is preferred (typically uppercase from the OS);
    /// `fallback_path` is used when WinFsp supplies None (cleanup callback).
    /// Returns None on any lookup failure - callers treat as "nothing to do".
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
        // The entry as the volume itself compares names: on a volume that
        // hashes them this reads only the records with the name's hash, so
        // the cost no longer grows with the directory (a write drops the
        // cached listings, and every callback after it listed again).
        if let Some((dir, e)) = fs.entry(&path).ok().flatten() {
            return Some((dir, e.name));
        }
        // Windows volumes are advertised case-insensitive (VolumeParams
        // case_sensitive_search=false) so the kernel hands us paths in
        // whichever case the user typed (often UPPERCASE). The on-disk
        // APFS catalog stores names verbatim - typically the lowercase
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
        // case-folding. ASCII-only eq would miss "schön" because the
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

    /// Run a write operation in the open group (opening one when none is).
    /// The closure receives the transaction plus the volume superblock and
    /// object-map header as the group has them. It lands whole or not at
    /// all: a failure reports its own status and leaves the group's other
    /// operations. A group that has reached its operation limit is committed
    /// here; otherwise the flusher thread or a flush commits it.
    fn with_writable<R, F>(&self, op: F) -> winfsp::Result<R>
    where
        F: FnOnce(&mut Transaction<D>, &[u8], &[u8]) -> Result<R, TxnError>,
    {
        self.with_writable_as(Names::Changed, op)
    }

    /// [`Self::with_writable`] for an operation that adds, removes and
    /// renames no entry (a write, a resize, times, attributes, an extended
    /// attribute): the mount's cached paths and listings stay valid.
    fn with_writable_keeping_names<R, F>(&self, op: F) -> winfsp::Result<R>
    where
        F: FnOnce(&mut Transaction<D>, &[u8], &[u8]) -> Result<R, TxnError>,
    {
        self.with_writable_as(Names::Kept, op)
    }

    fn with_writable_as<R, F>(&self, names: Names<'_>, op: F) -> winfsp::Result<R>
    where
        F: FnOnce(&mut Transaction<D>, &[u8], &[u8]) -> Result<R, TxnError>,
    {
        self.refuse_if_read_only()?;
        // #137/BUG-02: if a prior failure left the mount with no live view,
        // fail fast rather than touching a half-dead mount.
        if self.broken.load(Ordering::SeqCst) {
            return Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR));
        }
        let _t = crate::stats::timed("write_op");
        let (result, full) = {
            let wait = crate::stats::timed("write_op_lock_wait");
            let mut guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            drop(wait);
            let view = guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let mut clock = self
                .clock
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let result = write_in_group_as(view, &mut clock, names, op);
            self.group_open.store(clock.is_open(), Ordering::SeqCst);
            (result, clock.full(&self.policy))
        };
        refresh_dirty(&self.dirty, &self.dirty_handles, &self.group_open);
        match result {
            Ok(r) => {
                if full {
                    let _full = crate::stats::timed("write_op.commit_when_full");
                    self.commit_now()?;
                }
                Ok(r)
            }
            Err(GroupError::Txn(e)) => {
                eprintln!("with_writable: op failed: {e:?}");
                Err(winfsp::FspError::NTSTATUS(ntstatus_for(&e)))
            }
            Err(GroupError::Fs(e)) => {
                eprintln!("with_writable: reading the volume failed: {e:?}");
                Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))
            }
        }
    }

    /// Forensic self-check (#156): when `APFS_SELF_VERIFY=1`, walk the on-disk
    /// catalog after each committed op and assert the structural invariants whose
    /// violation produced the real-USB #156 corruption. `fsck_apfs` is macOS-only,
    /// so this is the Windows-runnable arbiter that pinpoints the FIRST corrupting
    /// op (scratch reproduction never triggered #156, so the real disk under the
    /// real WinFsp op-mix is where it must be caught). On a violation it appends
    /// `label` + the description to the self-verify log (env `APFS_SELF_VERIFY_LOG`,
    /// default a temp-dir file) and marks the mount broken to FREEZE further ops,
    /// so the first failure is captured instead of being buried by later writes.
    ///
    /// Read-only: walks the catalog as the open write group has it (reads go
    /// through the transaction; nothing is staged). No-op unless the env flag
    /// is set, and when no group is open (nothing changed since the check
    /// that ran before the last commit).
    fn self_verify(&self, label: &str) {
        if std::env::var("APFS_SELF_VERIFY").as_deref() != Ok("1") {
            return;
        }
        if self.broken.load(Ordering::SeqCst) {
            return;
        }
        let mut guard = match self.fs.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(view) = guard.as_mut() else {
            return;
        };
        let Ok((vsb_raw, omap_raw)) = view.read_vsb_omap_raw() else {
            return;
        };
        let GroupDev::Open(txn) = view.dev_mut() else {
            return;
        };
        let bsz = txn.nx.block_size as usize;
        let res = apfs_write_file::verify_fsroot(txn, &vsb_raw, &omap_raw, bsz);
        drop(guard);
        // Append a heartbeat for EVERY check (not only violations) so the log
        // confirms the self-check actually fired and shows the per-op tree health
        // progression - a silent "no violations" file is otherwise ambiguous
        // between "ran clean" and "never ran".
        let line = match &res {
            Ok(s) => format!(
                "[apfs self-verify] OK after {label}: {} nodes, {} records\n",
                s.nodes, s.records
            ),
            Err(e) => format!("[apfs self-verify] VIOLATION after {label}: {e}\n"),
        };
        eprintln!("{}", line.trim_end());
        let path = std::env::var("APFS_SELF_VERIFY_LOG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir().join("apfs-self-verify.log"));
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        if res.is_err() {
            // Freeze the mount: later ops must not bury the first corrupting op.
            self.broken.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Free space on the volume, as the spaceman currently reports it.
    fn free_bytes(&self) -> winfsp::Result<u64> {
        let mut fs_guard = self
            .fs
            .lock()
            .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
        let fs = fs_guard
            .as_mut()
            .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
        // An open group has taken space the committed space manager does not
        // show yet; its own count does.
        if let GroupDev::Open(txn) = fs.dev_mut() {
            return Ok(txn
                .sm
                .free_count
                .saturating_mul(u64::from(txn.nx.block_size)));
        }
        // Unknown free space refuses no write: the allocator still stops one
        // the volume cannot hold.
        Ok(fs.volume_size().map(|(_, free)| free).unwrap_or_else(|e| {
            // Asked before every write: said once.
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!("free space unreadable, not checked: {e:?}");
            }
            u64::MAX
        }))
    }

    /// Refuse a write the volume cannot hold, before accepting it.
    ///
    /// The figure is what has been committed. The reserve covers the metadata
    /// the write drags along - the catalog nodes, the extent records - which
    /// is small next to any write big enough to matter but decides the last
    /// one.
    fn ensure_room_for(&self, bytes: u64) -> winfsp::Result<()> {
        const METADATA_RESERVE: u64 = 256 * 1024;
        let needed = bytes.saturating_add(METADATA_RESERVE);
        if self.free_bytes()? >= needed {
            return Ok(());
        }
        Err(winfsp::FspError::NTSTATUS(STATUS_DISK_FULL))
    }

    /// Resize a file by reading it back and writing it cut or padded, the path
    /// for a size the metadata-only resize declines (a cut inside a block, a
    /// shared or compressed stream).
    fn shrink_by_rewrite(
        &self,
        parent_ino: u64,
        name: &str,
        path: &str,
        new_size: u64,
    ) -> winfsp::Result<()> {
        // A read failure is propagated rather than silently zeroing the file.
        let current_bytes = {
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            // A path that is not there yet is a legitimately empty start; a
            // getattr that FAILS is not, and must not be read as "size zero" -
            // that turns a transient read error into a file full of zeros.
            let size = fs
                .getattr(path)
                .map_err(fs_err("getattr"))?
                .map(|a| a.size)
                .unwrap_or(0);
            if size > 0 {
                fs.read(path, 0, size as usize).map_err(fs_err("read"))?
            } else {
                Vec::new()
            }
        };
        let mut new_content = current_bytes;
        new_content.resize(new_size as usize, 0u8);
        self.with_writable_keeping_names(|txn, vsb_raw, omap_raw| {
            apfs_write_file::overwrite_existing_file(
                txn,
                vsb_raw,
                omap_raw,
                parent_ino,
                name,
                &new_content,
            )
        })?;
        Ok(())
    }

    /// Carry out a catalog operation now, in its own transaction, and hand
    /// its real outcome back to the caller.
    ///
    /// These operations used to be queued and replayed at a later flush. A
    /// rename whose target existed then failed inside that flush, where no
    /// caller could hear it: Windows had already been told the rename
    /// succeeded, the failure was logged, and the file kept its old name.
    /// Running the operation at once gives Windows the real status (a
    /// collision for an existing target, for one) while the request is
    /// still open, and keeps the on-disk order the order of the requests.
    fn apply_now(&self, op: HostOp) -> winfsp::Result<()> {
        let run = |txn: &mut Transaction<D>, vsb_raw: &[u8], omap_raw: &[u8]| {
            apply_op(&op, txn, vsb_raw, omap_raw)
        };
        let outcome = match op {
            HostOp::Rename { .. } | HostOp::MoveEntry { .. } => self.with_writable(run)?,
            HostOp::SetInodeAttrs { .. } | HostOp::TruncateFast { .. } => {
                self.with_writable_keeping_names(run)?
            }
        };
        if let OpOutcome::Truncate(apfs_write_file::TruncateOutcome::FastPathDeclined) = outcome {
            if let HostOp::TruncateFast {
                parent_ino,
                name,
                path,
                new_size,
                ..
            } = &op
            {
                self.shrink_by_rewrite(*parent_ino, name, path, *new_size)?;
            }
        }
        self.self_verify(&describe_op(&op));
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

        // A symbolic link's reparse data carries two names: the one the object
        // manager resolves (SubstituteName) and the one shown to the user
        // (PrintName). For a target on a drive they are not the same string -
        // resolution needs the NT form, `\??\Z:\dir\file`, and handing back
        // the plain `Z:\dir\file` gets ERROR_INVALID_NAME, which is what every
        // absolute link did. A relative target is the same in both, and is
        // marked as relative so it is resolved against the link's directory.
        let drive_absolute = {
            let b = win_path.as_bytes();
            b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\'
        };
        // A target that was absolute in POSIX terms means nothing to Windows;
        // it is passed through as-is rather than dressed up as an NT path that
        // would not resolve either.
        let posix_absolute = target_str.starts_with('/');
        let substitute = if drive_absolute {
            format!("\\??\\{win_path}")
        } else {
            win_path.clone()
        };
        let relative = !drive_absolute && !posix_absolute;

        let sub16: Vec<u16> = substitute.encode_utf16().collect();
        let print16: Vec<u16> = win_path.encode_utf16().collect();
        let sub_bytes = sub16.len() * 2;
        let print_bytes = print16.len() * 2;
        let total = 20usize
            .saturating_add(sub_bytes)
            .saturating_add(print_bytes);
        if out.len() < total {
            return Err(winfsp::FspError::NTSTATUS(STATUS_BUFFER_OVERFLOW));
        }
        out[0..4].copy_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
        // ReparseDataLength counts everything after the 8-byte header.
        out[4..6].copy_from_slice(&((12 + sub_bytes + print_bytes) as u16).to_le_bytes());
        out[6..8].copy_from_slice(&0u16.to_le_bytes());
        out[8..10].copy_from_slice(&0u16.to_le_bytes());
        out[10..12].copy_from_slice(&(sub_bytes as u16).to_le_bytes());
        out[12..14].copy_from_slice(&(sub_bytes as u16).to_le_bytes());
        out[14..16].copy_from_slice(&(print_bytes as u16).to_le_bytes());
        let flags = if relative {
            SYMLINK_FLAG_RELATIVE
        } else {
            0u32
        };
        out[16..20].copy_from_slice(&flags.to_le_bytes());
        for (i, &c) in sub16.iter().enumerate() {
            let p = 20 + i * 2;
            out[p..p + 2].copy_from_slice(&c.to_le_bytes());
        }
        for (i, &c) in print16.iter().enumerate() {
            let p = 20 + sub_bytes + i * 2;
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

/// At unmount the flusher stops and the open group is committed, so what
/// was reported done is on the disk when the volume goes away.
impl<D: WritableBlockDevice + Send + 'static> Drop for ApfsFs<D> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.flusher.take() {
            if t.join().is_err() {
                eprintln!("unmount: the flusher thread panicked");
            }
        }
        if let Err(status) = commit_group_if(
            &self.fs,
            &self.clock,
            &self.broken,
            &self.group_open,
            GroupClock::is_open,
        ) {
            eprintln!("unmount: the last write group was not committed ({status:#x})");
        }
        crate::stats::dump();
    }
}

impl<D: WritableBlockDevice + Send + 'static> FileSystemContext for ApfsFs<D> {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _sd: Option<&mut [std::ffi::c_void]>,
        resolve: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let _t = crate::stats::timed("get_security_by_name");
        guard(|| {
            // P10: if WinFsp's resolver reports a reparse point on the path
            // (e.g. an APFS symlink as an intermediate component), return its
            // security so the kernel resolves the link instead of treating it
            // as a plain file. Without this, traversal *through* a symlinked
            // directory fails even though the symlink leaf is visible.
            if let Some(reparse_security) = resolve(file_name) {
                return Ok(reparse_security);
            }
            let path = win_to_unix(file_name);
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            match fs.getattr(&path).map_err(fs_err("getattr"))? {
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
                    // `reparse` here does not mean "this file is a reparse
                    // point" - it means "the path could not be walked to the
                    // end because a reparse point is in the way", and then
                    // `attributes` carries the index in the path where it sits,
                    // not file attributes. That is what the resolver branch
                    // above returns. Saying `true` for a symlink leaf handed
                    // the kernel an attribute word to use as a path offset, and
                    // every open of a symlink came back "the name of the file
                    // cannot be resolved by the system". The leaf is reported
                    // as an ordinary file carrying the reparse attribute; the
                    // kernel then asks for its reparse data through
                    // get_reparse_point.
                    Ok(FileSecurity {
                        reparse: false,
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
        let _t = crate::stats::timed("open");
        guard(|| {
            let path = win_to_unix(file_name);
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let attr = fs
                .getattr(&path)
                .map_err(fs_err("getattr"))?
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            Self::fill_info(&attr, file_info.as_mut());
            Ok(Handle {
                path,
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
        let _t = crate::stats::timed("get_file_info");
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
                .map_err(fs_err("getattr"))?
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
        let _t = crate::stats::timed("read");
        guard(|| {
            // A handle reads back what it wrote: its content waits in the
            // handle until cleanup (seeded from the disk at the first write,
            // so it is the whole file as this handle sees it), and the disk
            // would still answer with the bytes from before.
            {
                let wb = context
                    .write_buf
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                if let Some(content) = wb.as_ref() {
                    let from = usize::try_from(offset)
                        .unwrap_or(usize::MAX)
                        .min(content.len());
                    let n = (content.len() - from).min(buffer.len());
                    buffer
                        .get_mut(..n)
                        .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                        .copy_from_slice(
                            content
                                .get(from..from + n)
                                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?,
                        );
                    return Ok(n as u32);
                }
            }
            // Only the bytes asked for: the reader walks the extents the range
            // touches, or decodes the chunks that cover it for a compressed
            // file, so a large file costs no more than the part being read
            // and no handle holds a copy of the whole file.
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let data = fs
                .read(&context.path, offset, buffer.len())
                .map_err(fs_err("read"))?;
            let n = data.len().min(buffer.len());
            buffer
                .get_mut(..n)
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                .copy_from_slice(
                    data.get(..n)
                        .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?,
                );
            Ok(n as u32)
        })
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        // WinFsp never forwards the search pattern to us (we do not set
        // PassQueryDirectoryPattern): the FSD always performs its own pattern
        // matching over the full entry set we emit (winfsp.h ReadDirectory:
        // "the file system can choose to ignore [Pattern] ..."). Empirically
        // confirmed: pattern arrives as None for every `dir *.txt` / -Filter
        // query. So filtered listings hinge entirely on emitting each entry's
        // FileNameLength correctly (see set_name_raw note below), not on this.
        _pattern: Option<&U16CStr>,
        marker: winfsp::filesystem::DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        let _t = crate::stats::timed("read_directory");
        guard(|| {
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            // winfsp 0.13: DirMarker::inner() -> Option<&[u16]> (UTF-16 units).
            // None = first call; Some = resume strictly after this name.
            let resume: Option<String> = marker.inner().map(String::from_utf16_lossy);
            tracing::debug!(path = %context.path, marker = ?resume, "read_directory");
            // WinFsp resumes after the last name it was given, comparing byte
            // for byte. APFS keeps most directories in name-hash order, so the
            // listing is paged in name order (read_dir_after); left in hash
            // order, a resumed listing dropped an arbitrary part of it. Only
            // the pages this buffer takes are copied out of the listing.
            let mut after = resume;
            let mut cursor = 0u32;
            let mut emitted = 0u32;
            let mut full = false;
            'pages: loop {
                let page = fs
                    .read_dir_after(&context.path, after.as_deref(), DIR_PAGE)
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                for e in &page {
                    let mut di = DirInfo::<255>::new();
                    // #153: emit the name via set_name_raw with NO trailing NUL.
                    // winfsp 0.13 `DirInfo::set_name` appends a NUL terminator and
                    // then folds it into the entry Size, so FSP_FSCTL_DIR_INFO's
                    // FileNameLength counts one extra wide char. The WinFsp FSD then
                    // pattern-matches against "name\0", which breaks any suffix glob:
                    // `*.txt` fails to match "a.txt\0" (it ends in \0, not "txt")
                    // while prefix globs like `b*` still match - exactly the
                    // "filtered listing returns empty" bug. FSP_FSCTL_DIR_INFO is
                    // length-prefixed (not NUL-terminated, like memfs), so emitting
                    // the bare UTF-16 units yields the correct FileNameLength and the
                    // FSD's `*.ext` matching works.
                    let name_utf16: Vec<u16> = e.name.encode_utf16().collect();
                    di.set_name_raw(name_utf16.as_slice())
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
                        // Buffer full - WinFsp will re-call with marker = the
                        // last name that fit.
                        full = true;
                        break 'pages;
                    }
                    emitted += 1;
                }
                if page.len() < DIR_PAGE {
                    break;
                }
                after = page.last().map(|e| e.name.clone());
            }
            // The end marker tells WinFsp the directory has nothing more.
            // Written after a full buffer it ended every listing at its
            // first buffer, some 500 names, though the rest still opened.
            if !full {
                DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
            }
            tracing::debug!(path = %context.path, emitted, "read_directory done");
            Ok(cursor)
        })
    }

    /// One name in a directory, for the query WinFsp sends when a program
    /// asks whether a name exists (a copy asks for every file it copies).
    /// Without it WinFsp answers by reading the whole directory in small
    /// pieces: copying 1000 files into one folder made ~100 000
    /// read_directory calls. The name is reported as the volume stores it.
    fn get_dir_info_by_name(
        &self,
        context: &Self::FileContext,
        file_name: &U16CStr,
        out_dir_info: &mut DirInfo,
    ) -> winfsp::Result<()> {
        let _t = crate::stats::timed("get_dir_info_by_name");
        guard(|| {
            let mut fs_guard = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let fs = fs_guard
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let name = file_name.to_string_lossy();
            let child = if context.path == "/" {
                format!("/{name}")
            } else {
                format!("{}/{name}", context.path)
            };
            let (_, stored) = fs
                .entry(&child)
                .map_err(fs_err("entry"))?
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            let attr = fs
                .getattr(&child)
                .map_err(fs_err("getattr"))?
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            let name_utf16: Vec<u16> = stored.name.encode_utf16().collect();
            out_dir_info.reset();
            out_dir_info
                .set_name_raw(name_utf16.as_slice())
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            Self::fill_info(&attr, out_dir_info.file_info_mut());
            Ok(())
        })
    }

    /// FlushFileBuffers from user-space (Notepad++ save-as, atomic save
    /// patterns), or a flush of the whole volume. What the operations so far
    /// have done is made durable: the open write group is committed. (A
    /// handle's own buffered content still goes in at Cleanup.) Returning
    /// the default STATUS_INVALID_DEVICE_REQUEST surfaces as "Incorrect
    /// function" on every save dialog. Refresh the session FileInfo from the
    /// live FS so the caller sees the latest size.
    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let _t = crate::stats::timed("flush");
        guard(|| {
            self.commit_now()?;
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

    /// Volume info - REQUIRED override. WinFsp's default returns
    /// STATUS_INVALID_DEVICE_REQUEST which surfaces to user-space as
    /// "Incorrect function" the moment `dir`, Explorer or any tool
    /// queries volume metadata (GetVolumeInformationW). The device's
    /// total size is the underlying block device size; free size we
    /// approximate from FsView (full APFS spaceman accounting is
    /// future work - for now the reported free is a safe lower bound
    /// so Explorer doesn't claim the volume is full).
    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> winfsp::Result<()> {
        guard(|| {
            let (total, free) = {
                let mut fs_guard = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = fs_guard
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                fs.volume_size().unwrap_or_else(|e| {
                    // Windows asks every few seconds: said once. The figures
                    // below are what Explorer then shows, so a failure here
                    // (a 4 TB drive's space manager, #23) is visible as one.
                    static WARNED: AtomicBool = AtomicBool::new(false);
                    if !WARNED.swap(true, Ordering::Relaxed) {
                        tracing::warn!("volume size unreadable, reporting 1 GiB: {e:?}");
                    }
                    (1u64 << 30, 1u64 << 29)
                })
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
            self.with_writable_keeping_names(|txn, vsb_raw, _omap_raw| {
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
    /// the cleanup path) currently do nothing - handled by SetBasicInfo.
    /// Write a chunk into the handle's dirty buffer. Flushed on Cleanup
    /// (non-delete) by unlink + apfs_write::write_file.
    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let _t = crate::stats::timed("write");
        self.refuse_if_read_only()?;
        guard(|| {
            // Refuse a write the volume has no room for, here, where the
            // caller can still be told. The content waits in the handle and
            // only reaches disk at cleanup, which has no way to return an error:
            // filling a volume this way used to leave 37 zero-length files
            // behind with every WriteAllBytes reporting success.
            self.ensure_room_for(buffer.len() as u64)?;
            // Written to the volume at once when the file allows it: only
            // the blocks the write touches change, nothing waits in the
            // handle, every handle reads what was written, and an error
            // reaches the caller. A file the range writer declines (it is
            // compressed, or a clone macOS made shares its stream) keeps its
            // content in the handle until cleanup.
            let buffered = context
                .write_buf
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                .is_some();
            if !buffered {
                if let Some(n) = self.write_in_place(
                    context,
                    buffer,
                    offset,
                    write_to_eof,
                    constrained_io,
                    file_info,
                )? {
                    return Ok(n);
                }
            }
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
                    // means the catalog claims real bytes - if the read
                    // then fails we MUST propagate the error rather than
                    // silently seed an empty buffer, otherwise the partial-
                    // overwrite about to happen would clobber the existing
                    // content with zeros + the user's write.
                    let size = fs
                        .getattr(&context.path)
                        .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
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
                self.note_write_buffer(true);
            }
            let buf = wb
                .as_mut()
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
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
    /// CLEARING the disposition (Windows allows toggling) - also OK.
    /// Create a new file or directory. `create_options & 0x01` is
    /// FILE_DIRECTORY_FILE → mkdir; else create_file. Returns the new
    /// Handle. file_info is populated from the post-commit FsView so
    /// Windows sees correct attributes immediately.
    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        // winfsp 0.13 trait uses winfsp_sys::FILE_ACCESS_RIGHTS /
        // FILE_FLAGS_AND_ATTRIBUTES type aliases for these - both are
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
        let _t = crate::stats::timed("create");
        self.refuse_if_read_only()?;
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
            self.self_verify(&format!(
                "{}({name})",
                if is_dir { "mkdir" } else { "create" }
            ));

            // After commit, FsView was reopened - populate file_info from
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
        if delete_file {
            self.refuse_if_read_only()?;
        }
        context
            .delete_on_close
            .store(delete_file, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn cleanup(&self, context: &Self::FileContext, file_name: Option<&U16CStr>, flags: u32) {
        let _t = crate::stats::timed("cleanup");
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
        // Step D - write flush path. When the handle's dirty buffer is
        // populated (Some) and this is a non-delete cleanup, the chunked
        // writes accumulated through write() are flushed to disk via
        // unlink + apfs_write::write_file (apfs_write::write_file rejects
        // existing names, so we delete the empty placeholder first).
        if !is_delete {
            let dirty = context.write_buf.lock().ok().and_then(|mut g| g.take()); // take so we don't re-flush
            if dirty.is_some() {
                self.note_write_buffer(false);
            }
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
                    // BUG-01: surface the write-flush result. Dropping it here
                    // silently loses the file's content on a USB stall /
                    // out-of-space / any write error.
                    if let Err(e) = self.with_writable_keeping_names(|txn, vsb_raw, omap_raw| {
                        apfs_write_file::overwrite_existing_file(
                            txn, vsb_raw, omap_raw, parent_ino, &name, &content,
                        )
                    }) {
                        tracing::error!(
                            "cleanup: write flush failed for {:?} ({} bytes): {:?}",
                            context.path,
                            clen,
                            e
                        );
                    }
                    self.self_verify(&format!("overwrite({name})"));
                } else {
                    tracing::error!(
                        "cleanup: could not resolve parent/name for {:?} - \
                         {} dirty bytes dropped without flush",
                        context.path,
                        content.len()
                    );
                }
            }
            return;
        }
        // delete branch (FspCleanupDelete = 0x01). Written content that was
        // never closed goes with the file.
        if context
            .write_buf
            .lock()
            .ok()
            .and_then(|mut g| g.take())
            .is_some()
        {
            self.note_write_buffer(false);
        }
        if let Some((parent_ino, canonical_name)) =
            self.resolve_parent_and_name(file_name, &context.path)
        {
            // BUG-01: surface the unlink result - a dropped error left a file
            // the user asked to delete still on disk with no indication.
            let removed = Names::Removed {
                parent: parent_ino,
                name: &canonical_name,
            };
            if let Err(e) = self.with_writable_as(removed, |txn, vsb_raw, omap_raw| {
                apfs_write_file::unlink(txn, vsb_raw, omap_raw, parent_ino, &canonical_name)
            }) {
                tracing::error!("cleanup: unlink failed for {:?}: {:?}", context.path, e);
            }
            self.self_verify(&format!("unlink({canonical_name})"));
        } else {
            tracing::error!(
                "cleanup: could not resolve parent/name for delete of {:?} - \
                 file not removed",
                context.path
            );
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
        let _t = crate::stats::timed("rename");
        self.refuse_if_read_only()?;
        guard(|| {
            // Resolve the OLD name canonically - Windows often sends uppercase
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
            // Store the name the user gave. Only the directory-entry HASH is
            // computed over the folded form, and only on a volume that asked
            // for it; the name itself is written verbatim, which is what
            // create_file does. Folding it here meant renaming a file to
            // "MixedCase.TXT" put "mixedcase.txt" on disk, and that is the
            // name Windows then listed.
            let new_name = new_name_raw.clone();
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
            // Carried out at once: Windows gets the real outcome of THIS
            // rename (a collision when the target exists and replacing was
            // not asked for) while the request is open, and the on-disk order
            // is the request order.
            let op = if old_parent_ino == new_parent_ino {
                HostOp::Rename {
                    parent_ino: old_parent_ino,
                    old_name,
                    new_name,
                    replace_if_exists,
                }
            } else {
                HostOp::MoveEntry {
                    old_parent_ino,
                    old_name,
                    new_parent_ino,
                    new_name,
                    replace_if_exists,
                }
            };
            self.apply_now(op)
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
        let _t = crate::stats::timed("set_basic_info");
        self.refuse_if_read_only()?;
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
                let resolving = crate::stats::timed("set_basic_info.resolve");
                let resolved = self.resolve_parent_and_name(None, &context.path);
                drop(resolving);
                let _applying = crate::stats::timed("set_basic_info.apply");
                if let Some((parent_ino, name)) = resolved {
                    let op = HostOp::SetInodeAttrs {
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
                    self.apply_now(op)?;
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
                .map_err(fs_err("getattr"))?
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
    /// work (needs partial-extent truncate semantics) - accept the call
    /// and reflect the size into file_info so Windows progress dialogs
    /// don't stall, but do not persist a partial new size yet.
    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let _t = crate::stats::timed("set_file_size");
        self.refuse_if_read_only()?;
        guard(|| {
            // Current logical size - classifies a SetAllocationSize and drives
            // the truncate fast path below.
            //
            // A queued truncate has to be settled first when this call is the
            // allocation-size half of the pair: .NET's SetLength issues
            // SetEndOfFile(100) and then SetAllocationSize(4096), and comparing
            // the second against the size still on disk (10000) made it look
            // like a shrink rather than the bookkeeping it is. It then ran
            // after the real cut and grew the file back to 4096.
            let current_size = {
                let mut fs_guard = self
                    .fs
                    .lock()
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let fs = fs_guard
                    .as_mut()
                    .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                // The size decides whether this call is a real truncate or
                // just allocation bookkeeping to ignore. Reading a failure as
                // zero decides it on a guess.
                fs.getattr(&context.path)
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                    .map(|a| a.size)
                    .unwrap_or(0)
            };
            // Windows splits a .NET FileStream.SetLength into SetEndOfFile
            // (set_allocation_size=false, logical EOF) THEN SetAllocationSize
            // (set_allocation_size=true, allocation rounded UP to a cluster).
            // Honouring the rounded-up allocation would grow the file, so a
            // SetAllocationSize that is >= the current size is pure allocation
            // bookkeeping and is IGNORED. But a SetAllocationSize that SHRINKS
            // below the current size IS a real truncate: an open-for-overwrite
            // (e.g. PowerShell Set-Content) issues SetAllocationSize(0) with NO
            // preceding SetEndOfFile - ignoring it left the old bytes in place
            // so the following write APPENDED instead of replacing. #155.
            if set_allocation_size && new_size >= current_size {
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
            // The metadata-only resize runs now; when it declines (a cut
            // inside a block, a shared or compressed stream) the read-and-
            // rewrite path follows in the same call, so the size Windows sees
            // afterwards is the size on disk.
            let op = HostOp::TruncateFast {
                parent_ino,
                name: name.clone(),
                path: context.path.clone(),
                new_size,
                current_size,
            };
            self.apply_now(op)?;
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
        let _t = crate::stats::timed("overwrite");
        self.refuse_if_read_only()?;
        guard(|| {
            if let Ok(mut wb) = context.write_buf.lock() {
                if wb.take().is_some() {
                    self.note_write_buffer(false);
                }
            }
            if let Some((parent_ino, name)) = self.resolve_parent_and_name(None, &context.path) {
                self.with_writable_keeping_names(|txn, vsb_raw, omap_raw| {
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
            // WinFsp asks this for every directory on the way to each file it
            // opens, so a plain directory is the common answer, not a fault;
            // at warn level it wrote one log line per path component.
            let Some(target) = target else {
                tracing::debug!(?path, "get_reparse_point_by_name: not a symlink");
                return Err(winfsp::FspError::NTSTATUS(STATUS_NOT_A_REPARSE_POINT));
            };
            let slice: &[u8] = if target.last() == Some(&0) {
                &target[..target.len() - 1]
            } else {
                &target[..]
            };
            tracing::debug!(
                ?path,
                target = %String::from_utf8_lossy(slice),
                buffer_len = buffer.len(),
                "get_reparse_point_by_name: encoding symlink"
            );
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
    /// The two operations span two transactions - a brief ENOENT window
    /// exists between them. For typical `New-Item -ItemType SymbolicLink`
    /// or `mklink` flows this is invisible because no concurrent reader
    /// is racing the create.
    fn set_reparse_point(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        buffer: &[u8],
    ) -> winfsp::Result<()> {
        self.refuse_if_read_only()?;
        guard(|| {
            let target = Self::decode_symlink_reparse(buffer)?;
            let path = win_to_unix(file_name);
            let (parent_ino, name) = self
                .resolve_parent_and_name(None, &path)
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND))?;
            // Replacing the placeholder the Create callback made with the
            // symlink is one change, so it is one transaction: split across
            // two, a crash or a failure between them left the name deleted and
            // no symlink in its place - the file simply gone. A placeholder
            // that is not there (rare, something else raced the create) is not
            // an error; anything else the removal says is.
            self.with_writable(|txn, vsb_raw, omap_raw| {
                apfs_write_file::create_symlink(
                    txn, vsb_raw, omap_raw, parent_ino, &name, &target, true,
                )
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
                    // Buffer too small - stop. WinFsp will call again with a
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
                buffer[pos + 6..pos + 8].copy_from_slice(&(val_len as u16).to_le_bytes());
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
        self.refuse_if_read_only()?;
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
                let next_offset =
                    u32::from_le_bytes(buffer[offset..offset + 4].try_into().unwrap_or([0; 4]))
                        as usize;
                let name_len = buffer[offset + 5] as usize;
                let val_len =
                    u16::from_le_bytes(buffer[offset + 6..offset + 8].try_into().unwrap_or([0; 2]))
                        as usize;

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
                        // Non-UTF-8 EA name - skip (APFS names are UTF-8).
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
                    self.with_writable_keeping_names(|txn, vsb_raw, omap_raw| {
                        apfs_write_file::delete_xattr(txn, vsb_raw, omap_raw, ino, &name)
                    })?;
                } else {
                    self.with_writable_keeping_names(|txn, vsb_raw, omap_raw| {
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
    // FSCTL_DUPLICATE_EXTENTS_TO_FILE (0x000940CF) - Windows 11+ server-side
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
    // the control callback - the raw kernel HANDLE value arrives in `input`.
    // Resolving it requires NtQueryInformationFile / GetFinalPathNameByHandle
    // in-process on Windows, but from a WinFsp user-mode callback there is no
    // platform helper for this. Until WinFsp exposes a handle→path API the
    // clone path cannot be completed; STATUS_NOT_SUPPORTED is returned so the
    // caller (robocopy, Copy-Item -Reflink) falls back to a regular byte copy
    // transparently - no data loss, just no CoW sharing.
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
/// automatic - no PATH, no admin, no manual setup. (A packaged build ships
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
/// (Read-only by default - safe default for production mounts.)
///
/// Returns `(host, dirty_arc)`. The caller can poll `dirty_arc` to learn
/// whether some open handle still holds written content, for the tray's
/// dirty indicator.
///
/// Whether the mount writes: when it was asked to. A volume with snapshots
/// is written as any other (#11): a write copies the catalog nodes on its
/// path and the snapshots keep the versions they froze. It used to be
/// mounted read-only, because every write there rewrote the whole catalog.
pub(crate) fn effective_writable<D: apfs_core::block_device::BlockDevice>(
    fsview: &mut FsView<D>,
    requested: bool,
) -> bool {
    if requested {
        match fsview.volume_has_snapshots() {
            Ok(true) => tracing::info!("writable mount of a volume with snapshots"),
            Ok(false) => {}
            Err(e) => tracing::warn!("could not read the snapshot count: {e}"),
        }
    }
    requested
}

pub fn mount_apfs(
    drive: Option<u32>,
    mountpoint: &str,
    writable: bool,
    selector: apfs::VolumeSelector,
    snapshot: Option<&apfs::SnapshotRef>,
    // winfsp 0.13: FileSystemHost::new returns FileSystemHost<T, FineGuard> (default S)
) -> winfsp::Result<(
    FileSystemHost<ApfsFs<Dev>, FineGuard>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    crate::winraw::DrivePresence,
)> {
    // Self-provision the WinFsp DLL beside the exe before ANY winfsp call,
    // so the delay-loaded import resolves (else Win32 1285).
    ensure_winfsp_dll();
    let init = winfsp::winfsp_init().map_err(io_err("winfsp_init"))?;
    let _ = init;
    // A snapshot is never written: the drive is opened for reading only.
    let writable = writable && snapshot.is_none();
    let (pd, range) = match drive {
        Some(n) => {
            let mut pd = if writable {
                PhysicalDrive::open_rw(n).map_err(io_err("PhysicalDrive::open_rw"))?
            } else {
                PhysicalDrive::open(n).map_err(io_err("PhysicalDrive::open"))?
            };
            let sec = pd.sector();
            let r = crate::gpt::find_apfs_container(&mut pd, sec)
                .map_err(io_err("find_apfs_container"))?;
            (pd, r)
        }
        None => {
            // Autodetect always opens read-only first. If writable was
            // requested, reopen the matching drive number in RW mode.
            let (pd_ro, r) = PhysicalDrive::autodetect_apfs().map_err(io_err("autodetect_apfs"))?;
            if writable {
                // Re-open the same device id with write access. The
                // autodetect helper currently does not expose its drive
                // index; we re-scan to find it (cheap - only a few drives).
                drop(pd_ro);
                let mut found = None;
                for n in 0..16u32 {
                    if let Ok(mut p) = PhysicalDrive::open_rw(n) {
                        let sec = p.sector();
                        if let Ok(rr) = crate::gpt::find_apfs_container(&mut p, sec) {
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
    // Watched by the caller: once the drive is pulled out, the mount ends
    // instead of waiting on a device that will never answer again.
    let presence =
        crate::winraw::DrivePresence::open(pd.number()).map_err(io_err("DrivePresence::open"))?;
    let dev = CachingBlockDevice::new(OffsetBlockDevice::new(pd, range.offset, range.length));
    let mut fsview = match snapshot {
        Some(s) => FsView::open_snapshot(GroupDev::Plain(dev), selector, s),
        None => FsView::open_selected(GroupDev::Plain(dev), selector),
    }
    .map_err(io_err("FsView::open"))?;
    let writable = effective_writable(&mut fsview, writable);
    // P14 (FINDING-05): stable Windows volume serial from the APFS volume UUID,
    // OR 1 so it is never zero (Windows treats serial 0 as "unknown").
    let serial = {
        let u = fsview.volume_uuid().unwrap_or([0u8; 16]);
        u32::from_le_bytes([u[0], u[1], u[2], u[3]]) | 1
    };

    let mut vp = VolumeParams::new();
    vp.filesystem_name("APFS")
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .read_only_volume(!writable)
        // Cleanup / SetDelete routing: without PostCleanupWhenModifiedOnly our
        // Cleanup callback only receives flags=0x20 (SetLastAccessTime) on every
        // read and never sees FspCleanupDelete (0x01), so Remove-Item silently
        // fails. This single flag (plus FlushAndPurgeOnCleanup + PersistentAcls
        // below) carries the delete path; it is independent of directory queries.
        //   PostCleanupWhenModifiedOnly = 1   -> Cleanup only on modify
        .post_cleanup_when_modified_only(true)
        // PassQueryDirectoryFileName: the FSD hands a query for one name to
        // get_dir_info_by_name (registered through host_params) instead of
        // listing the whole directory to match it. #153 saw filtered listings
        // (`dir *.txt`, Get-ChildItem -Filter) come back empty with this flag
        // when the callback was not there; measured on WinFsp 2.1 against
        // both mount paths, they are complete with it.
        .pass_query_directory_filename(true)
        // memfs sets persistent_acls = 1. Empirically required for the
        // Windows kernel to route CreateFile through our Create callback;
        // without it, CreateFile(CREATE_ALWAYS) returns ERROR_PATH_NOT_FOUND
        // before our filesystem is consulted.
        .persistent_acls(true)
        // Drop OS caches on cleanup so the next CreateFile reaches our
        // open/set_delete callbacks instead of being satisfied from
        // a stale dir cache.
        .flush_and_purge_on_cleanup(true)
        // P14 (FINDING-05): give Explorer a stable volume identity + the APFS
        // 255-byte name limit (was defaulting to 0 = "unknown").
        .volume_serial_number(serial)
        .max_component_length(255)
        // P7: advertise reparse-point support so WinFsp surfaces APFS symlinks
        // (S_IFLNK <-> Windows reparse points) instead of hiding them. The
        // read path already resolves reparse points; without this flag the
        // kernel never queries them.
        .reparse_points(true);
    apply_info_timeouts(&mut vp);
    // NOTE: WinFsp 2.1 SDK (winfsp-sys 0.12 / winfsp 0.13) does not expose a
    // RemovableMedia bitfield in FSP_FSCTL_VOLUME_PARAMS - the field was present
    // in earlier WinFsp versions but is absent from the 2.1 bindgen output.
    // Removable-media reporting (DriveType=2, "Safely Remove Hardware" tray)
    // is a deferred enhancement tracked in the M13 roadmap.

    // winfsp 0.13: turbofish pins S=FineGuard so start() call is unambiguous
    let fs = ApfsFs::new(fsview).read_only(!writable);
    let dirty_arc = fs.dirty_arc();
    let mut host = FileSystemHost::<ApfsFs<Dev>, FineGuard>::new_with_options(host_params(vp), fs)
        .map_err(io_err("FileSystemHost::new"))?;
    // Plain drive-letter mount (WinFsp uses DefineDosDevice). Cross-elevation
    // visibility for the non-elevated Explorer is provided by the system-wide
    // `EnableLinkedConnections=1` policy set at install time - NOT by the
    // MountManager `\\?\X:` form, which produced an unreadable volume
    // ("the directory name is invalid") in empirical testing on this setup.
    host.mount(mountpoint).map_err(io_err("host.mount"))?;
    host.start().map_err(io_err("host.start"))?;
    Ok((host, dirty_arc, presence))
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
/// Errors are silently ignored - status display is best-effort.
pub fn write_status_file(
    path: &std::path::Path,
    drive: u32,
    mount: &str,
    dirty: bool,
) -> std::io::Result<()> {
    let json = format!("{{\"drive\":{drive},\"mount\":{mount:?},\"dirty\":{dirty}}}\n");
    std::fs::write(path, json)
}

#[cfg(test)]
mod info_timeout_tests {
    use super::*;

    /// No setting reaches the never-expiring window that turns WinFsp's
    /// data caching on.
    #[test]
    fn the_info_window_never_turns_data_caching_on() {
        assert_eq!(info_timeout_ms(None), DEFAULT_INFO_TIMEOUT_MS);
        assert_eq!(info_timeout_ms(Some("0")), 0);
        assert_eq!(info_timeout_ms(Some("250")), 250);
        assert_eq!(
            info_timeout_ms(Some("not a number")),
            DEFAULT_INFO_TIMEOUT_MS
        );
        for huge in ["4294967295", "4294967294", "600000"] {
            let ms = info_timeout_ms(Some(huge));
            assert_eq!(ms, MAX_INFO_TIMEOUT_MS, "{huge}");
            assert_ne!(ms, u32::MAX);
        }
    }
}

#[cfg(test)]
mod reparse_tests {
    use super::*;

    /// Decode the parts of a symlink reparse buffer that decide whether the
    /// object manager can resolve it.
    fn parts(buf: &[u8], len: usize) -> (u32, String, String) {
        let sub_off = u16::from_le_bytes([buf[8], buf[9]]) as usize;
        let sub_len = u16::from_le_bytes([buf[10], buf[11]]) as usize;
        let print_off = u16::from_le_bytes([buf[12], buf[13]]) as usize;
        let print_len = u16::from_le_bytes([buf[14], buf[15]]) as usize;
        let flags = u32::from_le_bytes([buf[16], buf[17], buf[18], buf[19]]);
        let data_len = u16::from_le_bytes([buf[4], buf[5]]) as usize;
        assert_eq!(data_len + 8, len, "ReparseDataLength must cover the body");
        let wide = |off: usize, len: usize| -> String {
            let start = 20 + off;
            let units: Vec<u16> = buf[start..start + len]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16(&units).expect("utf16")
        };
        (flags, wide(sub_off, sub_len), wide(print_off, print_len))
    }

    #[test]
    fn a_relative_target_is_marked_relative() {
        let mut buf = [0u8; 512];
        let n = ApfsFs::<Dev>::encode_symlink_reparse(b"target.txt", &mut buf).expect("encode")
            as usize;
        let (flags, sub, print) = parts(&buf, n);
        assert_eq!(flags, SYMLINK_FLAG_RELATIVE);
        assert_eq!(sub, "target.txt");
        assert_eq!(print, "target.txt");
    }

    /// The name the object manager resolves has to be the NT form. Handing it
    /// the plain drive path is ERROR_INVALID_NAME, which is what following an
    /// absolute link used to give.
    #[test]
    fn a_drive_target_resolves_through_the_object_manager() {
        let mut buf = [0u8; 512];
        let n = ApfsFs::<Dev>::encode_symlink_reparse(b"Z:/dir/file.txt", &mut buf).expect("encode")
            as usize;
        let (flags, sub, print) = parts(&buf, n);
        assert_eq!(flags, 0, "an absolute target is not relative");
        assert_eq!(sub, "\\??\\Z:\\dir\\file.txt");
        assert_eq!(print, "Z:\\dir\\file.txt", "the shown name stays plain");
    }

    /// A target that is absolute in POSIX terms means nothing on Windows and
    /// is passed through rather than dressed up as an NT path.
    #[test]
    fn a_posix_target_is_left_alone() {
        let mut buf = [0u8; 512];
        let n = ApfsFs::<Dev>::encode_symlink_reparse(b"/etc/hosts", &mut buf).expect("encode")
            as usize;
        let (flags, sub, print) = parts(&buf, n);
        assert_eq!(flags, 0);
        assert_eq!(sub, "\\etc\\hosts");
        assert_eq!(print, "\\etc\\hosts");
    }
}
