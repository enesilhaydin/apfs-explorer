//! Truncating a file macOS stored compressed.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + ditto).
//!
//! A compressed file's content lives in its decmpfs attribute and resource
//! fork; the data stream holds none of it. The metadata-only resize cannot
//! act on such a file - resizing an empty stream changes nothing a reader can
//! see - so it has to decline and let the caller take the path that reads the
//! content back, cuts it, and writes it as a plain file. Refusing outright
//! instead left every compressed file impossible to resize, which on a mounted
//! drive is every program that opens a file for writing and sets its length.
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
mod decmpfs_truncate {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{overwrite_existing_file, truncate_file_fast, TruncateOutcome};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-decmpfs-truncate.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const NAME: &str = "stored_compressed.bin";
    const FULL: usize = 200_000;
    const CUT: u64 = 100_003;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    /// Compressible, but every position is distinguishable from its
    /// neighbours a few bytes away, so a wrong cut shows.
    fn body() -> Vec<u8> {
        (0..FULL).map(|i| ((i / 7) % 251) as u8).collect()
    }

    /// Build an image and let macOS write the file into it, compressed.
    fn build_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-decmpfs-truncate.dmg");
        let src = "/tmp/apfs-decmpfs-truncate-src";
        let _ = std::fs::remove_dir_all(src);
        std::fs::create_dir_all(src).expect("mkdir src");
        std::fs::write(format!("{src}/{NAME}"), body()).expect("write source");

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
                "DECMPT",
                "/tmp/apfs-decmpfs-truncate",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-decmpfs-truncate.dmg", IMG).expect("rename");

        let (mount, parent) = attach(true);
        let st = Command::new("ditto")
            .args(["--hfsCompression", src, &mount])
            .status()
            .expect("ditto");
        assert!(st.success(), "ditto --hfsCompression failed");
        let flags = Command::new("stat")
            .args(["-f", "%Sf", &format!("{mount}/{NAME}")])
            .output()
            .expect("stat");
        assert!(
            String::from_utf8_lossy(&flags.stdout).contains("compressed"),
            "macOS left the sample uncompressed - the test would be vacuous"
        );
        let _ = Command::new("sync").status();
        detach(&parent);
    }

    /// Attach the image; mount it when `mount_it`. Returns (mount point, whole-disk node).
    fn attach(mount_it: bool) -> (String, String) {
        let mut args = vec![
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nobrowse",
        ];
        if !mount_it {
            args.push("-nomount");
        }
        args.push(IMG);
        let out = Command::new("hdiutil")
            .args(&args)
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .unwrap_or_default();
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        (mount, parent)
    }

    fn detach(parent: &str) {
        let _ = Command::new("hdiutil").args(["detach", parent]).status();
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

    fn run_fsck() {
        let (_, parent) = attach(false);
        let apfs = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &apfs]).output();
        detach(&parent);
        let fsck = fsck.expect("fsck_apfs spawn");
        let report = String::from_utf8_lossy(&fsck.stdout).into_owned()
            + &String::from_utf8_lossy(&fsck.stderr);
        assert!(
            fsck.status.success(),
            "fsck_apfs reported corruption:\n{report}"
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
            "fsck_apfs flagged inconsistencies:\n{}",
            complaints.join("\n")
        );
    }

    #[test]
    #[ignore = "fsck-gated; needs macOS to compress the sample"]
    fn a_compressed_file_can_be_cut_to_a_shorter_plain_one() {
        if !gate() {
            println!("decmpfs-truncate: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();
        let body = body();
        let path = format!("/{NAME}");

        // The reader must see the whole content through the compression,
        // otherwise the rest of this test would be measuring the reader.
        {
            let mut fs = open_fs();
            let got = fs.read(&path, 0, FULL).expect("read compressed");
            assert_eq!(got, body, "the compressed sample must read back whole");
        }

        // The metadata-only path has nothing to resize: it must hand the
        // operation on rather than refuse it, and must never claim to have
        // applied a cut nobody can see.
        let outcome = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let o = truncate_file_fast(&mut txn, &vsb, &om, 2, NAME, CUT, FULL as u64);
            let _ = txn.abort();
            o
        };
        match outcome {
            Ok(TruncateOutcome::FastPathDeclined) => {}
            Ok(TruncateOutcome::Applied) => panic!(
                "the fast path claimed to cut a compressed file, whose content is not in \
                 the stream it resized"
            ),
            Err(e) => panic!("the fast path must decline a compressed file, not refuse: {e}"),
        }

        // What the caller does on a decline: read the content back, cut it,
        // write it as a plain file.
        {
            let mut fs = open_fs();
            let current = fs.read(&path, 0, FULL).expect("read for the cut");
            drop(fs);
            let mut cut = current;
            cut.resize(CUT as usize, 0);
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            overwrite_existing_file(&mut txn, &vsb, &om, 2, NAME, &cut).expect("rewrite cut");
            txn.commit().expect("commit cut");
        }
        run_fsck();

        let expected = &body[..CUT as usize];
        {
            let mut fs = open_fs();
            let attr = fs
                .getattr(&path)
                .expect("getattr")
                .expect("file must exist");
            assert_eq!(attr.size, CUT, "the reported size must be the cut");
            assert_eq!(
                attr.bsd_flags & 0x20,
                0,
                "UF_COMPRESSED must be gone: {:#x}",
                attr.bsd_flags
            );
            let got = fs.read(&path, 0, CUT as usize).expect("read");
            assert_eq!(got, expected, "the surviving bytes must be the first {CUT}");
            let names = fs.list_xattrs(&path).expect("list xattrs");
            assert!(
                !names.iter().any(|n| n.contains("decmpfs")),
                "the decmpfs xattr must be gone: {names:?}"
            );
        }

        // macOS is the judge: a plain file holding exactly the kept prefix.
        let (mount, parent) = attach(true);
        let flags = Command::new("stat")
            .args(["-f", "%Sf", &format!("{mount}/{NAME}")])
            .output()
            .expect("stat");
        let flags = String::from_utf8_lossy(&flags.stdout).into_owned();
        let seen = std::fs::read(format!("{mount}/{NAME}"));
        detach(&parent);
        assert!(
            !flags.contains("compressed"),
            "macOS must no longer call the file compressed, got {flags:?}"
        );
        assert_eq!(
            seen.expect("macOS read"),
            expected,
            "macOS must read the kept prefix"
        );
        println!("decmpfs-truncate PASS: a compressed file was cut to a plain {CUT}-byte one");
    }
}
