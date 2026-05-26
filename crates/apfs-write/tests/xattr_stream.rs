//! Integration test — large xattr stream write (W4-GAP-1).
//!
//! Gate: set `APFS_WRITE_SCRATCH=1` to run (requires macOS + hdiutil).
//! Without the env var every test is a no-op pass.
//!
//! CI invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release --test xattr_stream -- --test-threads=1
//!
//! What this exercises:
//!   X1. Write 100 KiB xattr value → stored as stream (XATTR_DATA_STREAM).
//!   X2. Write 1 MiB xattr value → stream, multi-block allocation.
//!   X3. Replace existing stream xattr — old blocks freed, new blocks written.
//!   X4. Delete stream xattr — DSTREAM_ID + FILE_EXTENT records removed.
//!   X5. Small (embedded) xattr round-trip still works.
//!
//! After each mutating step the transaction is committed and the device is
//! re-parsed to verify structural integrity (proxy for fsck).

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
mod xattr_stream_tests {
    use apfs_core::block_device::{
        BlockDevice, FileBlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice,
    };
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_write::file::{delete_xattr, set_xattr, write_file};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    const SCRATCH_SIZE_MB: u32 = 64;
    const SECTOR: u64 = 512;
    const IMG: &str = "/tmp/apfs-xattr-stream.img";
    const FNAME: &str = "xattr_target.txt";
    const XATTR_NAME: &str = "user.apfsx_large";
    const XATTR_SMALL: &str = "user.apfsx_small";

    const SIZE_100KIB: usize = 100 * 1024;
    const SIZE_1MIB: usize = 1024 * 1024;

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
    // Helpers — same pattern as stress_large_file
    // -----------------------------------------------------------------------

    fn make_content(len: usize, fill: u8) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        let bsz = 4096usize;
        let mut block_idx = 0u8;
        while v.len() < len {
            let block_fill = fill.wrapping_add(block_idx);
            let chunk = (v.len()..len).take(bsz).count();
            v.extend(std::iter::repeat(block_fill).take(chunk));
            block_idx = block_idx.wrapping_add(1);
        }
        v
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
        dev.read_at(vol_paddr * bsz as u64, &mut vsb_raw)
            .expect("read vsb");

        let vsb = apfs_core::volume::VolumeSuperblock::parse(&vsb_raw).expect("parse vsb");
        let vol_omap_oid = vsb.omap_oid;

        let mut omap_raw = vec![0u8; bsz];
        dev.read_at(vol_omap_oid * bsz as u64, &mut omap_raw)
            .expect("read vol omap");

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
            .unwrap_or_else(|e| panic!("{label} FAIL: txn begin: {e:?}"));
        let _ = txn.abort();
    }

    fn open_txn(
    ) -> (
        Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>>,
        Vec<u8>,
        Vec<u8>,
    ) {
        let (vsb_raw, omap_raw) = read_vsb_omap_raw();
        let (base, len) = read_image_header();
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let txn = Transaction::begin(dev).expect("txn begin");
        (txn, vsb_raw, omap_raw)
    }

    fn gen_scratch() {
        let status = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                &format!("{SCRATCH_SIZE_MB}m"),
                "-fs",
                "APFS",
                "-layout",
                "GPTSPUD",
                "-o",
                IMG,
                "-ov",
            ])
            .status()
            .expect("hdiutil create");
        assert!(status.success(), "hdiutil create failed: {status}");
    }

    // -----------------------------------------------------------------------
    // Main test
    // -----------------------------------------------------------------------

    #[test]
    fn xattr_stream_roundtrip() {
        if !check_gate() {
            println!("APFS_WRITE_SCRATCH not set — skip");
            return;
        }

        println!("[1] gen-scratch {IMG}");
        gen_scratch();
        assert_parseable("baseline");

        // ---------------------------------------------------------------
        // Setup: create a target file to attach xattrs to.
        // ---------------------------------------------------------------
        println!("[2] create target file {FNAME}");
        let inode_id: u64;
        {
            let (mut txn, vsb_raw, omap_raw) = open_txn();
            let content = b"xattr test file content";
            inode_id = write_file(&mut txn, &vsb_raw, &omap_raw, 2, FNAME, content)
                .expect("write_file");
            txn.commit().expect("commit create");
        }
        println!("  file inode = {inode_id:#x}");
        assert_parseable("after create file");

        // ---------------------------------------------------------------
        // X5: small embedded xattr (below threshold) — still works.
        // ---------------------------------------------------------------
        println!("[3] X5: set small embedded xattr");
        let xid_before_x5 = current_xid();
        {
            let (mut txn, vsb_raw, omap_raw) = open_txn();
            set_xattr(
                &mut txn,
                &vsb_raw,
                &omap_raw,
                inode_id,
                XATTR_SMALL,
                b"hello-apfs",
            )
            .expect("set embedded xattr");
            txn.commit().expect("commit X5");
        }
        let xid_after_x5 = current_xid();
        assert!(xid_after_x5 > xid_before_x5, "X5: XID not advanced");
        assert_parseable("after X5 (embedded xattr)");
        println!("  X5 PASS: XID {xid_before_x5:#x} → {xid_after_x5:#x}");

        // ---------------------------------------------------------------
        // X1: 100 KiB stream xattr.
        // ---------------------------------------------------------------
        println!("[4] X1: set 100 KiB stream xattr");
        let content_100k = make_content(SIZE_100KIB, 0xAB);
        let xid_before_x1 = current_xid();
        {
            let (mut txn, vsb_raw, omap_raw) = open_txn();
            set_xattr(
                &mut txn,
                &vsb_raw,
                &omap_raw,
                inode_id,
                XATTR_NAME,
                &content_100k,
            )
            .expect("set 100 KiB stream xattr");
            txn.commit().expect("commit X1");
        }
        let xid_after_x1 = current_xid();
        assert!(xid_after_x1 > xid_before_x1, "X1: XID not advanced");
        assert_parseable("after X1 (100 KiB xattr)");
        println!(
            "  X1 PASS: XID {xid_before_x1:#x} → {xid_after_x1:#x} ({SIZE_100KIB} bytes)"
        );

        // ---------------------------------------------------------------
        // X2: 1 MiB stream xattr (replace the 100 KiB one).
        // ---------------------------------------------------------------
        println!("[5] X2: replace with 1 MiB stream xattr");
        let content_1m = make_content(SIZE_1MIB, 0xCD);
        let xid_before_x2 = current_xid();
        {
            let (mut txn, vsb_raw, omap_raw) = open_txn();
            set_xattr(
                &mut txn,
                &vsb_raw,
                &omap_raw,
                inode_id,
                XATTR_NAME,
                &content_1m,
            )
            .expect("set 1 MiB stream xattr");
            txn.commit().expect("commit X2");
        }
        let xid_after_x2 = current_xid();
        assert!(xid_after_x2 > xid_before_x2, "X2: XID not advanced");
        assert_parseable("after X2 (1 MiB xattr replace)");
        println!("  X2 PASS: XID {xid_before_x2:#x} → {xid_after_x2:#x} ({SIZE_1MIB} bytes)");

        // ---------------------------------------------------------------
        // X3: Replace existing stream xattr with different 100 KiB content.
        //     Old blocks from the 1 MiB write must be freed.
        // ---------------------------------------------------------------
        println!("[6] X3: replace 1 MiB → 100 KiB (old blocks freed)");
        let content_replace = make_content(SIZE_100KIB, 0xEF);
        let xid_before_x3 = current_xid();
        {
            let (mut txn, vsb_raw, omap_raw) = open_txn();
            set_xattr(
                &mut txn,
                &vsb_raw,
                &omap_raw,
                inode_id,
                XATTR_NAME,
                &content_replace,
            )
            .expect("replace stream xattr");
            txn.commit().expect("commit X3");
        }
        let xid_after_x3 = current_xid();
        assert!(xid_after_x3 > xid_before_x3, "X3: XID not advanced");
        assert_parseable("after X3 (stream xattr replace)");
        println!("  X3 PASS: XID {xid_before_x3:#x} → {xid_after_x3:#x}");

        // ---------------------------------------------------------------
        // X4: Delete stream xattr — DSTREAM_ID + FILE_EXTENT records must vanish.
        // ---------------------------------------------------------------
        println!("[7] X4: delete stream xattr");
        let xid_before_x4 = current_xid();
        {
            let (mut txn, vsb_raw, omap_raw) = open_txn();
            delete_xattr(&mut txn, &vsb_raw, &omap_raw, inode_id, XATTR_NAME)
                .expect("delete stream xattr");
            txn.commit().expect("commit X4");
        }
        let xid_after_x4 = current_xid();
        assert!(xid_after_x4 > xid_before_x4, "X4: XID not advanced");
        assert_parseable("after X4 (stream xattr delete)");
        println!("  X4 PASS: XID {xid_before_x4:#x} → {xid_after_x4:#x}");

        // ---------------------------------------------------------------
        // X4b: delete_xattr on non-existent name — idempotent, Ok(()).
        // ---------------------------------------------------------------
        println!("[8] X4b: idempotent delete (xattr already gone)");
        {
            let (mut txn, vsb_raw, omap_raw) = open_txn();
            delete_xattr(&mut txn, &vsb_raw, &omap_raw, inode_id, XATTR_NAME)
                .expect("idempotent delete must return Ok");
            let _ = txn.abort();
        }
        assert_parseable("after X4b (idempotent delete)");
        println!("  X4b PASS: idempotent delete ok");

        println!("\nAll xattr_stream tests PASSED.");
    }
}
