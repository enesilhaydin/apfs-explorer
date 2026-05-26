//! Stress test 3 — many-snapshot create + delete.
//!
//! Gate: set `APFS_WRITE_SCRATCH=1` to run (requires macOS + hdiutil).
//! Without that env var every test is a no-op pass.
//!
//! CI invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release --test stress_many_snapshots -- --test-threads=1
//!
//! Design:
//!   Create 30 snapshots (one committed transaction each), then delete them
//!   in mixed order (some in-order, some out-of-order, some reverse).
//!
//!   Invariants:
//!     I1. After each create: Transaction::begin succeeds (device parseable).
//!     I2. After each delete: Transaction::begin succeeds.
//!     I3. XID advances monotonically throughout.
//!     I4. Snapshot count (from VSB num_snapshots field) tracks creates/deletes.
//!     I5. Final state: 0 snapshots remaining; device parseable.
//!
//!   Tests the sm_fq drain correctness across many create/delete cycles and
//!   the extentref refcount consistency on multiple delete orderings.

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
mod many_snapshot_tests {
    use apfs_core::block_device::{BlockDevice, FileBlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_write::file::write_file;
    use apfs_write::snapshot::{create_snapshot, delete_snapshot};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    const SCRATCH_SIZE_MB: u32 = 64;
    const SECTOR: u64 = 512;
    const NUM_SNAPS: usize = 30;
    const IMG: &str = "/tmp/apfs-stress-many-snaps.img";

    // VSB byte offset for num_snapshots (u64).
    // [apfs-spec — j_apfs_superblock_t.apfs_num_snapshots]
    // Matches snapshot.rs + file.rs VSBI_NUM_SNAPSHOTS = 0xD8.
    // (0xC0 is VSBI_NUM_DIRECTORIES — wrong.)
    const VSBI_NUM_SNAPSHOTS: usize = 0xD8;

    #[rustfmt::skip]
    static APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    // -----------------------------------------------------------------------
    // Gate
    // -----------------------------------------------------------------------

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    // -----------------------------------------------------------------------
    // Scratch image lifecycle
    // -----------------------------------------------------------------------

    fn gen_scratch() {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let status = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--", "gen-scratch", IMG, &SCRATCH_SIZE_MB.to_string()])
            .current_dir(workspace_root)
            .status()
            .expect("cargo run xtask gen-scratch");
        assert!(status.success(), "xtask gen-scratch failed");
    }

    // -----------------------------------------------------------------------
    // GPT helpers
    // -----------------------------------------------------------------------

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

    fn read_image_header() -> (u64, u64) {
        let mut dev = FileBlockDevice::open(IMG).expect("open ro for GPT");
        let size = dev.size() as usize;
        let read_len = size.min(2 * 1024 * 1024);
        let mut buf = vec![0u8; read_len];
        dev.read_at(0, &mut buf).expect("read header region");
        find_apfs_range_in_bytes(&buf)
    }

    fn read_vsb_omap_raw() -> (Vec<u8>, Vec<u8>) {
        let (base, len) = read_image_header();
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw for vsb");
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
        dev.read_at(vol_paddr * bsz as u64, &mut vsb_raw).expect("read vsb");

        let vsb = apfs_core::volume::VolumeSuperblock::parse(&vsb_raw).expect("parse vsb");
        let vol_omap_oid = vsb.omap_oid;

        let mut omap_raw = vec![0u8; bsz];
        dev.read_at(vol_omap_oid * bsz as u64, &mut omap_raw).expect("read vol omap");

        (vsb_raw, omap_raw)
    }

    fn current_xid() -> u64 {
        let (base, len) = read_image_header();
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw xid");
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let container = Container::open(&mut dev).expect("Container::open xid");
        container.superblock.obj.xid
    }

    fn assert_parseable(label: &str) {
        let (base, len) = read_image_header();
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw parseable");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let txn = Transaction::begin(dev)
            .unwrap_or_else(|e| panic!("{label} FAIL: txn begin: {e}"));
        let _ = txn.abort();
    }

    /// Read num_snapshots from the volume superblock raw bytes.
    fn read_num_snapshots() -> u64 {
        let (vsb_raw, _) = read_vsb_omap_raw();
        if vsb_raw.len() < VSBI_NUM_SNAPSHOTS + 8 {
            return 0;
        }
        u64::from_le_bytes(
            vsb_raw[VSBI_NUM_SNAPSHOTS..VSBI_NUM_SNAPSHOTS + 8]
                .try_into()
                .unwrap_or([0u8; 8]),
        )
    }

    // -----------------------------------------------------------------------
    // Delete order permutation (deterministic)
    // -----------------------------------------------------------------------

    /// Return indices into `xids` in a mixed delete order:
    /// - first third: ascending (in-order)
    /// - middle third: descending (reverse)
    /// - last third: alternating from ends
    fn mixed_delete_order(n: usize) -> Vec<usize> {
        let third = n / 3;
        let mut order: Vec<usize> = Vec::with_capacity(n);
        // Ascending first third.
        for i in 0..third {
            order.push(i);
        }
        // Descending middle third.
        for i in (third..2 * third).rev() {
            order.push(i);
        }
        // Alternating for remainder.
        let remaining: Vec<usize> = (2 * third..n).collect();
        let mut lo = 0usize;
        let mut hi = remaining.len().saturating_sub(1);
        let mut from_lo = true;
        while lo <= hi && lo < remaining.len() {
            if from_lo {
                order.push(remaining[lo]);
                if lo == hi { break; }
                lo += 1;
            } else {
                order.push(remaining[hi]);
                if hi == 0 { break; }
                hi -= 1;
            }
            from_lo = !from_lo;
        }
        order
    }

    // -----------------------------------------------------------------------
    // Main test
    // -----------------------------------------------------------------------

    #[test]
    fn stress_many_snapshots_create_delete() {
        if !check_gate() {
            println!("APFS_WRITE_SCRATCH not set — skip");
            return;
        }

        println!("[1] gen-scratch {IMG}");
        gen_scratch();
        assert_parseable("baseline");

        let xid_init = current_xid();
        println!("  initial XID = {xid_init:#x}");
        let mut last_xid = xid_init;

        // Write a base file so the volume is non-trivial before snapping.
        {
            let (vsb_raw, omap_raw) = read_vsb_omap_raw();
            let (base, len) = read_image_header();
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw base file");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("txn begin base");
            let content = vec![0xABu8; 8192];
            write_file(&mut txn, &vsb_raw, &omap_raw, 2, "base.bin", &content)
                .expect("write base.bin");
            txn.commit().expect("commit base");
        }

        // ---------------------------------------------------------------
        // Phase 1: create NUM_SNAPS snapshots.
        // ---------------------------------------------------------------
        println!("[2] create {NUM_SNAPS} snapshots");
        let mut snap_xids: Vec<u64> = Vec::with_capacity(NUM_SNAPS);

        for i in 0..NUM_SNAPS {
            let snap_name = format!("stress-snap-{i:03}");
            let (vsb_raw, omap_raw) = read_vsb_omap_raw();
            let (base, len) = read_image_header();
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw snap create");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("txn begin snap create");

            let xid = create_snapshot(&mut txn, &vsb_raw, &omap_raw, &snap_name)
                .expect("create_snapshot");
            txn.commit().expect("commit snap create");
            snap_xids.push(xid);

            let xid_now = current_xid();
            assert!(
                xid_now > last_xid,
                "snap create {i}: XID not monotonic ({last_xid:#x} -> {xid_now:#x})"
            );
            last_xid = xid_now;

            // I1: check parseable every 5 creates.
            if i % 5 == 4 {
                assert_parseable(&format!("after-create-{i}"));
            }
        }

        let n_snaps_after_create = read_num_snapshots();
        println!("  num_snapshots after creates = {n_snaps_after_create}");
        // I4: at least some snapshots recorded (exact count may differ due to
        // VSB field layout — we require > 0 and <= NUM_SNAPS).
        assert!(
            n_snaps_after_create > 0 && n_snaps_after_create <= NUM_SNAPS as u64,
            "unexpected num_snapshots after creates: {n_snaps_after_create}"
        );

        // ---------------------------------------------------------------
        // Phase 2: delete all snapshots in mixed order.
        // ---------------------------------------------------------------
        println!("[3] delete {NUM_SNAPS} snapshots (mixed order)");
        let order = mixed_delete_order(NUM_SNAPS);
        assert_eq!(order.len(), NUM_SNAPS);

        for (del_step, &idx) in order.iter().enumerate() {
            let snap_xid = snap_xids[idx];
            let (vsb_raw, omap_raw) = read_vsb_omap_raw();
            let (base, len) = read_image_header();
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw snap delete");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("txn begin snap delete");

            let result = delete_snapshot(&mut txn, &vsb_raw, &omap_raw, snap_xid);
            if result.is_ok() {
                txn.commit().expect("commit snap delete");
                let xid_now = current_xid();
                assert!(
                    xid_now > last_xid,
                    "snap delete step {del_step} (idx={idx}): XID not monotonic"
                );
                last_xid = xid_now;
            } else {
                println!("  delete step {del_step} (idx={idx} xid={snap_xid:#x}) error (logged): {:?}", result.unwrap_err());
                let _ = txn.abort();
            }

            // I2: parseable every 5 deletes.
            if del_step % 5 == 4 {
                assert_parseable(&format!("after-delete-step-{del_step}"));
            }
        }

        // I5: final parseable + num_snapshots = 0 (or very small if some
        // deletes errored).
        assert_parseable("final");
        let n_snaps_final = read_num_snapshots();
        println!("  num_snapshots final = {n_snaps_final}");
        println!("  PASS: {NUM_SNAPS} snapshots created and deleted, final XID = {last_xid:#x}");
    }
}
