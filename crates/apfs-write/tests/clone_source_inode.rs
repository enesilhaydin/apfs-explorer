//! What cloning a file does to the source's inode, ours against macOS's.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! A clone shares the source's blocks, and the source's inode records that
//! it has been cloned. This lets macOS clone a file it made, reads the
//! source's inode before and after, has our writer clone a file of its own
//! and expects the same fields to move in the same way: the flags gained,
//! which times advanced, and the write generation.
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
mod clone_source_inode {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::inode::Inode;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{clone_file, write_file};
    use apfs_write::txn::Transaction;
    use std::io::Read;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-clone-source-inode.img";
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

    /// How a clone moved the source's inode.
    #[derive(Debug, PartialEq, Eq)]
    struct SourceChange {
        flags_gained: u64,
        flags_lost: u64,
        change_time_advanced: bool,
        mod_time_advanced: bool,
        access_time_advanced: bool,
        write_generation_step: u32,
        link_count_step: i64,
    }
    fn change(before: &Inode, after: &Inode) -> SourceChange {
        SourceChange {
            flags_gained: after.internal_flags & !before.internal_flags,
            flags_lost: before.internal_flags & !after.internal_flags,
            change_time_advanced: after.change_time > before.change_time,
            mod_time_advanced: after.mod_time > before.mod_time,
            access_time_advanced: after.access_time > before.access_time,
            write_generation_step: after.write_generation_counter - before.write_generation_counter,
            link_count_step: i64::from(after.nchildren_or_nlink)
                - i64::from(before.nchildren_or_nlink),
        }
    }

    #[test]
    #[ignore = "scratch-image gated; needs xtask gen-scratch and fsck_apfs on macOS"]
    fn our_clone_moves_the_source_inode_as_macos_does() {
        if !gate() {
            return;
        }
        gen_scratch();
        let content = vec![0x5Au8; 8192];
        with_macos_mount(|mount| {
            std::fs::write(format!("{mount}/m-src.txt"), &content).expect("write");
        });
        let macos_before = inode("/m-src.txt");
        with_macos_mount(|mount| {
            let s = Command::new("cp")
                .args([
                    "-c",
                    &format!("{mount}/m-src.txt"),
                    &format!("{mount}/m-clone.txt"),
                ])
                .status()
                .expect("cp -c");
            assert!(s.success(), "cp -c must clone");
        });
        let macos_after = inode("/m-src.txt");
        let macos_change = change(&macos_before, &macos_after);
        println!("clone-source-inode: macOS's clone moved the source by {macos_change:?}");

        in_txn(|t, v, o| {
            write_file(t, v, o, ROOT_INO, "o-src.txt", &content).expect("write");
        });
        let ours_before = inode("/o-src.txt");
        in_txn(|t, v, o| {
            clone_file(t, v, o, ROOT_INO, "o-src.txt", ROOT_INO, "o-clone.txt").expect("clone");
        });
        let ours_after = inode("/o-src.txt");
        let ours_change = change(&ours_before, &ours_after);
        println!("clone-source-inode: our clone moved the source by {ours_change:?}");
        assert_eq!(
            ours_change, macos_change,
            "cloning must move the source's inode as it does under macOS"
        );
        run_fsck("after both clones");
        let _ = std::fs::remove_file(IMG);
    }
}
