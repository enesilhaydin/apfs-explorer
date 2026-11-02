//! Removing the name a hard-linked file records for itself.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + ln).
//!
//! A file with several names records one of them in its inode - the primary
//! link, the one with the lowest sibling id - and every other name is a
//! sibling record. When the primary name is removed, the inode has to be
//! re-pointed at the next lowest sibling, or it goes on describing an entry
//! that no longer exists. macOS does exactly that; the first half of the test
//! measures it, the second half requires the same of us.
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
mod hard_link_primary {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::unlink;
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-hardlink-primary.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const BODY: &[u8] = b"the same bytes under three names\n";
    const ROOT_INO: u64 = 2;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    /// Build an image where macOS creates `first.txt` in a subdirectory and
    /// links it as `second.txt` at the root and `third.txt` in the
    /// subdirectory. When `macos_removes_first`, macOS also removes the
    /// primary name before handing the image over.
    fn build_image(macos_removes_first: bool) {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-hardlink-primary.dmg");
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
                "HLPRIM",
                "/tmp/apfs-hardlink-primary",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-hardlink-primary.dmg", IMG).expect("rename");

        let (mount, parent) = attach(true);
        std::fs::create_dir(format!("{mount}/sub")).expect("mkdir sub");
        std::fs::write(format!("{mount}/sub/first.txt"), BODY).expect("write first");
        for target in ["second.txt", "sub/third.txt"] {
            let st = Command::new("ln")
                .args([
                    &format!("{mount}/sub/first.txt"),
                    &format!("{mount}/{target}"),
                ])
                .status()
                .expect("ln");
            assert!(st.success(), "ln {target} failed");
        }
        if macos_removes_first {
            std::fs::remove_file(format!("{mount}/sub/first.txt")).expect("macOS rm");
        }
        let _ = Command::new("sync").status();
        detach(&parent);
    }

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
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
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

    /// After `sub/first.txt` is gone, the inode must describe the lowest
    /// surviving sibling: `second.txt` at the root, created before
    /// `sub/third.txt`.
    fn assert_primary_repointed(label: &str) {
        let mut fs = open_fs();
        assert!(
            fs.lookup("/sub/first.txt").expect("lookup").is_none(),
            "{label}: the removed name must be gone"
        );
        let links = fs.sibling_links("/second.txt").expect("sibling links");
        assert_eq!(
            links.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            vec!["second.txt", "third.txt"],
            "{label}: two sibling records must remain, in creation order"
        );
        let lowest = &links[0];
        let primary = fs
            .primary_link("/second.txt")
            .expect("primary link")
            .expect("the inode must record a name");
        assert_eq!(
            (primary.parent_id, primary.name.as_str()),
            (lowest.parent_id, lowest.name.as_str()),
            "{label}: the inode must record the lowest surviving sibling as its own \
             name and parent (that sibling sits in directory {} as {:?}); a stale record \
             describes an entry that no longer exists",
            lowest.parent_id,
            lowest.name
        );
        assert_eq!(
            primary.parent_id, ROOT_INO,
            "{label}: second.txt lives at the root"
        );
        let attr = fs
            .getattr("/second.txt")
            .expect("getattr")
            .expect("second.txt");
        assert_eq!(attr.nchildren_or_nlink, 2, "{label}: two names remain");
        assert_eq!(
            fs.read("/sub/third.txt", 0, BODY.len()).expect("read"),
            BODY,
            "{label}: the content must be intact"
        );
    }

    #[test]
    #[ignore = "fsck-gated; needs macOS to author the links"]
    fn macos_repoints_the_inode_when_the_primary_name_goes() {
        if !gate() {
            println!("hard-link-primary: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image(true);
        run_fsck("as macOS left it");
        assert_primary_repointed("macOS removed the primary name");
        println!("hard-link-primary PASS: macOS re-points the inode at the next sibling");
    }

    #[test]
    #[ignore = "fsck-gated; needs macOS to author the links"]
    fn removing_the_primary_name_repoints_the_inode() {
        if !gate() {
            println!("hard-link-primary: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image(false);
        let sub_ino = {
            let mut fs = open_fs();
            fs.lookup("/sub").expect("lookup sub").expect("sub exists")
        };
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            unlink(&mut txn, &vsb, &om, sub_ino, "first.txt").expect("unlink the primary name");
            txn.commit().expect("commit unlink");
        }
        run_fsck("after we removed the primary name");
        assert_primary_repointed("we removed the primary name");

        // macOS must agree on every name it can still see.
        let (mount, parent) = attach(true);
        let seen_second = std::fs::read(format!("{mount}/second.txt"));
        let seen_third = std::fs::read(format!("{mount}/sub/third.txt"));
        let first_exists = std::path::Path::new(&format!("{mount}/sub/first.txt")).exists();
        detach(&parent);
        assert!(!first_exists, "macOS must not see the removed name");
        assert_eq!(seen_second.expect("macOS read second"), BODY);
        assert_eq!(seen_third.expect("macOS read third"), BODY);
        println!("hard-link-primary PASS: the inode follows the next sibling as macOS does");
    }
}
