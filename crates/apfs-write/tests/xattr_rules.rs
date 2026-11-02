//! What macOS lets an extended attribute be, and what a resource fork does
//! to the inode.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! Measured on macOS 15 (`setxattr` through libc): `com.apple.FinderInfo` is
//! accepted only at exactly 32 bytes (ERANGE otherwise), a name may be 127
//! bytes but not 128 (ENAMETOOLONG), an empty name is EINVAL, and
//! `com.apple.system.*` is refused (EPERM). A resource fork, however long,
//! is an ordinary attribute - but the inode records that it has one. The
//! first half of the test lets macOS write a resource fork and reads the
//! inode flags it leaves; the second half has our writer do the same and
//! expects the same flags, then takes the fork away again.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
    )
)]

#[cfg(test)]
mod xattr_rules {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{delete_xattr, set_xattr, write_file};
    use apfs_write::txn::{Transaction, TxnError};
    use std::io::Read;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-xattr-rules.img";
    const IMAGE_MB: &str = "64";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// `j_inode_val_t.internal_flags`: the inode has a resource fork / has none.
    const INODE_HAS_RSRC_FORK: u64 = 0x4000;
    const INODE_NO_RSRC_FORK: u64 = 0x8000;
    const INODE_HAS_FINDER_INFO: u64 = 0x100;
    const RESOURCE_FORK: &str = "com.apple.ResourceFork";
    const FINDER_INFO: &str = "com.apple.FinderInfo";

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn root_dir() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }
    fn gen_scratch() {
        let _ = std::fs::remove_file(IMG);
        let s = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                IMG,
                IMAGE_MB,
            ])
            .current_dir(root_dir())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
    }
    fn find_apfs_range() -> (u64, u64) {
        let mut f = std::fs::File::open(IMG).unwrap();
        let mut head = vec![0u8; 64 * 1024];
        f.read_exact(&mut head).unwrap();
        let hdr = &head[512..1024];
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let off = base + i * sz;
            let e = match head.get(off..off + sz) {
                Some(e) => e,
                None => break,
            };
            if e.get(..16) == Some(&APFS_GPT_TYPE[..]) {
                let s = u64::from_le_bytes(e[32..40].try_into().unwrap());
                let en = u64::from_le_bytes(e[40..48].try_into().unwrap());
                return (s * SECTOR, (en - s + 1) * SECTOR);
            }
        }
        panic!("no APFS partition");
    }
    fn open_dev() -> Dev {
        let (base, len) = find_apfs_range();
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        WritableOffsetBlockDevice::new(inner, base, len)
    }
    fn read_vsb_omap() -> (Vec<u8>, Vec<u8>) {
        let mut dev = open_dev();
        let c = Container::open(&mut dev).unwrap();
        let nx = &c.superblock;
        let bsz = nx.block_size as usize;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).unwrap();
        let fs_oid = *nx.fs_oids.first().unwrap();
        let vp = omap
            .resolve(&mut dev, fs_oid, nx.obj.xid, nx.block_size)
            .unwrap()
            .unwrap();
        let mut vsb = vec![0u8; bsz];
        dev.read_at(vp * bsz as u64, &mut vsb).unwrap();
        let v = VolumeSuperblock::parse(&vsb).unwrap();
        let mut om = vec![0u8; bsz];
        dev.read_at(v.omap_oid * bsz as u64, &mut om).unwrap();
        (vsb, om)
    }
    fn in_txn<T>(f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> T) -> T {
        let (vsb, om) = read_vsb_omap();
        let mut txn = Transaction::begin(open_dev()).expect("begin");
        let out = f(&mut txn, &vsb, &om);
        txn.commit().expect("commit");
        out
    }
    fn open_fs() -> apfs::FsView<Dev> {
        apfs::FsView::open(open_dev()).expect("FsView::open")
    }
    fn internal_flags(path: &str) -> u64 {
        open_fs()
            .inode_record(path)
            .expect("inode_record")
            .unwrap_or_else(|| panic!("{path} must exist"))
            .internal_flags
    }
    fn whole_disk(attached: &str) -> String {
        attached
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node")
    }
    fn detach(disk: &str) {
        for attempt in 0..8u64 {
            let st = Command::new("hdiutil")
                .args(["detach", disk])
                .output()
                .expect("detach");
            if st.status.success() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
        }
        let _ = Command::new("hdiutil")
            .args(["detach", "-force", disk])
            .status();
    }
    /// Mount read-write (or read-only) and run `f` on the mount point.
    fn with_macos_mount<T>(readonly: bool, f: impl FnOnce(&str) -> T) -> T {
        let mut args = vec![
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nobrowse",
        ];
        if readonly {
            args.push("-readonly");
        }
        args.push(IMG);
        let out = Command::new("hdiutil")
            .args(&args)
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach (mount) failed");
        let attached = String::from_utf8_lossy(&out.stdout).into_owned();
        let disk = whole_disk(&attached);
        let mount = attached
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let out = f(&mount);
        let _ = Command::new("sync").status();
        detach(&disk);
        out
    }
    fn run_fsck(label: &str) {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                "-nomount",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let attached = String::from_utf8_lossy(&out.stdout).into_owned();
        let disk = whole_disk(&attached);
        let part = attached
            .lines()
            .filter(|l| l.contains("Apple_APFS"))
            .filter_map(|l| l.split_whitespace().next())
            .next()
            .expect("no APFS partition node")
            .to_string();
        let rdev = part.replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        detach(&disk);
        let fsck = fsck.expect("fsck_apfs spawn");
        let report = String::from_utf8_lossy(&fsck.stdout).into_owned()
            + &String::from_utf8_lossy(&fsck.stderr);
        assert!(
            fsck.status.success(),
            "{label}: fsck_apfs reported corruption:\n{report}"
        );
        let complaints: Vec<&str> = report
            .lines()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("warning:")
                    || l.contains("error:")
                    || l.contains("overallocation")
                    || l.contains("underallocation")
                    || l.contains("is not valid")
            })
            .collect();
        assert!(
            complaints.is_empty(),
            "{label}: fsck_apfs flagged inconsistencies:\n{}",
            complaints.join("\n")
        );
    }
    /// `getxattr` on the mounted volume through libc (xattr(1) cannot print
    /// binary values on this macOS), as bytes; `None` when absent.
    fn macos_xattr(mount: &str, name: &str, file: &str) -> Option<Vec<u8>> {
        let script = "import ctypes, sys\n\
            libc = ctypes.CDLL(None, use_errno=True)\n\
            libc.getxattr.restype = ctypes.c_ssize_t\n\
            libc.getxattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]\n\
            p, n = sys.argv[1].encode(), sys.argv[2].encode()\n\
            size = libc.getxattr(p, n, None, 0, 0, 0)\n\
            if size < 0: sys.exit(1)\n\
            buf = ctypes.create_string_buffer(size)\n\
            got = libc.getxattr(p, n, buf, size, 0, 0)\n\
            if got < 0: sys.exit(1)\n\
            sys.stdout.write(buf.raw[:got].hex())\n";
        let out = Command::new("python3")
            .args(["-c", script, &format!("{mount}/{file}"), name])
            .output()
            .expect("python3");
        if !out.status.success() {
            return None;
        }
        let hex = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Some(
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect(),
        )
    }

    /// `setxattr` on the mounted volume through libc; the value comes from a file.
    fn macos_set_xattr(path: &str, name: &str, value: &[u8]) -> bool {
        let tmp = "/tmp/apfs-xattr-rules.value";
        std::fs::write(tmp, value).unwrap();
        let script = "import ctypes, sys\n\
            libc = ctypes.CDLL(None, use_errno=True)\n\
            libc.setxattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]\n\
            v = open(sys.argv[3], 'rb').read()\n\
            sys.exit(0 if libc.setxattr(sys.argv[1].encode(), sys.argv[2].encode(), v, len(v), 0, 0) == 0 else 1)\n";
        Command::new("python3")
            .args(["-c", script, path, name, tmp])
            .status()
            .expect("python3")
            .success()
    }

    #[test]
    #[ignore = "scratch-image gated; needs xtask gen-scratch and fsck_apfs on macOS"]
    fn a_resource_fork_marks_the_inode_the_way_macos_does_and_bad_attributes_are_refused() {
        if !gate() {
            return;
        }
        gen_scratch();
        for name in ["by-macos.txt", "by-us.txt", "plain.txt"] {
            in_txn(|t, v, o| {
                write_file(t, v, o, 2, name, b"content").expect("write");
            });
        }
        let fork: Vec<u8> = (0..300u32).map(|i| (i % 253) as u8).collect();

        // macOS writes a resource fork onto the first file.
        with_macos_mount(false, |mount| {
            let path = format!("{mount}/by-macos.txt");
            assert!(
                macos_set_xattr(&path, RESOURCE_FORK, &fork),
                "macOS must take the resource fork"
            );
        });
        let macos_flags = internal_flags("/by-macos.txt");
        let plain_flags = internal_flags("/plain.txt");
        println!(
            "xattr-rules: macOS leaves internal flags {macos_flags:#x} on a file with a resource \
             fork, {plain_flags:#x} on one without"
        );
        assert_eq!(macos_flags & INODE_HAS_RSRC_FORK, INODE_HAS_RSRC_FORK);
        assert_eq!(macos_flags & INODE_NO_RSRC_FORK, 0);
        assert_eq!(plain_flags & INODE_NO_RSRC_FORK, INODE_NO_RSRC_FORK);

        // Our writer does the same on the second file and the inode must say
        // the same; taking the fork away must say the opposite again.
        let ino = open_fs()
            .lookup("/by-us.txt")
            .expect("lookup")
            .expect("by-us.txt");
        in_txn(|t, v, o| {
            set_xattr(t, v, o, ino, RESOURCE_FORK, &fork).expect("set resource fork");
        });
        let ours = internal_flags("/by-us.txt");
        assert_eq!(
            ours & (INODE_HAS_RSRC_FORK | INODE_NO_RSRC_FORK),
            macos_flags & (INODE_HAS_RSRC_FORK | INODE_NO_RSRC_FORK),
            "our resource fork must leave the flags macOS leaves ({ours:#x} vs {macos_flags:#x})"
        );
        run_fsck("after our resource fork");
        with_macos_mount(true, |mount| {
            assert_eq!(
                macos_xattr(mount, RESOURCE_FORK, "by-us.txt"),
                Some(fork.clone())
            );
            assert_eq!(
                macos_xattr(mount, RESOURCE_FORK, "by-macos.txt"),
                Some(fork.clone())
            );
        });
        in_txn(|t, v, o| {
            delete_xattr(t, v, o, ino, RESOURCE_FORK).expect("delete resource fork");
        });
        let ours = internal_flags("/by-us.txt");
        assert_eq!(ours & INODE_HAS_RSRC_FORK, 0, "the fork is gone: {ours:#x}");
        assert_eq!(
            ours & INODE_NO_RSRC_FORK,
            INODE_NO_RSRC_FORK,
            "the fork is gone: {ours:#x}"
        );
        run_fsck("after removing our resource fork");

        // What macOS refuses, we refuse, before anything is staged.
        let plain = open_fs()
            .lookup("/plain.txt")
            .expect("lookup")
            .expect("plain.txt");
        let refused: Vec<(&str, Vec<u8>)> = vec![
            (FINDER_INFO, vec![1u8; 5]),
            (FINDER_INFO, vec![1u8; 33]),
            ("", b"v".to_vec()),
            ("com.apple.system.Security", b"S".repeat(20)),
        ];
        for (name, value) in &refused {
            let outcome = in_txn(|t, v, o| set_xattr(t, v, o, plain, name, value));
            assert!(
                matches!(outcome, Err(TxnError::InvalidArgument(_))),
                "{name:?} with {} bytes must be refused, got {outcome:?}",
                value.len()
            );
        }
        let long = "a".repeat(128);
        let outcome = in_txn(|t, v, o| set_xattr(t, v, o, plain, &long, b"v"));
        assert!(
            matches!(outcome, Err(TxnError::InvalidArgument(_))),
            "a 128-byte name must be refused, got {outcome:?}"
        );
        let longest = "a".repeat(127);
        in_txn(|t, v, o| {
            set_xattr(t, v, o, plain, &longest, b"v")
                .expect("a 127-byte name is the longest macOS takes");
        });
        in_txn(|t, v, o| {
            set_xattr(t, v, o, plain, FINDER_INFO, &[2u8; 32]).expect("32 bytes of Finder info");
        });
        // Finder info is recorded on the inode too (fsck_apfs: "finder info
        // internal flag is missing, need to set internal_flags 0x100").
        let flags = internal_flags("/plain.txt");
        assert_eq!(
            flags & INODE_HAS_FINDER_INFO,
            INODE_HAS_FINDER_INFO,
            "flags {flags:#x}"
        );
        run_fsck("after the accepted attributes");
        with_macos_mount(true, |mount| {
            assert_eq!(
                macos_xattr(mount, &longest, "plain.txt"),
                Some(b"v".to_vec())
            );
            assert_eq!(
                macos_xattr(mount, FINDER_INFO, "plain.txt"),
                Some(vec![2u8; 32])
            );
        });
        in_txn(|t, v, o| {
            delete_xattr(t, v, o, plain, FINDER_INFO).expect("delete Finder info");
        });
        let flags = internal_flags("/plain.txt");
        assert_eq!(
            flags & INODE_HAS_FINDER_INFO,
            0,
            "the Finder info is gone: {flags:#x}"
        );
        run_fsck("after removing the Finder info");
        let _ = std::fs::remove_file(IMG);
    }
}
