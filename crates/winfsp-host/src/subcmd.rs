//! Cross-platform one-shot subcommands for apfs-mount.
//!
//! These subcommands do not require WinFsp and work on any OS where a flat
//! APFS image file is accessible (macOS development, CI, Windows production).
//!
//! Device spec: `--image <path>` (flat .img/.dmg, no GPT wrapping).
//! All write operations open the image writable via `FileBlockDeviceRw`.
//! All read-only operations use `FileBlockDevice`.

use apfs_core::block_device::{BlockDevice, FileBlockDevice, FileBlockDeviceRw};
use apfs_core::container::Container;
use apfs_core::omap::Omap;
use apfs_core::volume::VolumeSuperblock;
use apfs_write::snapshot::{create_snapshot, delete_snapshot, list_snapshots, revert_to_snapshot};
use apfs_write::txn::{Transaction, TxnError};

use crate::offset_dev::OffsetBlockDevice;

// ---------------------------------------------------------------------------
// Subcommand enum parsed from argv
// ---------------------------------------------------------------------------

/// Where a snapshot subcommand works: an image file, or (Windows only) a
/// physical drive, `\\.\PhysicalDrive<N>`.
pub enum Target {
    Image(String),
    Drive(u32),
}

/// What a snapshot subcommand does.
pub enum SnapOp {
    List,
    Create(String),
    Delete(u64),
    Revert(u64),
}

pub enum Subcommand {
    /// `snapshot (list | create --name <name> | delete --xid <n> |
    /// revert --xid <n>) (--image <path> | --drive <N>)`
    Snapshot { target: Target, op: SnapOp },
    /// `clone --image <path> --src <path> --dst <path>`
    Clone {
        image: String,
        src: String,
        dst: String,
    },
    /// `volume list --image <path>`
    VolumeList { image: String },
}

// ---------------------------------------------------------------------------
// Top-level dispatch: returns true if a subcommand was handled, false if the
// caller should fall through to the existing mount logic.
// ---------------------------------------------------------------------------

/// Attempt to parse and execute a subcommand from `argv`.
/// Returns `true` (and calls `process::exit`) if a subcommand was matched.
/// Returns `false` if the first argument is not a known subcommand keyword.
pub fn try_dispatch(args: &[String]) -> bool {
    let sub = match args.first().map(|s| s.as_str()) {
        Some("snapshot") => parse_snapshot(&args[1..]),
        Some("clone") => parse_clone(&args[1..]),
        Some("volume") => parse_volume(&args[1..]),
        _ => return false,
    };

    let code = match sub {
        Ok(cmd) => run(cmd),
        Err(msg) => {
            eprintln!("error: {msg}");
            print_subcmd_usage();
            1
        }
    };
    std::process::exit(code);
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

fn parse_snapshot(args: &[String]) -> Result<Subcommand, String> {
    let action = args
        .first()
        .ok_or_else(|| "snapshot: expected action (list|create|delete|revert)".to_string())?
        .as_str();

    let rest = &args[1..];
    let op = match action {
        "list" => SnapOp::List,
        "create" => SnapOp::Create(require_flag("--name", rest)?),
        "delete" => SnapOp::Delete(parse_xid(&require_flag("--xid", rest)?)?),
        "revert" => SnapOp::Revert(parse_xid(&require_flag("--xid", rest)?)?),
        other => {
            return Err(format!(
                "snapshot: unknown action {other:?} (expected list|create|delete|revert)"
            ))
        }
    };
    Ok(Subcommand::Snapshot {
        target: parse_target(rest)?,
        op,
    })
}

/// Exactly one of `--image <path>` and `--drive <N>`.
fn parse_target(args: &[String]) -> Result<Target, String> {
    match (opt_flag("--image", args), opt_flag("--drive", args)) {
        (Some(image), None) => Ok(Target::Image(image)),
        (None, Some(n)) => n
            .parse::<u32>()
            .map(Target::Drive)
            .map_err(|_| format!("--drive: invalid integer {n:?}")),
        _ => Err("snapshot: pass exactly one of --image <path> or --drive <N>".to_string()),
    }
}

/// A transaction id in decimal or, as `snapshot list` prints it, `0x` hex.
fn parse_xid(s: &str) -> Result<u64, String> {
    let parsed = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => s.parse::<u64>(),
    };
    parsed.map_err(|_| format!("--xid: invalid integer {s:?}"))
}

fn parse_clone(args: &[String]) -> Result<Subcommand, String> {
    let image = require_flag("--image", args)?;
    let src = require_flag("--src", args)?;
    let dst = require_flag("--dst", args)?;
    Ok(Subcommand::Clone { image, src, dst })
}

fn parse_volume(args: &[String]) -> Result<Subcommand, String> {
    let action = args
        .first()
        .ok_or_else(|| "volume: expected action (list)".to_string())?
        .as_str();

    match action {
        "list" => {
            let image = require_flag("--image", &args[1..])?;
            Ok(Subcommand::VolumeList { image })
        }
        other => Err(format!("volume: unknown action {other:?} (expected list)")),
    }
}

/// Look for `--flag <value>` in `args`; return `Some(value)` or `None`.
fn opt_flag(flag: &str, args: &[String]) -> Option<String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a.as_str() == flag {
            return it.next().cloned();
        }
    }
    None
}

/// Look for `--flag <value>` in `args` and return the value.
fn require_flag(flag: &str, args: &[String]) -> Result<String, String> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a.as_str() == flag {
            return it
                .next()
                .map(|s| s.clone())
                .ok_or_else(|| format!("{flag}: missing value"));
        }
    }
    Err(format!("missing required flag {flag}"))
}

// ---------------------------------------------------------------------------
// Executor - returns exit code (0 = success, 1 = error)
// ---------------------------------------------------------------------------

fn run(cmd: Subcommand) -> i32 {
    match cmd {
        Subcommand::Snapshot { target, op } => run_snapshot(&target, op),
        Subcommand::Clone { image, src, dst } => run_clone(&image, &src, &dst),
        Subcommand::VolumeList { image } => run_volume_list(&image),
    }
}

// ---------------------------------------------------------------------------
// Helpers: open device + read vsb/omap
// ---------------------------------------------------------------------------

/// Where the APFS container lies in `dev`: the whole image when it starts
/// with a container superblock, else the first APFS partition of its GPT
/// (what `hdiutil` writes, and any copy of a whole disk).
fn container_range<D: apfs_core::block_device::BlockDevice>(
    dev: &mut D,
) -> Result<(u64, u64), String> {
    /// `nx_superblock_t.nx_magic` and where it sits.
    const NX_MAGIC: &[u8; 4] = b"NXSB";
    const NX_MAGIC_AT: usize = 32;
    let mut head = [0u8; 512];
    dev.read_at(0, &mut head)
        .map_err(|e| format!("read image head: {e}"))?;
    if head.get(NX_MAGIC_AT..NX_MAGIC_AT + 4) == Some(&NX_MAGIC[..]) {
        return Ok((0, dev.size()));
    }
    let range = crate::gpt::find_apfs_container(dev, GPT_LBA_SIZE)
        .map_err(|e| format!("no APFS container in the image: {e:?}"))?;
    Ok((range.offset, range.length))
}

/// Logical block size of a disk image's partition table.
const GPT_LBA_SIZE: u64 = 512;

/// The APFS container in `image`, read-only.
fn open_container_ro(image: &str) -> Result<OffsetBlockDevice<FileBlockDevice>, String> {
    let mut dev = FileBlockDevice::open(image).map_err(|e| format!("open {image}: {e}"))?;
    let (base, len) = container_range(&mut dev)?;
    Ok(OffsetBlockDevice::new(dev, base, len))
}

/// The APFS container in `image`, read-write.
fn open_container_rw(
    image: &str,
) -> Result<OffsetBlockDevice<apfs_core::block_device::FileBlockDeviceRw>, String> {
    let mut dev = FileBlockDeviceRw::open_rw(image).map_err(|e| format!("open rw {image}: {e}"))?;
    let (base, len) = container_range(&mut dev)?;
    Ok(OffsetBlockDevice::new(dev, base, len))
}

/// Open the container in `image`, and return its volume superblock and
/// object-map header alongside a writable device on the container.
#[allow(clippy::type_complexity)]
fn open_rw_vsb_omap(
    image: &str,
) -> Result<
    (
        OffsetBlockDevice<apfs_core::block_device::FileBlockDeviceRw>,
        Vec<u8>,
        Vec<u8>,
    ),
    String,
> {
    let mut dev = open_container_rw(image)?;
    let (vsb_raw, omap_raw) = read_vsb_omap(&mut dev)?;
    Ok((dev, vsb_raw, omap_raw))
}

/// Open `image` read-only and return `(vsb_raw, omap_raw)`.
#[cfg(test)]
fn open_ro_vsb_omap(image: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    read_vsb_omap(&mut open_container_ro(image)?)
}

/// The first volume's superblock and object-map header in the container
/// `dev` holds.
fn read_vsb_omap<D: BlockDevice>(dev: &mut D) -> Result<(Vec<u8>, Vec<u8>), String> {
    let container = Container::open(dev).map_err(|e| format!("container open: {e:?}"))?;
    let nx = &container.superblock;
    let bsz = nx.block_size as usize;
    let xid = nx.obj.xid;

    let omap = Omap::open(dev, nx.omap_oid, nx.block_size)
        .map_err(|e| format!("container omap: {e:?}"))?;
    let fs_oid = *nx.fs_oids.first().ok_or("no volumes in container")?;
    let vol_paddr = omap
        .resolve(dev, fs_oid, xid, nx.block_size)
        .map_err(|e| format!("omap resolve: {e:?}"))?
        .ok_or("volume not found in omap")?;

    let mut vsb_raw = vec![0u8; bsz];
    dev.read_at(vol_paddr * bsz as u64, &mut vsb_raw)
        .map_err(|e| format!("read vsb: {e}"))?;

    let vsb = VolumeSuperblock::parse(&vsb_raw).map_err(|e| format!("parse vsb: {e:?}"))?;
    let mut omap_raw = vec![0u8; bsz];
    dev.read_at(vsb.omap_oid * bsz as u64, &mut omap_raw)
        .map_err(|e| format!("read vol omap: {e}"))?;

    Ok((vsb_raw, omap_raw))
}

fn txn_err(e: TxnError) -> i32 {
    eprintln!("error: {e}");
    1
}

// ---------------------------------------------------------------------------
// snapshot list / create / delete / revert
// ---------------------------------------------------------------------------

fn run_snapshot(target: &Target, op: SnapOp) -> i32 {
    let done = match target {
        Target::Image(path) => open_container_rw(path).and_then(|dev| snapshot_on(dev, op)),
        Target::Drive(n) => snapshot_on_drive(*n, op),
    };
    match done {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

/// Run `op` on the container `dev` holds, the same way for an image and a
/// drive.
fn snapshot_on<D: apfs_core::block_device::WritableBlockDevice>(
    mut dev: D,
    op: SnapOp,
) -> Result<(), String> {
    let (vsb_raw, omap_raw) = read_vsb_omap(&mut dev)?;
    let mut txn = Transaction::begin(dev).map_err(|e| e.to_string())?;
    let done = match &op {
        // A transaction gives read access to the tree; nothing is staged and
        // it is aborted, never committed.
        SnapOp::List => {
            let listed = list_snapshots(&mut txn, &vsb_raw, &omap_raw);
            drop(txn.abort());
            print_snapshots(&vsb_raw, &omap_raw, &listed.map_err(|e| e.to_string())?);
            return Ok(());
        }
        SnapOp::Create(name) => create_snapshot(&mut txn, &vsb_raw, &omap_raw, name)
            .map(|xid| format!("snapshot created: xid={xid:#x} name={name:?}")),
        SnapOp::Delete(xid) => delete_snapshot(&mut txn, &vsb_raw, &omap_raw, *xid)
            .map(|()| format!("snapshot deleted: xid={xid:#x}")),
        SnapOp::Revert(xid) => revert_to_snapshot(&mut txn, &vsb_raw, &omap_raw, *xid)
            .map(|()| format!("reverted to snapshot xid={xid:#x}")),
    };
    match done {
        Ok(message) => {
            txn.commit().map_err(|e| e.to_string())?;
            println!("{message}");
            Ok(())
        }
        Err(e) => {
            // Nothing staged reaches the disk: dropping the transaction
            // leaves the volume as it was.
            drop(txn.abort());
            Err(e.to_string())
        }
    }
}

#[cfg(not(windows))]
fn snapshot_on_drive(_n: u32, _op: SnapOp) -> Result<(), String> {
    Err("--drive is only supported on Windows".to_string())
}

/// `\\.\PhysicalDrive<n>` read-write, its APFS container found through the
/// GPT. Opened unshared, so a drive a mount holds is refused rather than
/// written under it.
#[cfg(windows)]
fn snapshot_on_drive(n: u32, op: SnapOp) -> Result<(), String> {
    let mut pd = crate::winraw::PhysicalDrive::open_exclusive_rw(n)
        .map_err(|e| format!("open PhysicalDrive{n} rw: {e}"))?;
    let sec = pd.sector();
    let range = crate::gpt::find_apfs_container(&mut pd, sec)
        .map_err(|e| format!("find apfs container: {e:?}"))?;
    snapshot_on(OffsetBlockDevice::new(pd, range.offset, range.length), op)
}

// ---------------------------------------------------------------------------
// snapshot list
// ---------------------------------------------------------------------------

/// Read `om_snap_count` (u32 LE at offset 36 in the volume omap header).
const OMAP_SNAP_COUNT_OFF: usize = 36;
/// `om_most_recent_snap` (u64 LE at offset 0x40 in the volume omap header).
const OMAP_MOST_RECENT_SNAP_OFF: usize = 0x40;
/// `apfs_snap_meta_tree_oid` in the VSB (physical paddr of snap_meta root).
const VSBI_SNAP_META_TREE_OID: usize = 0x98;
/// `apfs_volname` offset in VSB.
const VSBI_VOLNAME: usize = 0x2C0;
/// `apfs_volname` max length (256 bytes including NUL).
const VSBI_VOLNAME_LEN: usize = 256;

fn print_snapshots(
    vsb_raw: &[u8],
    omap_raw: &[u8],
    snapshots: &[apfs_write::snapshot::SnapshotInfo],
) {
    let snap_count = u32::from_le_bytes(
        omap_raw[OMAP_SNAP_COUNT_OFF..OMAP_SNAP_COUNT_OFF + 4]
            .try_into()
            .unwrap_or([0u8; 4]),
    );
    let most_recent = u64::from_le_bytes(
        omap_raw[OMAP_MOST_RECENT_SNAP_OFF..OMAP_MOST_RECENT_SNAP_OFF + 8]
            .try_into()
            .unwrap_or([0u8; 8]),
    );
    let snap_meta_paddr = u64::from_le_bytes(
        vsb_raw[VSBI_SNAP_META_TREE_OID..VSBI_SNAP_META_TREE_OID + 8]
            .try_into()
            .unwrap_or([0u8; 8]),
    );
    let vol_name = read_cstr(&vsb_raw[VSBI_VOLNAME..VSBI_VOLNAME + VSBI_VOLNAME_LEN]);

    println!("Volume: {vol_name}");
    println!("Snapshots: {snap_count}  most-recent xid: {most_recent:#x}");
    println!("snap_meta_tree paddr: {snap_meta_paddr:#x}");

    if snap_count == 0 || snap_meta_paddr == 0 {
        println!("(no snapshots)");
        return;
    }

    println!("{:<20} {:<40}", "XID", "Name");
    println!("{}", "-".repeat(62));
    for s in snapshots {
        println!("{:<20} {:<40}", format!("{:#x}", s.xid), s.name);
    }
}

// ---------------------------------------------------------------------------
// clone
// ---------------------------------------------------------------------------

/// Parse `path` into `(parent_ino, filename)`.
/// The path is treated as a volume-relative POSIX path (e.g. `/dir/file`).
/// Returns an error string if the path is empty or is the root `/`.
///
/// To resolve the parent inode we walk the catalog using a read-only `FsView`.
fn split_path(path: &str) -> Result<(String, String), String> {
    let comps: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let Some(&last) = comps.last() else {
        return Err(format!("path {path:?} has no filename component"));
    };
    let name = last.to_string();
    let parent_path = if comps.len() == 1 {
        "/".to_string()
    } else {
        format!("/{}", comps[..comps.len() - 1].join("/"))
    };
    Ok((parent_path, name))
}

fn run_clone(image: &str, src: &str, dst: &str) -> i32 {
    use apfs::FsView;
    use apfs_write::file::clone_file;

    let (src_parent_path, src_name) = match split_path(src) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: src {e}");
            return 1;
        }
    };
    let (dst_parent_path, dst_name) = match split_path(dst) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: dst {e}");
            return 1;
        }
    };

    // Resolve parent inode numbers via a read-only FsView.
    let dev_ro = match open_container_ro(image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let mut fs = match FsView::open(dev_ro) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: FsView open: {e}");
            return 1;
        }
    };

    let src_parent_ino = match fs.lookup(&src_parent_path) {
        Ok(Some(ino)) => ino,
        Ok(None) => {
            eprintln!("error: src parent directory {src_parent_path:?} not found");
            return 1;
        }
        Err(e) => {
            eprintln!("error: lookup {src_parent_path}: {e}");
            return 1;
        }
    };
    let dst_parent_ino = match fs.lookup(&dst_parent_path) {
        Ok(Some(ino)) => ino,
        Ok(None) => {
            eprintln!("error: dst parent directory {dst_parent_path:?} not found");
            return 1;
        }
        Err(e) => {
            eprintln!("error: lookup {dst_parent_path}: {e}");
            return 1;
        }
    };
    drop(fs);

    let (dev_rw, vsb_raw, omap_raw) = match open_rw_vsb_omap(image) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let mut txn = match Transaction::begin(dev_rw) {
        Ok(t) => t,
        Err(e) => return txn_err(e),
    };
    match clone_file(
        &mut txn,
        &vsb_raw,
        &omap_raw,
        src_parent_ino,
        &src_name,
        dst_parent_ino,
        &dst_name,
    ) {
        Ok(new_ino) => match txn.commit() {
            Ok(_) => {
                println!("cloned {src:?} -> {dst:?} (new inode {new_ino})");
                0
            }
            Err(e) => txn_err(e),
        },
        Err(e) => {
            let _ = txn.abort();
            txn_err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// volume list
// ---------------------------------------------------------------------------

fn run_volume_list(image: &str) -> i32 {
    let mut dev = match open_container_ro(image) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let container = match Container::open(&mut dev) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: container open: {e:?}");
            return 1;
        }
    };
    let volumes = match container.list_volumes(&mut dev) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: list_volumes: {e:?}");
            return 1;
        }
    };

    if volumes.is_empty() {
        println!("(no volumes found)");
        return 0;
    }

    println!("{:<5} {:<36} {:<12} {:<20}", "IDX", "UUID", "ROLE", "NAME");
    println!("{}", "-".repeat(76));
    for v in &volumes {
        let uuid_str = format_uuid(&v.uuid);
        println!(
            "{:<5} {:<36} {:<12} {}",
            v.index,
            uuid_str,
            format!("{:?}", v.role),
            v.name
        );
    }
    0
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn format_uuid(uuid: &[u8; 16]) -> String {
    format!(
        "{:02X}{:02X}{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        uuid[0], uuid[1], uuid[2], uuid[3],
        uuid[4], uuid[5],
        uuid[6], uuid[7],
        uuid[8], uuid[9],
        uuid[10], uuid[11], uuid[12], uuid[13], uuid[14], uuid[15],
    )
}

fn read_cstr(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

pub fn print_subcmd_usage() {
    eprintln!(
        "Subcommands:\n\
         \n\
         apfs-mount snapshot list   <target>\n\
         apfs-mount snapshot create <target> --name <name>\n\
         apfs-mount snapshot delete <target> --xid <xid>\n\
         apfs-mount snapshot revert <target> --xid <xid>\n\
         apfs-mount clone           --image <path> --src <vol-path> --dst <vol-path>\n\
         apfs-mount volume  list    --image <path>\n\
         \n\
         Note: <target> is --image <path> or, on Windows, --drive <N> (a drive\n\
               that is not mounted; N as in Get-Disk).\n\
         Note: <path> is an APFS container image or a GPT disk image holding one.\n\
         Note: <xid> is decimal or hex (e.g. 0x1a2).\n\
         Note: <vol-path> is volume-relative POSIX path (e.g. /dir/file.txt)."
    );
}

// ---------------------------------------------------------------------------
// Smoke tests (gated: APFS_WRITE_SCRATCH=1, macOS only)
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    const IMG: &str = "/tmp/apfs-subcmd-smoke.img";
    const SIZE_MB: u32 = 32;

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    /// Create a fresh scratch image using `xtask gen-scratch`.
    fn gen_scratch() {
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        let status = std::process::Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                IMG,
                &SIZE_MB.to_string(),
            ])
            .current_dir(manifest)
            .status()
            .expect("cargo run xtask gen-scratch");
        assert!(status.success(), "xtask gen-scratch failed");
    }

    /// snapshot create → snapshot list (confirms xid present) → snapshot delete
    /// → snapshot list (confirms xid absent).
    #[test]
    fn smoke_snapshot_create_list_delete() {
        if !check_gate() {
            println!("smoke_snapshot_create_list_delete: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let image = || Target::Image(IMG.to_string());
        // --- create ---
        let code = run_snapshot(&image(), SnapOp::Create("smoke-test-snap".to_string()));
        assert_eq!(code, 0, "snapshot create failed");

        // --- list: capture printed xid ---
        // We call run_snapshot_list which prints to stdout; we cannot easily
        // capture that in a test, so instead we open the omap directly and
        // read the snap count.
        let (_vsb_raw, omap_raw) = open_ro_vsb_omap(IMG).expect("open_ro_vsb_omap after create");
        let snap_count = u32::from_le_bytes(
            omap_raw[OMAP_SNAP_COUNT_OFF..OMAP_SNAP_COUNT_OFF + 4]
                .try_into()
                .unwrap(),
        );
        assert!(
            snap_count >= 1,
            "expected at least 1 snapshot after create, got {snap_count}"
        );

        // --- list subcommand exit code ---
        let code = run_snapshot(&image(), SnapOp::List);
        assert_eq!(code, 0, "snapshot list failed");

        // --- resolve the snapshot's xid the way `snapshot list` does ---
        let (vsb_now, omap_now) = open_ro_vsb_omap(IMG).expect("open_ro_vsb_omap for xid");
        let dev_rw = open_container_rw(IMG).expect("open the container");
        let mut txn = Transaction::begin(dev_rw).expect("Transaction::begin");
        let snaps = list_snapshots(&mut txn, &vsb_now, &omap_now).expect("list_snapshots");
        drop(txn.abort());
        let snap_xid = snaps
            .iter()
            .find(|s| s.name == "smoke-test-snap")
            .map(|s| s.xid)
            .expect("the new snapshot is listed by name");

        // --- delete ---
        let code = run_snapshot(&image(), SnapOp::Delete(snap_xid));
        assert_eq!(code, 0, "snapshot delete xid={snap_xid:#x} failed");

        // --- list after delete: snap_count should decrease ---
        let (_, omap_raw2) = open_ro_vsb_omap(IMG).expect("open_ro_vsb_omap after delete");
        let snap_count2 = u32::from_le_bytes(
            omap_raw2[OMAP_SNAP_COUNT_OFF..OMAP_SNAP_COUNT_OFF + 4]
                .try_into()
                .unwrap(),
        );
        assert!(
            snap_count2 < snap_count,
            "snap_count should decrease after delete: before={snap_count} after={snap_count2}"
        );
    }

    /// volume list subcommand returns exit code 0 on a valid scratch image.
    #[test]
    fn smoke_volume_list() {
        if !check_gate() {
            println!("smoke_volume_list: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        // Reuse or create scratch image.
        if !std::path::Path::new(IMG).exists() {
            gen_scratch();
        }
        let code = run_volume_list(IMG);
        assert_eq!(code, 0, "volume list failed");
    }

    /// try_dispatch returns false for unknown first token.
    #[test]
    fn dispatch_no_match_returns_false() {
        let args: Vec<String> = vec!["--image".into(), "/tmp/x.img".into()];
        assert!(!try_dispatch(&args));
    }

    /// parse_snapshot rejects missing --image flag.
    #[test]
    fn parse_snapshot_list_missing_image() {
        let args: Vec<String> = vec!["list".into()];
        assert!(parse_snapshot(&args).is_err());
    }

    /// `--xid` takes the hex form `snapshot list` prints as well as decimal.
    #[test]
    fn parse_snapshot_xid_hex_or_decimal() {
        for (given, want) in [("0x1a2", 0x1a2u64), ("0X10", 16), ("418", 418)] {
            let args: Vec<String> = ["delete", "--image", "/tmp/x.img", "--xid", given]
                .iter()
                .map(|s| s.to_string())
                .collect();
            match parse_snapshot(&args) {
                Ok(Subcommand::Snapshot {
                    op: SnapOp::Delete(xid),
                    ..
                }) => assert_eq!(xid, want, "{given}"),
                _ => panic!("--xid {given} was not accepted"),
            }
        }
        let args: Vec<String> = ["revert", "--image", "/tmp/x.img", "--xid", "0xzz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!(parse_snapshot(&args).is_err());
    }

    /// Every snapshot action takes a drive as well as an image, and exactly
    /// one of the two.
    #[test]
    fn parse_snapshot_target_image_or_drive() {
        let parse = |a: &[&str]| {
            let args: Vec<String> = a.iter().map(|s| s.to_string()).collect();
            parse_snapshot(&args)
        };
        for action in [
            &["list"][..],
            &["create", "--name", "s"],
            &["delete", "--xid", "0x10"],
            &["revert", "--xid", "16"],
        ] {
            let with = |extra: &[&'static str]| -> Vec<&'static str> { [action, extra].concat() };
            assert!(matches!(
                parse(&with(&["--drive", "3"])),
                Ok(Subcommand::Snapshot {
                    target: Target::Drive(3),
                    ..
                })
            ));
            assert!(matches!(
                parse(&with(&["--image", "/tmp/x.img"])),
                Ok(Subcommand::Snapshot {
                    target: Target::Image(_),
                    ..
                })
            ));
            assert!(parse(&with(&[])).is_err(), "{action:?} without a target");
            assert!(
                parse(&with(&["--image", "/tmp/x.img", "--drive", "3"])).is_err(),
                "{action:?} with both targets"
            );
            assert!(parse(&with(&["--drive", "x"])).is_err());
        }
    }

    /// parse_clone rejects missing --src flag.
    #[test]
    fn parse_clone_missing_src() {
        let args: Vec<String> = vec!["--image".into(), "/tmp/x.img".into()];
        assert!(parse_clone(&args).is_err());
    }

    /// split_path handles root-level file and nested path.
    #[test]
    fn split_path_variants() {
        let (parent, name) = split_path("/file.txt").unwrap();
        assert_eq!(parent, "/");
        assert_eq!(name, "file.txt");

        let (parent, name) = split_path("/dir/sub/file.txt").unwrap();
        assert_eq!(parent, "/dir/sub");
        assert_eq!(name, "file.txt");
    }

    /// split_path rejects root-only path.
    #[test]
    fn split_path_root_rejected() {
        assert!(split_path("/").is_err());
    }
}
