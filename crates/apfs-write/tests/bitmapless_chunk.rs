//! Filling a container past its first allocation chunk.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! A chunk covers `blocks_per_chunk` blocks - 32768 on a 4 KiB container, so
//! 128 MiB. The formatter writes a bitmap only for the chunk it actually used;
//! every later chunk carries `ci_bitmap_addr == 0`, which states that the whole
//! chunk is free. Allocating there means creating that bitmap first, out of the
//! spaceman's internal pool. Until that existed the allocator skipped such a
//! chunk, so a half-gigabyte volume refused everything past its first 128 MiB
//! while reporting the rest as free.
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
mod bitmapless_chunk {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::write_file;
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-bitmapless-chunk.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// 512 MiB: four chunks of 128 MiB, only the first of which the formatter
    /// gives a bitmap.
    const IMAGE_MB: &str = "512";
    /// One chunk's worth of bytes on a 4 KiB container.
    const CHUNK_BYTES: u64 = 32768 * 4096;
    const FILE_BYTES: usize = 4 * 1024 * 1024;
    /// Enough files to run well past the first chunk without filling the volume.
    const FILE_COUNT: usize = 44;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn root_dir() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }
    fn gen_scratch() {
        let _ = std::fs::remove_file(IMG);
        let s = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                IMG,
                IMAGE_MB,
            ])
            .current_dir(root_dir())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
    }
    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let hdr = &raw[512..1024];
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let off = base + i * sz;
            let e = match raw.get(off..off + sz) {
                Some(e) => e,
                None => break,
            };
            if e.get(..16) == Some(&APFS_GPT_TYPE[..]) {
                let s = u64::from_le_bytes(e[32..40].try_into().unwrap());
                let en = u64::from_le_bytes(e[40..48].try_into().unwrap());
                return (s * SECTOR, (en - s + 1) * SECTOR);
            }
        }
        panic!("no APFS partition");
    }
    fn read_vsb_omap() -> (Vec<u8>, Vec<u8>) {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let c = Container::open(&mut dev).unwrap();
        let nx = &c.superblock;
        let bsz = nx.block_size as usize;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).unwrap();
        let fs_oid = *nx.fs_oids.first().unwrap();
        let vp = omap
            .resolve(&mut dev, fs_oid, nx.obj.xid, nx.block_size)
            .unwrap()
            .unwrap();
        let mut vsb = vec![0u8; bsz];
        dev.read_at(vp * bsz as u64, &mut vsb).unwrap();
        let v = VolumeSuperblock::parse(&vsb).unwrap();
        let mut om = vec![0u8; bsz];
        dev.read_at(v.omap_oid * bsz as u64, &mut om).unwrap();
        (vsb, om)
    }
    fn open_txn() -> Transaction<Dev> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }
    fn open_fs() -> apfs::FsView<Dev> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        apfs::FsView::open(WritableOffsetBlockDevice::new(inner, base, len)).expect("FsView::open")
    }

    fn attach_nomount() -> String {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                "-nomount",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string()
    }

    /// fsck_apfs exits 0 on damage it considers repairable, so the warnings
    /// have to be read as failures too.
    fn run_fsck(label: &str) {
        let parent = attach_nomount();
        let rdev = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        let _ = Command::new("hdiutil").args(["detach", &parent]).status();
        let fsck = fsck.expect("fsck_apfs spawn");
        let report = String::from_utf8_lossy(&fsck.stdout).into_owned()
            + &String::from_utf8_lossy(&fsck.stderr);
        assert!(
            fsck.status.success(),
            "{label}: fsck_apfs reported corruption:\n{report}"
        );
        let complaints: Vec<&str> = report
            .lines()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("warning:")
                    || l.contains("overallocation")
                    || l.contains("underallocation")
                    || l.contains("is not valid")
            })
            .collect();
        assert!(
            complaints.is_empty(),
            "{label}: fsck_apfs flagged inconsistencies:\n{}",
            complaints.join("\n")
        );
    }

    #[test]
    #[ignore = "scratch-image gated; writes ~176 MiB"]
    fn a_volume_can_be_filled_past_its_first_allocation_chunk() {
        if !gate() {
            println!("bitmapless-chunk: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let body: Vec<u8> = (0..FILE_BYTES).map(|i| (i % 251) as u8).collect();
        let mut written: u64 = 0;
        for i in 0..FILE_COUNT {
            let name = format!("fill{i:03}.bin");
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            match write_file(&mut txn, &vsb, &om, 2, &name, &body) {
                Ok(_) => {}
                Err(e) => panic!(
                    "write of {name} failed after {written} bytes ({} MiB), with the \
                     volume {} MiB in size: {e}",
                    written / (1024 * 1024),
                    IMAGE_MB
                ),
            }
            txn.commit().expect("commit");
            written += FILE_BYTES as u64;
        }

        assert!(
            written > CHUNK_BYTES,
            "the test must write past one chunk ({} MiB), it wrote {} MiB",
            CHUNK_BYTES / (1024 * 1024),
            written / (1024 * 1024)
        );

        // Every file must read back exactly: a block handed out of a freshly
        // materialised bitmap must not also be handed to the next file.
        {
            let mut fs = open_fs();
            for i in 0..FILE_COUNT {
                let path = format!("/fill{i:03}.bin");
                let got = fs.read(&path, 0, FILE_BYTES).expect("read");
                assert_eq!(got, body, "{path} did not read back");
            }
        }

        run_fsck("after filling past the first chunk");
        println!(
            "bitmapless-chunk PASS: {} MiB written across more than one chunk",
            written / (1024 * 1024)
        );
    }
}
