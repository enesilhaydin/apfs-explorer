//! Per-drive `apfs-mount.exe` child supervision (Windows-only).
//!
//! A Windows Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is created
//! once for the process lifetime. Every spawned child is assigned to it.
//! When `apfs-tray.exe` exits for any reason (graceful or force-kill) the OS
//! closes the job handle and terminates all children automatically, which
//! causes WinFsp to unmount the drive. This is the only reliable cleanup path
//! for force-termination.

use std::collections::HashMap;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::{Child, Command};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// Tracks one `apfs-mount` child per physical-drive number.
pub struct Children {
    map: HashMap<u32, Child>,
    /// Job object handle; kept alive for process lifetime so KILL_ON_JOB_CLOSE fires.
    job: Option<HANDLE>,
}

impl Default for Children {
    fn default() -> Self {
        Self::new()
    }
}

/// Create a Job Object with KILL_ON_JOB_CLOSE so all children die when
/// apfs-tray exits for any reason (graceful or force-kill).
fn create_kill_job() -> Option<HANDLE> {
    unsafe {
        // windows 0.58: CreateJobObjectW(attr, name) -> Result<HANDLE>
        let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok.is_ok() {
            Some(job)
        } else {
            None
        }
    }
}

/// Assign a child process to the kill-job (best-effort; non-fatal on failure).
fn assign_to_job(job: HANDLE, child: &Child) {
    let raw = child.as_raw_handle();
    if !raw.is_null() {
        unsafe {
            let _ = AssignProcessToJobObject(job, HANDLE(raw as _));
        }
    }
}

/// Locate `apfs-mount.exe` next to the current exe (packaged layout) or
/// fall back to the cargo target dir during development.
fn apfs_mount_path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join("apfs-mount.exe");
            if p.is_file() {
                return p;
            }
        }
    }
    PathBuf::from("apfs-mount.exe") // rely on PATH/cwd as last resort
}

impl Children {
    pub fn new() -> Self {
        Self {
            map: HashMap::new(),
            job: create_kill_job(),
        }
    }

    /// Spawn (or respawn) `apfs-mount` for `drive`. Any existing child for
    /// that drive is killed first (respawn path). Auto free drive-letter
    /// (no `--mount`).
    ///
    /// Known minor race: between `Command::spawn` returning and
    /// `AssignProcessToJobObject` finishing, the child has run a few
    /// instructions of CRT startup. `apfs-mount` does not fork or
    /// register CreateProcess hooks before `main`, so the race is
    /// invisible in practice. A `CREATE_SUSPENDED` + primary-thread
    /// resume would close it but requires either bypassing
    /// `std::process::Command` for `CreateProcessW` directly or
    /// Toolhelp32-enumerating the new process's primary thread; deferred
    /// until a real symptom appears.
    pub fn spawn(&mut self, drive: u32) -> std::io::Result<()> {
        self.spawn_with_opts(drive, true)
    }

    /// Spawn (or respawn) `apfs-mount` for `drive` with explicit writability.
    /// `writable = true` passes `--writable`; `false` omits it (read-only).
    /// Any existing child for that drive is killed first (respawn path).
    ///
    /// Known minor race: between `Command::spawn` returning and
    /// `AssignProcessToJobObject` finishing, the child has run a few
    /// instructions of CRT startup. `apfs-mount` does not fork or
    /// register CreateProcess hooks before `main`, so the race is
    /// invisible in practice.
    pub fn spawn_with_opts(&mut self, drive: u32, writable: bool) -> std::io::Result<()> {
        self.kill(drive);
        let mut cmd = Command::new(apfs_mount_path());
        cmd.arg("--drive")
            .arg(drive.to_string())
            .arg("--wait-for-kill");
        if writable {
            cmd.arg("--writable");
        }
        let child = cmd.spawn()?;
        if let Some(job) = self.job {
            assign_to_job(job, &child);
        }
        self.map.insert(drive, child);
        Ok(())
    }

    /// Kill and reap the child for `drive` (WinFsp unmounts on exit).
    pub fn kill(&mut self, drive: u32) {
        if let Some(mut c) = self.map.remove(&drive) {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Is the child for `drive` still running? (`try_wait` == Ok(None)).
    pub fn alive(&mut self, drive: u32) -> bool {
        match self.map.get_mut(&drive) {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }

    pub fn kill_all(&mut self) {
        let drives: Vec<u32> = self.map.keys().copied().collect();
        for d in drives {
            self.kill(d);
        }
    }
}

/// RAII: close the kill-on-close job handle on drop so we never leak a
/// kernel handle across an apfs-tray restart. JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
/// fires as soon as the last open handle to the job closes — that is
/// precisely the behaviour we want at shutdown (graceful exits and panics
/// alike kill every assigned child).
impl Drop for Children {
    fn drop(&mut self) {
        // Best-effort kill of any tracked child so kill_all's blocking
        // `wait()` runs from a known thread before the job tears them
        // down via KILL_ON_JOB_CLOSE. Avoids a tiny window where the OS
        // would terminate children we haven't reaped yet.
        self.kill_all();
        if let Some(h) = self.job.take() {
            unsafe {
                let _ = CloseHandle(h);
            }
        }
    }
}
