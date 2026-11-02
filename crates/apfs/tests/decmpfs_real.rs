//! Reading files macOS itself compressed.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + ditto).
//!
//! `ditto --hfsCompression` stores a file's content in the `com.apple.decmpfs`
//! xattr (and, when it does not fit inline, the resource fork) and marks the
//! inode UF_COMPRESSED. The data stream is then empty, so a reader that does
//! not decode decmpfs returns nothing for a file that plainly has content.
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
mod decmpfs_real {
    use apfs_core::block_device::{FileBlockDeviceRw, WritableOffsetBlockDevice};
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-decmpfs.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    /// Content macOS compresses well enough to actually store as decmpfs.
    fn samples() -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("repetitive.txt", b"A".repeat(200_000)),
            (
                "text.txt",
                "lorem ipsum dolor sit amet ".repeat(40_000).into_bytes(),
            ),
        ]
    }

    /// Build an image and let macOS write the samples into it, compressed.
    fn build_image() -> Vec<(&'static str, Vec<u8>)> {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-decmpfs.dmg");
        let src = "/tmp/apfs-decmpfs-src";
        let _ = std::fs::remove_dir_all(src);
        std::fs::create_dir_all(src).expect("mkdir src");
        let files = samples();
        for (name, data) in &files {
            std::fs::write(format!("{src}/{name}"), data).expect("write source");
        }

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
                "DECMPFS",
                "/tmp/apfs-decmpfs",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-decmpfs.dmg", IMG).expect("rename");

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
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();

        let st = Command::new("ditto")
            .args(["--hfsCompression", src, &mount])
            .status()
            .expect("ditto");
        assert!(st.success(), "ditto --hfsCompression failed");

        // Only assert on the ones macOS actually chose to compress.
        let mut compressed = Vec::new();
        for (name, data) in files {
            let flags = Command::new("stat")
                .args(["-f", "%Sf", &format!("{mount}/{name}")])
                .output()
                .expect("stat");
            if String::from_utf8_lossy(&flags.stdout).contains("compressed") {
                compressed.push((name, data));
            } else {
                println!("decmpfs: macOS left {name} uncompressed - not asserted");
            }
        }

        let _ = Command::new("sync").status();
        assert!(
            Command::new("hdiutil")
                .args(["detach", &parent])
                .status()
                .expect("detach")
                .success(),
            "hdiutil detach failed"
        );
        compressed
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
    fn macos_compressed_files_read_back_intact() {
        if !check_gate() {
            println!("decmpfs: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let expected = build_image();
        assert!(
            !expected.is_empty(),
            "macOS compressed none of the samples - the test would be vacuous"
        );

        let raw = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut fs = apfs::FsView::open(dev).expect("FsView::open");

        for (name, data) in &expected {
            let path = format!("/{name}");
            let attr = fs
                .getattr(&path)
                .expect("getattr")
                .unwrap_or_else(|| panic!("{path} must exist"));
            assert_eq!(
                attr.size as usize,
                data.len(),
                "{path}: reported size must be the DECOMPRESSED length, not the \
                 empty data stream"
            );
            let got = fs.read(&path, 0, data.len()).expect("read");
            assert_eq!(
                got.len(),
                data.len(),
                "{path}: short read of a compressed file"
            );
            assert!(got == *data, "{path}: decompressed content differs");
            println!("decmpfs OK: {name} ({} bytes)", data.len());
        }
        println!("decmpfs PASS: macOS-compressed files read back intact");
    }
}
