//! Timestamps around links, clones and directories, ours against macOS's.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! Five more operations, each on entries in a directory of its own:
//! removing one of a file's two names, cloning a file, removing an empty
//! directory, setting a file's times explicitly, and creating a directory.
//! macOS runs each on entries it made, our writer on entries of its own, and
//! the same timestamps must move.
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
mod link_clone_times {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::inode::Inode;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{clone_file, link, mkdir, set_inode_attrs, unlink, write_file};
    use apfs_write::txn::Transaction;
    use std::io::Read;
    use std::process::Command;
    use std::time::Duration;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-link-clone-times.img";
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
    /// 2020-01-01 00:00:00 UTC, in nanoseconds since the epoch.
    const EXPLICIT_NS: u64 = 1_577_836_800_000_000_000;

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
            std::thread::sleep(Duration::from_millis(250 * (attempt + 1)));
        }
        let _ = Command::new("hdiutil")
            .args(["detach", "-force", disk])
            .status();
    }
    /// macOS mounts the image read-write and runs `f` on the mount point; the
    /// image is detached on the way out, a panic inside `f` included.
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

    /// Which of an inode's timestamps differ between two readings.
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
    /// What each operation moved: (subject, directory).
    #[derive(Debug, PartialEq, Eq)]
    struct Effects {
        /// Removing one of two names: the surviving inode.
        unlink_one_name: (Moved, Moved),
        /// Cloning: the clone's timestamps against the source's.
        clone: (Moved, Moved),
        /// Removing an empty directory: the directory it was in.
        rmdir: Moved,
        /// Setting the modification and access times explicitly.
        set_times: (Moved, Moved),
        /// Creating a directory: the directory it is made in.
        mkdir: Moved,
    }
    struct Before {
        links_file: Inode,
        links_dir: Inode,
        clone_src: Inode,
        clone_dir: Inode,
        rmdir_dir: Inode,
        times_file: Inode,
        times_dir: Inode,
        mkdir_dir: Inode,
    }
    fn before(prefix: &str) -> Before {
        Before {
            links_file: inode(&format!("/{prefix}-links/f.txt")),
            links_dir: inode(&format!("/{prefix}-links")),
            clone_src: inode(&format!("/{prefix}-clone/f.txt")),
            clone_dir: inode(&format!("/{prefix}-clone")),
            rmdir_dir: inode(&format!("/{prefix}-rmdir")),
            times_file: inode(&format!("/{prefix}-times/f.txt")),
            times_dir: inode(&format!("/{prefix}-times")),
            mkdir_dir: inode(&format!("/{prefix}-mkdir")),
        }
    }
    fn effects(prefix: &str, b: &Before) -> Effects {
        let p = |s: &str| format!("/{prefix}-{s}");
        Effects {
            unlink_one_name: (
                moved(&b.links_file, &inode(&p("links/f.txt"))),
                moved(&b.links_dir, &inode(&p("links"))),
            ),
            clone: (
                moved(&b.clone_src, &inode(&p("clone/c.txt"))),
                moved(&b.clone_dir, &inode(&p("clone"))),
            ),
            rmdir: moved(&b.rmdir_dir, &inode(&p("rmdir"))),
            set_times: (
                moved(&b.times_file, &inode(&p("times/f.txt"))),
                moved(&b.times_dir, &inode(&p("times"))),
            ),
            mkdir: moved(&b.mkdir_dir, &inode(&p("mkdir"))),
        }
    }

    #[test]
    #[ignore = "scratch-image gated; needs xtask gen-scratch and fsck_apfs on macOS"]
    fn our_links_clones_and_directories_move_the_timestamps_macos_moves() {
        if !gate() {
            return;
        }
        gen_scratch();
        with_macos_mount(|mount| {
            for d in ["links", "clone", "rmdir", "times", "mkdir"] {
                std::fs::create_dir(format!("{mount}/m-{d}")).expect("mkdir");
            }
            std::fs::write(format!("{mount}/m-links/f.txt"), b"twelve bytes").expect("write");
            std::fs::hard_link(
                format!("{mount}/m-links/f.txt"),
                format!("{mount}/m-links/second"),
            )
            .expect("link");
            std::fs::write(format!("{mount}/m-clone/f.txt"), b"twelve bytes").expect("write");
            std::fs::create_dir(format!("{mount}/m-rmdir/sub")).expect("mkdir");
            std::fs::write(format!("{mount}/m-times/f.txt"), b"twelve bytes").expect("write");
        });
        let m_before = before("m");
        std::thread::sleep(GAP);
        with_macos_mount(|mount| {
            std::fs::remove_file(format!("{mount}/m-links/second")).expect("unlink");
            let s = Command::new("cp")
                .args([
                    "-c",
                    &format!("{mount}/m-clone/f.txt"),
                    &format!("{mount}/m-clone/c.txt"),
                ])
                .status()
                .expect("cp -c");
            assert!(s.success(), "cp -c must clone");
            std::fs::remove_dir(format!("{mount}/m-rmdir/sub")).expect("rmdir");
            // The time is read in the local zone; pin it to UTC.
            let s = Command::new("touch")
                .env("TZ", "UTC")
                .args(["-t", "202001010000", &format!("{mount}/m-times/f.txt")])
                .status()
                .expect("touch");
            assert!(s.success(), "touch must set the times");
            std::fs::create_dir(format!("{mount}/m-mkdir/sub")).expect("mkdir");
        });
        let macos = effects("m", &m_before);
        println!("link-clone-times: macOS moved {macos:?}");
        let m_times = inode("/m-times/f.txt");
        assert_eq!(
            m_times.mod_time, EXPLICIT_NS,
            "touch -t sets the modification time"
        );
        assert_eq!(
            m_times.create_time, EXPLICIT_NS,
            "a modification time earlier than the birth time pulls the birth time back to it"
        );

        for d in ["links", "clone", "rmdir", "times", "mkdir"] {
            in_txn(|t, v, o| mkdir(t, v, o, ROOT_INO, &format!("o-{d}")).expect("mkdir"));
        }
        let dir = |d: &str| ino(&format!("/o-{d}"));
        let (links, clone, rmdir, times, mkdir_dir) = (
            dir("links"),
            dir("clone"),
            dir("rmdir"),
            dir("times"),
            dir("mkdir"),
        );
        in_txn(|t, v, o| write_file(t, v, o, links, "f.txt", b"twelve bytes").expect("write"));
        in_txn(|t, v, o| link(t, v, o, links, "f.txt", links, "second").expect("link"));
        in_txn(|t, v, o| write_file(t, v, o, clone, "f.txt", b"twelve bytes").expect("write"));
        in_txn(|t, v, o| mkdir(t, v, o, rmdir, "sub").expect("mkdir"));
        in_txn(|t, v, o| write_file(t, v, o, times, "f.txt", b"twelve bytes").expect("write"));
        let o_before = before("o");
        std::thread::sleep(GAP);
        in_txn(|t, v, o| unlink(t, v, o, links, "second").expect("unlink"));
        in_txn(|t, v, o| clone_file(t, v, o, clone, "f.txt", clone, "c.txt").expect("clone"));
        in_txn(|t, v, o| unlink(t, v, o, rmdir, "sub").expect("rmdir"));
        in_txn(|t, v, o| {
            set_inode_attrs(
                t,
                v,
                o,
                times,
                "f.txt",
                None,
                None,
                Some(EXPLICIT_NS),
                None,
                Some(EXPLICIT_NS),
                None,
                None,
                None,
            )
            .expect("set times");
        });
        in_txn(|t, v, o| mkdir(t, v, o, mkdir_dir, "sub").expect("mkdir"));
        let ours = effects("o", &o_before);
        println!("link-clone-times: ours moved {ours:?}");
        assert_eq!(
            ours, macos,
            "every operation must move the timestamps macOS moves"
        );
        run_fsck("after both writers");
        let _ = std::fs::remove_file(IMG);
    }
}
