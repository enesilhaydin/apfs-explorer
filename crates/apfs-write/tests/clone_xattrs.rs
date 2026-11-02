//! Cloning a file that keeps something in an extended attribute.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, xattr, ditto).
//!
//! A clone shares the source's blocks instead of copying them, which says
//! nothing about the source's extended attributes - and a compressed file keeps
//! its whole content in one of them, so a clone that drops them is not a clone
//! of anything.
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
mod clone_xattrs {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{clone_file, delete_xattr, set_xattr};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-clone-xattr.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const PLAIN: &str = "plain.txt";
    const PLAIN_BODY: &[u8] = b"a file with an attribute\n";
    const XATTR_NAME: &str = "user.note";
    const XATTR_VALUE: &str = "keep me";
    /// An attribute too large for its record: macOS stores it in a stream,
    /// and a clone shares that stream's blocks.
    const BIG_NAME: &str = "user.big";
    const BIG_LEN: usize = 10_000;

    fn big_value() -> String {
        (0..BIG_LEN)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect()
    }
    /// 32 bytes of Finder info: type `TEXT`, creator `ttxt`, the rest zero.
    const FINDER_INFO_HEX: &str =
        "54455854747478740000000000000000 00000000000000000000000000000000";
    /// Compresses well, and is big enough that macOS spills it into the
    /// resource fork rather than keeping it inside the decmpfs attribute.
    const COMPRESSED: &str = "compressed.txt";

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn build_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-clone-xattr.dmg");
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
                "CLONEX",
                "/tmp/apfs-clone-xattr",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-clone-xattr.dmg", IMG).expect("rename");

        let (mount, parent) = attach(true);
        std::fs::write(format!("{mount}/{PLAIN}"), PLAIN_BODY).expect("write plain");
        let st = Command::new("xattr")
            .args(["-w", XATTR_NAME, XATTR_VALUE, &format!("{mount}/{PLAIN}")])
            .status()
            .expect("xattr -w");
        assert!(st.success(), "xattr -w failed");
        // Finder info: macOS marks the inode as carrying it, and the checker
        // wants the clone marked the same way when it copies the attribute.
        // The system tool: a Python `xattr` earlier on PATH has no -x.
        let st = Command::new("/usr/bin/xattr")
            .args([
                "-wx",
                "com.apple.FinderInfo",
                FINDER_INFO_HEX,
                &format!("{mount}/{PLAIN}"),
            ])
            .status()
            .expect("xattr -wx");
        assert!(st.success(), "xattr -wx com.apple.FinderInfo failed");
        let st = Command::new("/usr/bin/xattr")
            .args(["-w", BIG_NAME, &big_value(), &format!("{mount}/{PLAIN}")])
            .status()
            .expect("xattr -w big");
        assert!(st.success(), "xattr -w {BIG_NAME} failed");
        // A compressed file: its whole content lives in an attribute, and past
        // a few kilobytes in a stream behind one.
        let src = "/tmp/apfs-clone-xattr-src";
        let _ = std::fs::remove_dir_all(src);
        std::fs::create_dir_all(src).expect("mkdir src");
        std::fs::write(format!("{src}/{COMPRESSED}"), compressed_body()).expect("write source");
        let st = Command::new("ditto")
            .args(["--hfsCompression", src, &mount])
            .status()
            .expect("ditto");
        assert!(st.success(), "ditto --hfsCompression failed");
        let flags = Command::new("stat")
            .args(["-f", "%Sf", &format!("{mount}/{COMPRESSED}")])
            .output()
            .expect("stat");
        assert!(
            String::from_utf8_lossy(&flags.stdout).contains("compressed"),
            "macOS left the sample uncompressed - half the test would be vacuous"
        );
        let _ = Command::new("sync").status();
        detach(&parent);
    }

    fn compressed_body() -> Vec<u8> {
        b"A".repeat(200_000)
    }

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
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
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
    #[ignore = "fsck-gated; needs macOS to author the attribute"]
    fn a_clone_carries_the_extended_attributes() {
        if !gate() {
            println!("clone-xattr: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();

        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            clone_file(&mut txn, &vsb, &om, 2, PLAIN, 2, "clone.txt").expect("clone");
            txn.commit().expect("commit clone");
        }
        run_fsck();

        {
            let mut fs = open_fs();
            let got = fs.read("/clone.txt", 0, PLAIN_BODY.len()).expect("read");
            assert_eq!(got, PLAIN_BODY, "the clone must have the same content");
            let names = fs.list_xattrs("/clone.txt").expect("list xattrs");
            assert!(
                names.iter().any(|n| n == XATTR_NAME),
                "the clone must carry the source's attributes, got {names:?}"
            );
            let value = fs
                .get_xattr("/clone.txt", XATTR_NAME)
                .expect("get xattr")
                .expect("the attribute must be readable");
            assert_eq!(
                value,
                XATTR_VALUE.as_bytes(),
                "the attribute's value must come across too"
            );
        }

        // macOS is the judge.
        let (mount, parent) = attach(true);
        let listed = Command::new("xattr")
            .arg(format!("{mount}/clone.txt"))
            .output()
            .expect("xattr");
        let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
        detach(&parent);
        assert!(
            listed.contains(XATTR_NAME),
            "macOS must list the attribute on the clone, got {listed:?}"
        );
        println!("clone-xattr PASS: a clone carries the source's attributes");
    }

    /// A clone shares the blocks of an attribute stream with its source.
    /// Removing the attribute from one side, or replacing it, lets go of that
    /// side's reference only: the blocks stay for the other side.
    #[test]
    #[ignore = "fsck-gated; needs macOS to author the attribute"]
    fn changing_a_shared_attribute_stream_on_one_side_keeps_the_other() {
        if !gate() {
            println!("clone-xattr: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();
        let big = big_value().into_bytes();
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            clone_file(&mut txn, &vsb, &om, 2, PLAIN, 2, "clone.txt").expect("clone");
            txn.commit().expect("commit clone");
        }
        run_fsck();
        let ino = |path: &str| open_fs().lookup(path).expect("lookup").expect("file");

        // The clone drops the attribute; the source keeps it whole.
        let clone_ino = ino("/clone.txt");
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            delete_xattr(&mut txn, &vsb, &om, clone_ino, BIG_NAME).expect("delete");
            txn.commit().expect("commit delete");
        }
        run_fsck();
        let got = open_fs()
            .get_xattr(&format!("/{PLAIN}"), BIG_NAME)
            .expect("get")
            .expect("the source keeps its attribute");
        assert!(got == big, "the source's attribute must read back whole");

        // A second clone shares the stream again; replacing the source's value
        // leaves the second clone the old one.
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            clone_file(&mut txn, &vsb, &om, 2, PLAIN, 2, "clone2.txt").expect("clone");
            txn.commit().expect("commit clone");
        }
        let fresh: Vec<u8> = (0..9_000).map(|i| b'0' + (i % 10) as u8).collect();
        let src_ino = ino(&format!("/{PLAIN}"));
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            set_xattr(&mut txn, &vsb, &om, src_ino, BIG_NAME, &fresh).expect("set");
            txn.commit().expect("commit set");
        }
        run_fsck();
        let mut fs = open_fs();
        let got = fs
            .get_xattr("/clone2.txt", BIG_NAME)
            .expect("get")
            .expect("the second clone keeps its attribute");
        assert!(got == big, "the second clone keeps the old value");
        let got = fs
            .get_xattr(&format!("/{PLAIN}"), BIG_NAME)
            .expect("get")
            .expect("present");
        assert!(got == fresh, "the source has the new value");
        drop(fs);

        // macOS is the judge.
        let (mount, parent) = attach(true);
        let read = |path: &str| {
            let out = Command::new("/usr/bin/xattr")
                .args(["-p", BIG_NAME, &format!("{mount}/{path}")])
                .output()
                .expect("xattr -p");
            String::from_utf8_lossy(&out.stdout).trim_end().to_string()
        };
        let (on_clone2, on_source) = (read("clone2.txt"), read(PLAIN));
        detach(&parent);
        assert_eq!(
            on_clone2,
            big_value(),
            "macOS reads the old value on the clone"
        );
        assert_eq!(
            on_source.as_bytes(),
            fresh.as_slice(),
            "macOS reads the new value on the source"
        );
    }

    /// The hard half: a compressed file keeps its content in an attribute whose
    /// value has spilled into a stream of its own. The clone needs a stream of
    /// its own too, pointing at the same blocks.
    #[test]
    #[ignore = "fsck-gated; needs macOS to compress the sample"]
    fn a_clone_of_a_compressed_file_has_its_content() {
        if !gate() {
            println!("clone-xattr: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();

        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            clone_file(
                &mut txn,
                &vsb,
                &om,
                2,
                COMPRESSED,
                2,
                "compressed-clone.txt",
            )
            .expect("clone a compressed file");
            txn.commit().expect("commit clone");
        }
        run_fsck();

        let expected = compressed_body();
        {
            let mut fs = open_fs();
            let attr = fs
                .getattr("/compressed-clone.txt")
                .expect("getattr")
                .expect("the clone must exist");
            assert_eq!(
                attr.size as usize,
                expected.len(),
                "the clone's size must be the decompressed length"
            );
            let got = fs
                .read("/compressed-clone.txt", 0, expected.len())
                .expect("read");
            assert!(got == expected, "the clone must have the same content");
        }

        let (mount, parent) = attach(true);
        let seen = std::fs::read(format!("{mount}/compressed-clone.txt"));
        detach(&parent);
        assert!(
            seen.expect("macOS read") == expected,
            "macOS must read the clone's content too"
        );
        println!("clone-xattr PASS: a clone of a compressed file has its content");
    }
}
