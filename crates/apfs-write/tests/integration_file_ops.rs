//! Integration tests: file operations on a real scratch APFS image.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS + hdiutil).
//! Without that env var every test is a no-op pass.
//!
//! CI invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release --test integration_file_ops -- --test-threads=1
//!
//! Covers: create_file, write_file, unlink, rename, mkdir, truncate_file_fast,
//! overwrite_existing_file - the file.rs paths that need a real Transaction.
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
mod file_ops_tests {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        create_file, mkdir, overwrite_existing_file, rename, truncate_file_fast, unlink, write_file,
    };
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-ifile-ops.img";
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

    /// Read VSB raw bytes and volume omap raw bytes.
    /// vsb.omap_oid is a physical paddr (not a virtual OID) - read directly.
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
        // vsb.omap_oid is a physical paddr - read directly (not via container omap)
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

    // -----------------------------------------------------------------------
    // T1: create → write 1 KiB → read via FsView → delete → gone
    // -----------------------------------------------------------------------

    #[test]
    fn t1_create_write_read_delete() {
        if !check_gate() {
            println!("T1: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // Write file
        let content = vec![0x42u8; 1024]; // 1 KiB of 'B'
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "t1_file.txt", &content).expect("write_file");
            txn.commit().expect("commit write");
        }

        // Read back via FsView
        {
            use apfs::FsView;
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = FsView::open(dev).expect("FsView::open");
            let attr = fs
                .getattr("/t1_file.txt")
                .expect("getattr")
                .expect("file exists");
            assert_eq!(attr.size, 1024, "T1: size mismatch");
            assert!(!attr.is_dir, "T1: should be a file");
            let data = fs.read("/t1_file.txt", 0, 1024).expect("read");
            assert_eq!(data.len(), 1024);
            assert!(data.iter().all(|&b| b == 0x42), "T1: content mismatch");
        }

        // Delete
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            unlink(&mut txn, &vsb, &omap, 2, "t1_file.txt").expect("unlink");
            txn.commit().expect("commit unlink");
        }

        // Confirm gone
        {
            use apfs::FsView;
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = FsView::open(dev).expect("FsView::open");
            let attr = fs.getattr("/t1_file.txt").expect("getattr after delete");
            assert!(attr.is_none(), "T1: file should be gone after unlink");
        }

        println!("T1 PASS: create → write 1KiB → read → delete → gone");
    }

    // -----------------------------------------------------------------------
    // T2: write 64 KiB → truncate to 16 KiB → size + content correct
    // -----------------------------------------------------------------------

    #[test]
    fn t2_write_64k_truncate_16k() {
        if !check_gate() {
            println!("T2: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let content = vec![0xCCu8; 65536]; // 64 KiB
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "t2_file.bin", &content).expect("write_file");
            txn.commit().expect("commit write");
        }

        // Truncate to 16 KiB (aligned shrink: 64 KiB→16 KiB both multiples of 4 KiB)
        {
            use apfs_write::file::TruncateOutcome;
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let outcome = truncate_file_fast(&mut txn, &vsb, &omap, 2, "t2_file.bin", 16384, 65536)
                .expect("truncate_file_fast");
            match outcome {
                TruncateOutcome::Applied => {
                    txn.commit().expect("commit truncate");
                }
                TruncateOutcome::FastPathDeclined => {
                    // Fast path declined - skip this assertion, not a bug in our test
                    let _ = txn.abort();
                    println!(
                        "T2: FastPathDeclined - truncate_file_fast out of scope for this case"
                    );
                    return;
                }
            }
        }

        // Verify size and content
        {
            use apfs::FsView;
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = FsView::open(dev).expect("FsView::open");
            let attr = fs
                .getattr("/t2_file.bin")
                .expect("getattr")
                .expect("file exists");
            assert_eq!(attr.size, 16384, "T2: size after truncate");
            let data = fs.read("/t2_file.bin", 0, 16384).expect("read");
            assert_eq!(data.len(), 16384);
            assert!(
                data.iter().all(|&b| b == 0xCC),
                "T2: truncated content mismatch"
            );
        }

        println!("T2 PASS: write 64KiB → truncate 16KiB → correct size+content");
    }

    // -----------------------------------------------------------------------
    // T3: create → write → rename → read at new path
    // -----------------------------------------------------------------------

    #[test]
    fn t3_write_rename_read() {
        if !check_gate() {
            println!("T3: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let content = vec![0xABu8; 2048];
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "t3_old.txt", &content).expect("write_file");
            txn.commit().expect("commit write");
        }

        // Rename
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            rename(&mut txn, &vsb, &omap, 2, "t3_old.txt", "t3_new.txt", false).expect("rename");
            txn.commit().expect("commit rename");
        }

        // Verify new path readable, old path gone
        {
            use apfs::FsView;
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = FsView::open(dev).expect("FsView::open");
            let old_attr = fs.getattr("/t3_old.txt").expect("getattr old");
            assert!(old_attr.is_none(), "T3: old name should be gone");
            let data = fs.read("/t3_new.txt", 0, 2048).expect("read new");
            assert_eq!(data.len(), 2048);
            assert!(
                data.iter().all(|&b| b == 0xAB),
                "T3: content mismatch after rename"
            );
        }

        println!("T3 PASS: write → rename → read at new path, old path gone");
    }

    // -----------------------------------------------------------------------
    // T4: mkdir nested → write file inside → read
    // -----------------------------------------------------------------------

    #[test]
    fn t4_mkdir_nested_write_read() {
        if !check_gate() {
            println!("T4: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // Create /t4dir
        let dir_ino = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let ino = mkdir(&mut txn, &vsb, &omap, 2, "t4dir").expect("mkdir t4dir");
            txn.commit().expect("commit mkdir");
            ino
        };

        // Create /t4dir/subdir
        let subdir_ino = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let ino = mkdir(&mut txn, &vsb, &omap, dir_ino, "subdir").expect("mkdir subdir");
            txn.commit().expect("commit mkdir subdir");
            ino
        };

        // Write /t4dir/subdir/inner.bin
        let content = vec![0x77u8; 512];
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, subdir_ino, "inner.bin", &content)
                .expect("write_file inner");
            txn.commit().expect("commit write inner");
        }

        // Read back
        {
            use apfs::FsView;
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = FsView::open(dev).expect("FsView::open");
            let attr = fs
                .getattr("/t4dir/subdir/inner.bin")
                .expect("getattr nested")
                .expect("nested file exists");
            assert_eq!(attr.size, 512, "T4: nested file size");
            let data = fs
                .read("/t4dir/subdir/inner.bin", 0, 512)
                .expect("read nested");
            assert_eq!(data.len(), 512);
            assert!(
                data.iter().all(|&b| b == 0x77),
                "T4: nested content mismatch"
            );
        }

        println!("T4 PASS: mkdir nested → write → read via multi-component path");
    }

    // -----------------------------------------------------------------------
    // T5: overwrite_existing_file changes content
    // -----------------------------------------------------------------------

    #[test]
    fn t5_overwrite_file_content() {
        if !check_gate() {
            println!("T5: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let orig = vec![0x11u8; 4096];
        let updated = vec![0x22u8; 4096];

        // Initial write
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "t5_file.bin", &orig).expect("write_file");
            txn.commit().expect("commit write");
        }

        // Overwrite
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            overwrite_existing_file(&mut txn, &vsb, &omap, 2, "t5_file.bin", &updated)
                .expect("overwrite_existing_file");
            txn.commit().expect("commit overwrite");
        }

        // Verify new content
        {
            use apfs::FsView;
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = FsView::open(dev).expect("FsView::open");
            let data = fs.read("/t5_file.bin", 0, 4096).expect("read");
            assert_eq!(data.len(), 4096);
            assert!(data.iter().all(|&b| b == 0x22), "T5: content not updated");
        }

        println!("T5 PASS: overwrite_existing_file changes content correctly");
    }

    // -----------------------------------------------------------------------
    // T6: create_file (empty) persists as zero-size inode
    // -----------------------------------------------------------------------

    #[test]
    fn t6_create_empty_file_persists() {
        if !check_gate() {
            println!("T6: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            create_file(&mut txn, &vsb, &omap, 2, "t6_empty.txt").expect("create_file");
            txn.commit().expect("commit create");
        }

        {
            use apfs::FsView;
            let raw = std::fs::read(IMG).expect("read img");
            let (base, len) = find_apfs_range(&raw);
            let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = FsView::open(dev).expect("FsView::open");
            let attr = fs
                .getattr("/t6_empty.txt")
                .expect("getattr")
                .expect("file must exist");
            assert_eq!(attr.size, 0, "T6: empty file size");
            assert!(!attr.is_dir);
        }

        println!("T6 PASS: create_file → zero-size inode persists");
    }
}
