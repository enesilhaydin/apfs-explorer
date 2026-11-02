//! Which timestamps each operation moves, ours against macOS's.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! Every operation touches some of an inode's four timestamps and some of
//! its parent directory's. This lets macOS run each operation on an entry it
//! made, reads the entry's and the directory's timestamps before and after,
//! has our writer do the same on entries of its own and expects the same
//! timestamps to move for every operation.
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
mod operation_times {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::inode::Inode;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        create_symlink, delete_xattr, link, mkdir, overwrite_existing_file, set_inode_attrs,
        set_xattr, truncate_file_fast, unlink, write_file,
    };
    use apfs_write::txn::Transaction;
    use std::io::{Read, Write};
    use std::process::Command;
    use std::time::Duration;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-operation-times.img";
    const IMAGE_MB: &str = "64";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ROOT_INO: u64 = 2;
    /// Long enough for a moved timestamp to differ from an unmoved one.
    const GAP: Duration = Duration::from_millis(1100);
    const ATTR: &str = "user.note";

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
    fn inode(path: &str) -> Inode {
        open_fs()
            .inode_record(path)
            .expect("inode record")
            .unwrap_or_else(|| panic!("{path} must exist"))
    }
    fn ino(path: &str) -> u64 {
        open_fs()
            .lookup(path)
            .expect("lookup")
            .unwrap_or_else(|| panic!("{path} must exist"))
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
            std::thread::sleep(Duration::from_millis(250 * (attempt + 1)));
        }
        let _ = Command::new("hdiutil")
            .args(["detach", "-force", disk])
            .status();
    }
    /// macOS mounts the image read-write and runs `f` on the mount point.
    fn with_macos_mount<T>(f: impl FnOnce(&str) -> T) -> T {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach (mount) failed");
        let attached = String::from_utf8_lossy(&out.stdout).into_owned();
        let disk = whole_disk(&attached);
        let mount = attached
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        // Detached on the way out, a panic inside `f` included, so a failing
        // step never leaves the image attached.
        struct Detach(String);
        impl Drop for Detach {
            fn drop(&mut self) {
                let _ = Command::new("sync").status();
                detach(&self.0);
            }
        }
        let _guard = Detach(disk);
        f(&mount)
    }
    /// `setxattr` / `removexattr` through libc.
    fn macos_xattr(path: &str, name: &str, value: Option<&str>) -> bool {
        // No indented blocks: a string continuation strips leading spaces.
        let script = "import ctypes, sys\n\
            libc = ctypes.CDLL(None, use_errno=True)\n\
            p, n = sys.argv[1].encode(), sys.argv[2].encode()\n\
            libc.setxattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]\n\
            libc.removexattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int]\n\
            v = sys.argv[3].encode() if len(sys.argv) > 3 else None\n\
            rc = libc.setxattr(p, n, v, len(v), 0, 0) if v is not None else libc.removexattr(p, n, 0)\n\
            sys.exit(0 if rc == 0 else 1)\n";
        let mut args = vec!["-c", script, path, name];
        if let Some(v) = value {
            args.push(v);
        }
        Command::new("python3")
            .args(&args)
            .status()
            .expect("python3")
            .success()
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

    /// Which of an inode's timestamps moved.
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    struct Moved {
        create: bool,
        modify: bool,
        change: bool,
        access: bool,
    }
    fn moved(before: &Inode, after: &Inode) -> Moved {
        Moved {
            create: after.create_time != before.create_time,
            modify: after.mod_time != before.mod_time,
            change: after.change_time != before.change_time,
            access: after.access_time != before.access_time,
        }
    }
    /// The entries one writer operates on, each in its own directory so the
    /// directory's timestamps are the operation's alone.
    const OPS: [&str; 9] = [
        "setxattr",
        "removexattr",
        "shrink",
        "extend",
        "chmod",
        "link",
        "symlink",
        "unlink",
        "overwrite",
    ];
    /// The subject file of `op` under `prefix`, and its directory.
    fn paths(prefix: &str, op: &str) -> (String, String) {
        (format!("/{prefix}-{op}/f.txt"), format!("/{prefix}-{op}"))
    }
    /// Timestamps of each operation's subject and directory.
    fn snapshot(prefix: &str) -> Vec<(Inode, Inode)> {
        OPS.iter()
            .map(|op| {
                let (f, d) = paths(prefix, op);
                let file = if *op == "unlink" {
                    inode(&d)
                } else {
                    inode(&f)
                };
                (file, inode(&d))
            })
            .collect()
    }
    /// What each operation moved: (subject, directory). For `link` the
    /// subject is the link's target; for `unlink` the directory stands in for
    /// the subject.
    fn effects(before: &[(Inode, Inode)], after: &[(Inode, Inode)]) -> Vec<(String, Moved, Moved)> {
        OPS.iter()
            .zip(before.iter().zip(after.iter()))
            .map(|(op, ((bf, bd), (af, ad)))| (op.to_string(), moved(bf, af), moved(bd, ad)))
            .collect()
    }

    #[test]
    #[ignore = "scratch-image gated; needs xtask gen-scratch and fsck_apfs on macOS"]
    fn our_operations_move_the_timestamps_macos_moves() {
        if !gate() {
            return;
        }
        gen_scratch();

        // macOS makes the entries, then operates on them after a gap.
        with_macos_mount(|mount| {
            for op in OPS {
                std::fs::create_dir(format!("{mount}/m-{op}")).expect("mkdir");
                std::fs::write(format!("{mount}/m-{op}/f.txt"), b"twelve bytes").expect("write");
            }
            assert!(macos_xattr(
                &format!("{mount}/m-removexattr/f.txt"),
                ATTR,
                Some("v")
            ));
        });
        let before = snapshot("m");
        std::thread::sleep(GAP);
        with_macos_mount(|mount| {
            let f = |op: &str| format!("{mount}/m-{op}/f.txt");
            assert!(macos_xattr(&f("setxattr"), ATTR, Some("value")));
            assert!(macos_xattr(&f("removexattr"), ATTR, None));
            std::fs::OpenOptions::new()
                .write(true)
                .open(f("shrink"))
                .unwrap()
                .set_len(3)
                .unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(f("extend"))
                .unwrap()
                .set_len(9000)
                .unwrap();
            assert!(Command::new("chmod")
                .args(["600", &f("chmod")])
                .status()
                .unwrap()
                .success());
            std::fs::hard_link(f("link"), format!("{mount}/m-link/second")).expect("link");
            std::os::unix::fs::symlink("f.txt", format!("{mount}/m-symlink/l")).expect("symlink");
            std::fs::remove_file(f("unlink")).expect("unlink");
            let mut w = std::fs::OpenOptions::new()
                .write(true)
                .open(f("overwrite"))
                .unwrap();
            w.write_all(b"new content!").unwrap();
        });
        let macos = effects(&before, &snapshot("m"));
        for (op, file, dir) in &macos {
            println!("operation-times: macOS {op:<12} subject {file:?} directory {dir:?}");
        }

        // Ours: the same entries and operations, one per transaction.
        for op in OPS {
            let dir =
                in_txn(|t, v, o| mkdir(t, v, o, ROOT_INO, &format!("o-{op}")).expect("mkdir"));
            in_txn(|t, v, o| {
                write_file(t, v, o, dir, "f.txt", b"twelve bytes").expect("write");
            });
        }
        let removexattr_ino = ino("/o-removexattr/f.txt");
        in_txn(|t, v, o| set_xattr(t, v, o, removexattr_ino, ATTR, b"v").expect("set attribute"));
        let before = snapshot("o");
        std::thread::sleep(GAP);
        let dir_ino = |op: &str| ino(&format!("/o-{op}"));
        let setxattr_ino = ino("/o-setxattr/f.txt");
        in_txn(|t, v, o| set_xattr(t, v, o, setxattr_ino, ATTR, b"value").expect("set attribute"));
        in_txn(|t, v, o| delete_xattr(t, v, o, removexattr_ino, ATTR).expect("remove attribute"));
        let (shrink_dir, extend_dir) = (dir_ino("shrink"), dir_ino("extend"));
        in_txn(|t, v, o| {
            truncate_file_fast(t, v, o, shrink_dir, "f.txt", 0, 12).expect("shrink");
        });
        in_txn(|t, v, o| {
            truncate_file_fast(t, v, o, extend_dir, "f.txt", 9000, 12).expect("extend");
        });
        let chmod_dir = dir_ino("chmod");
        in_txn(|t, v, o| {
            set_inode_attrs(
                t,
                v,
                o,
                chmod_dir,
                "f.txt",
                Some(0o600),
                Some(0o777),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .expect("chmod");
        });
        let link_dir = dir_ino("link");
        in_txn(|t, v, o| link(t, v, o, link_dir, "f.txt", link_dir, "second").expect("link"));
        let symlink_dir = dir_ino("symlink");
        in_txn(|t, v, o| {
            create_symlink(t, v, o, symlink_dir, "l", b"f.txt", false).expect("symlink");
        });
        let unlink_dir = dir_ino("unlink");
        in_txn(|t, v, o| unlink(t, v, o, unlink_dir, "f.txt").expect("unlink"));
        let overwrite_dir = dir_ino("overwrite");
        in_txn(|t, v, o| {
            overwrite_existing_file(t, v, o, overwrite_dir, "f.txt", b"new content!")
                .expect("overwrite");
        });
        let ours = effects(&before, &snapshot("o"));
        for (op, file, dir) in &ours {
            println!("operation-times: ours  {op:<12} subject {file:?} directory {dir:?}");
        }
        let differing: Vec<String> = ours
            .iter()
            .zip(macos.iter())
            .filter(|(o, m)| o != m)
            .map(|(o, m)| {
                format!(
                    "{}: ours {:?}/{:?}, macOS {:?}/{:?}",
                    o.0, o.1, o.2, m.1, m.2
                )
            })
            .collect();
        assert!(
            differing.is_empty(),
            "every operation must move the timestamps macOS moves:\n{}",
            differing.join("\n")
        );
        run_fsck("after both writers");
        let _ = std::fs::remove_file(IMG);
    }
}
