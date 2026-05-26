//! apfs-mount — mount an APFS USB/SSD or disk-image file as a Windows drive
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

    // No subcommand matched — show help.
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
    // parser below sees only mount-specific flags — preserving backward compat.
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

    let mut drive: Option<u32> = None;
    let mut image: Option<std::path::PathBuf> = None;
    let mut mount_arg: Option<String> = None;
    let mut wait_for_kill = false;
    let mut writable = false;
    // Volume selector — at most one of uuid / role / index may be set.
    let mut volume_uuid: Option<String> = None;
    let mut volume_role: Option<String> = None;
    let mut volume_index: Option<u32> = None;

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

    // Log selected volume selector (informational; actual selection happens
    // inside the mount host when it opens the container).
    if let Some(ref u) = volume_uuid {
        println!("volume selector: uuid={u}");
    } else if let Some(ref r) = volume_role {
        println!("volume selector: role={r}");
    } else if let Some(idx) = volume_index {
        println!("volume selector: index={idx}");
    }

    // Resolve mount point (auto-pick free letter if not specified).
    let mount = match winfsp_host::drive::resolve_mountpoint(mount_arg.as_deref()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("mount point selection failed: {e}");
            std::process::exit(1);
        }
    };

    // Macro to run the host loop — identical behaviour for both device types,
    // but the two call sites have different concrete `FileSystemHost<T, S>`
    // types, so a generic fn would need higher-ranked bounds across two
    // unrelated T params.  A macro avoids that while keeping the code DRY.
    //
    // `$dirty` is the `Arc<AtomicBool>` returned alongside the host.
    macro_rules! run_host {
        ($host:ident, $dirty:ident) => {{
            println!(
                "APFS mounted at {mount} ({}). Press Ctrl+C to unmount.",
                if writable { "writable" } else { "read-only" }
            );
            if wait_for_kill {
                // Poll loop: write dirty status to a temp-file every 2 s so
                // the tray process can display a dirty/clean indicator without
                // a heavyweight IPC channel. The file is removed on exit.
                let status_path = winfsp_host::mount::status_file_path(drive.unwrap_or(u32::MAX));
                loop {
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
                let _ = ctrlc_set(move || {
                    let _ = tx.send(());
                });
                let _ = rx.recv();
                $host.stop();
                $host.unmount();
                println!("Unmounted.");
            }
        }};
    }

    if let Some(img_path) = image {
        // -- image mode --
        match winfsp_host::image_mount::mount_apfs_image(&img_path, writable, &mount) {
            Ok((mut host, dirty)) => run_host!(host, dirty),
            Err(e) => {
                eprintln!("image mount failed: {e:?}");
                std::process::exit(1);
            }
        }
    } else {
        // -- physical drive mode --
        match winfsp_host::mount::mount_apfs(drive, &mount, writable) {
            Ok((mut host, dirty)) => run_host!(host, dirty),
            Err(e) => {
                eprintln!("mount failed: {e:?}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(windows)]
fn print_usage() {
    eprintln!(
        "Usage (Windows WinFsp mount):\n\
         apfs-mount [mount] [--drive N] [--mount Z:] [--writable] [--wait-for-kill]\n\
         apfs-mount [mount] --image <path> [--mount Z:] [--writable] [--wait-for-kill]\n\
         \n\
         Volume selection (at most one):\n\
           --volume-uuid <UUID>    select volume by UUID (hex, hyphens optional)\n\
           --volume-role <ROLE>    select first volume with role (case-insensitive)\n\
           --volume-index <N>      select volume at fs_oids[N] (0-based)\n\
         \n\
         Known roles: none system user recovery vm preboot installer data\n\
                      baseband update xart hardware backup\n"
    );
    winfsp_host::subcmd::print_subcmd_usage();
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
