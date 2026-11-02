//! Integration tests: snapshot lifecycle on a real scratch APFS image.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS + hdiutil).
//!
//! CI invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release --test integration_snapshot -- --test-threads=1
//!
//! Covers: create_snapshot, delete_snapshot, multi-snapshot ordering,
//! snapshot survives file delete - the snapshot.rs paths not yet hit.
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
mod snapshot_tests {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{unlink, write_file};
    use apfs_write::snapshot::{
        create_snapshot, delete_snapshot, list_snapshots, revert_to_snapshot,
    };
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-isnap.img";
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
        // vsb.omap_oid is a physical paddr - read directly
        let vsb = VolumeSuperblock::parse(&vsb_raw).expect("parse vsb");
        let mut omap_raw = vec![0u8; bsz];
        dev.read_at(vsb.omap_oid * bsz as u64, &mut omap_raw)
            .expect("read vol omap");
        (vsb_raw, omap_raw)
    }

    /// Read om_snap_count from the volume omap header (offset 36, u32 LE).
    /// OMAP_SNAP_COUNT_OFF = 36 (confirmed: snapshot.rs line 82).
    fn read_snap_count(img: &str) -> u32 {
        let (_, omap_raw) = read_vsb_omap_raw(img);
        u32::from_le_bytes(omap_raw[36..40].try_into().unwrap())
    }

    /// `apfs_snap_meta_tree_oid` from the volume superblock (offset 0x98).
    /// A macOS-formatted volume that has never been snapshotted still has a
    /// real, empty tree here - checked on a freshly created image - so zero is
    /// not what "no snapshots" looks like.
    fn read_snap_meta_tree_oid(img: &str) -> u64 {
        let (vsb_raw, _) = read_vsb_omap_raw(img);
        u64::from_le_bytes(vsb_raw[0x98..0xA0].try_into().unwrap())
    }

    /// The newest snapshot's xid, from the volume omap header
    /// (`om_most_recent_snap`, offset 64).
    fn read_newest_snap_xid(img: &str) -> u64 {
        let (_, omap_raw) = read_vsb_omap_raw(img);
        u64::from_le_bytes(omap_raw[64..72].try_into().unwrap())
    }

    /// Read a file's full content through the reader, or None if absent.
    fn read_file(img: &str, path: &str) -> Option<Vec<u8>> {
        use apfs::FsView;
        let raw = std::fs::read(img).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut fs = FsView::open(dev).expect("FsView::open");
        let meta = fs.getattr(path).expect("getattr")?;
        Some(fs.read(path, 0, meta.size as usize).expect("read"))
    }

    /// Attach the raw image without mounting and run Apple's fsck_apfs against
    /// the container partition - the authoritative on-disk consistency oracle.
    /// The snapshot tests previously asserted only counters, which is why
    /// structural damage done by delete_snapshot went unnoticed.
    fn run_fsck(img: &str) {
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
                parent_dev = Some(dev.to_string());
            }
            if apfs_dev.is_none() && line.contains("Apple_APFS") {
                apfs_dev = Some(dev.to_string());
            }
        }
        let parent = parent_dev.expect("no whole-disk node in hdiutil attach output");
        let apfs = apfs_dev.expect("no Apple_APFS partition in hdiutil attach output");
        let rdev = apfs.replacen("/dev/disk", "/dev/rdisk", 1);

        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        let _ = Command::new("hdiutil").args(["detach", &parent]).status();
        let fsck = fsck.expect("fsck_apfs spawn");
        let report = String::from_utf8_lossy(&fsck.stdout).into_owned()
            + &String::from_utf8_lossy(&fsck.stderr);
        assert!(
            fsck.status.success(),
            "fsck_apfs reported corruption on {rdev}:\n{report}"
        );
        // fsck exits 0 for repairable inconsistencies and only prints them as
        // warnings: leaked blocks and drifted counters would otherwise pass
        // unnoticed. A correct writer leaves none, so treat them as failures.
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
            "fsck_apfs flagged inconsistencies on {rdev}:\n{}",
            complaints.join("\n")
        );
    }

    fn open_txn(img: &str) -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(img).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open_rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        Transaction::begin(dev).expect("Transaction::begin")
    }

    fn read_current_xid(img: &str) -> u64 {
        let raw_img = std::fs::read(img).expect("read img");
        let (base, len) = find_apfs_range(&raw_img);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw for xid");
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let container = Container::open(&mut dev).expect("Container::open");
        container.superblock.obj.xid
    }

    // -----------------------------------------------------------------------
    // S1: create_snapshot → snap count increments → delete_snapshot → count=0
    // -----------------------------------------------------------------------

    #[test]
    fn s1_create_snapshot_increments_count() {
        if !check_gate() {
            println!("S1: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let count_before = read_snap_count(IMG);
        assert_eq!(count_before, 0, "S1: fresh image must have 0 snapshots");

        let snap_xid = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, "s1-snap").expect("create_snapshot");
            txn.commit().expect("commit create");
            xid
        };
        println!("S1: snap_xid = {snap_xid:#x}");

        let count_after_create = read_snap_count(IMG);
        assert_eq!(
            count_after_create, 1,
            "S1: snap count must be 1 after create"
        );

        // Delete
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            delete_snapshot(&mut txn, &vsb, &omap, snap_xid).expect("delete_snapshot");
            txn.commit().expect("commit delete");
        }

        let count_after_delete = read_snap_count(IMG);
        assert_eq!(
            count_after_delete, 0,
            "S1: snap count must be 0 after delete"
        );

        println!("S1 PASS: create increments count, delete decrements count back to 0");
    }

    // -----------------------------------------------------------------------
    // S2: multi-snapshot create → in-order delete (oldest first)
    // -----------------------------------------------------------------------

    #[test]
    fn s2_multi_snapshot_inorder_delete() {
        if !check_gate() {
            println!("S2: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let mut snap_xids = Vec::new();
        for i in 0..3u32 {
            let name = format!("s2-snap-{i}");
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, &name).expect("create_snapshot");
            txn.commit().expect("commit create");
            snap_xids.push(xid);
            println!("S2: created snap[{i}] xid={xid:#x}");
        }

        assert_eq!(read_snap_count(IMG), 3, "S2: must have 3 snapshots");

        // Delete in order (oldest first)
        for (i, &xid) in snap_xids.iter().enumerate() {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            delete_snapshot(&mut txn, &vsb, &omap, xid).expect("delete_snapshot");
            txn.commit().expect("commit delete");
            let remaining = read_snap_count(IMG);
            let expected = (3 - i - 1) as u32;
            assert_eq!(
                remaining, expected,
                "S2: after delete[{i}] count={remaining} expected={expected}"
            );
            println!("S2: deleted snap[{i}] xid={xid:#x}, remaining={remaining}");
        }

        // Image must remain parseable
        let txn = open_txn(IMG);
        assert!(txn.xid >= 2);
        let _ = txn.abort();

        println!("S2 PASS: 3 snapshots created and deleted in order");
    }

    // -----------------------------------------------------------------------
    // S3: multi-snapshot create → reverse-order delete (newest first)
    // -----------------------------------------------------------------------

    #[test]
    fn s3_multi_snapshot_reverse_delete() {
        if !check_gate() {
            println!("S3: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let mut snap_xids = Vec::new();
        for i in 0..3u32 {
            let name = format!("s3-snap-{i}");
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, &name).expect("create_snapshot");
            txn.commit().expect("commit create");
            snap_xids.push(xid);
        }

        assert_eq!(read_snap_count(IMG), 3, "S3: must have 3 snapshots");

        // Delete in reverse order (newest first)
        for (i, &xid) in snap_xids.iter().rev().enumerate() {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            delete_snapshot(&mut txn, &vsb, &omap, xid).expect("delete_snapshot");
            txn.commit().expect("commit delete");
            let remaining = read_snap_count(IMG);
            let expected = (3 - i - 1) as u32;
            assert_eq!(
                remaining, expected,
                "S3: after reverse-delete[{i}] count={remaining}"
            );
        }

        // Image parseable
        let txn = open_txn(IMG);
        assert!(txn.xid >= 2);
        let _ = txn.abort();

        println!("S3 PASS: 3 snapshots deleted in reverse order, image still parseable");
    }

    // -----------------------------------------------------------------------
    // S4: snapshot survives file delete in live tree
    // -----------------------------------------------------------------------

    #[test]
    fn s4_snapshot_survives_live_file_delete() {
        if !check_gate() {
            println!("S4: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // Write a file, then snapshot, then delete the file in live tree
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "s4_file.txt", &[0xFFu8; 4096])
                .expect("write_file");
            txn.commit().expect("commit write");
        }

        let snap_xid = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, "s4-snap").expect("create_snapshot");
            txn.commit().expect("commit create");
            xid
        };
        println!("S4: snap_xid = {snap_xid:#x}");

        // Delete file in live tree (snapshot still holds it)
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            unlink(&mut txn, &vsb, &omap, 2, "s4_file.txt").expect("unlink");
            txn.commit().expect("commit unlink");
        }

        // Snapshot count must still be 1 (snapshot is not affected by live delete)
        let snap_count = read_snap_count(IMG);
        assert_eq!(
            snap_count, 1,
            "S4: snapshot must still exist after live file delete"
        );

        // Live file is gone
        {
            use apfs::FsView;
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = FsView::open(dev).expect("FsView::open");
            assert!(
                fs.getattr("/s4_file.txt").expect("getattr").is_none(),
                "S4: live file must be gone"
            );
        }

        // Now delete the snapshot - should succeed
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            delete_snapshot(&mut txn, &vsb, &omap, snap_xid).expect("delete_snapshot");
            txn.commit().expect("commit delete");
        }

        assert_eq!(
            read_snap_count(IMG),
            0,
            "S4: snap count must be 0 after delete"
        );

        // Image still parseable
        let xid_final = read_current_xid(IMG);
        println!("S4: final xid = {xid_final:#x}");
        assert!(xid_final >= 2);

        println!("S4 PASS: snapshot survives live file delete; delete_snapshot cleans up");
    }

    // -----------------------------------------------------------------------
    // S5: create_snapshot returns strictly increasing XIDs
    // -----------------------------------------------------------------------

    #[test]
    fn s5_snapshot_xids_increasing() {
        if !check_gate() {
            println!("S5: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let mut prev_xid = 0u64;
        for i in 0..3u32 {
            let name = format!("s5-snap-{i}");
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, &name).expect("create_snapshot");
            txn.commit().expect("commit");
            assert!(
                xid > prev_xid,
                "S5[{i}]: snap xid {xid:#x} must be > prev {prev_xid:#x}"
            );
            prev_xid = xid;
        }

        println!("S5 PASS: snapshot XIDs strictly increasing; last = {prev_xid:#x}");
    }

    // -----------------------------------------------------------------------
    // S6: the snapshot round trip must leave a structurally valid volume.
    // Creating a snapshot moves the live extent references into the snapshot's
    // own extentref tree; deleting it has to hand those references back. The
    // counter-only assertions of S1-S5 cannot see whether that happened, so
    // this one reads the data back AND runs fsck.
    // -----------------------------------------------------------------------
    #[test]
    fn s6_snapshot_round_trip_is_fsck_clean() {
        if !check_gate() {
            println!("S6: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let content = vec![0x5Au8; 80 * 1024];
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "s6_file.bin", &content).expect("write");
            txn.commit().expect("commit write");
        }

        let snap_xid = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, "s6-snap").expect("create_snapshot");
            txn.commit().expect("commit snapshot");
            xid
        };

        // A volume carrying a snapshot must already be consistent.
        run_fsck(IMG);

        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            delete_snapshot(&mut txn, &vsb, &omap, snap_xid).expect("delete_snapshot");
            txn.commit().expect("commit delete");
        }

        assert_eq!(read_snap_count(IMG), 0, "S6: snapshot count must be 0");
        let data = read_file(IMG, "/s6_file.bin").expect("S6: file must survive the round trip");
        assert_eq!(
            data, content,
            "S6: live file content changed across the snapshot round trip"
        );

        assert_ne!(
            read_snap_meta_tree_oid(IMG),
            0,
            "S6: the volume must keep an empty snapshot-metadata tree, not none"
        );
        run_fsck(IMG);
        println!("S6 PASS: snapshot create + delete leaves the volume fsck-clean");
    }

    // -----------------------------------------------------------------------
    // S7: writing under a snapshot pushes the copied-away blocks onto the
    // spaceman free queue, which is the only path that exercises the queue's
    // on-disk record encoding. The volume must stay fsck-clean while those
    // records are pending.
    // -----------------------------------------------------------------------
    // apfs_fs_alloc_count must stay right while entries are pending: a queued
    // block is still reachable through the snapshot that pinned it, so it is
    // still volume-owned until delete_snapshot drains the queue.
    #[test]
    fn s7_free_queue_records_are_fsck_clean() {
        if !check_gate() {
            println!("S7: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let first = vec![0x71u8; 40 * 1024];
        let second = vec![0x72u8; 40 * 1024];
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "s7_file.bin", &first).expect("write");
            txn.commit().expect("commit write");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            create_snapshot(&mut txn, &vsb, &omap, "s7-snap").expect("snapshot");
            txn.commit().expect("commit snapshot");
        }
        // Each mutation under the snapshot copies metadata away and queues the
        // old blocks for release once the snapshot goes.
        for i in 0..3u32 {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let name = format!("s7_extra_{i}.bin");
            write_file(&mut txn, &vsb, &omap, 2, &name, &second).expect("write extra");
            txn.commit().expect("commit extra");
        }

        run_fsck(IMG);

        let data = read_file(IMG, "/s7_file.bin").expect("S7: original file must survive");
        assert_eq!(data, first, "S7: content changed under the snapshot");
        println!("S7 PASS: pending free-queue records leave the volume fsck-clean");
    }

    // -----------------------------------------------------------------------
    // S8: revert had no fsck coverage at all - every revert test so far only
    // re-read the image. Undoing a post-snapshot write must leave a volume
    // fsck accepts, not merely one that parses.
    // -----------------------------------------------------------------------
    #[test]
    fn s8_revert_is_fsck_clean() {
        if !check_gate() {
            println!("S8: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let original = vec![0x81u8; 24 * 1024];
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "s8_file.bin", &original).expect("write");
            txn.commit().expect("commit write");
        }
        let snap_xid = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, "s8-snap").expect("snapshot");
            txn.commit().expect("commit snapshot");
            xid
        };
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "s8_after.bin", &[0x82u8; 4096]).expect("write");
            txn.commit().expect("commit post-snapshot write");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            revert_to_snapshot(&mut txn, &vsb, &omap, snap_xid).expect("revert");
            txn.commit().expect("commit revert");
        }

        assert!(
            read_file(IMG, "/s8_after.bin").is_none(),
            "S8: the file written after the snapshot must be gone"
        );
        let data = read_file(IMG, "/s8_file.bin").expect("S8: pre-snapshot file must survive");
        assert_eq!(data, original, "S8: pre-snapshot content changed");

        assert_ne!(
            read_snap_meta_tree_oid(IMG),
            0,
            "S8: the volume must keep an empty snapshot-metadata tree, not none"
        );
        run_fsck(IMG);
        println!("S8 PASS: revert leaves the volume fsck-clean");
    }

    // -----------------------------------------------------------------------
    // S9: with writes between two snapshots, the older one's extent references
    // belong to the newer snapshot's view, not to the live volume. Deleting the
    // older one is refused outright rather than handing them over, which blocks
    // every multi-snapshot workflow that actually writes anything.
    // -----------------------------------------------------------------------
    #[test]
    fn s9_delete_older_snapshot_hands_extents_to_the_newer_one() {
        if !check_gate() {
            println!("S9: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let early = vec![0x91u8; 32 * 1024];
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "s9_early.bin", &early).expect("write early");
            txn.commit().expect("commit early");
        }
        let older = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, "s9-older").expect("older snapshot");
            txn.commit().expect("commit older");
            xid
        };
        // A write between the two snapshots is what puts records in the older
        // snapshot's extentref tree.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(
                &mut txn,
                &vsb,
                &omap,
                2,
                "s9_middle.bin",
                &[0x92u8; 32 * 1024],
            )
            .expect("write middle");
            txn.commit().expect("commit middle");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            create_snapshot(&mut txn, &vsb, &omap, "s9-newer").expect("newer snapshot");
            txn.commit().expect("commit newer");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            delete_snapshot(&mut txn, &vsb, &omap, older).expect("delete older snapshot");
            txn.commit().expect("commit delete");
        }

        assert_eq!(read_snap_count(IMG), 1, "S9: one snapshot must remain");
        let data = read_file(IMG, "/s9_early.bin").expect("S9: pre-snapshot file must survive");
        assert_eq!(data, early, "S9: content changed across the delete");
        run_fsck(IMG);

        // Deleting the survivor afterwards hands the same records on again,
        // this time to the live volume - the state the first delete produced
        // has to be one the second can consume.
        let newer = read_newest_snap_xid(IMG);
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            delete_snapshot(&mut txn, &vsb, &omap, newer).expect("delete newer snapshot");
            txn.commit().expect("commit second delete");
        }
        assert_eq!(read_snap_count(IMG), 0, "S9: no snapshot may remain");
        let data = read_file(IMG, "/s9_early.bin").expect("S9: file must survive both deletes");
        assert_eq!(data, early, "S9: content changed across the second delete");
        assert!(
            read_file(IMG, "/s9_middle.bin").is_some(),
            "S9: the file written between the snapshots must survive"
        );
        run_fsck(IMG);
        println!("S9 PASS: the older snapshot's extents survive its deletion");
    }

    // -----------------------------------------------------------------------
    // S10: list_snapshots reports every snapshot by xid and name, oldest
    // first, and forgets a deleted one.
    // -----------------------------------------------------------------------
    #[test]
    fn s10_list_snapshots_reports_names_and_xids() {
        if !check_gate() {
            println!("S10: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let list = |img: &str| -> Vec<(u64, String)> {
            let (vsb, omap) = read_vsb_omap_raw(img);
            let mut txn = open_txn(img);
            let out = list_snapshots(&mut txn, &vsb, &omap).expect("list_snapshots");
            let _ = txn.abort();
            out.into_iter().map(|s| (s.xid, s.name)).collect()
        };
        assert!(
            list(IMG).is_empty(),
            "S10: a fresh volume lists no snapshot"
        );

        let mut made = Vec::new();
        for name in ["alpha", "beta"] {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, name).expect("create_snapshot");
            txn.commit().expect("commit create");
            made.push((xid, name.to_string()));
        }
        assert_eq!(list(IMG), made, "S10: both snapshots, oldest first");

        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            delete_snapshot(&mut txn, &vsb, &omap, made[0].0).expect("delete_snapshot");
            txn.commit().expect("commit delete");
        }
        assert_eq!(list(IMG), made[1..], "S10: only the newer snapshot is left");
        println!("S10 PASS: list_snapshots follows create and delete");
    }
}
