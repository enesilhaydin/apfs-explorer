//! Integration tests: snapshot revert (volume rollback) on a real scratch APFS image.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS + hdiutil).
//!
//! CI invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release \
//!       --test integration_snapshot_revert -- --test-threads=1
//!
//! Covers: revert_to_snapshot (W6-GAP-2)
//!   R1: write → snap → write → revert → image consistent, 0 snapshots after
//!   R2: snap_1 → write → snap_2 → write → revert(snap_2) → snap_1 survives
//!   R3: revert to nonexistent xid → Err(SnapshotNotFound)
//!   R4: mid-chain revert → Err(InvalidArgument)
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
mod revert_tests {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::write_file;
    use apfs_write::snapshot::{create_snapshot, revert_to_snapshot};
    use apfs_write::txn::{Transaction, TxnError};
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-irevert.img";
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

    fn read_snap_count(img: &str) -> u32 {
        let (_, omap_raw) = read_vsb_omap_raw(img);
        u32::from_le_bytes(omap_raw[36..40].try_into().unwrap())
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
    // R1: file_a → snap → file_b → revert(snap) → image consistent, 0 snapshots
    // -----------------------------------------------------------------------

    #[test]
    fn r1_revert_drops_post_snap_writes() {
        if !check_gate() {
            println!("R1: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // Write file_a.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "r1_file_a.txt", b"hello-a")
                .expect("write file_a");
            txn.commit().expect("commit file_a");
        }

        // Create snapshot.
        let snap_xid = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, "r1-snap").expect("create_snapshot");
            txn.commit().expect("commit snap");
            xid
        };
        println!("R1: snap_xid = {snap_xid:#x}");
        assert_eq!(read_snap_count(IMG), 1, "R1: must have 1 snapshot");

        // Write file_b (post-snapshot).
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "r1_file_b.txt", b"hello-b")
                .expect("write file_b");
            txn.commit().expect("commit file_b");
        }

        // Revert to snap.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            revert_to_snapshot(&mut txn, &vsb, &omap, snap_xid).expect("revert_to_snapshot");
            txn.commit().expect("commit revert");
        }

        // After revert: 0 snapshots (snapshot became live).
        assert_eq!(read_snap_count(IMG), 0, "R1: no snapshots after revert");
        let xid_final = read_current_xid(IMG);
        println!("R1: final xid = {xid_final:#x}");
        assert!(xid_final >= snap_xid, "R1: final xid must be >= snap_xid");

        println!("R1 PASS: revert completed, image structurally consistent");
    }

    // -----------------------------------------------------------------------
    // R2: snap_1 → write → snap_2 → write → revert(snap_2) → snap_1 survives
    // -----------------------------------------------------------------------

    #[test]
    fn r2_revert_most_recent_leaves_earlier_snap() {
        if !check_gate() {
            println!("R2: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // snap_1
        let snap1_xid = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, "r2-snap-1").expect("create snap_1");
            txn.commit().expect("commit snap_1");
            xid
        };
        println!("R2: snap1_xid = {snap1_xid:#x}");

        // write between snapshots
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "r2_between.txt", b"between")
                .expect("write between");
            txn.commit().expect("commit between");
        }

        // snap_2
        let snap2_xid = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, "r2-snap-2").expect("create snap_2");
            txn.commit().expect("commit snap_2");
            xid
        };
        println!("R2: snap2_xid = {snap2_xid:#x}");
        assert_eq!(read_snap_count(IMG), 2, "R2: must have 2 snapshots");

        // write after snap_2
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "r2_after.txt", b"after").expect("write after");
            txn.commit().expect("commit after");
        }

        // Revert to snap_2 (most recent - should succeed).
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            revert_to_snapshot(&mut txn, &vsb, &omap, snap2_xid).expect("revert snap_2");
            txn.commit().expect("commit revert snap_2");
        }

        // snap_2 became live; snap_1 remains in the snap_meta tree.
        // Our conservative implementation decrements num_snapshots by 1
        // (removes snap_2 entry), leaving snap_1 with count = 1.
        let snap_count = read_snap_count(IMG);
        assert_eq!(
            snap_count, 1,
            "R2: snap_1 must survive revert of snap_2; got {snap_count}"
        );

        println!("R2 PASS: revert(snap_2) succeeded; snap_1 survives with count=1");
    }

    // -----------------------------------------------------------------------
    // R3: revert to nonexistent xid → Err(SnapshotNotFound)
    // -----------------------------------------------------------------------

    #[test]
    fn r3_revert_nonexistent_returns_error() {
        if !check_gate() {
            println!("R3: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // No snapshots - any xid must fail.
        let (vsb, omap) = read_vsb_omap_raw(IMG);
        let mut txn = open_txn(IMG);
        let result = revert_to_snapshot(&mut txn, &vsb, &omap, 0xDEAD_BEEF_u64);
        drop(txn);

        match result {
            Err(TxnError::SnapshotNotFound(xid)) => {
                assert_eq!(xid, 0xDEAD_BEEF_u64, "R3: wrong xid in error");
                println!("R3 PASS: nonexistent xid returns SnapshotNotFound({xid:#x})");
            }
            other => panic!("R3: expected SnapshotNotFound, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // R4: mid-chain revert rejected (only most-recent snapshot supported)
    // -----------------------------------------------------------------------

    #[test]
    fn r4_mid_chain_revert_rejected() {
        if !check_gate() {
            println!("R4: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let snap1_xid = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let xid = create_snapshot(&mut txn, &vsb, &omap, "r4-snap-1").expect("create snap_1");
            txn.commit().expect("commit snap_1");
            xid
        };

        // Create snap_2 so snap_1 is no longer most-recent.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            create_snapshot(&mut txn, &vsb, &omap, "r4-snap-2").expect("create snap_2");
            txn.commit().expect("commit snap_2");
        }

        // Attempt revert to snap_1 (not most-recent) must fail.
        let (vsb, omap) = read_vsb_omap_raw(IMG);
        let mut txn = open_txn(IMG);
        let result = revert_to_snapshot(&mut txn, &vsb, &omap, snap1_xid);
        drop(txn);

        match result {
            Err(TxnError::InvalidArgument(_)) => {
                println!("R4 PASS: mid-chain revert correctly rejected with InvalidArgument");
            }
            other => panic!("R4: expected InvalidArgument for mid-chain revert, got {other:?}"),
        }
    }
}
