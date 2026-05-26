//! Integration tests: snapshot lifecycle on a real scratch APFS image.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS + hdiutil).
//!
//! CI invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release --test integration_snapshot -- --test-threads=1
//!
//! Covers: create_snapshot, delete_snapshot, multi-snapshot ordering,
//! snapshot survives file delete — the snapshot.rs paths not yet hit.
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
    use apfs_write::snapshot::{create_snapshot, delete_snapshot};
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
            .args(["run", "--quiet", "-p", "xtask", "--", "gen-scratch", IMG, &SIZE_MB.to_string()])
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
        dev.read_at(vol_paddr * bsz as u64, &mut vsb_raw).expect("read vsb");
        // vsb.omap_oid is a physical paddr — read directly
        let vsb = VolumeSuperblock::parse(&vsb_raw).expect("parse vsb");
        let mut omap_raw = vec![0u8; bsz];
        dev.read_at(vsb.omap_oid * bsz as u64, &mut omap_raw).expect("read vol omap");
        (vsb_raw, omap_raw)
    }

    /// Read om_snap_count from the volume omap header (offset 36, u32 LE).
    /// OMAP_SNAP_COUNT_OFF = 36 (confirmed: snapshot.rs line 82).
    fn read_snap_count(img: &str) -> u32 {
        let (_, omap_raw) = read_vsb_omap_raw(img);
        u32::from_le_bytes(omap_raw[36..40].try_into().unwrap())
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
            println!("S1: APFS_WRITE_SCRATCH not set — skip");
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
        assert_eq!(count_after_create, 1, "S1: snap count must be 1 after create");

        // Delete
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            delete_snapshot(&mut txn, &vsb, &omap, snap_xid).expect("delete_snapshot");
            txn.commit().expect("commit delete");
        }

        let count_after_delete = read_snap_count(IMG);
        assert_eq!(count_after_delete, 0, "S1: snap count must be 0 after delete");

        println!("S1 PASS: create increments count, delete decrements count back to 0");
    }

    // -----------------------------------------------------------------------
    // S2: multi-snapshot create → in-order delete (oldest first)
    // -----------------------------------------------------------------------

    #[test]
    fn s2_multi_snapshot_inorder_delete() {
        if !check_gate() {
            println!("S2: APFS_WRITE_SCRATCH not set — skip");
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
            assert_eq!(remaining, expected, "S2: after delete[{i}] count={remaining} expected={expected}");
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
            println!("S3: APFS_WRITE_SCRATCH not set — skip");
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
            assert_eq!(remaining, expected, "S3: after reverse-delete[{i}] count={remaining}");
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
            println!("S4: APFS_WRITE_SCRATCH not set — skip");
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
        assert_eq!(snap_count, 1, "S4: snapshot must still exist after live file delete");

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

        // Now delete the snapshot — should succeed
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            delete_snapshot(&mut txn, &vsb, &omap, snap_xid).expect("delete_snapshot");
            txn.commit().expect("commit delete");
        }

        assert_eq!(read_snap_count(IMG), 0, "S4: snap count must be 0 after delete");

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
            println!("S5: APFS_WRITE_SCRATCH not set — skip");
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
            assert!(xid > prev_xid, "S5[{i}]: snap xid {xid:#x} must be > prev {prev_xid:#x}");
            prev_xid = xid;
        }

        println!("S5 PASS: snapshot XIDs strictly increasing; last = {prev_xid:#x}");
    }
}
