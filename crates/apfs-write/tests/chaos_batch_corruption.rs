//! Regression test: batch-commit omap double-free corruption (M8 #3 bug).
//!
//! Gate: set `APFS_WRITE_SCRATCH=1` to run (requires macOS + hdiutil).
//!
//! Root cause: when N ops sharing the same virtual OID (e.g., N renames
//! each staging a new VSB at a different paddr) are committed in one batch
//! transaction, `apply_omap_entries` in `commit()` processes all N entries
//! for the same OID sequentially. The merge loop frees the intermediate
//! padrs (B1..B_{N-1}) into the in-transaction reclaim pool. `alloc_run_cached`
//! then re-uses those freed addresses for the new container-omap B-tree node
//! and omap header. Step 2 (pending flush) subsequently overwrites those
//! freshly-allocated structures with stale staged data, corrupting the
//! container omap. On re-open, `Omap::open` sees a non-OMAP object type
//! and returns `BadMagic { expected: 11, found: 3 }`.
//!
//! This scenario is triggered by:
//!   - batch of ≥ 2 renames committed in a single transaction (same parent), OR
//!   - any workload that stages multiple versions of the same virtual OID
//!     within one `Transaction`.
//!
//! Test pattern (S9):
//!   1. Create 32 files in a fresh scratch image.
//!   2. Rename all 32 in a **single** transaction (replicates the batch flush).
//!   3. Re-open the device (`FsView::open` path via `Transaction::begin`).
//!   4. Assert no `BadMagic` / parse error.
//!   5. A follow-up transaction must also succeed.

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
mod chaos_batch {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_write::file::{create_file, rename};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const CHAOS_SIZE_MB: u32 = 64;
    const SECTOR: u64 = 512;

    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC,
        0xAC,
    ];

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn gen_scratch_at(img: &str) {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let status = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                img,
                &CHAOS_SIZE_MB.to_string(),
            ])
            .current_dir(workspace_root)
            .status()
            .expect("cargo run xtask gen-scratch");
        assert!(status.success(), "xtask gen-scratch failed for {img}");
    }

    fn find_apfs_range_in_bytes(raw: &[u8]) -> (u64, u64) {
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
            let entry = &raw[off..off + part_entry_size];
            if entry[..16] == APFS_GPT_TYPE {
                let start_lba = u64::from_le_bytes(entry[32..40].try_into().unwrap());
                let end_lba = u64::from_le_bytes(entry[40..48].try_into().unwrap());
                return (start_lba * SECTOR, (end_lba - start_lba + 1) * SECTOR);
            }
        }
        panic!("no APFS partition found in GPT");
    }

    fn read_image_header(img: &str) -> (u64, u64) {
        let mut dev = apfs_core::block_device::FileBlockDevice::open(img).expect("open ro");
        let size = dev.size() as usize;
        let read_len = size.min(2 * 1024 * 1024);
        let mut buf = vec![0u8; read_len];
        dev.read_at(0, &mut buf).expect("read header region");
        find_apfs_range_in_bytes(&buf)
    }

    fn read_vsb_omap_raw(img: &str) -> (Vec<u8>, Vec<u8>) {
        let (base, len) = read_image_header(img);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw for vsb");
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);

        let container = Container::open(&mut dev).expect("Container::open");
        let nx = &container.superblock;
        let bsz = nx.block_size as usize;
        let xid = nx.obj.xid;

        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).expect("omap open");
        let fs_oid = *nx.fs_oids.first().expect("no fs_oids");
        let vol_paddr = omap
            .resolve(&mut dev, fs_oid, xid, nx.block_size)
            .expect("omap resolve")
            .expect("vol_paddr not found");

        let mut vsb_raw = vec![0u8; bsz];
        dev.read_at(vol_paddr * bsz as u64, &mut vsb_raw)
            .expect("read vsb");

        let vsb = apfs_core::volume::VolumeSuperblock::parse(&vsb_raw).expect("parse vsb");
        let vol_omap_oid = vsb.omap_oid;

        let mut omap_raw = vec![0u8; bsz];
        dev.read_at(vol_omap_oid * bsz as u64, &mut omap_raw)
            .expect("read vol omap");

        (vsb_raw, omap_raw)
    }

    fn assert_device_parseable(img: &str, label: &str) {
        let (base, len) = read_image_header(img);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw for verify");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let txn = Transaction::begin(dev)
            .unwrap_or_else(|e| panic!("{label} FAIL: Transaction::begin: {e}"));
        println!("  [{label}] post-commit xid = {:#x}", txn.xid);
        let _ = txn.abort();
    }

    // -----------------------------------------------------------------------
    // S9: batch-rename omap corruption regression
    //
    // Proves: committing N renames in a single Transaction does NOT corrupt
    // the container omap (the M8 #3 regression).
    // -----------------------------------------------------------------------

    /// Commit `batch_size` renames in one Transaction, then verify the device
    /// is still parseable and a follow-up write succeeds.
    fn run_batch_rename_regression(img: &str, batch_size: usize) {
        let parent_ino: u64 = 2; // ROOT_DIR_INO_NUM

        // Step 1: create `batch_size` files in individual transactions.
        for i in 0..batch_size {
            let (vsb, omap) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw create");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("txn begin (create)");
            let name = format!("rn_{i}.txt");
            create_file(&mut txn, &vsb, &omap, parent_ino, &name)
                .unwrap_or_else(|e| panic!("create_file {name}: {e}"));
            txn.commit()
                .unwrap_or_else(|e| panic!("commit create {name}: {e}"));
        }

        println!("  created {batch_size} files");

        // Step 2: rename ALL files in a SINGLE transaction.
        // This is the regression trigger: each rename calls stage_virtual(vsb_oid, …)
        // which pushes a new OmapEntry for the same container-level OID.
        // apply_omap_entries in commit() must not free intermediate padrs that
        // are still staged in self.pending.
        {
            let (vsb, omap) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw batch-rename");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("txn begin (batch rename)");

            for i in 0..batch_size {
                let old_name = format!("rn_{i}.txt");
                let new_name = format!("rnd_{i}.txt");
                // Re-read vsb/omap from the txn's staged state each iteration
                // (same vsb_raw/omap_raw is fine - the txn accumulates pending
                // writes and the catalog view is rebuilt from scratch per rename).
                rename(
                    &mut txn, &vsb, &omap, parent_ino, &old_name, &new_name, false,
                )
                .unwrap_or_else(|e| panic!("rename {old_name} -> {new_name}: {e}"));
            }

            txn.commit()
                .unwrap_or_else(|e| panic!("batch rename commit FAILED (M8 #3 regression): {e}"));
        }

        println!("  batch-renamed {batch_size} files in one transaction");
    }

    #[test]
    fn s9_batch_rename_no_omap_corruption_small() {
        if !check_gate() {
            println!("S9-small: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        println!("S9-small: batch 4 renames in one txn - omap must not corrupt");

        let img = "/tmp/apfs-chaos-s9-small.img";
        gen_scratch_at(img);

        run_batch_rename_regression(img, 4);
        assert_device_parseable(img, "S9-small-post-batch");

        // Follow-up write must succeed (no double-alloc or structural corruption).
        {
            let (vsb, omap) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw follow-up");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("follow-up txn begin");
            create_file(&mut txn, &vsb, &omap, 2, "s9_follow_small.txt")
                .expect("follow-up create_file");
            txn.commit().expect("follow-up commit");
        }
        assert_device_parseable(img, "S9-small-final");
        println!("S9-small PASS");
    }

    #[test]
    fn s9_batch_rename_no_omap_corruption_medium() {
        if !check_gate() {
            println!("S9-medium: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        println!("S9-medium: batch 16 renames in one txn - omap must not corrupt");

        let img = "/tmp/apfs-chaos-s9-medium.img";
        gen_scratch_at(img);

        run_batch_rename_regression(img, 16);
        assert_device_parseable(img, "S9-medium-post-batch");

        {
            let (vsb, omap) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw follow-up");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("follow-up txn begin");
            create_file(&mut txn, &vsb, &omap, 2, "s9_follow_medium.txt")
                .expect("follow-up create_file");
            txn.commit().expect("follow-up commit");
        }
        assert_device_parseable(img, "S9-medium-final");
        println!("S9-medium PASS");
    }

    #[test]
    fn s9_batch_rename_no_omap_corruption_large() {
        if !check_gate() {
            println!("S9-large: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        println!("S9-large: batch 32 renames in one txn - omap must not corrupt");

        let img = "/tmp/apfs-chaos-s9-large.img";
        gen_scratch_at(img);

        run_batch_rename_regression(img, 32);
        assert_device_parseable(img, "S9-large-post-batch");

        {
            let (vsb, omap) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw follow-up");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("follow-up txn begin");
            create_file(&mut txn, &vsb, &omap, 2, "s9_follow_large.txt")
                .expect("follow-up create_file");
            txn.commit().expect("follow-up commit");
        }
        assert_device_parseable(img, "S9-large-final");
        println!("S9-large PASS");
    }
}
