//! Integration tests: Transaction lifecycle on a real scratch APFS image.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS + hdiutil).
//!
//! CI invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release --test integration_txn_lifecycle -- --test-threads=1
//!
//! Covers: Transaction::begin, commit, abort (drop), XID monotonicity,
//! state persistence and rollback — the txn.rs paths not covered by unit tests.
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
mod txn_lifecycle_tests {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, write_file};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-itxn-lifecycle.img";
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
    // L1: begin → multiple ops → commit → reopen → state persisted
    // -----------------------------------------------------------------------

    #[test]
    fn l1_commit_persists_state() {
        if !check_gate() {
            println!("L1: APFS_WRITE_SCRATCH not set — skip");
            return;
        }
        gen_scratch();

        let xid_before = read_current_xid(IMG);
        println!("L1: xid_before = {xid_before:#x}");

        // Commit file a then file b in sequential transactions
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let txn_xid = txn.xid;
            assert_eq!(txn_xid, xid_before + 1, "L1: txn XID must be current+1");
            write_file(&mut txn, &vsb, &omap, 2, "l1_a.txt", &[0xAA; 512]).expect("write a");
            txn.commit().expect("commit a");
        }
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "l1_b.txt", &[0xBB; 512]).expect("write b");
            txn.commit().expect("commit b");
        }

        let xid_after = read_current_xid(IMG);
        println!("L1: xid_after = {xid_after:#x}");
        assert!(xid_after > xid_before, "L1: XID must advance after commit");

        // Reopen and confirm state
        {
            use apfs::FsView;
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = FsView::open(dev).expect("FsView::open");
            assert!(
                fs.getattr("/l1_a.txt").expect("getattr a").is_some(),
                "L1: l1_a.txt must persist"
            );
            assert!(
                fs.getattr("/l1_b.txt").expect("getattr b").is_some(),
                "L1: l1_b.txt must persist"
            );
        }

        println!("L1 PASS: commit persists state; XID advanced {xid_before:#x} → {xid_after:#x}");
    }

    // -----------------------------------------------------------------------
    // L2: begin → ops → abort (drop without commit) → state unchanged
    // -----------------------------------------------------------------------

    #[test]
    fn l2_abort_leaves_state_unchanged() {
        if !check_gate() {
            println!("L2: APFS_WRITE_SCRATCH not set — skip");
            return;
        }
        gen_scratch();

        // Commit one file first so we have a known baseline
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "l2_baseline.txt", &[0x11; 256])
                .expect("write baseline");
            txn.commit().expect("commit baseline");
        }

        let xid_baseline = read_current_xid(IMG);
        println!("L2: xid_baseline = {xid_baseline:#x}");

        // Start a new txn, stage some ops, then drop (abort) without committing
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            // Stage something into the txn
            create_file(&mut txn, &vsb, &omap, 2, "l2_aborted.txt").expect("stage create");
            // Drop txn without commit — this is the abort
            let _ = txn.abort();
        }

        let xid_after_abort = read_current_xid(IMG);
        println!("L2: xid_after_abort = {xid_after_abort:#x}");
        assert_eq!(
            xid_baseline, xid_after_abort,
            "L2: XID must not advance after abort"
        );

        // Confirm aborted file does not exist
        {
            use apfs::FsView;
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = FsView::open(dev).expect("FsView::open");
            assert!(
                fs.getattr("/l2_aborted.txt").expect("getattr aborted").is_none(),
                "L2: aborted file must not appear"
            );
            assert!(
                fs.getattr("/l2_baseline.txt").expect("getattr baseline").is_some(),
                "L2: baseline file must still exist"
            );
        }

        println!("L2 PASS: abort leaves state unchanged, XID = {xid_baseline:#x}");
    }

    // -----------------------------------------------------------------------
    // L3: multiple sequential commits → XID increments monotonically
    // -----------------------------------------------------------------------

    #[test]
    fn l3_sequential_commits_xid_monotonic() {
        if !check_gate() {
            println!("L3: APFS_WRITE_SCRATCH not set — skip");
            return;
        }
        gen_scratch();

        let mut prev_xid = read_current_xid(IMG);
        println!("L3: initial xid = {prev_xid:#x}");

        for i in 0..5u32 {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let txn_xid = txn.xid;
            assert_eq!(txn_xid, prev_xid + 1, "L3[{i}]: txn XID not prev+1");
            write_file(&mut txn, &vsb, &omap, 2, &format!("l3_{i}.txt"), &[i as u8; 256])
                .expect("write");
            txn.commit().expect("commit");
            let new_xid = read_current_xid(IMG);
            assert!(
                new_xid > prev_xid,
                "L3[{i}]: XID did not advance ({prev_xid:#x} → {new_xid:#x})"
            );
            prev_xid = new_xid;
        }

        println!("L3 PASS: 5 sequential commits, XID incremented each time, final = {prev_xid:#x}");
    }

    // -----------------------------------------------------------------------
    // L4: Transaction::begin on fresh image succeeds and has valid XID
    // -----------------------------------------------------------------------

    #[test]
    fn l4_begin_on_fresh_image_valid_xid() {
        if !check_gate() {
            println!("L4: APFS_WRITE_SCRATCH not set — skip");
            return;
        }
        gen_scratch();

        let txn = open_txn(IMG);
        // APFS checkpoint XIDs start at 1 (next_xid in superblock is 2 initially
        // for a freshly formatted volume). Our txn XID = current_xid + 1 >= 2.
        assert!(txn.xid >= 2, "L4: fresh image txn XID must be >= 2, got {}", txn.xid);
        // Drop without commit — no state change
        let _ = txn.abort();

        println!("L4 PASS: Transaction::begin on fresh image succeeds, xid >= 2");
    }

    // -----------------------------------------------------------------------
    // L5: alloc_block / free_block exercised in txn context
    // -----------------------------------------------------------------------

    #[test]
    fn l5_alloc_free_block_in_txn() {
        if !check_gate() {
            println!("L5: APFS_WRITE_SCRATCH not set — skip");
            return;
        }
        gen_scratch();

        // Alloc a block and immediately free it in the same txn — should not
        // corrupt the bitmap. Commit must succeed.
        {
            let mut txn = open_txn(IMG);
            let paddr = txn.alloc_block().expect("alloc_block");
            assert!(paddr > 0, "L5: paddr must be non-zero");
            txn.free_block(paddr).expect("free_block");
            txn.commit().expect("commit after alloc+free");
        }

        // Reopen must succeed — image must still be parseable
        let txn2 = open_txn(IMG);
        assert!(txn2.xid >= 2, "L5: re-open XID must be valid");
        let _ = txn2.abort();

        println!("L5 PASS: alloc_block + free_block in same txn, image remains parseable");
    }
}
