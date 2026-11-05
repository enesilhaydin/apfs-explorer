//! apfs-mount - mount an APFS USB/SSD or disk-image file as a Windows drive
//! letter via WinFsp, or run cross-platform image subcommands.
//!
//! Usage (cross-platform, image-based):
//!   apfs-mount snapshot list   --image <path>
//!   apfs-mount snapshot create --image <path> --name <name>
//!   apfs-mount snapshot delete --image <path> --xid <xid>
//!   apfs-mount snapshot revert --image <path> --xid <xid>
//!   apfs-mount clone           --image <path> --src <vol-path> --dst <vol-path>
//!   apfs-mount volume  list    --image <path>
//!
//! Usage (Windows-only WinFsp mount):
//!   apfs-mount [mount] [--drive N] [--mount Z:] [--writable] [--wait-for-kill]
//!   apfs-mount [mount] --image <path> [--mount Z:] [--writable] [--wait-for-kill]
//!
//! --drive N              physical drive index (0 = \\.\PhysicalDrive0); default:
//!                        auto-detect the first APFS drive.
//! --image `<path>`       raw .img or .dmg file to mount (mutually exclusive with
//!                        --drive).
//! --mount Z:             target drive letter (default: auto-select first free).
//! --writable             opt-in to read+write mount (default: read-only).
//! --wait-for-kill        block until the process is killed instead of reading
//!                        stdin; useful when launched detached from SSH/scripts.
//! --volume-uuid <UUID>   select volume by UUID (hex, with or without hyphens).
//!                        Default: first user-visible volume.
//! --volume-role <ROLE>   select first volume with this role (case-insensitive).
//!                        Known roles: none system user recovery vm preboot
//!                        installer data baseband update xart hardware backup.
//! --volume-index <N>     select volume at position N in the container's
//!                        fs_oids array (0-based). Mutually exclusive with
//!                        --volume-uuid and --volume-role.
//! --snapshot <NAME>      mount the volume as snapshot NAME holds it,
//!                        read-only (`snapshot list` shows the names).
//! --snapshot-xid <XID>   the same, naming the snapshot by its xid.

#[cfg(not(windows))]
fn main() {
    // On non-Windows hosts the WinFsp mount path is unavailable, but the
    // cross-platform image subcommands (snapshot/clone/volume) still work.
    let argv: Vec<String> = std::env::args().collect();
    let rest = &argv[1..];

    // Explicit "mount" keyword on non-Windows → helpful error.
    if rest.first().map(|s| s.as_str()) == Some("mount") {
        eprintln!("error: 'mount' subcommand requires Windows + WinFsp.");
        std::process::exit(1);
    }

    // Try cross-platform subcommands; fall through to mount-only message.
    if winfsp_host::subcmd::try_dispatch(rest) {
        // try_dispatch called process::exit internally.
        unreachable!();
    }

    if matches!(rest.first().map(|s| s.as_str()), Some("--version" | "-V")) {
        println!("{}", version_line());
        std::process::exit(0);
    }

    // No subcommand matched - show help.
    if rest.first().map(|s| s.as_str()) == Some("--help")
        || rest.first().map(|s| s.as_str()) == Some("-h")
    {
        winfsp_host::subcmd::print_subcmd_usage();
        eprintln!("\nNote: WinFsp mount subcommand requires Windows.");
        std::process::exit(0);
    }

    eprintln!(
        "apfs-mount: no subcommand matched. \
         On non-Windows only image subcommands are available."
    );
    winfsp_host::subcmd::print_subcmd_usage();
    std::process::exit(1);
}

/// `apfs-mount <version> (<commit>, built <date>)`, for `--version` and the
/// log's start line, so a log says which build wrote it.
fn version_line() -> String {
    format!(
        "apfs-mount {} ({})",
        env!("APFS_PRODUCT_VERSION"),
        env!("APFS_BUILD_ID")
    )
}

/// How often a drive mount checks that its drive is still there.
#[cfg(windows)]
const PRESENCE_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// Exit code of a mount that ended because its drive was pulled out.
#[cfg(windows)]
const EXIT_DRIVE_REMOVED: i32 = 3;

#[cfg(windows)]
fn main() {
    use std::sync::mpsc::channel;

    let argv: Vec<String> = std::env::args().collect();
    let rest = &argv[1..];

    // Dispatch cross-platform subcommands (snapshot/clone/volume) first.
    // If the first token is "snapshot", "clone", or "volume" this returns
    // true (after calling process::exit internally).
    //
    // The explicit "mount" keyword is consumed here so the legacy flag
    // parser below sees only mount-specific flags - preserving backward compat.
    let mount_args: &[String] = match rest.first().map(|s| s.as_str()) {
        Some("snapshot") | Some("clone") | Some("volume") => {
            if winfsp_host::subcmd::try_dispatch(rest) {
                unreachable!();
            }
            rest
        }
        Some("mount") => &rest[1..],
        _ => rest,
    };

    run_mount(mount_args);
}

/// Windows-only WinFsp mount entry point.
/// Accepts the argv slice *after* the optional "mount" token has been stripped.
#[cfg(windows)]
fn run_mount(args: &[String]) {
    use std::sync::mpsc::channel;

    // Handle --help / -h BEFORE redirecting std streams to the log file: a user
    // running `apfs-mount --help` in a terminal must see the usage on the
    // console, not have it silently captured into apfs-mount.log.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print_usage();
        return;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("{}", version_line());
        return;
    }

    // Capture this process's stdout+stderr into a fresh log file before any
    // mount work. apfs-mount is normally launched detached + elevated by the
    // GUI (ShellExecuteEx "runas", CREATE_NO_WINDOW), so there is no console
    // to see the mount banner, the per-callback `eprintln!` error reports, or
    // a panic backtrace. Routing both streams to
    // %PROGRAMDATA%\APFS Explorer\logs\apfs-mount.log makes every mount/FS
    // error visible and lets the GUI reliably parse the assigned drive letter
    // from the fresh "APFS mounted at X:" line (previously it could read a
    // stale log from an earlier run).
    redirect_std_to_log();
    // Install a tracing subscriber that writes to stderr - which
    // redirect_std_to_log has pointed at apfs-mount.log. This captures the
    // structured diagnostics emitted across the crates (e.g. the APFS volume
    // that was selected, write-path errors), defaulting to INFO and overridable
    // via RUST_LOG so the hot read path stays quiet/fast in normal use.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
    println!(
        "{} starting: argv={:?} (pid {})",
        version_line(),
        args,
        std::process::id()
    );

    let mut drive: Option<u32> = None;
    let mut image: Option<std::path::PathBuf> = None;
    let mut mount_arg: Option<String> = None;
    let mut wait_for_kill = false;
    let mut writable = false;
    // When set, apfs-mount waits on a named Win32 event for the unmount
    // signal instead of stdin.  Used by `apfs-explorer-gui` which spawns us
    // elevated via ShellExecuteEx (no stdin pipe propagates across the UAC
    // boundary, so stdin read would return EOF immediately).  Default is
    // false so manual `echo | apfs-mount` invocations still work.
    let mut signal_via_event = false;
    // Volume selector - at most one of uuid / role / index may be set.
    let mut volume_uuid: Option<String> = None;
    let mut volume_role: Option<String> = None;
    let mut volume_index: Option<u32> = None;
    let mut snapshot: Option<apfs::SnapshotRef> = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--drive" => {
                if image.is_some() {
                    eprintln!("error: --drive and --image are mutually exclusive");
                    std::process::exit(1);
                }
                drive = it.next().and_then(|s| s.parse().ok());
            }
            "--image" => {
                if drive.is_some() {
                    eprintln!("error: --drive and --image are mutually exclusive");
                    std::process::exit(1);
                }
                image = it.next().map(|s| std::path::PathBuf::from(s));
            }
            "--mount" => {
                mount_arg = it.next().cloned();
            }
            "--wait-for-kill" => {
                wait_for_kill = true;
            }
            "--signal-via-event" => {
                signal_via_event = true;
            }
            "--writable" => {
                writable = true;
            }
            "--volume-uuid" => {
                volume_uuid = it.next().cloned();
            }
            "--volume-role" => {
                volume_role = it.next().cloned();
            }
            "--volume-index" => {
                volume_index = it.next().and_then(|s| s.parse().ok());
            }
            "--snapshot" | "--snapshot-xid" => {
                if snapshot.is_some() {
                    eprintln!("error: --snapshot and --snapshot-xid are mutually exclusive");
                    std::process::exit(1);
                }
                let Some(v) = it.next() else {
                    eprintln!("error: {a} needs a value");
                    std::process::exit(1);
                };
                snapshot = Some(if a == "--snapshot" {
                    apfs::SnapshotRef::Name(v.clone())
                } else {
                    let hex = v.trim_start_matches("0x").trim_start_matches("0X");
                    match u64::from_str_radix(hex, 16) {
                        Ok(x) => apfs::SnapshotRef::Xid(x),
                        Err(_) => {
                            eprintln!("error: --snapshot-xid takes a hex xid, as `snapshot list` prints it");
                            std::process::exit(1);
                        }
                    }
                });
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown arg: {other}");
                print_usage();
                std::process::exit(1);
            }
        }
    }

    // A snapshot is never written.
    if snapshot.is_some() && writable {
        println!("a snapshot mounts read-only; --writable is ignored");
        writable = false;
    }

    // Validate: at most one volume selector.
    {
        let selectors = [
            volume_uuid.is_some(),
            volume_role.is_some(),
            volume_index.is_some(),
        ];
        if selectors.iter().filter(|&&s| s).count() > 1 {
            eprintln!(
                "error: --volume-uuid, --volume-role, and --volume-index are mutually exclusive"
            );
            std::process::exit(1);
        }
    }

    // Validate --volume-role is a known role name.
    if let Some(ref role_str) = volume_role {
        use apfs_core::VolumeRole;
        if VolumeRole::from_name(role_str).is_none() {
            eprintln!(
                "error: unknown volume role {role_str:?}. Known roles: \
                 none system user recovery vm preboot installer data \
                 baseband update xart hardware backup"
            );
            std::process::exit(1);
        }
    }

    // Build the VolumeSelector that the mount host uses when opening the
    // container. Default (no flag) picks the user-data volume automatically
    // (preferring the Data role, skipping Preboot/Recovery/VM). The CLI
    // selectors override that.
    let selector: apfs::VolumeSelector = if let Some(ref u) = volume_uuid {
        match parse_volume_uuid(u) {
            Some(bytes) => {
                println!("volume selector: uuid={u}");
                apfs::VolumeSelector::Uuid(bytes)
            }
            None => {
                eprintln!("error: --volume-uuid {u:?} is not a valid 16-byte hex UUID");
                std::process::exit(1);
            }
        }
    } else if let Some(ref r) = volume_role {
        // Validated above; from_name is guaranteed Some here.
        let raw = apfs_core::VolumeRole::from_name(r)
            .map(|vr| vr.to_raw())
            .unwrap_or(0);
        println!("volume selector: role={r} (0x{raw:04x})");
        apfs::VolumeSelector::Role(raw)
    } else if let Some(idx) = volume_index {
        println!("volume selector: index={idx}");
        apfs::VolumeSelector::Index(idx as usize)
    } else {
        println!("volume selector: default (first user-data volume)");
        apfs::VolumeSelector::Default
    };

    // Resolve mount point (auto-pick free letter if not specified).
    let mount = match winfsp_host::drive::resolve_mountpoint(mount_arg.as_deref()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mount point selection failed: {e}");
            std::process::exit(1);
        }
    };

    // Macro to run the host loop - identical behaviour for both device types,
    // but the two call sites have different concrete `FileSystemHost<T, S>`
    // types, so a generic fn would need higher-ranked bounds across two
    // unrelated T params.  A macro avoids that while keeping the code DRY.
    //
    // `$dirty` is the `Arc<AtomicBool>` returned alongside the host;
    // `$presence` the drive's `DrivePresence`, `None` for an image.
    //
    // A drive pulled out ends the mount: the drive letter goes, nothing is
    // committed to a device that is no longer there (the volume keeps its
    // last checkpoint), and the process exits with EXIT_DRIVE_REMOVED rather
    // than waiting for a stop that may never come.
    macro_rules! run_host {
        ($host:ident, $dirty:ident, $presence:expr) => {{
            let presence: Option<winfsp_host::winraw::DrivePresence> = $presence;
            let drive_gone = || presence.as_ref().is_some_and(|p| !p.present());
            macro_rules! end_after_removal {
                () => {{
                    $host.stop();
                    $host.unmount();
                    eprintln!(
                        "the drive was removed; unmounted. Changes from its last \
                         moments are lost; the volume stays consistent."
                    );
                    std::process::exit(EXIT_DRIVE_REMOVED);
                }};
            }
            println!(
                "APFS mounted at {mount} ({}). Press Ctrl+C to unmount.",
                if writable { "writable" } else { "read-only" }
            );
            // However the process is stopped from outside (Ctrl+C, Ctrl+Break,
            // the console closing), the open write group is committed first.
            commit_on_console_stop();
            if wait_for_kill {
                // Poll loop: write dirty status to a temp-file every 2 s so
                // the tray process can display a dirty/clean indicator without
                // a heavyweight IPC channel. The file is removed on exit.
                let status_path = winfsp_host::mount::status_file_path(drive.unwrap_or(u32::MAX));
                loop {
                    if drive_gone() {
                        end_after_removal!();
                    }
                    let dirty = $dirty.load(std::sync::atomic::Ordering::Relaxed);
                    let _ = winfsp_host::mount::write_status_file(
                        &status_path,
                        drive.unwrap_or(u32::MAX),
                        &mount,
                        dirty,
                    );
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            } else {
                let (tx, rx) = channel();
                if signal_via_event {
                    let drive_id = drive.unwrap_or(u32::MAX);
                    let tx_clone = tx.clone();
                    if let Err(e) = wait_on_stop_event(drive_id, move || {
                        let _ = tx_clone.send(());
                    }) {
                        eprintln!("warning: stop-event setup failed: {e}; falling back to stdin");
                        let tx_clone = tx.clone();
                        let _ = ctrlc_set(move || {
                            let _ = tx_clone.send(());
                        });
                    }
                } else {
                    let tx_clone = tx.clone();
                    let _ = ctrlc_set(move || {
                        let _ = tx_clone.send(());
                    });
                }
                loop {
                    match rx.recv_timeout(PRESENCE_TICK) {
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if drive_gone() {
                                end_after_removal!();
                            }
                        }
                        _ => break,
                    }
                }
                winfsp_host::mount::commit_before_exit();
                $host.stop();
                $host.unmount();
                println!("Unmounted.");
            }
        }};
    }

    if let Some(img_path) = image {
        // -- image mode --
        match winfsp_host::image_mount::mount_apfs_image(
            &img_path,
            writable,
            &mount,
            selector,
            snapshot.as_ref(),
        ) {
            Ok((mut host, dirty)) => run_host!(host, dirty, None),
            Err(e) => {
                eprintln!("image mount failed: {e:?}");
                std::process::exit(1);
            }
        }
    } else {
        // -- physical drive mode --
        match winfsp_host::mount::mount_apfs(drive, &mount, writable, selector, snapshot.as_ref()) {
            Ok((mut host, dirty, presence)) => run_host!(host, dirty, Some(presence)),
            Err(e) => {
                eprintln!("mount failed: {e:?}");
                std::process::exit(1);
            }
        }
    }
}

/// Parse a volume UUID string (`apfs_vol_uuid`) into 16 bytes. Accepts the
/// canonical hyphenated form (`XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX`) or a
/// bare 32-hex-digit string. Returns `None` on any malformed input.
#[cfg(windows)]
fn parse_volume_uuid(s: &str) -> Option<[u8; 16]> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = hex.get(i * 2..i * 2 + 2)?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

#[cfg(windows)]
fn print_usage() {
    // Raw string so the indentation below renders verbatim (a `\`-continuation
    // string would strip the leading spaces of each line).
    print!(
        r#"apfs-mount - mount an APFS disk or image as a read-only Windows drive letter

USAGE:
  apfs-mount [mount] --drive <N>     [options]   mount a physical disk
  apfs-mount [mount] --image <path>  [options]   mount a raw .img / .dmg file
  apfs-mount <subcommand> --image <path> ...     snapshot / clone / volume ops

MOUNT OPTIONS:
  --drive <N>           physical drive index N (from \\.\PhysicalDriveN)
  --image <path>        raw .img/.dmg file (mutually exclusive with --drive)
  --mount <X:>          target drive letter (default: first free letter)
  --writable            opt in to read+write  (DEFAULT: read-only)
  --wait-for-kill       keep the drive mounted until this process is killed
  -h, --help            show this help and exit

VOLUME SELECTION (at most one; default: the user Data volume):
  --volume-uuid <UUID>  select volume by UUID (hex, hyphens optional)
  --volume-role <ROLE>  select first volume with this role (case-insensitive)
  --volume-index <N>    select volume at fs_oids[N] (0-based)
  roles: none system user recovery vm preboot installer data
         baseband update xart hardware backup

SNAPSHOT (read-only; `apfs-mount snapshot list` shows them):
  --snapshot <NAME>     mount the volume as snapshot NAME holds it
  --snapshot-xid <XID>  the same, by the xid `snapshot list` prints

EXAMPLES:
  apfs-mount --drive 2                      read-only mount of PhysicalDrive2, auto letter
  apfs-mount --drive 2 --mount E:           read-only mount at E:
  apfs-mount --image backup.dmg --mount E:  mount a disk image at E:
  apfs-mount --drive 2 --snapshot before-edit   the volume as that snapshot holds it

"#
    );
    winfsp_host::subcmd::print_subcmd_usage();
}

/// Redirect this process's stdout and stderr to
/// `%PROGRAMDATA%\APFS Explorer\logs\apfs-mount.log`, appending; a log past
/// `LOG_ROTATE_BYTES` is first moved to `apfs-mount.log.1`.
///
/// We set the Win32 std handles *before* the first `println!`/`eprintln!` so
/// Rust's lazily-initialised `Stdout`/`Stderr` pick up the file handle (they
/// call `GetStdHandle` on first use). The file handle is intentionally leaked
/// (`forget`) so it stays valid for the lifetime of the process.
#[cfg(windows)]
fn redirect_std_to_log() {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Console::{SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};

    let dir = std::path::Path::new(
        &std::env::var("PROGRAMDATA").unwrap_or_else(|_| String::from(r"C:\ProgramData")),
    )
    .join("APFS Explorer")
    .join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("apfs-mount.log");
    // Every run appends, so without a bound the log only grows. Another
    // mount holding the file open makes the move fail; this run then appends
    // and a later start rotates it. The console still shows the reason.
    if let Err(e) =
        winfsp_host::logfile::rotate_if_large(&path, winfsp_host::logfile::LOG_ROTATE_BYTES)
    {
        eprintln!("warning: could not rotate {}: {e}", path.display());
    }

    // Append (not truncate) so a crash/restart keeps the prior run's diagnostics.
    // The GUI parses the LAST "APFS mounted at X:" line, so appending is safe.
    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(_) => return,
    };
    let raw = file.as_raw_handle();
    // SAFETY: raw is a valid file HANDLE owned by `file`; SetStdHandle only
    //         stores it. We forget `file` below so the handle outlives this fn.
    unsafe {
        let h = HANDLE(raw as _);
        let _ = SetStdHandle(STD_OUTPUT_HANDLE, h);
        let _ = SetStdHandle(STD_ERROR_HANDLE, h);
    }
    std::mem::forget(file);
}

/// Commit the open write group on a console stop event, then let the
/// default handling end the process.
#[cfg(windows)]
fn commit_on_console_stop() {
    use windows::Win32::Foundation::BOOL;
    use windows::Win32::System::Console::SetConsoleCtrlHandler;

    unsafe extern "system" fn on_stop(_event: u32) -> BOOL {
        winfsp_host::mount::commit_before_exit();
        BOOL(0)
    }
    // SAFETY: `on_stop` is a plain function with the handler signature and
    // lives for the whole process; registering it has no other effect.
    if let Err(e) = unsafe { SetConsoleCtrlHandler(Some(on_stop), true) } {
        eprintln!("warning: no commit on console stop: {e}");
    }
}

// Minimal Ctrl+C handler: blocks on a stdin line so `echo | apfs-mount`
// triggers a clean unmount without an extra crate dependency.
#[cfg(windows)]
fn ctrlc_set<F: FnOnce() + Send + 'static>(f: F) -> std::io::Result<()> {
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        f();
    });
    Ok(())
}

/// Wait on a named Win32 event for the unmount signal.  This is the path
/// `apfs-explorer-gui` uses: the elevated child cannot share stdin across
/// the UAC privilege boundary, so the GUI signals via `Local\APFSExplorerStop-<drive>`
/// instead.
///
/// The event is created here (`CreateEventW`) with the default DACL, which
/// grants the creator's SID full access.  The GUI runs as the same Windows
/// user (just non-elevated), so `OpenEventW(EVENT_MODIFY_STATE)` from the
/// GUI succeeds.  Manual-reset so multiple waiters could theoretically be
/// signalled once but in practice we only have one (this thread).
#[cfg(windows)]
fn wait_on_stop_event<F: FnOnce() + Send + 'static>(drive: u32, f: F) -> std::io::Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

    let name = format!(r"Local\APFSExplorerStop-{drive}");
    let name_w = HSTRING::from(name.as_str());
    // SAFETY: name_w outlives the call; lpEventAttributes=None uses default DACL.
    let event = unsafe { CreateEventW(None, true, false, &name_w) }
        .map_err(|e| std::io::Error::other(format!("CreateEvent({name}): {e}")))?;
    // HANDLE wraps `*mut c_void` and is therefore !Send.  Disjoint-capture
    // would make the closure capture `wrapper.0` (the !Send field) even if
    // we wrapped HANDLE in a Send newtype, so we route the value through an
    // `isize` which is Send.  The kernel handle is just a typed alias for a
    // pointer-sized integer - reconstructing it inside the worker thread is
    // safe as long as the process still owns it (it does: we only return
    // from the parent function after the spawn, and the handle is closed
    // only by process exit).
    let event_value = event.0 as isize;
    std::thread::spawn(move || {
        // SAFETY: event_value came from a valid CreateEventW return; the
        //         parent process owns the handle and never closes it
        //         explicitly (lives until process exit).
        let h = windows::Win32::Foundation::HANDLE(event_value as *mut core::ffi::c_void);
        let _ = unsafe { WaitForSingleObject(h, INFINITE) };
        f();
    });
    Ok(())
}
