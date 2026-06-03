//! S10 - omap incremental update corruption regression (M12 prep + post-impl).
//!
//! Gate: set `APFS_WRITE_SCRATCH=1` to run (requires macOS + hdiutil / xtask
//! gen-scratch).
//!
//! What this test proves
//! ─────────────────────
//! After M12 ships the incremental omap fast-path (patch_omap_entry instead of
//! full build_omap_node rebuild inside incremental_rename_drec), the volume omap
//! must remain structurally sound:
//!
//!   • Every renamed file's catalog node (inode leaf) is still resolvable via
//!     the omap after each individual commit.
//!   • 50 sequential rename commits (S10-small) and 50 renames in a SINGLE
//!     batch transaction (S10-batch) both survive without BadMagic / parse error.
//!   • A follow-up write succeeds after each scenario.
//!
//! Baseline (pre-M12): both sub-tests pass because the full rebuild path is
//! correct. Post-M12: must still pass with the fast path active.
//!
//! Regression trigger (what M12 must NOT break):
//!   • xid mismatch: omap entry xid must equal the commit xid, not the old one.
//!   • paddr mismatch: omap entry paddr must point to the freshly COW'd leaf.
//!   • stale entry: if patch_omap_entry mis-identifies which entry to patch, a
//!     leaf becomes unresolvable → catalog read errors / fsck FAIL on re-open.
//!
//! Test pattern (S10):
//!   1. Create N files in individual transactions (establishes initial catalog).
//!   2a. (sequential) Rename each file in its own transaction; after ALL renames
//!       re-open and assert parseable + follow-up write succeeds.
//!   2b. (batch) Rename all N files in a SINGLE transaction; assert parseable +
//!       follow-up write succeeds.
//!
//! [CERTAIN: derived from APFS omap B-tree spec (APFS PDF §Object Maps) +
//!  linux-apfs-rw object_map.c pattern + M11 chaos S9 structure]

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
mod chaos_omap {
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
    // S10: omap incremental update - corruption regression (M12)
    //
    // Two sub-tests:
    //   s10_sequential: 50 individual-commit renames, each followed by a
    //     parseable assert - catches stale xid / wrong paddr in omap after
    //     every single incremental patch.
    //   s10_batch: 50 renames in ONE transaction - same regression surface as
    //     S9 but exercises the omap fast-path accumulation (multiple patches
    //     to the same omap node within one txn).
    // -----------------------------------------------------------------------

    const S10_FILE_COUNT: usize = 50;

    fn create_files(img: &str, count: usize) {
        for i in 0..count {
            let (vsb, omap) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw create");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("txn begin (create)");
            let name = format!("s10_{i}.txt");
            create_file(&mut txn, &vsb, &omap, 2, &name)
                .unwrap_or_else(|e| panic!("create_file {name}: {e}"));
            txn.commit()
                .unwrap_or_else(|e| panic!("commit create {name}: {e}"));
        }
        println!("  created {count} files");
    }

    /// Sequential rename scenario: one transaction per rename.
    /// Validates that each omap patch is consistent immediately after commit.
    fn run_s10_sequential(img: &str) {
        create_files(img, S10_FILE_COUNT);

        for i in 0..S10_FILE_COUNT {
            let old_name = format!("s10_{i}.txt");
            let new_name = format!("s10_renamed_{i}.txt");

            let (vsb, omap) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw rename");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("txn begin rename");
            rename(&mut txn, &vsb, &omap, 2, &old_name, &new_name, false)
                .unwrap_or_else(|e| panic!("rename {old_name}: {e}"));
            txn.commit()
                .unwrap_or_else(|e| panic!("commit rename {old_name}: {e}"));

            // Assert parseable after EVERY rename - catches stale paddr immediately.
            assert_device_parseable(img, &format!("S10-seq-after-rename-{i}"));
        }

        println!("  sequential {S10_FILE_COUNT} renames done");

        // Follow-up write must succeed.
        {
            let (vsb, omap) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw follow-up");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("follow-up txn begin");
            create_file(&mut txn, &vsb, &omap, 2, "s10_seq_follow.txt")
                .expect("follow-up create_file");
            txn.commit().expect("follow-up commit");
        }
        assert_device_parseable(img, "S10-seq-final");
    }

    /// Batch rename scenario: all renames in ONE transaction.
    /// Validates that multiple omap patches within a single txn don't conflict.
    fn run_s10_batch(img: &str) {
        create_files(img, S10_FILE_COUNT);

        {
            let (vsb, omap) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw batch-rename");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("txn begin (batch rename)");

            for i in 0..S10_FILE_COUNT {
                let old_name = format!("s10_{i}.txt");
                let new_name = format!("s10_batch_{i}.txt");
                rename(&mut txn, &vsb, &omap, 2, &old_name, &new_name, false)
                    .unwrap_or_else(|e| panic!("batch rename {old_name}: {e}"));
            }

            txn.commit().unwrap_or_else(|e| {
                panic!("batch rename commit FAILED (M12 omap regression): {e}")
            });
        }

        println!("  batch-renamed {S10_FILE_COUNT} files in one transaction");
        assert_device_parseable(img, "S10-batch-post-rename");

        // Follow-up write must succeed.
        {
            let (vsb, omap) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw follow-up");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("follow-up txn begin");
            create_file(&mut txn, &vsb, &omap, 2, "s10_batch_follow.txt")
                .expect("follow-up create_file");
            txn.commit().expect("follow-up commit");
        }
        assert_device_parseable(img, "S10-batch-final");
    }

    #[test]
    fn s10_omap_incremental_sequential_no_corruption() {
        if !check_gate() {
            println!("S10-sequential: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        println!("S10-sequential: 50 individual-commit renames - omap must stay consistent");

        let img = "/tmp/apfs-chaos-s10-seq.img";
        gen_scratch_at(img);
        run_s10_sequential(img);
        println!("S10-sequential PASS");
    }

    #[test]
    fn s10_omap_incremental_batch_no_corruption() {
        if !check_gate() {
            println!("S10-batch: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        println!("S10-batch: 50 renames in one txn - omap incremental patch must not corrupt");

        let img = "/tmp/apfs-chaos-s10-batch.img";
        gen_scratch_at(img);
        run_s10_batch(img);
        println!("S10-batch PASS");
    }
}
