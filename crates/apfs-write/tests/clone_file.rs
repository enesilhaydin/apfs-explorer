//! Integration tests: clone_file COW reflink semantics on a real scratch APFS image.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS + hdiutil).
//! Without that env var every test is a no-op pass.
//!
//! CI invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release --test clone_file -- --test-threads=1
//!
//! Proves:
//!   C1 - clone_file roundtrip: dst content == src content after clone
//!   C2 - COW write to clone: src unchanged after writing to dst
//!   C3 - COW write to src: clone unchanged after writing to src
//!   C4 - delete src: clone still readable (refcnt protects blocks)
//!   C5 - delete clone after src gone: src unaffected + image clean
//!   C6 - reject directory clone
//!   C7 - reject duplicate dst name
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
mod clone_tests {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        clone_file, overwrite_existing_file, truncate_file_fast, unlink, write_file,
        TruncateOutcome,
    };
    use apfs_write::snapshot::create_snapshot;
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-clone-file.img";
    const SIZE_MB: u32 = 64;

    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const SECTOR: u64 = 512;

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

    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let mbr = &raw[..512];
        assert_eq!(&mbr[510..512], &[0x55u8, 0xAA], "no MBR signature");
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
                let start_lba = u64::from_le_bytes(entry[32..40].try_into().unwrap());
                let end_lba = u64::from_le_bytes(entry[40..48].try_into().unwrap());
                return (start_lba * SECTOR, (end_lba - start_lba + 1) * SECTOR);
            }
        }
        panic!("no APFS partition in GPT");
    }

    fn read_vsb_omap_raw(img: &str) -> (Vec<u8>, Vec<u8>) {
        let raw_img = std::fs::read(img).expect("read img");
        let (base, len) = find_apfs_range(&raw_img);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw");
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let container = Container::open(&mut dev).expect("Container::open");
        let nx = &container.superblock;
        let bsz = nx.block_size as usize;
        let xid = nx.obj.xid;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).expect("container omap");
        let fs_oid = *nx.fs_oids.first().expect("no fs_oids");
        let vol_paddr = omap
            .resolve(&mut dev, fs_oid, xid, nx.block_size)
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

    fn open_txn(img: &str) -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(img).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open_rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        Transaction::begin(dev).expect("Transaction::begin")
    }

    fn read_file(img: &str, path: &str) -> Option<Vec<u8>> {
        use apfs::FsView;
        let raw = std::fs::read(img).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut fs = FsView::open(dev).expect("FsView::open");
        let attr = fs.getattr(path).expect("getattr");
        let meta = attr?;
        Some(fs.read(path, 0, meta.size as usize).expect("read"))
    }

    fn file_exists(img: &str, path: &str) -> bool {
        use apfs::FsView;
        let raw = std::fs::read(img).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut fs = FsView::open(dev).expect("FsView::open");
        fs.getattr(path).expect("getattr").is_some()
    }

    fn run_fsck(img: &str) {
        // Attach the raw image WITHOUT mounting and run Apple's fsck_apfs against
        // the APFS container partition - the authoritative on-disk consistency
        // oracle. (hdiutil verify only validates a UDIF checksum, which a raw .img
        // does not have, so it is not a corruption check at all.)
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-nomount",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                img,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);

        let mut apfs_dev: Option<String> = None;
        let mut parent_dev: Option<String> = None;
        for line in stdout.lines() {
            let dev = line.split_whitespace().next().unwrap_or("");
            if !dev.starts_with("/dev/disk") {
                continue;
            }
            let tail = dev.trim_start_matches("/dev/disk");
            // hdiutil can list disk10 before disk9: the image's own node is the
            // lowest-numbered whole disk (shorter name, then smaller).
            if !tail.contains('s')
                && parent_dev
                    .as_deref()
                    .is_none_or(|p: &str| (p.len(), p) > (dev.len(), dev))
            {
                parent_dev = Some(dev.to_string()); // whole-disk node, for detach
            }
            if apfs_dev.is_none() && line.contains("Apple_APFS") {
                apfs_dev = Some(dev.to_string()); // the APFS container partition
            }
        }
        let parent = parent_dev.expect("no whole-disk node in hdiutil attach output");
        let apfs = apfs_dev.expect("no Apple_APFS partition in hdiutil attach output");
        let rdev = apfs.replacen("/dev/disk", "/dev/rdisk", 1);

        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).status();
        // Always detach, even if fsck failed, so we never leak an attachment.
        let _ = Command::new("hdiutil").args(["detach", &parent]).status();
        let fsck = fsck.expect("fsck_apfs spawn");
        assert!(fsck.success(), "fsck_apfs reported corruption on {rdev}");
    }

    // -----------------------------------------------------------------------
    // C1: clone roundtrip - dst content == src content
    // -----------------------------------------------------------------------
    #[test]
    fn c1_clone_readback_equals_source() {
        if !check_gate() {
            println!("C1: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let content = vec![0xABu8; 100 * 1024]; // 100 KiB

        // Write source.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "c1_src.bin", &content).expect("write src");
            txn.commit().expect("commit write");
        }

        // Clone to dst.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            clone_file(&mut txn, &vsb, &omap, 2, "c1_src.bin", 2, "c1_dst.bin")
                .expect("clone_file");
            txn.commit().expect("commit clone");
        }

        // Read dst - must equal src.
        let dst_data = read_file(IMG, "/c1_dst.bin").expect("c1_dst.bin not found");
        assert_eq!(dst_data.len(), content.len(), "C1: size mismatch");
        assert_eq!(dst_data, content, "C1: content mismatch");

        run_fsck(IMG);
        println!("C1 PASS: clone readback == source");
    }

    // -----------------------------------------------------------------------
    // C2: COW write to clone - src unchanged
    // -----------------------------------------------------------------------
    #[test]
    fn c2_write_clone_src_unchanged() {
        if !check_gate() {
            println!("C2: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let src_content = vec![0x11u8; 100 * 1024];
        let new_content = vec![0x22u8; 100 * 1024];

        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "c2_src.bin", &src_content).expect("write");
            txn.commit().expect("commit");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            clone_file(&mut txn, &vsb, &omap, 2, "c2_src.bin", 2, "c2_dst.bin").expect("clone");
            txn.commit().expect("commit clone");
        }
        // Overwrite clone with different content.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            overwrite_existing_file(&mut txn, &vsb, &omap, 2, "c2_dst.bin", &new_content)
                .expect("overwrite clone");
            txn.commit().expect("commit overwrite");
        }

        // src must still have original content.
        let src_data = read_file(IMG, "/c2_src.bin").expect("c2_src.bin");
        assert_eq!(
            src_data, src_content,
            "C2: src changed after writing to clone"
        );

        // dst must have new content.
        let dst_data = read_file(IMG, "/c2_dst.bin").expect("c2_dst.bin");
        assert_eq!(dst_data, new_content, "C2: dst has wrong content");

        run_fsck(IMG);
        println!("C2 PASS: write to clone does not affect source");
    }

    // -----------------------------------------------------------------------
    // C3: COW write to src - clone unchanged
    // -----------------------------------------------------------------------
    #[test]
    fn c3_write_src_clone_unchanged() {
        if !check_gate() {
            println!("C3: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let src_content = vec![0x33u8; 100 * 1024];
        let new_src = vec![0x44u8; 100 * 1024];

        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "c3_src.bin", &src_content).expect("write");
            txn.commit().expect("commit");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            clone_file(&mut txn, &vsb, &omap, 2, "c3_src.bin", 2, "c3_dst.bin").expect("clone");
            txn.commit().expect("commit clone");
        }
        // Overwrite source.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            overwrite_existing_file(&mut txn, &vsb, &omap, 2, "c3_src.bin", &new_src)
                .expect("overwrite src");
            txn.commit().expect("commit overwrite");
        }

        // clone must still have the original content.
        let clone_data = read_file(IMG, "/c3_dst.bin").expect("c3_dst.bin");
        assert_eq!(
            clone_data, src_content,
            "C3: clone changed after writing to src"
        );

        run_fsck(IMG);
        println!("C3 PASS: write to source does not affect clone");
    }

    // -----------------------------------------------------------------------
    // C4: delete src - clone still readable (refcnt protects shared blocks)
    // -----------------------------------------------------------------------
    #[test]
    fn c4_delete_src_clone_survives() {
        if !check_gate() {
            println!("C4: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let content = vec![0x55u8; 100 * 1024];

        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "c4_src.bin", &content).expect("write");
            txn.commit().expect("commit");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            clone_file(&mut txn, &vsb, &omap, 2, "c4_src.bin", 2, "c4_dst.bin").expect("clone");
            txn.commit().expect("commit clone");
        }
        // Delete source.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            unlink(&mut txn, &vsb, &omap, 2, "c4_src.bin").expect("unlink src");
            txn.commit().expect("commit unlink");
        }

        assert!(!file_exists(IMG, "/c4_src.bin"), "C4: src should be gone");
        let clone_data = read_file(IMG, "/c4_dst.bin").expect("C4: clone should still exist");
        assert_eq!(
            clone_data, content,
            "C4: clone content wrong after src deleted"
        );

        run_fsck(IMG);
        println!("C4 PASS: clone survives after source deleted");
    }

    // -----------------------------------------------------------------------
    // C5: delete clone after src - src unaffected, image clean
    // -----------------------------------------------------------------------
    #[test]
    fn c5_delete_clone_src_survives() {
        if !check_gate() {
            println!("C5: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let content = vec![0x66u8; 100 * 1024];

        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "c5_src.bin", &content).expect("write");
            txn.commit().expect("commit");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            clone_file(&mut txn, &vsb, &omap, 2, "c5_src.bin", 2, "c5_dst.bin").expect("clone");
            txn.commit().expect("commit clone");
        }
        // Delete clone (refcnt decrement; src keeps blocks).
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            unlink(&mut txn, &vsb, &omap, 2, "c5_dst.bin").expect("unlink clone");
            txn.commit().expect("commit unlink clone");
        }

        assert!(!file_exists(IMG, "/c5_dst.bin"), "C5: clone should be gone");
        let src_data = read_file(IMG, "/c5_src.bin").expect("C5: src should still exist");
        assert_eq!(
            src_data, content,
            "C5: src content wrong after clone deleted"
        );

        run_fsck(IMG);
        println!("C5 PASS: src survives after clone deleted");
    }

    // -----------------------------------------------------------------------
    // C6: reject directory clone
    // -----------------------------------------------------------------------
    #[test]
    fn c6_reject_directory_clone() {
        if !check_gate() {
            println!("C6: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // Create a directory.
        {
            use apfs_write::file::mkdir;
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            mkdir(&mut txn, &vsb, &omap, 2, "c6_dir").expect("mkdir");
            txn.commit().expect("commit mkdir");
        }

        let (vsb, omap) = read_vsb_omap_raw(IMG);
        let mut txn = open_txn(IMG);
        let result = clone_file(&mut txn, &vsb, &omap, 2, "c6_dir", 2, "c6_dir_clone");
        assert!(result.is_err(), "C6: cloning a directory should fail");

        println!("C6 PASS: directory clone rejected");
    }

    // -----------------------------------------------------------------------
    // C7: reject duplicate dst name
    // -----------------------------------------------------------------------
    #[test]
    fn c7_reject_duplicate_dst() {
        if !check_gate() {
            println!("C7: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let content = vec![0x77u8; 4096];
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "c7_src.txt", &content).expect("write src");
            write_file(&mut txn, &vsb, &omap, 2, "c7_existing.txt", &content)
                .expect("write existing");
            txn.commit().expect("commit");
        }

        let (vsb, omap) = read_vsb_omap_raw(IMG);
        let mut txn = open_txn(IMG);
        let result = clone_file(&mut txn, &vsb, &omap, 2, "c7_src.txt", 2, "c7_existing.txt");
        assert!(result.is_err(), "C7: clone to existing name should fail");

        println!("C7 PASS: duplicate dst name rejected");
    }

    // -----------------------------------------------------------------------
    // C8: clone THEN snapshot - the image must stay fsck-clean. Real-disk repro
    // of the omap key-order corruption found on a USB.
    // -----------------------------------------------------------------------
    #[test]
    fn c8_clone_then_snapshot_fsck_clean() {
        if !check_gate() {
            println!("C8: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        let content = vec![0x88u8; 80 * 1024];
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "c8_src.bin", &content).expect("write");
            txn.commit().expect("commit write");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            clone_file(&mut txn, &vsb, &omap, 2, "c8_src.bin", 2, "c8_dst.bin").expect("clone");
            txn.commit().expect("commit clone");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            create_snapshot(&mut txn, &vsb, &omap, "c8-snap").expect("snapshot");
            txn.commit().expect("commit snapshot");
        }
        run_fsck(IMG);
        println!("C8 PASS: clone + snapshot stays fsck-clean");
    }

    // -----------------------------------------------------------------------
    // C9: snapshot WITHOUT a preceding clone - control that isolates whether the
    // corruption is snapshot-alone or the clone+snapshot interaction.
    // -----------------------------------------------------------------------
    #[test]
    fn c9_snapshot_only_fsck_clean() {
        if !check_gate() {
            println!("C9: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        let content = vec![0x99u8; 80 * 1024];
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "c9_src.bin", &content).expect("write");
            txn.commit().expect("commit write");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            create_snapshot(&mut txn, &vsb, &omap, "c9-snap").expect("snapshot");
            txn.commit().expect("commit snapshot");
        }
        run_fsck(IMG);
        println!("C9 PASS: snapshot-only stays fsck-clean");
    }

    // -----------------------------------------------------------------------
    // C10: truncate src to 0 - clone still readable (refcnt protects shared
    // blocks). Regression: truncate_file_fast freed clone-shared blocks without
    // consulting the extref refcnt that clone_file bumps, unlike unlink/overwrite.
    // -----------------------------------------------------------------------
    #[test]
    fn c10_truncate_src_clone_survives() {
        if !check_gate() {
            println!("C10: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let content = vec![0xC1u8; 100 * 1024]; // 100 KiB, multi-extent

        // Write source.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "c10_src.bin", &content).expect("write");
            txn.commit().expect("commit");
        }
        // Clone to dst (shares src's physical extents via refcnt).
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            clone_file(&mut txn, &vsb, &omap, 2, "c10_src.bin", 2, "c10_dst.bin").expect("clone");
            txn.commit().expect("commit clone");
        }
        // Truncate the source to 0 - drops every extent. The blocks are still
        // referenced by the clone, so they must be refcnt-decremented, not freed.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let outcome = truncate_file_fast(
                &mut txn,
                &vsb,
                &omap,
                2,
                "c10_src.bin",
                0,
                content.len() as u64,
            )
            .expect("truncate_file_fast");
            assert!(
                matches!(outcome, TruncateOutcome::Applied),
                "C10: truncate-to-0 should take the fast path (whole-extent drops), got {outcome:?}"
            );
            txn.commit().expect("commit truncate");
        }

        // The clone must still read its full original content.
        let clone_data = read_file(IMG, "/c10_dst.bin").expect("C10: clone should still exist");
        assert_eq!(
            clone_data, content,
            "C10: clone content corrupted after truncating the source (clone-shared blocks freed)"
        );

        run_fsck(IMG);
        println!("C10 PASS: clone survives truncate-to-zero of source");
    }

    // -----------------------------------------------------------------------
    // C11: snapshot THEN clone THEN delete source. With a snapshot pinning the
    // extents, clone_file records the shared reference as a KIND_UPDATE(+1)
    // extentref instead of bumping a KIND_NEW refcount. A free path that reads
    // the refcnt field of an UPDATE record as if it were an absolute count
    // sees 1, removes the record and frees blocks that both the snapshot and
    // the surviving clone still reference.
    // -----------------------------------------------------------------------
    #[test]
    fn c11_snapshot_clone_delete_source_keeps_data() {
        if !check_gate() {
            println!("C11: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let content = vec![0xD1u8; 100 * 1024];

        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "c11_src.bin", &content).expect("write");
            txn.commit().expect("commit write");
        }
        // Snapshot pins the current extents; the live extentref tree no longer
        // carries a KIND_NEW record for them.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            create_snapshot(&mut txn, &vsb, &omap, "c11-snap").expect("snapshot");
            txn.commit().expect("commit snapshot");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            clone_file(&mut txn, &vsb, &omap, 2, "c11_src.bin", 2, "c11_dst.bin").expect("clone");
            txn.commit().expect("commit clone");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            unlink(&mut txn, &vsb, &omap, 2, "c11_src.bin").expect("unlink src");
            txn.commit().expect("commit unlink");
        }

        assert!(!file_exists(IMG, "/c11_src.bin"), "C11: src should be gone");
        let clone_data = read_file(IMG, "/c11_dst.bin").expect("C11: clone should still exist");
        assert_eq!(
            clone_data, content,
            "C11: clone content corrupted after deleting the source of a snapshot-pinned clone"
        );

        run_fsck(IMG);
        println!("C11 PASS: snapshot + clone + delete source keeps data and stays fsck-clean");
    }
}
