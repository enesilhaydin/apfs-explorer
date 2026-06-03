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
    use apfs_write::file::{clone_file, overwrite_existing_file, unlink, write_file};
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
        // Mount via hdiutil to trigger fsck, or run fsck_apfs directly.
        let status = Command::new("hdiutil")
            .args(["verify", img])
            .status()
            .expect("hdiutil verify");
        assert!(status.success(), "hdiutil verify failed (image corrupt)");
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
}
