//! Writing to one side of a clone macOS made.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + cp -c).
//!
//! macOS clones a file by giving the copy the original's data-stream id: both
//! inodes name the same stream. Replacing the content of either side must give
//! that side a stream of its own and leave the shared one to the other, with
//! its blocks untouched. Replacing the content in place - or freeing the old
//! blocks because the extent count said they were only used once - changes or
//! destroys the other file.
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
mod macos_clone_overwrite {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{overwrite_existing_file, truncate_file_fast, TruncateOutcome};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-macos-clone-overwrite.img";
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

    fn replacement() -> Vec<u8> {
        (0..SIZE / 2).map(|i| ((i * 11) % 241) as u8).collect()
    }

    /// Build an image, let macOS write the original and clone it.
    fn build_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-macos-clone-overwrite.dmg");
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
                "CLONEOW",
                "/tmp/apfs-macos-clone-overwrite",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-macos-clone-overwrite.dmg", IMG).expect("rename");

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

    fn read_both() -> (Vec<u8>, Vec<u8>) {
        let mut fs = open_fs();
        let a = fs
            .getattr(&format!("/{ORIGINAL}"))
            .expect("getattr")
            .expect("original");
        let b = fs
            .getattr(&format!("/{CLONE}"))
            .expect("getattr")
            .expect("clone");
        let oa = fs
            .read(&format!("/{ORIGINAL}"), 0, a.size as usize)
            .expect("read original");
        let cb = fs
            .read(&format!("/{CLONE}"), 0, b.size as usize)
            .expect("read clone");
        (oa, cb)
    }

    /// Replace `target`'s content; `other` must keep the shared content, both
    /// through our reader and through macOS, with the volume fsck-clean.
    fn overwrite_one_and_check_the_other(target: &str, other: &str) {
        build_image();
        let body = body();
        let new = replacement();
        {
            let (a, b) = read_both();
            assert_eq!(a, body, "the original must read whole before the write");
            assert_eq!(b, body, "the clone must read whole before the write");
        }

        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            overwrite_existing_file(&mut txn, &vsb, &om, 2, target, &new).expect("overwrite");
            txn.commit().expect("commit overwrite");
        }
        run_fsck(&format!("after overwriting {target}"));

        {
            let mut fs = open_fs();
            let got_target = fs
                .read(&format!("/{target}"), 0, new.len())
                .expect("read target");
            assert_eq!(got_target, new, "{target} must hold the new content");
            let got_other = fs.read(&format!("/{other}"), 0, SIZE).expect("read other");
            assert_eq!(
                got_other, body,
                "{other} must still hold the content it shared with {target}"
            );
        }

        let (mount, parent) = attach(true);
        let seen_target = std::fs::read(format!("{mount}/{target}"));
        let seen_other = std::fs::read(format!("{mount}/{other}"));
        detach(&parent);
        assert_eq!(
            seen_target.expect("macOS read target"),
            new,
            "macOS must read the new content of {target}"
        );
        assert_eq!(
            seen_other.expect("macOS read other"),
            body,
            "macOS must read {other} intact"
        );
    }

    #[test]
    #[ignore = "fsck-gated; needs macOS to make the clone"]
    fn overwriting_the_clone_leaves_the_original_intact() {
        if !gate() {
            println!("macos-clone-overwrite: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        overwrite_one_and_check_the_other(CLONE, ORIGINAL);
        println!("macos-clone-overwrite PASS: the original survived a write to the clone");
    }

    #[test]
    #[ignore = "fsck-gated; needs macOS to make the clone"]
    fn overwriting_the_original_leaves_the_clone_intact() {
        if !gate() {
            println!("macos-clone-overwrite: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        overwrite_one_and_check_the_other(ORIGINAL, CLONE);
        println!("macos-clone-overwrite PASS: the clone survived a write to the original");
    }

    /// Windows replaces a file by truncating it to nothing first and writing
    /// afterwards, in two operations. Emptying one side of a clone while the
    /// other still shares the stream, then writing the emptied side anew,
    /// must leave the other side whole, with the volume fsck-clean at both
    /// steps.
    #[test]
    #[ignore = "fsck-gated; needs macOS to make the clone"]
    fn emptying_one_side_then_writing_it_keeps_the_other() {
        if !gate() {
            println!("macos-clone-overwrite: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();
        let body = body();
        let new = replacement();
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            overwrite_existing_file(&mut txn, &vsb, &om, 2, CLONE, &[]).expect("empty the clone");
            txn.commit().expect("commit empty");
        }
        run_fsck("after emptying the clone");
        {
            let mut fs = open_fs();
            let attr = fs
                .getattr(&format!("/{CLONE}"))
                .expect("getattr")
                .expect("clone");
            assert_eq!(attr.size, 0, "the emptied side reports no content");
            let other = fs
                .read(&format!("/{ORIGINAL}"), 0, SIZE)
                .expect("read original");
            assert_eq!(
                other, body,
                "the original must still hold the shared content"
            );
        }
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            overwrite_existing_file(&mut txn, &vsb, &om, 2, CLONE, &new).expect("write anew");
            txn.commit().expect("commit write");
        }
        run_fsck("after writing the emptied side anew");
        let (a, b) = read_both();
        assert_eq!(a, body, "the original must still hold the shared content");
        assert_eq!(b, new, "the clone must hold the new content");
        let (mount, parent) = attach(true);
        let seen_a = std::fs::read(format!("{mount}/{ORIGINAL}"));
        let seen_b = std::fs::read(format!("{mount}/{CLONE}"));
        detach(&parent);
        assert_eq!(
            seen_a.expect("macOS read original"),
            body,
            "macOS must read the original intact"
        );
        assert_eq!(
            seen_b.expect("macOS read clone"),
            new,
            "macOS must read the new content"
        );
        println!("macos-clone-overwrite PASS: emptying then rewriting one side kept the other");
    }

    /// The sequence the Windows host produced: one side of a clone is emptied
    /// (the truncation Windows does before a rewrite), then another file is
    /// renamed over the other side, then the emptied side is written anew.
    /// Every step must leave the volume fsck-clean and the survivors whole.
    #[test]
    #[ignore = "fsck-gated; needs macOS to make the clone"]
    fn emptying_one_side_then_renaming_over_the_other_then_writing() {
        if !gate() {
            println!("macos-clone-overwrite: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();
        let body = body();
        let new = replacement();
        let other: Vec<u8> = (0..8000u32).map(|i| (i % 253) as u8).collect();
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            apfs_write::file::write_file(&mut txn, &vsb, &om, 2, "other.txt", &other)
                .expect("write other");
            txn.commit().expect("commit other");
        }
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            overwrite_existing_file(&mut txn, &vsb, &om, 2, CLONE, &[]).expect("empty the clone");
            txn.commit().expect("commit empty");
        }
        run_fsck("after emptying the clone");
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            apfs_write::file::rename(&mut txn, &vsb, &om, 2, "other.txt", ORIGINAL, true)
                .expect("rename over");
            txn.commit().expect("commit rename");
        }
        run_fsck("after renaming over the original");
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            overwrite_existing_file(&mut txn, &vsb, &om, 2, CLONE, &new).expect("write anew");
            txn.commit().expect("commit write");
        }
        run_fsck("after writing the emptied side anew");
        let (a, b) = read_both();
        assert_eq!(a, other, "the original's name now holds the renamed file");
        assert_eq!(b, new, "the clone holds the new content");
        let _ = body;
        println!("macos-clone-overwrite PASS: empty, rename over the other side, write anew");
    }

    /// The same two-step rewrite on the SOURCE side of the clone (the file
    /// `cp -c` copied from), followed by the removal of the clone: the source
    /// must keep the content it was given, with the volume fsck-clean.
    #[test]
    #[ignore = "fsck-gated; needs macOS to make the clone"]
    fn emptying_the_source_then_writing_it_then_removing_the_clone() {
        if !gate() {
            println!("macos-clone-overwrite: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();
        let new = replacement();
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            overwrite_existing_file(&mut txn, &vsb, &om, 2, ORIGINAL, &[])
                .expect("empty the source");
            txn.commit().expect("commit empty");
        }
        run_fsck("after emptying the source");
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            overwrite_existing_file(&mut txn, &vsb, &om, 2, ORIGINAL, &new).expect("write anew");
            txn.commit().expect("commit write");
        }
        run_fsck("after writing the source anew");
        {
            let mut fs = open_fs();
            let got = fs
                .read(&format!("/{ORIGINAL}"), 0, new.len())
                .expect("read source");
            assert_eq!(got, new, "the source must hold the new content");
        }
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            apfs_write::file::unlink(&mut txn, &vsb, &om, 2, CLONE).expect("remove the clone");
            txn.commit().expect("commit unlink");
        }
        run_fsck("after removing the clone");
        let mut fs = open_fs();
        let got = fs
            .read(&format!("/{ORIGINAL}"), 0, new.len())
            .expect("read source");
        assert_eq!(got, new, "the source must still hold the new content");
        println!("macos-clone-overwrite PASS: empty and rewrite the source, then remove the clone");
    }

    /// The metadata-only resize edits the stream's extents in place, which on
    /// a shared stream would resize the other file too. It has to hand the
    /// operation on to the read-and-rewrite path instead.
    #[test]
    #[ignore = "fsck-gated; needs macOS to make the clone"]
    fn the_fast_resize_declines_a_shared_stream() {
        if !gate() {
            println!("macos-clone-overwrite: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();
        let outcome = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let o = truncate_file_fast(&mut txn, &vsb, &om, 2, CLONE, 4096, SIZE as u64);
            let _ = txn.abort();
            o
        };
        match outcome {
            Ok(TruncateOutcome::FastPathDeclined) => {}
            Ok(TruncateOutcome::Applied) => {
                panic!("the fast path cut a stream that another file still reads")
            }
            Err(e) => panic!("the fast path must decline a shared stream, not fail: {e}"),
        }
        println!("macos-clone-overwrite PASS: the fast resize declined the shared stream");
    }
}
