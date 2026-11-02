//! Name resolution on a case-SENSITIVE APFS volume.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + a mounted volume).
//!
//! A case-sensitive volume (`APFSX`, `diskutil` personality
//! "Case-sensitive APFS") sets NORMALIZATION_INSENSITIVE (0x8) but leaves
//! CASE_INSENSITIVE (0x1) clear. Its directory keys are still hashed, so the
//! reader must normalize, but folding case as well makes `README` and `readme`
//! resolve to the same entry - the caller gets whichever the scan reached
//! first.
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
mod case_sensitive {
    use apfs_core::block_device::FileBlockDeviceRw;
    use apfs_core::block_device::WritableOffsetBlockDevice;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-case-sensitive.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    /// Build a case-sensitive APFS image and let macOS write two files whose
    /// names differ only in case - something only this volume kind permits.
    fn build_case_sensitive_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-case-sensitive.dmg");
        let out = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "64m",
                "-layout",
                "GPTSPUD",
                "-fs",
                "Case-sensitive APFS",
                "-volname",
                "CSVOL",
                "/tmp/apfs-case-sensitive",
            ])
            .output()
            .expect("hdiutil create");
        assert!(
            out.status.success(),
            "hdiutil create failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::rename("/tmp/apfs-case-sensitive.dmg", IMG).expect("rename dmg -> img");

        // Attach read-write and populate through macOS itself.
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
            .expect("no /Volumes mount point");
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();

        std::fs::write(format!("{mount}/README"), b"UPPER").expect("write README");
        std::fs::write(format!("{mount}/readme"), b"lower").expect("write readme");

        let _ = Command::new("sync").status();
        let det = Command::new("hdiutil")
            .args(["detach", &parent])
            .status()
            .expect("hdiutil detach");
        assert!(det.success(), "hdiutil detach failed");
    }

    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
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
                let s = u64::from_le_bytes(entry[32..40].try_into().unwrap());
                let e = u64::from_le_bytes(entry[40..48].try_into().unwrap());
                return (s * SECTOR, (e - s + 1) * SECTOR);
            }
        }
        panic!("no APFS partition in GPT");
    }

    #[test]
    fn distinct_case_names_resolve_distinctly() {
        if !check_gate() {
            println!("case-sensitive: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_case_sensitive_image();

        let raw = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut fs = apfs::FsView::open(dev).expect("FsView::open");

        let upper = fs.read("/README", 0, 16).expect("read /README");
        let lower = fs.read("/readme", 0, 16).expect("read /readme");

        assert_eq!(
            upper, b"UPPER",
            "/README must resolve to the file macOS created under that exact name"
        );
        assert_eq!(
            lower, b"lower",
            "/readme must resolve to its own file, not to /README - a case-sensitive \
             volume keeps the two apart"
        );

        // A name that differs only in case from neither must not match either.
        assert!(
            fs.lookup("/ReadMe").expect("lookup").is_none(),
            "a case variant that was never created must not resolve on a \
             case-sensitive volume"
        );

        println!("case-sensitive PASS: names differing only in case stay distinct");
    }
}
