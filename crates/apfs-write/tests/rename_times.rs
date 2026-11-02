//! Which timestamps a rename moves, ours against macOS's.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! Renaming an entry changes the directory's content and the inode's name,
//! not the file's data. This lets macOS rename a file and a directory
//! entry it made, reads the renamed inode's and the directory's timestamps
//! before and after, has our writer do the same on entries of its own and
//! expects the same timestamps to move.
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
mod rename_times {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::inode::Inode;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, mkdir, move_entry, rename, write_file};
    use apfs_write::txn::Transaction;
    use std::io::Read;
    use std::process::Command;
    use std::time::Duration;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-rename-times.img";
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

    /// Which of an inode's timestamps moved.
    #[derive(Debug, PartialEq, Eq)]
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
    /// What a rename moved: on the renamed file, on the directory whose entry
    /// was renamed, and on the parent of a renamed directory entry.
    #[derive(Debug, PartialEq, Eq)]
    struct RenameEffect {
        file: Moved,
        root: Moved,
        dir: Moved,
    }

    #[test]
    #[ignore = "scratch-image gated; needs xtask gen-scratch and fsck_apfs on macOS"]
    fn our_rename_moves_the_timestamps_macos_moves() {
        if !gate() {
            return;
        }
        gen_scratch();
        with_macos_mount(|mount| {
            std::fs::write(format!("{mount}/m-a.txt"), b"content").expect("write");
            std::fs::create_dir(format!("{mount}/m-dir")).expect("mkdir");
            std::fs::File::create(format!("{mount}/m-dir/x")).expect("create");
        });
        let (m_file, m_root, m_dir) = (inode("/m-a.txt"), inode("/"), inode("/m-dir"));
        std::thread::sleep(GAP);
        with_macos_mount(|mount| {
            std::fs::rename(format!("{mount}/m-a.txt"), format!("{mount}/m-b.txt"))
                .expect("rename");
            std::fs::rename(format!("{mount}/m-dir/x"), format!("{mount}/m-dir/y"))
                .expect("rename");
        });
        let macos = RenameEffect {
            file: moved(&m_file, &inode("/m-b.txt")),
            root: moved(&m_root, &inode("/")),
            dir: moved(&m_dir, &inode("/m-dir")),
        };
        println!("rename-times: macOS's rename moved {macos:?}");

        in_txn(|t, v, o| {
            write_file(t, v, o, ROOT_INO, "o-a.txt", b"content").expect("write");
        });
        let dir_ino = in_txn(|t, v, o| mkdir(t, v, o, ROOT_INO, "o-dir").expect("mkdir"));
        in_txn(|t, v, o| {
            create_file(t, v, o, dir_ino, "x").expect("create");
        });
        let (o_file, o_root, o_dir) = (inode("/o-a.txt"), inode("/"), inode("/o-dir"));
        std::thread::sleep(GAP);
        in_txn(|t, v, o| {
            rename(t, v, o, ROOT_INO, "o-a.txt", "o-b.txt", false).expect("rename");
        });
        in_txn(|t, v, o| {
            rename(t, v, o, dir_ino, "x", "y", false).expect("rename");
        });
        let ours = RenameEffect {
            file: moved(&o_file, &inode("/o-b.txt")),
            root: moved(&o_root, &inode("/")),
            dir: moved(&o_dir, &inode("/o-dir")),
        };
        println!("rename-times: our rename moved {ours:?}");
        assert_eq!(ours, macos, "a rename must move the timestamps macOS moves");

        // A move to another directory: the file changes directories, not
        // content.
        let m_file = inode("/m-b.txt");
        std::thread::sleep(GAP);
        with_macos_mount(|mount| {
            std::fs::rename(format!("{mount}/m-b.txt"), format!("{mount}/m-dir/m-b.txt"))
                .expect("move");
        });
        let macos_move = moved(&m_file, &inode("/m-dir/m-b.txt"));
        println!("rename-times: macOS's move to another directory moved the file's {macos_move:?}");
        let o_file = inode("/o-b.txt");
        std::thread::sleep(GAP);
        in_txn(|t, v, o| {
            move_entry(t, v, o, ROOT_INO, "o-b.txt", dir_ino, "o-b.txt", false).expect("move");
        });
        let our_move = moved(&o_file, &inode("/o-dir/o-b.txt"));
        println!("rename-times: our move to another directory moved the file's {our_move:?}");
        assert_eq!(
            our_move, macos_move,
            "a move must move the timestamps macOS moves"
        );
        run_fsck("after both renames and moves");
        let _ = std::fs::remove_file(IMG);
    }
}
