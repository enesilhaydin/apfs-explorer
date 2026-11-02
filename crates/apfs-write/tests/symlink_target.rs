//! Symlink target encoding on a real scratch APFS image.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS + hdiutil).
//!
//! APFS stores a symlink's target in the `com.apple.fs.symlink` xattr as a
//! NUL-terminated string: readers take the value length minus the terminator.
//! Storing the target without it makes every consumer see a target one byte
//! short, so the link resolves to the wrong path.
#![cfg(feature = "write")]
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
mod symlink_tests {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_symlink, unlink};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-symlink.img";
    /// `apfs_superblock_t`: `apfs_num_files` and `apfs_num_symlinks`.
    const VSB_NUM_FILES: usize = 0xB8;
    const VSB_NUM_SYMLINKS: usize = 0xC8;
    const SIZE_MB: u32 = 64;
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn gen_scratch() {
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
        let status = Command::new("cargo")
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

    /// Attach without mounting and let Apple's fsck judge the image. Warnings
    /// count: a symlink counted in the wrong field is exactly that kind of
    /// finding, and it exits 0.
    fn run_fsck() {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-nomount",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut apfs_dev = None;
        let mut parent_dev = None;
        for line in stdout.lines() {
            let dev = line.split_whitespace().next().unwrap_or("");
            if !dev.starts_with("/dev/disk") {
                continue;
            }
            if parent_dev.is_none() && !dev.trim_start_matches("/dev/disk").contains('s') {
                parent_dev = Some(dev.to_string());
            }
            if apfs_dev.is_none() && line.contains("Apple_APFS") {
                apfs_dev = Some(dev.to_string());
            }
        }
        let parent = parent_dev.expect("no whole-disk node");
        let apfs = apfs_dev.expect("no Apple_APFS partition");
        let rdev = apfs.replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        let _ = Command::new("hdiutil").args(["detach", &parent]).status();
        let fsck = fsck.expect("fsck_apfs spawn");
        let report = String::from_utf8_lossy(&fsck.stdout).into_owned()
            + &String::from_utf8_lossy(&fsck.stderr);
        assert!(
            fsck.status.success(),
            "fsck_apfs reported corruption:\n{report}"
        );
        let complaints: Vec<&str> = report
            .lines()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("warning:") || l.contains("is not valid")
            })
            .collect();
        assert!(
            complaints.is_empty(),
            "fsck_apfs flagged inconsistencies:\n{}",
            complaints.join("\n")
        );
    }

    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let hdr = &raw[512..1024];
        assert_eq!(&hdr[..8], b"EFI PART", "no GPT signature");
        let part_entry_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let num_parts = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let part_entry_size = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let entries_off = (part_entry_lba * SECTOR) as usize;
        for i in 0..num_parts {
            let off = entries_off + i * part_entry_size;
            let entry = match raw.get(off..off + part_entry_size) {
                Some(e) => e,
                None => break,
            };
            if entry.get(..16) == Some(&APFS_GPT_TYPE[..]) {
                let start = u64::from_le_bytes(entry[32..40].try_into().unwrap());
                let end = u64::from_le_bytes(entry[40..48].try_into().unwrap());
                return (start * SECTOR, (end - start + 1) * SECTOR);
            }
        }
        panic!("no APFS partition in GPT");
    }

    fn read_vsb_omap_raw() -> (Vec<u8>, Vec<u8>) {
        let raw_img = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw_img);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let container = Container::open(&mut dev).expect("Container::open");
        let nx = &container.superblock;
        let bsz = nx.block_size as usize;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).expect("container omap");
        let fs_oid = *nx.fs_oids.first().expect("no fs_oids");
        let vol_paddr = omap
            .resolve(&mut dev, fs_oid, nx.obj.xid, nx.block_size)
            .expect("omap resolve")
            .expect("volume not found");
        let mut vsb_raw = vec![0u8; bsz];
        dev.read_at(vol_paddr * bsz as u64, &mut vsb_raw)
            .expect("read vsb");
        let vsb = VolumeSuperblock::parse(&vsb_raw).expect("parse vsb");
        let mut omap_raw = vec![0u8; bsz];
        dev.read_at(vsb.omap_oid * bsz as u64, &mut omap_raw)
            .expect("read vol omap");
        (vsb_raw, omap_raw)
    }

    fn open_dev() -> WritableOffsetBlockDevice<FileBlockDeviceRw> {
        let raw = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open_rw");
        WritableOffsetBlockDevice::new(inner, base, len)
    }

    fn open_txn() -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        Transaction::begin(open_dev()).expect("Transaction::begin")
    }

    /// The raw bytes stored in the symlink xattr, exactly as they are on disk.
    fn raw_symlink_xattr(path_ino: u64) -> Vec<u8> {
        use apfs_core::catalog::Catalog;
        let raw = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let container = Container::open(&mut dev).expect("Container::open");
        let nx = &container.superblock;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).expect("container omap");
        let fs_oid = *nx.fs_oids.first().expect("no fs_oids");
        let vol_paddr = omap
            .resolve(&mut dev, fs_oid, nx.obj.xid, nx.block_size)
            .expect("resolve")
            .expect("volume");
        let mut vsb_raw = vec![0u8; nx.block_size as usize];
        dev.read_at(vol_paddr * nx.block_size as u64, &mut vsb_raw)
            .expect("read vsb");
        let vsb = VolumeSuperblock::parse(&vsb_raw).expect("parse vsb");
        let catalog = Catalog::open(&mut dev, &vsb, nx.block_size).expect("catalog");
        catalog
            .get_xattr(&mut dev, path_ino, "com.apple.fs.symlink")
            .expect("get_xattr")
            .expect("symlink xattr must exist")
    }

    #[test]
    fn symlink_target_is_nul_terminated_on_disk() {
        if !check_gate() {
            println!("symlink: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let target = b"/Users/example/target.txt";
        let ino = {
            let (vsb, omap) = read_vsb_omap_raw();
            let mut txn = open_txn();
            let ino = create_symlink(&mut txn, &vsb, &omap, 2, "link.lnk", target, false)
                .expect("create_symlink");
            txn.commit().expect("commit");
            ino
        };

        let stored = raw_symlink_xattr(ino);
        assert_eq!(
            stored.last(),
            Some(&0u8),
            "symlink target must be stored NUL-terminated; without it every reader \
             takes length-minus-one and resolves a truncated path. stored={:?}",
            String::from_utf8_lossy(&stored)
        );
        assert_eq!(
            stored.len(),
            target.len() + 1,
            "stored value must be the target plus exactly one terminator"
        );
        assert_eq!(
            &stored[..target.len()],
            target,
            "target bytes must round-trip"
        );

        run_fsck();

        // The reader must hand back the target itself, terminator removed.
        use apfs::FsView;
        let raw = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut fs = FsView::open(dev).expect("FsView::open");
        let got = fs
            .read_symlink("/link.lnk")
            .expect("read_symlink")
            .expect("link must exist");
        assert_eq!(
            got, target,
            "read_symlink must return the target without the on-disk terminator"
        );

        println!("symlink PASS: target stored NUL-terminated and read back intact");
    }

    #[test]
    fn symlink_rejects_degenerate_targets() {
        if !check_gate() {
            println!("symlink: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let (vsb, omap) = read_vsb_omap_raw();
        let mut txn = open_txn();
        assert!(
            create_symlink(&mut txn, &vsb, &omap, 2, "empty.lnk", b"", false).is_err(),
            "an empty symlink target must be refused"
        );
        assert!(
            create_symlink(&mut txn, &vsb, &omap, 2, "interior.lnk", b"/a\0/b", false).is_err(),
            "a target with an interior NUL must be refused"
        );
        // macOS refuses a 1024-byte target with ENAMETOOLONG (PATH_MAX less the
        // terminator); so do we, before anything is staged. A longer one would
        // not even fit the attribute that stores it.
        let too_long = vec![b'a'; 1024];
        assert!(
            create_symlink(&mut txn, &vsb, &omap, 2, "long.lnk", &too_long, false).is_err(),
            "a 1024-byte target must be refused"
        );
        println!("symlink PASS: degenerate targets refused");
    }

    /// The longest target macOS itself accepts has to round-trip: written by
    /// us, judged by fsck_apfs, read back by macOS.
    #[test]
    fn the_longest_target_macos_accepts_round_trips() {
        if !check_gate() {
            println!("symlink: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        let target: Vec<u8> = (0..1023u32).map(|i| b'a' + (i % 26) as u8).collect();
        {
            let (vsb, omap) = read_vsb_omap_raw();
            let mut txn = open_txn();
            create_symlink(&mut txn, &vsb, &omap, 2, "longest.lnk", &target, false)
                .expect("a 1023-byte target is the longest macOS accepts and must be taken");
            txn.commit().expect("commit");
        }
        run_fsck();
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                "-readonly",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach (mount) failed");
        let attached = String::from_utf8_lossy(&out.stdout).into_owned();
        let disk = attached
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
        let mount = attached
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let seen = std::fs::read_link(format!("{mount}/longest.lnk"));
        let _ = Command::new("hdiutil").args(["detach", &disk]).output();
        let seen = seen.expect("macOS must read the link");
        assert_eq!(
            seen.as_os_str().as_encoded_bytes(),
            target.as_slice(),
            "macOS must read back the whole 1023-byte target"
        );
        println!("symlink PASS: a 1023-byte target round-trips through macOS");
    }

    /// The attribute holding a symlink's target is owned by the file system,
    /// and macOS marks it so; ours has to carry the same flags, or tools that
    /// list attributes show the target as a user attribute.
    #[test]
    fn our_symlink_attribute_carries_the_flags_macos_gives_it() {
        if !check_gate() {
            println!("symlink: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        // macOS makes one.
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
        let disk = attached
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
        let mount = attached
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let st = Command::new("ln")
            .args(["-s", "target.txt", &format!("{mount}/macos.lnk")])
            .status()
            .expect("ln -s");
        assert!(st.success(), "macOS must make the link");
        let _ = Command::new("sync").status();
        let _ = Command::new("hdiutil").args(["detach", &disk]).output();
        // We make one.
        {
            let (vsb, omap) = read_vsb_omap_raw();
            let mut txn = open_txn();
            create_symlink(&mut txn, &vsb, &omap, 2, "ours.lnk", b"target.txt", false)
                .expect("create_symlink");
            txn.commit().expect("commit");
        }
        let mut fs = apfs::FsView::open(open_dev()).expect("open");
        let theirs = fs
            .xattr_flags("/macos.lnk", "com.apple.fs.symlink")
            .expect("flags")
            .expect("macOS's link has the attribute");
        let ours = fs
            .xattr_flags("/ours.lnk", "com.apple.fs.symlink")
            .expect("flags")
            .expect("our link has the attribute");
        println!(
            "symlink: macOS stores the target attribute with flags {theirs:#x}, we with {ours:#x}"
        );
        assert_eq!(
            ours, theirs,
            "the target attribute must carry the flags macOS gives it"
        );
        let listed = fs.list_xattrs("/ours.lnk").expect("list");
        assert!(
            !listed.iter().any(|n| n == "com.apple.fs.symlink"),
            "the target attribute is the file system's, not a user attribute: {listed:?}"
        );
        drop(fs);
        run_fsck();
        println!("symlink PASS: the target attribute is file-system owned");
    }

    /// The volume counts symlinks apart from regular files, so removing one
    /// has to come off the symlink count, not the file count.
    #[test]
    fn removing_a_symlink_comes_off_the_symlink_count() {
        if !check_gate() {
            println!("symlink: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        let counts = |vsb: &[u8]| {
            let n = |o: usize| u64::from_le_bytes(vsb[o..o + 8].try_into().unwrap());
            (n(VSB_NUM_FILES), n(VSB_NUM_SYMLINKS))
        };
        let (files0, links0) = counts(&read_vsb_omap_raw().0);
        {
            let (vsb, omap) = read_vsb_omap_raw();
            let mut txn = open_txn();
            create_symlink(&mut txn, &vsb, &omap, 2, "counted.lnk", b"target", false)
                .expect("create_symlink");
            txn.commit().expect("commit");
        }
        let (files1, links1) = counts(&read_vsb_omap_raw().0);
        assert_eq!(
            (files1, links1),
            (files0, links0 + 1),
            "creating a symlink counts a symlink"
        );
        {
            let (vsb, omap) = read_vsb_omap_raw();
            let mut txn = open_txn();
            unlink(&mut txn, &vsb, &omap, 2, "counted.lnk").expect("unlink");
            txn.commit().expect("commit");
        }
        let (files2, links2) = counts(&read_vsb_omap_raw().0);
        assert_eq!(
            (files2, links2),
            (files0, links0),
            "removing the symlink must take it off the symlink count and leave the file count alone"
        );
        run_fsck();
        println!("symlink PASS: the symlink count follows create and remove");
    }
}
