//! Removing one side of a clone macOS made.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + cp -c).
//!
//! macOS clones a file by giving the copy the original's data-stream id: both
//! inodes name the same stream, and the stream's reference count says two.
//! Removing either name must leave the other reading exactly what it did,
//! which means the stream and its blocks stay until the last reference goes.
//! An unlink that gathers a file's extents by inode id and frees them once the
//! inode is gone destroys the clone when the original is removed - and leaves
//! the stream's count claiming a reference that no longer exists when the
//! clone is.
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
mod macos_clone_unlink {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::unlink;
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-macos-clone-unlink.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ORIGINAL: &str = "original.bin";
    const CLONE: &str = "clone.bin";
    const SIZE: usize = 300 * 1024;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn body() -> Vec<u8> {
        (0..SIZE).map(|i| ((i * 7) % 253) as u8).collect()
    }

    /// Build an image, let macOS write the original and clone it.
    fn build_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-macos-clone-unlink.dmg");
        let out = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "64m",
                "-layout",
                "GPTSPUD",
                "-fs",
                "APFS",
                "-volname",
                "CLONEUL",
                "/tmp/apfs-macos-clone-unlink",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-macos-clone-unlink.dmg", IMG).expect("rename");

        let (mount, parent) = attach(true);
        std::fs::write(format!("{mount}/{ORIGINAL}"), body()).expect("write original");
        let st = Command::new("cp")
            .args([
                "-c",
                &format!("{mount}/{ORIGINAL}"),
                &format!("{mount}/{CLONE}"),
            ])
            .status()
            .expect("cp -c");
        assert!(st.success(), "cp -c failed");
        let _ = Command::new("sync").status();
        detach(&parent);
    }

    /// Attach the image; mount it when `mount_it`. Returns (mount point, whole-disk node).
    fn attach(mount_it: bool) -> (String, String) {
        let mut args = vec![
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nobrowse",
        ];
        if !mount_it {
            args.push("-nomount");
        }
        args.push(IMG);
        let out = Command::new("hdiutil")
            .args(&args)
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .unwrap_or_default();
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        (mount, parent)
    }

    fn detach(parent: &str) {
        let _ = Command::new("hdiutil").args(["detach", parent]).status();
    }

    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let hdr = &raw[512..1024];
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let off = base + i * sz;
            let e = match raw.get(off..off + sz) {
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

    fn read_vsb_omap() -> (Vec<u8>, Vec<u8>) {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
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

    fn open_txn() -> Transaction<Dev> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }

    fn open_fs() -> apfs::FsView<Dev> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        apfs::FsView::open(WritableOffsetBlockDevice::new(inner, base, len)).expect("FsView::open")
    }

    fn run_fsck(label: &str) {
        let (_, parent) = attach(false);
        let apfs = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &apfs]).output();
        detach(&parent);
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

    /// Remove `victim`, then require `survivor` to read exactly as before, both
    /// through our reader and through macOS, with the volume fsck-clean.
    fn remove_one_and_check_the_other(victim: &str, survivor: &str) {
        build_image();
        let body = body();
        {
            let mut fs = open_fs();
            for name in [ORIGINAL, CLONE] {
                let got = fs.read(&format!("/{name}"), 0, SIZE).expect("read");
                assert_eq!(
                    got, body,
                    "{name} must read whole before anything is removed"
                );
            }
        }

        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            unlink(&mut txn, &vsb, &om, 2, victim).expect("unlink");
            txn.commit().expect("commit unlink");
        }
        run_fsck(&format!("after removing {victim}"));

        {
            let mut fs = open_fs();
            assert!(
                fs.lookup(&format!("/{victim}")).expect("lookup").is_none(),
                "{victim} must be gone"
            );
            let got = fs
                .read(&format!("/{survivor}"), 0, SIZE)
                .expect("read survivor");
            assert_eq!(
                got, body,
                "{survivor} must still read what it did before {victim} was removed"
            );
        }

        let (mount, parent) = attach(true);
        let seen = std::fs::read(format!("{mount}/{survivor}"));
        let victim_exists = std::path::Path::new(&format!("{mount}/{victim}")).exists();
        detach(&parent);
        assert!(!victim_exists, "macOS must not see {victim} any more");
        assert_eq!(
            seen.expect("macOS read"),
            body,
            "macOS must read {survivor} intact"
        );
    }

    #[test]
    #[ignore = "fsck-gated; needs macOS to make the clone"]
    fn removing_the_original_keeps_the_clone_intact() {
        if !gate() {
            println!("macos-clone-unlink: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        remove_one_and_check_the_other(ORIGINAL, CLONE);
        println!("macos-clone-unlink PASS: the clone survived removing the original");
    }

    #[test]
    #[ignore = "fsck-gated; needs macOS to make the clone"]
    fn removing_the_clone_keeps_the_original_intact() {
        if !gate() {
            println!("macos-clone-unlink: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        remove_one_and_check_the_other(CLONE, ORIGINAL);
        println!("macos-clone-unlink PASS: the original survived removing the clone");
    }
}
