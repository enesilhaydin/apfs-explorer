//! Growing a file past its end without writing.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! Setting a file's length beyond its content is how programs preallocate:
//! the new tail reads as zeros and owns no blocks until written. On disk that
//! tail has to be described - a hole extent covering it, the stream's
//! allocated size and sparse-byte count agreeing - or the stream claims a
//! size its extents do not reach, which fsck rejects.
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
mod truncate_extend {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, truncate_file_fast, write_file, TruncateOutcome};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-truncate-extend.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
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
                "64",
            ])
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
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

    fn run_fsck(label: &str) {
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
        let parent = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
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

    /// Grow `name` from `from` bytes (its current size) to `to`; whatever the
    /// fast path decides, the result must be fsck-clean and read as the old
    /// content followed by zeros.
    fn grow_and_check(name: &str, content: &[u8], to: u64) {
        let outcome = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let o = truncate_file_fast(&mut txn, &vsb, &om, 2, name, to, content.len() as u64)
                .expect("truncate");
            if matches!(o, TruncateOutcome::Applied) {
                txn.commit().expect("commit extend");
            } else {
                let _ = txn.abort();
            }
            o
        };
        println!(
            "truncate-extend: {name} {} -> {to}: {outcome:?}",
            content.len()
        );
        if !matches!(outcome, TruncateOutcome::Applied) {
            return;
        }
        run_fsck(&format!("after growing {name} to {to}"));
        let mut fs = open_fs();
        let path = format!("/{name}");
        let attr = fs.getattr(&path).expect("getattr").expect("file");
        assert_eq!(attr.size, to, "{name} must report the new length");
        let got = fs.read(&path, 0, to as usize).expect("read");
        let mut expected = content.to_vec();
        expected.resize(to as usize, 0);
        assert_eq!(
            got, expected,
            "{name} must read as its content followed by zeros"
        );
        drop(fs);
        assert_grown_shape("ours", name, content.len() as u64, to);
    }

    /// What a stream grown without writing has to look like on disk: the
    /// tail past the last real block is a hole extent, the allocated size
    /// covers the whole stream in blocks, and the inode counts the hole bytes
    /// and carries the sparse flag. This is what macOS writes (measured
    /// below) and what the reference checker requires.
    fn assert_grown_shape(label: &str, name: &str, content_len: u64, size: u64) {
        const INODE_IS_SPARSE: u64 = 0x200;
        let bsz = 4096u64;
        let mut fs = open_fs();
        let path = format!("/{name}");
        let inode = fs.inode_record(&path).expect("inode").expect("file");
        let ds = inode.dstream.expect("a stream");
        let extents = fs.file_extents(&path).expect("extents");
        let real_bytes = content_len.div_ceil(bsz) * bsz;
        let alloced = size.div_ceil(bsz) * bsz;
        assert_eq!(ds.size, size, "{label}: {name} size");
        assert_eq!(
            ds.alloced_size, alloced,
            "{label}: {name} allocated size must cover the whole stream"
        );
        let covered: u64 = extents.iter().map(|e| e.len).sum();
        assert_eq!(
            covered, alloced,
            "{label}: {name} extents must tile the stream: {extents:?}"
        );
        let hole_bytes: u64 = extents
            .iter()
            .filter(|e| e.phys_block_num == 0)
            .map(|e| e.len)
            .sum();
        assert_eq!(
            hole_bytes,
            alloced - real_bytes,
            "{label}: {name} the tail past the content must be a hole: {extents:?}"
        );
        assert_eq!(
            inode.sparse_bytes,
            Some(hole_bytes),
            "{label}: {name} the inode must count the hole bytes"
        );
        assert_ne!(
            inode.internal_flags & INODE_IS_SPARSE,
            0,
            "{label}: {name} the inode must carry the sparse flag"
        );
    }

    /// The oracle: macOS grows two files the same way and the shape above is
    /// read back from what it wrote.
    #[test]
    #[ignore = "needs macOS to grow the files"]
    fn macos_grows_a_file_with_a_hole_extent() {
        if !gate() {
            println!("truncate-extend: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-truncate-extend.dmg");
        let out = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "64m",
                "-layout",
                "GPTSPUD",
                "-fs",
                "APFS",
                "-volname",
                "GROWN",
                "/tmp/apfs-truncate-extend",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-truncate-extend.dmg", IMG).expect("rename");
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("mount point");
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("whole disk")
            .to_string();
        std::fs::write(format!("{mount}/grown.bin"), vec![0x5Au8; 10_000]).expect("write");
        std::fs::write(format!("{mount}/preallocated.bin"), b"").expect("create");
        for (name, size) in [("grown.bin", "100000"), ("preallocated.bin", "65536")] {
            let st = Command::new("truncate")
                .args(["-s", size, &format!("{mount}/{name}")])
                .status()
                .expect("truncate");
            assert!(st.success(), "truncate {name}");
        }
        let _ = Command::new("sync").status();
        let _ = Command::new("hdiutil").args(["detach", &parent]).status();

        assert_grown_shape("macOS", "grown.bin", 10_000, 100_000);
        assert_grown_shape("macOS", "preallocated.bin", 0, 65_536);
        println!("truncate-extend PASS: macOS grows a file with a hole extent");
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn growing_a_file_past_its_end_leaves_a_valid_stream() {
        if !gate() {
            println!("truncate-extend: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // A file with content, grown by several blocks.
        let content: Vec<u8> = (0..10_000u32).map(|i| (i % 253) as u8).collect();
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, "grown.bin", &content).expect("write");
            txn.commit().expect("commit write");
        }
        grow_and_check("grown.bin", &content, 100_000);

        // A file created empty, grown from nothing: it has no stream at all
        // to begin with.
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            create_file(&mut txn, &vsb, &om, 2, "preallocated.bin").expect("create");
            txn.commit().expect("commit create");
        }
        grow_and_check("preallocated.bin", &[], 65_536);
        println!("truncate-extend PASS");
    }
}
