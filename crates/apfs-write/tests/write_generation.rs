//! The inode's write-generation counter, ours against macOS's.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! Every inode carries a counter the kernel steps when the inode or its
//! content changes. This lets macOS create, write, change the mode of and
//! add children to entries on a scratch image, reads the counter after each
//! step, has our writer do the same on entries of its own and expects the
//! same starting values and the same steps.
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
mod write_generation {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        create_file, mkdir, overwrite_existing_file, rename, set_inode_attrs, set_xattr,
        truncate_file_fast, unlink, write_file, TruncateOutcome,
    };
    use apfs_write::txn::Transaction;
    use std::io::{Read, Write};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-write-generation.img";
    const IMAGE_MB: &str = "64";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ROOT_INO: u64 = 2;

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
    fn in_txn(f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8])) {
        let (vsb, om) = read_vsb_omap();
        let mut txn = Transaction::begin(open_dev()).expect("begin");
        f(&mut txn, &vsb, &om);
        txn.commit().expect("commit");
    }
    fn open_fs() -> apfs::FsView<Dev> {
        apfs::FsView::open(open_dev()).expect("FsView::open")
    }
    /// The counter of the entry at `path`.
    fn counter(path: &str) -> u32 {
        open_fs()
            .inode_record(path)
            .expect("inode record")
            .unwrap_or_else(|| panic!("{path} must exist"))
            .write_generation_counter
    }
    fn whole_disk(attached: &str) -> String {
        attached
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string()
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

    /// The counters of one writer's entries.
    #[derive(Debug, PartialEq, Eq)]
    struct Counters {
        /// A file created empty; its mode is changed later.
        empty: u32,
        /// A file created with content; more is appended later.
        written: u32,
        /// A file created with content; it is truncated later.
        trunc: u32,
        /// A file created empty; an attribute is set on it later.
        xattr: u32,
        /// A file created empty; it is renamed later (read by its final name).
        renamed: u32,
        /// A directory; a child is created in it, then removed.
        dir: u32,
        /// A directory holding one entry; the entry is renamed in place.
        dir2: u32,
        root: u32,
    }
    fn counters(prefix: &str) -> Counters {
        let named = |n: &str| counter(&format!("/{prefix}-{n}"));
        Counters {
            empty: named("empty.txt"),
            written: named("file.txt"),
            trunc: named("trunc.txt"),
            xattr: named("xattr.txt"),
            renamed: if open_fs()
                .lookup(&format!("/{prefix}-renamed.txt"))
                .unwrap()
                .is_some()
            {
                named("renamed.txt")
            } else {
                named("rename.txt")
            },
            dir: named("dir"),
            dir2: named("dir2"),
            root: counter("/"),
        }
    }
    /// The counters' steps from `a` to `b`.
    fn steps(a: &Counters, b: &Counters) -> Counters {
        Counters {
            empty: b.empty - a.empty,
            written: b.written - a.written,
            trunc: b.trunc - a.trunc,
            xattr: b.xattr - a.xattr,
            renamed: b.renamed - a.renamed,
            dir: b.dir - a.dir,
            dir2: b.dir2 - a.dir2,
            root: b.root - a.root,
        }
    }
    /// `setxattr` through libc.
    fn macos_set_xattr(path: &str, name: &str, value: &str) -> bool {
        let script = "import ctypes, sys\n\
            libc = ctypes.CDLL(None, use_errno=True)\n\
            libc.setxattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_char_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]\n\
            v = sys.argv[3].encode()\n\
            sys.exit(0 if libc.setxattr(sys.argv[1].encode(), sys.argv[2].encode(), v, len(v), 0, 0) == 0 else 1)\n";
        Command::new("python3")
            .args(["-c", script, path, name, value])
            .status()
            .expect("python3")
            .success()
    }

    #[test]
    #[ignore = "scratch-image gated; needs xtask gen-scratch and fsck_apfs on macOS"]
    fn our_counter_starts_and_steps_as_macos_does() {
        if !gate() {
            return;
        }
        gen_scratch();
        let root_before = counter("/");

        // macOS: make the entries.
        with_macos_mount(|mount| {
            std::fs::File::create(format!("{mount}/m-empty.txt")).expect("create");
            std::fs::write(format!("{mount}/m-file.txt"), b"one").expect("write");
            std::fs::write(format!("{mount}/m-trunc.txt"), b"truncate me").expect("write");
            std::fs::File::create(format!("{mount}/m-xattr.txt")).expect("create");
            std::fs::File::create(format!("{mount}/m-rename.txt")).expect("create");
            std::fs::create_dir(format!("{mount}/m-dir")).expect("mkdir");
            std::fs::create_dir(format!("{mount}/m-dir2")).expect("mkdir");
            std::fs::File::create(format!("{mount}/m-dir2/x")).expect("create");
        });
        let macos_made = counters("m");
        // macOS: change them.
        with_macos_mount(|mount| {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(format!("{mount}/m-file.txt"))
                .expect("open for append");
            f.write_all(b"two").expect("append");
            drop(f);
            let s = Command::new("chmod")
                .args(["600", &format!("{mount}/m-empty.txt")])
                .status()
                .expect("chmod");
            assert!(s.success(), "chmod must succeed");
            std::fs::File::create(format!("{mount}/m-trunc.txt")).expect("truncate");
            assert!(
                macos_set_xattr(&format!("{mount}/m-xattr.txt"), "user.note", "value"),
                "macOS must take the attribute"
            );
            std::fs::rename(
                format!("{mount}/m-rename.txt"),
                format!("{mount}/m-renamed.txt"),
            )
            .expect("rename");
            std::fs::File::create(format!("{mount}/m-dir/child.txt")).expect("child");
            std::fs::rename(format!("{mount}/m-dir2/x"), format!("{mount}/m-dir2/y"))
                .expect("rename in place");
        });
        let macos_changed = counters("m");
        // macOS: remove the child again.
        with_macos_mount(|mount| {
            std::fs::remove_file(format!("{mount}/m-dir/child.txt")).expect("unlink");
        });
        let macos_removed = counters("m");
        let macos_change_steps = steps(&macos_made, &macos_changed);
        let macos_remove_steps = steps(&macos_changed, &macos_removed);
        println!(
            "write-generation: root before {root_before}; macOS made {macos_made:?}; \
             changes stepped {macos_change_steps:?}; the removal stepped {macos_remove_steps:?}"
        );

        // Ours: the same entries and the same changes, one per transaction.
        in_txn(|t, v, o| {
            create_file(t, v, o, ROOT_INO, "o-empty.txt").expect("create");
        });
        in_txn(|t, v, o| {
            write_file(t, v, o, ROOT_INO, "o-file.txt", b"one").expect("write");
        });
        in_txn(|t, v, o| {
            write_file(t, v, o, ROOT_INO, "o-trunc.txt", b"truncate me").expect("write");
        });
        in_txn(|t, v, o| {
            create_file(t, v, o, ROOT_INO, "o-xattr.txt").expect("create");
        });
        in_txn(|t, v, o| {
            create_file(t, v, o, ROOT_INO, "o-rename.txt").expect("create");
        });
        in_txn(|t, v, o| {
            mkdir(t, v, o, ROOT_INO, "o-dir").expect("mkdir");
        });
        let dir2_ino =
            in_txn_returning(|t, v, o| mkdir(t, v, o, ROOT_INO, "o-dir2").expect("mkdir"));
        in_txn(|t, v, o| {
            create_file(t, v, o, dir2_ino, "x").expect("create");
        });
        let ours_made = counters("o");
        in_txn(|t, v, o| {
            overwrite_existing_file(t, v, o, ROOT_INO, "o-file.txt", b"onetwo").expect("append");
        });
        in_txn(|t, v, o| {
            set_inode_attrs(
                t,
                v,
                o,
                ROOT_INO,
                "o-empty.txt",
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
        in_txn(|t, v, o| {
            let outcome =
                truncate_file_fast(t, v, o, ROOT_INO, "o-trunc.txt", 0, 11).expect("truncate");
            assert!(
                matches!(outcome, TruncateOutcome::Applied),
                "truncate to 0 is the fast path"
            );
        });
        let xattr_ino = open_fs()
            .lookup("/o-xattr.txt")
            .expect("lookup")
            .expect("o-xattr.txt");
        in_txn(|t, v, o| {
            set_xattr(t, v, o, xattr_ino, "user.note", b"value").expect("set attribute");
        });
        in_txn(|t, v, o| {
            rename(t, v, o, ROOT_INO, "o-rename.txt", "o-renamed.txt", false).expect("rename");
        });
        let dir_ino = open_fs().lookup("/o-dir").expect("lookup").expect("o-dir");
        in_txn(|t, v, o| {
            create_file(t, v, o, dir_ino, "child.txt").expect("child");
        });
        in_txn(|t, v, o| {
            rename(t, v, o, dir2_ino, "x", "y", false).expect("rename in place");
        });
        let ours_changed = counters("o");
        in_txn(|t, v, o| {
            unlink(t, v, o, dir_ino, "child.txt").expect("unlink");
        });
        let ours_removed = counters("o");
        let ours_change_steps = steps(&ours_made, &ours_changed);
        let ours_remove_steps = steps(&ours_changed, &ours_removed);
        println!(
            "write-generation: ours made {ours_made:?}; changes stepped {ours_change_steps:?}; \
             the removal stepped {ours_remove_steps:?}"
        );

        // The root's field is the step its seven new entries gave it.
        let mut ours_start = ours_made;
        ours_start.root -= macos_removed.root;
        let mut macos_start = macos_made;
        macos_start.root -= root_before;
        assert_eq!(
            ours_start, macos_start,
            "a new entry's counter must start where macOS starts it (root: by the entries added)"
        );
        assert_eq!(
            ours_change_steps, macos_change_steps,
            "each change must step the counter as it does under macOS"
        );
        assert_eq!(
            ours_remove_steps, macos_remove_steps,
            "removing a child must step as it does under macOS"
        );
        run_fsck("after both writers");
        let _ = std::fs::remove_file(IMG);
    }

    fn in_txn_returning<T>(f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> T) -> T {
        let (vsb, om) = read_vsb_omap();
        let mut txn = Transaction::begin(open_dev()).expect("begin");
        let out = f(&mut txn, &vsb, &om);
        txn.commit().expect("commit");
        out
    }
}
