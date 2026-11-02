//! Finding a macOS-written name whose fold expands.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + a mounted volume).
//!
//! macOS applies FULL Unicode case folding on a case-insensitive volume, which
//! may expand one scalar into several: a file created as `straße` also answers
//! to `STRASSE`, and one created as `ﬁle` answers to `file`. A reader folding
//! with the simple lowercase mapping cannot find either.
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
mod full_fold {
    use apfs_core::block_device::{FileBlockDeviceRw, WritableOffsetBlockDevice};
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-full-fold.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    /// Case-insensitive image whose names macOS itself wrote.
    fn build_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-full-fold.dmg");
        let out = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "32m",
                "-layout",
                "GPTSPUD",
                "-fs",
                "APFS",
                "-volname",
                "FOLDVOL",
                "/tmp/apfs-full-fold",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-full-fold.dmg", IMG).expect("rename");

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
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");

        std::fs::write(format!("{mount}/stra\u{df}e"), b"sharp-s").expect("write sharp-s name");
        std::fs::write(format!("{mount}/\u{fb01}le"), b"ligature").expect("write ligature name");
        std::fs::write(format!("{mount}/Normal.txt"), b"plain").expect("write plain name");
        std::fs::write(format!("{mount}/cafe\u{301}"), b"decomposed").expect("write nfd name");
        let _ = Command::new("sync").status();
        assert!(
            Command::new("hdiutil")
                .args(["detach", &parent])
                .status()
                .expect("detach")
                .success(),
            "hdiutil detach failed"
        );
    }

    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let hdr = &raw[512..1024];
        assert_eq!(&hdr[..8], b"EFI PART");
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

    #[test]
    fn macos_written_expanding_names_are_found() {
        if !check_gate() {
            println!("full-fold: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();

        let raw = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut fs = apfs::FsView::open(dev).expect("FsView::open");

        // Exact name, as stored.
        assert_eq!(
            fs.read("/stra\u{df}e", 0, 16).expect("read exact"),
            b"sharp-s",
            "the name exactly as macOS stored it must resolve"
        );

        // The forms macOS resolves to the same file must resolve here too.
        assert_eq!(
            fs.read("/STRASSE", 0, 16).expect("read folded"),
            b"sharp-s",
            "macOS folds sharp-s to ss, so /STRASSE is the same file"
        );
        assert_eq!(
            fs.read("/strasse", 0, 16).expect("read folded lower"),
            b"sharp-s"
        );
        assert_eq!(
            fs.read("/file", 0, 16).expect("read ligature folded"),
            b"ligature",
            "macOS folds the fi ligature to its letters, so /file is the same file"
        );

        // Ordinary case-insensitive matching must still work.
        assert_eq!(fs.read("/NORMAL.TXT", 0, 16).expect("read plain"), b"plain");

        // A name that shares no fold must still miss.
        assert!(
            fs.lookup("/strase").expect("lookup").is_none(),
            "an unrelated name must not resolve"
        );

        // `entry` hands back the name as stored, found by the name's hash
        // while the directory is not listed and from the listing once it is.
        let stored = |fs: &mut apfs::FsView<_>, path: &str| {
            fs.entry(path).expect("entry").map(|(dir, e)| (dir, e.name))
        };
        let root = apfs_core::jkey::ROOT_DIR_INO_NUM;
        let by_hash = [
            stored(&mut fs, "/STRASSE"),
            stored(&mut fs, "/CAF\u{c9}"),
            stored(&mut fs, "/nothing"),
        ];
        fs.read_dir("/").expect("read_dir");
        let by_listing = [
            stored(&mut fs, "/STRASSE"),
            stored(&mut fs, "/CAF\u{c9}"),
            stored(&mut fs, "/nothing"),
        ];
        let want = [
            Some((root, "stra\u{df}e".to_string())),
            Some((root, "cafe\u{301}".to_string())),
            None,
        ];
        assert_eq!(by_hash, want, "found by the name's hash");
        assert_eq!(by_listing, want, "found in the directory listing");
        assert!(fs.entry("/missing/STRASSE").expect("entry").is_none());

        println!("full-fold PASS: expanding folds resolve the same file macOS resolves");
    }
}
