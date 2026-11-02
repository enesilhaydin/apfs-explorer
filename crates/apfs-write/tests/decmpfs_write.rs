//! Writing to a file macOS compressed.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + ditto).
//!
//! A compressed file keeps its content in the `com.apple.decmpfs` xattr and,
//! past a few kilobytes, in the resource fork; its data stream is empty. Write
//! the stream and leave those in place and every reader keeps returning the old
//! content. The file has to stop being compressed instead.
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
mod decmpfs_write {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::overwrite_existing_file;
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-decmpfs-write.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// Compresses well enough that macOS stores it as decmpfs, and is big
    /// enough that it does not fit inline and lands in the resource fork.
    const NAME: &str = "compressed.txt";

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn body() -> Vec<u8> {
        b"A".repeat(200_000)
    }

    /// Build an image and let macOS write the file into it, compressed.
    fn build_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-decmpfs-write.dmg");
        let src = "/tmp/apfs-decmpfs-write-src";
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
                "DECMPW",
                "/tmp/apfs-decmpfs-write",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-decmpfs-write.dmg", IMG).expect("rename");

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
    fn writing_a_compressed_file_leaves_it_uncompressed() {
        if !gate() {
            println!("decmpfs-write: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();

        let replacement = b"plain bytes, no longer compressed\n".repeat(300);
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            overwrite_existing_file(&mut txn, &vsb, &om, 2, NAME, &replacement)
                .expect("overwrite a compressed file");
            txn.commit().expect("commit overwrite");
        }
        run_fsck();

        {
            let mut fs = open_fs();
            let path = format!("/{NAME}");
            let attr = fs
                .getattr(&path)
                .expect("getattr")
                .expect("file must exist");
            assert_eq!(
                attr.size as usize,
                replacement.len(),
                "the reported size must be the new content's"
            );
            assert_eq!(
                attr.bsd_flags & 0x20,
                0,
                "UF_COMPRESSED must be gone: {:#x}",
                attr.bsd_flags
            );
            let got = fs.read(&path, 0, replacement.len()).expect("read");
            assert_eq!(got, replacement, "the new content must be what reads back");
            let names = fs.list_xattrs(&path).expect("list xattrs");
            assert!(
                !names.iter().any(|n| n.contains("decmpfs")),
                "the decmpfs xattr must be gone: {names:?}"
            );
        }

        // macOS is the judge: it must see a plain file with the new content.
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
            replacement,
            "macOS must read the new content"
        );
        println!("decmpfs-write PASS: writing a compressed file decompresses it");
    }
}
