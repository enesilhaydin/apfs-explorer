//! Integration tests: FsView read-side against a scratch-populated image.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS + hdiutil).
//!
//! CI invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release --test integration_fsview -- --test-threads=1
//!
//! Covers: FsView::open, lookup, read_dir, getattr, read — the apfs/src/lib.rs
//! paths that need a populated image (not covered by the pure path-parser unit
//! tests in Wave 7).
//!
//! This test lives in apfs-write (not in apfs) because creating the scratch
//! image requires apfs-write; the apfs crate has no dev-dep on apfs-write.
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
mod fsview_tests {
    use apfs::FsView;
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{mkdir, write_file};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-ifsview.img";
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

    fn open_fsview(img: &str) -> FsView<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(img).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open_rw for fsview");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        FsView::open(dev).expect("FsView::open")
    }

    // -----------------------------------------------------------------------
    // F1: mount → read_dir root → stat → read file
    // -----------------------------------------------------------------------

    #[test]
    fn f1_mount_readdir_stat_read() {
        if !check_gate() {
            println!("F1: APFS_WRITE_SCRATCH not set — skip");
            return;
        }
        gen_scratch();

        // Populate: write a file at root
        let content = vec![0x5Au8; 4096];
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, "f1_file.bin", &content).expect("write_file");
            txn.commit().expect("commit");
        }

        let mut fs = open_fsview(IMG);

        // read_dir "/"
        let root_entries = fs.read_dir("/").expect("read_dir /");
        let names: Vec<&str> = root_entries.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"f1_file.bin"),
            "F1: root dir must contain f1_file.bin, got: {names:?}"
        );

        // getattr
        let attr = fs.getattr("/f1_file.bin").expect("getattr").expect("file exists");
        assert_eq!(attr.size, 4096, "F1: size must be 4096");
        assert!(!attr.is_dir, "F1: must not be a dir");

        // read full
        let data = fs.read("/f1_file.bin", 0, 4096).expect("read full");
        assert_eq!(data.len(), 4096);
        assert!(data.iter().all(|&b| b == 0x5A), "F1: content mismatch");

        // read partial (middle)
        let mid = fs.read("/f1_file.bin", 100, 50).expect("read mid");
        assert_eq!(mid.len(), 50);
        assert!(mid.iter().all(|&b| b == 0x5A), "F1: partial content mismatch");

        // read past EOF
        let past = fs.read("/f1_file.bin", 99999, 10).expect("read past eof");
        assert!(past.is_empty(), "F1: past-EOF read must be empty");

        println!("F1 PASS: mount → read_dir → getattr → read (full + partial + past-EOF)");
    }

    // -----------------------------------------------------------------------
    // F2: lookup non-existent path → None; lookup empty string → root inode
    // -----------------------------------------------------------------------

    #[test]
    fn f2_lookup_nonexistent_and_root() {
        if !check_gate() {
            println!("F2: APFS_WRITE_SCRATCH not set — skip");
            return;
        }
        gen_scratch();

        let mut fs = open_fsview(IMG);

        // Non-existent
        let r = fs.lookup("/does_not_exist").expect("lookup absent");
        assert!(r.is_none(), "F2: non-existent path must resolve to None");

        let r2 = fs.lookup("/a/b/c").expect("lookup deep absent");
        assert!(r2.is_none(), "F2: deep non-existent must resolve to None");

        // Root inode (empty path)
        let root_ino = fs.lookup("").expect("lookup root");
        assert_eq!(root_ino, Some(2), "F2: root inode must be 2");

        // getattr non-existent
        let ga = fs.getattr("/nope").expect("getattr absent");
        assert!(ga.is_none(), "F2: getattr non-existent must be None");

        // volume_size returns (total, free)
        let (total, free) = fs.volume_size().expect("volume_size");
        assert!(total > 0, "F2: total size must be > 0");
        assert!(free <= total, "F2: free must be <= total");

        println!("F2 PASS: lookup non-existent=None, root=2, volume_size total={total} free={free}");
    }

    // -----------------------------------------------------------------------
    // F3: listing pagination — 50+ files in one directory
    // -----------------------------------------------------------------------

    #[test]
    fn f3_read_dir_pagination_50_files() {
        if !check_gate() {
            println!("F3: APFS_WRITE_SCRATCH not set — skip");
            return;
        }
        gen_scratch();

        // Write 50 files in root
        for i in 0..50u32 {
            let name = format!("f3_{i:03}.bin");
            let content = vec![i as u8; 256];
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, 2, &name, &content).expect("write_file");
            txn.commit().expect("commit");
        }

        let mut fs = open_fsview(IMG);
        let entries = fs.read_dir("/").expect("read_dir /");
        let names: std::collections::BTreeSet<String> =
            entries.into_iter().map(|e| e.name).collect();

        for i in 0..50u32 {
            let name = format!("f3_{i:03}.bin");
            assert!(names.contains(&name), "F3: missing {name} in listing");
        }

        println!("F3 PASS: read_dir returns all 50 files");
    }

    // -----------------------------------------------------------------------
    // F4: path with multiple components resolves correctly
    // -----------------------------------------------------------------------

    #[test]
    fn f4_multi_component_path() {
        if !check_gate() {
            println!("F4: APFS_WRITE_SCRATCH not set — skip");
            return;
        }
        gen_scratch();

        // Build /f4a/f4b/deep.txt
        let dir_a_ino = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let ino = mkdir(&mut txn, &vsb, &omap, 2, "f4a").expect("mkdir f4a");
            txn.commit().expect("commit");
            ino
        };
        let dir_b_ino = {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            let ino = mkdir(&mut txn, &vsb, &omap, dir_a_ino, "f4b").expect("mkdir f4b");
            txn.commit().expect("commit");
            ino
        };
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &omap, dir_b_ino, "deep.txt", &[0xDDu8; 128])
                .expect("write deep");
            txn.commit().expect("commit");
        }

        let mut fs = open_fsview(IMG);

        // Lookup at each level
        let ino_a = fs.lookup("/f4a").expect("lookup /f4a").expect("f4a exists");
        assert!(ino_a > 2, "F4: /f4a inode > 2");

        let ino_b = fs.lookup("/f4a/f4b").expect("lookup /f4a/f4b").expect("f4b exists");
        assert!(ino_b > ino_a, "F4: /f4a/f4b inode > /f4a inode");

        let ino_file = fs
            .lookup("/f4a/f4b/deep.txt")
            .expect("lookup deep")
            .expect("deep.txt exists");
        assert!(ino_file > 2, "F4: deep.txt inode > 2");

        // getattr + read at deep path
        let attr = fs
            .getattr("/f4a/f4b/deep.txt")
            .expect("getattr deep")
            .expect("deep.txt exists");
        assert_eq!(attr.size, 128, "F4: deep file size");

        let data = fs.read("/f4a/f4b/deep.txt", 0, 128).expect("read deep");
        assert_eq!(data.len(), 128);
        assert!(data.iter().all(|&b| b == 0xDD), "F4: content mismatch");

        // read_dir at nested level
        let kids = fs.read_dir("/f4a/f4b").expect("read_dir /f4a/f4b");
        let kid_names: Vec<&str> = kids.iter().map(|e| e.name.as_str()).collect();
        assert!(
            kid_names.contains(&"deep.txt"),
            "F4: /f4a/f4b must list deep.txt"
        );

        println!("F4 PASS: multi-component path lookup/getattr/read/read_dir all correct");
    }

    // -----------------------------------------------------------------------
    // F5: getattr directory returns is_dir=true
    // -----------------------------------------------------------------------

    #[test]
    fn f5_getattr_dir_is_dir() {
        if !check_gate() {
            println!("F5: APFS_WRITE_SCRATCH not set — skip");
            return;
        }
        gen_scratch();

        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            mkdir(&mut txn, &vsb, &omap, 2, "f5_dir").expect("mkdir");
            txn.commit().expect("commit");
        }

        let mut fs = open_fsview(IMG);
        let attr = fs.getattr("/f5_dir").expect("getattr dir").expect("dir exists");
        assert!(attr.is_dir, "F5: directory must have is_dir=true");
        assert_eq!(attr.size, 0, "F5: directory size is 0");

        println!("F5 PASS: getattr directory returns is_dir=true");
    }
}
