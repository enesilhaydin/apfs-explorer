//! Removing one name of a file that has several.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + ln).
//!
//! macOS binds each name of a hard-linked file to the shared inode with a
//! sibling-link and a sibling-map record, and marks the directory entry with
//! the sibling id it belongs to. Removing a name means removing those three
//! records and decrementing the link count - not removing the inode, which is
//! what an unlink that ignores them would do to every other name at once.
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
mod hard_links {
    use apfs_core::block_device::{FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{link, mkdir, move_entry, rename, unlink};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-hardlink.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// Content big enough that losing the inode would be unmistakable.
    const BODY: &[u8] = b"the same bytes under two names\n";

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    /// Build an image and let macOS create the links, so the sibling records
    /// are exactly the ones the kernel writes.
    fn build_image() {
        build_image_with_links(&["second.txt"]);
    }

    /// Build the image with `first.txt` plus one extra name per entry.
    fn build_image_with_links(extra: &[&str]) {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-hardlink.dmg");
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
                "HARDLINK",
                "/tmp/apfs-hardlink",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-hardlink.dmg", IMG).expect("rename");

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
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");

        std::fs::write(format!("{mount}/first.txt"), BODY).expect("write first");
        for name in extra {
            let st = Command::new("ln")
                .args([format!("{mount}/first.txt"), format!("{mount}/{name}")])
                .status()
                .expect("ln");
            assert!(st.success(), "ln failed for {name}");
        }
        let _ = Command::new("sync").status();
        assert!(
            Command::new("hdiutil")
                .args(["detach", &parent])
                .status()
                .expect("detach")
                .success(),
            "hdiutil detach failed"
        );
    }

    /// Attach and mount the image, run `f` with the mount point, then detach.
    /// Used to ask macOS itself what it makes of the records we wrote.
    fn with_mounted<T>(f: impl FnOnce(&str) -> T) -> T {
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
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        let result = f(&mount);
        let _ = Command::new("hdiutil").args(["detach", &parent]).status();
        result
    }

    /// `(inode number, link count)` as macOS reports them.
    fn macos_stat(mount: &str, name: &str) -> (String, String) {
        let out = Command::new("stat")
            .args(["-f", "%i %l", &format!("{mount}/{name}")])
            .output()
            .expect("stat");
        assert!(out.status.success(), "stat failed for {name}");
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let mut it = s.split_whitespace();
        (
            it.next().unwrap_or_default().to_string(),
            it.next().unwrap_or_default().to_string(),
        )
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
        use apfs_core::block_device::BlockDevice;
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

    /// Attach without mounting and let Apple's fsck judge the result. Warnings
    /// count: fsck exits 0 on damage it considers repairable.
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
            // hdiutil can list disk10 before disk9: the image's own node is the
            // lowest-numbered whole disk (shorter name, then smaller).
            if !dev.trim_start_matches("/dev/disk").contains('s')
                && parent_dev
                    .as_deref()
                    .is_none_or(|p: &str| (p.len(), p) > (dev.len(), dev))
            {
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
                l.contains("warning:")
                    || l.contains("overallocation")
                    || l.contains("underallocation")
                    || l.contains("is not valid")
            })
            .collect();
        assert!(
            complaints.is_empty(),
            "fsck_apfs flagged inconsistencies:\n{}",
            complaints.join("\n")
        );
    }

    #[test]
    #[ignore = "fsck-gated; needs macOS to author the links"]
    fn removing_one_name_leaves_the_other_intact() {
        if !gate() {
            println!("hard-link: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();

        // Precondition: macOS really made them two names for one file.
        {
            let mut fs = open_fs();
            let a = fs.lookup("/first.txt").expect("lookup").expect("first.txt");
            let b = fs
                .lookup("/second.txt")
                .expect("lookup")
                .expect("second.txt");
            assert_eq!(a, b, "hard-link: both names must resolve to one inode");
            let attr = fs
                .getattr("/first.txt")
                .expect("getattr")
                .expect("first.txt");
            assert_eq!(
                attr.nchildren_or_nlink, 2,
                "hard-link: the file must have two links"
            );
        }

        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            unlink(&mut txn, &vsb, &om, 2, "second.txt").expect("unlink one of two names");
            txn.commit().expect("commit unlink");
        }

        {
            let mut fs = open_fs();
            assert!(
                fs.lookup("/second.txt").expect("lookup").is_none(),
                "hard-link: the removed name must be gone"
            );
            let attr = fs
                .getattr("/first.txt")
                .expect("getattr")
                .expect("hard-link: the surviving name must still exist");
            assert_eq!(
                attr.nchildren_or_nlink, 1,
                "hard-link: the link count must drop with the name"
            );
            let got = fs.read("/first.txt", 0, BODY.len()).expect("read");
            assert_eq!(got, BODY, "hard-link: the shared content must be intact");
        }

        run_fsck();
        println!("hard-link PASS: one name removed, the other keeps the file");
    }

    /// The name the inode itself records is the one with the lowest sibling id,
    /// and it is the one a user is most likely to remove first. Nothing about
    /// removing it may differ from removing any other name.
    #[test]
    #[ignore = "fsck-gated; needs macOS to author the links"]
    fn removing_the_first_name_leaves_the_other_intact() {
        if !gate() {
            println!("hard-link: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image_with_links(&["second.txt"]);

        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            unlink(&mut txn, &vsb, &om, 2, "first.txt").expect("unlink the first name");
            txn.commit().expect("commit unlink");
        }

        {
            let mut fs = open_fs();
            assert!(
                fs.lookup("/first.txt").expect("lookup").is_none(),
                "hard-link: the removed name must be gone"
            );
            let attr = fs
                .getattr("/second.txt")
                .expect("getattr")
                .expect("hard-link: the surviving name must still exist");
            assert_eq!(
                attr.nchildren_or_nlink, 1,
                "hard-link: the link count must drop with the name"
            );
            let got = fs.read("/second.txt", 0, BODY.len()).expect("read");
            assert_eq!(got, BODY, "hard-link: the shared content must be intact");
        }

        run_fsck();
        println!("hard-link PASS: the first name can go like any other");
    }

    /// Three names, removed one at a time down to the last: the last removal is
    /// an ordinary unlink again, and has to take the file with it.
    #[test]
    #[ignore = "fsck-gated; needs macOS to author the links"]
    fn removing_every_name_in_turn_ends_with_the_file_gone() {
        if !gate() {
            println!("hard-link: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image_with_links(&["second.txt", "third.txt"]);

        let expected_links = [2u32, 1];
        for (i, name) in ["first.txt", "second.txt", "third.txt"].iter().enumerate() {
            {
                let (vsb, om) = read_vsb_omap();
                let mut txn = open_txn();
                unlink(&mut txn, &vsb, &om, 2, name)
                    .unwrap_or_else(|e| panic!("unlink {name}: {e}"));
                txn.commit().expect("commit unlink");
            }
            let mut fs = open_fs();
            assert!(
                fs.lookup(&format!("/{name}")).expect("lookup").is_none(),
                "hard-link: {name} must be gone"
            );
            if let Some(&links) = expected_links.get(i) {
                let survivor = if i == 0 { "/second.txt" } else { "/third.txt" };
                let attr = fs
                    .getattr(survivor)
                    .expect("getattr")
                    .unwrap_or_else(|| panic!("hard-link: {survivor} must still exist"));
                assert_eq!(
                    attr.nchildren_or_nlink, links,
                    "hard-link: link count after removing {name}"
                );
                let got = fs.read(survivor, 0, BODY.len()).expect("read");
                assert_eq!(got, BODY, "hard-link: content must survive");
                // Every sibling map left must pair with a living link of the
                // survivor: a map is keyed by its sibling id, not by the
                // inode, so it is the record a removal can overlook.
                let ino = fs.lookup(survivor).expect("lookup").expect("survivor");
                let links = fs.sibling_links(survivor).expect("sibling links");
                let maps = fs.sibling_maps().expect("sibling maps");
                for (sid, mapped) in &maps {
                    assert!(
                        *mapped == ino && links.iter().any(|l| l.sibling_id == *sid),
                        "hard-link: after removing {name}, sibling map {sid} -> {mapped} has no \
                         living link (links {links:?})"
                    );
                }
            }
            run_fsck();
        }

        let mut fs = open_fs();
        assert!(
            fs.lookup("/third.txt").expect("lookup").is_none(),
            "hard-link: the file must be gone once its last name is"
        );
        let maps = fs.sibling_maps().expect("sibling maps");
        assert!(
            maps.is_empty(),
            "hard-link: no sibling map may outlive the file it maps, found {maps:?}"
        );
        println!("hard-link PASS: names removed one at a time, file gone with the last");
    }

    /// The other half: giving a file a second name. macOS is the judge - it has
    /// to agree the two names are one file, and count the links the same way.
    #[test]
    #[ignore = "fsck-gated; mounts the image to ask macOS"]
    fn a_second_name_is_the_same_file_to_macos() {
        if !gate() {
            println!("hard-link: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image_with_links(&[]);

        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            link(&mut txn, &vsb, &om, 2, "first.txt", 2, "second.txt").expect("link");
            txn.commit().expect("commit link");
        }
        run_fsck();

        with_mounted(|mount| {
            let (ino_a, links_a) = macos_stat(mount, "first.txt");
            let (ino_b, links_b) = macos_stat(mount, "second.txt");
            assert_eq!(ino_a, ino_b, "macOS must see one file under both names");
            assert_eq!(links_a, "2", "macOS must count two links");
            assert_eq!(links_b, "2", "macOS must count two links");
            let body = std::fs::read(format!("{mount}/second.txt")).expect("read second");
            assert_eq!(body, BODY, "the new name must reach the same content");
        });

        // And the name we added can be taken away again.
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            unlink(&mut txn, &vsb, &om, 2, "second.txt").expect("unlink the added name");
            txn.commit().expect("commit unlink");
        }
        run_fsck();
        with_mounted(|mount| {
            let (_, links) = macos_stat(mount, "first.txt");
            assert_eq!(links, "1", "macOS must count one link again");
            let body = std::fs::read(format!("{mount}/first.txt")).expect("read first");
            assert_eq!(body, BODY, "content must survive the round trip");
        });
        println!("hard-link PASS: macOS reads our second name as the same file");
    }

    /// Renaming one name of a file that has several has to carry that name's
    /// binding with it: the entry keeps its sibling id, and the record holding
    /// the name behind that id has to say the new one.
    #[test]
    #[ignore = "fsck-gated; mounts the image to ask macOS"]
    fn renaming_one_name_keeps_both_links() {
        if !gate() {
            println!("hard-link: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image_with_links(&["second.txt"]);

        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            rename(&mut txn, &vsb, &om, 2, "second.txt", "renamed.txt", false)
                .expect("rename one of two names");
            txn.commit().expect("commit rename");
        }
        run_fsck();

        // The records behind the names, not just what a stat reports: a rename
        // that drops the entry's sibling id or leaves the old name in the
        // sibling-link record still looks right from the outside.
        {
            let mut fs = open_fs();
            let names: Vec<String> = fs
                .sibling_links("/first.txt")
                .expect("sibling links")
                .into_iter()
                .map(|l| l.name)
                .collect();
            assert!(
                names.iter().any(|n| n == "renamed.txt"),
                "the sibling link must carry the new name, got {names:?}"
            );
            assert!(
                !names.iter().any(|n| n == "second.txt"),
                "no sibling link may still name the old one, got {names:?}"
            );
            assert_eq!(names.len(), 2, "one sibling link per name, got {names:?}");
        }

        with_mounted(|mount| {
            let (ino_a, links_a) = macos_stat(mount, "first.txt");
            let (ino_b, links_b) = macos_stat(mount, "renamed.txt");
            assert_eq!(
                ino_a, ino_b,
                "macOS must still see one file under both names"
            );
            assert_eq!(links_a, "2", "macOS must still count two links");
            assert_eq!(links_b, "2", "macOS must still count two links");
            let body = std::fs::read(format!("{mount}/renamed.txt")).expect("read renamed");
            assert_eq!(body, BODY, "the renamed name must reach the same content");
        });

        // Both names must still be removable one at a time.
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            unlink(&mut txn, &vsb, &om, 2, "renamed.txt").expect("unlink the renamed name");
            txn.commit().expect("commit unlink");
        }
        run_fsck();
        {
            let mut fs = open_fs();
            let names: Vec<String> = fs
                .sibling_links("/first.txt")
                .expect("sibling links")
                .into_iter()
                .map(|l| l.name)
                .collect();
            assert!(
                !names.iter().any(|n| n == "renamed.txt"),
                "the removed name must leave no sibling link behind, got {names:?}"
            );
        }
        with_mounted(|mount| {
            let (_, links) = macos_stat(mount, "first.txt");
            assert_eq!(links, "1", "macOS must count one link after the removal");
        });
        println!("hard-link PASS: a renamed link is still a link");
    }

    /// Moving a link into another directory changes both halves of what its
    /// sibling-link record says: the name and the directory holding it.
    #[test]
    #[ignore = "fsck-gated; mounts the image to ask macOS"]
    fn moving_one_name_keeps_both_links() {
        if !gate() {
            println!("hard-link: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image_with_links(&["second.txt"]);

        let dst_ino = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            mkdir(&mut txn, &vsb, &om, 2, "sub").expect("mkdir");
            txn.commit().expect("commit mkdir");
            let mut fs = open_fs();
            fs.lookup("/sub").expect("lookup").expect("sub dir")
        };
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            move_entry(
                &mut txn,
                &vsb,
                &om,
                2,
                "second.txt",
                dst_ino,
                "moved.txt",
                false,
            )
            .expect("move one of two names");
            txn.commit().expect("commit move");
        }
        run_fsck();

        {
            let mut fs = open_fs();
            let links = fs.sibling_links("/first.txt").expect("sibling links");
            let moved = links
                .iter()
                .find(|l| l.name == "moved.txt")
                .unwrap_or_else(|| panic!("no sibling link for the moved name: {links:?}"));
            assert_eq!(
                moved.parent_id, dst_ino,
                "the sibling link must name the directory the link moved into"
            );
            assert!(
                !links.iter().any(|l| l.name == "second.txt"),
                "no sibling link may still name the old one: {links:?}"
            );
            assert_eq!(links.len(), 2, "one sibling link per name: {links:?}");
        }

        with_mounted(|mount| {
            let (ino_a, links_a) = macos_stat(mount, "first.txt");
            let (ino_b, links_b) = macos_stat(mount, "sub/moved.txt");
            assert_eq!(
                ino_a, ino_b,
                "macOS must still see one file under both names"
            );
            assert_eq!(links_a, "2", "macOS must still count two links");
            assert_eq!(links_b, "2", "macOS must still count two links");
        });
        println!("hard-link PASS: a moved link is still a link");
    }
}
