//! Reading through a device that only takes sector-aligned requests.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, ditto).
//!
//! A raw disk node (macOS `/dev/rdiskN`, Windows `\\.\PhysicalDriveN`)
//! refuses a read whose offset or length is not a whole number of sectors.
//! Every read the reader makes has to be aligned, including the tail of a
//! file whose size is not a multiple of the block size and the pieces of a
//! compressed file's resource fork.
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
mod aligned {
    use apfs_core::block_device::{
        BlockDevice, BlockError, FileBlockDeviceRw, WritableOffsetBlockDevice,
    };
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-aligned-reads.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    /// Refuses what a raw disk node refuses.
    struct SectorsOnly(WritableOffsetBlockDevice<FileBlockDeviceRw>);

    impl BlockDevice for SectorsOnly {
        fn size(&self) -> u64 {
            self.0.size()
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            if offset % SECTOR != 0 || buf.len() as u64 % SECTOR != 0 {
                return Err(BlockError::Io(std::io::Error::from_raw_os_error(22)));
            }
            self.0.read_at(offset, buf)
        }
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
        let out = Command::new("hdiutil").args(&args).output().unwrap();
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
            .unwrap();
        (mount, parent)
    }

    fn open() -> SectorsOnly {
        let mut head = vec![0u8; 64 * 1024];
        std::io::Read::read_exact(&mut std::fs::File::open(IMG).unwrap(), &mut head).unwrap();
        let hdr = &head[512..1024];
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let e = &head[base + i * sz..base + (i + 1) * sz];
            if e[..16] == APFS_GPT_TYPE {
                let s = u64::from_le_bytes(e[32..40].try_into().unwrap());
                let en = u64::from_le_bytes(e[40..48].try_into().unwrap());
                let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
                return SectorsOnly(WritableOffsetBlockDevice::new(
                    inner,
                    s * SECTOR,
                    (en - s + 1) * SECTOR,
                ));
            }
        }
        panic!("no APFS partition");
    }

    #[test]
    fn every_read_is_whole_sectors() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            println!("aligned_reads: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-aligned-reads.dmg");
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
                "ALIGNED",
                "/tmp/apfs-aligned-reads",
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "hdiutil create");
        std::fs::rename("/tmp/apfs-aligned-reads.dmg", IMG).unwrap();
        let (mount, parent) = attach(true);
        let text: Vec<u8> = (0..300_000).map(|i| b"compress me "[i % 12]).collect();
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("one.bin", vec![7]),
            ("odd.bin", (0..5000).map(|i| (i % 251) as u8).collect()),
            ("text.txt", text.clone()),
        ];
        for (name, body) in &files {
            std::fs::write(format!("{mount}/{name}"), body).unwrap();
        }
        let st = Command::new("ditto")
            .args([
                "--hfsCompression",
                &format!("{mount}/text.txt"),
                &format!("{mount}/comp.txt"),
            ])
            .status()
            .unwrap();
        assert!(st.success(), "ditto --hfsCompression");
        let _ = Command::new("sync").status();
        let _ = Command::new("hdiutil").args(["detach", &parent]).status();

        let mut fs = apfs::FsView::open(open()).expect("FsView::open");
        let mut all = files.clone();
        all.push(("comp.txt", text));
        for (name, want) in &all {
            let path = format!("/{name}");
            let got = fs.read(&path, 0, want.len() + 100).expect("read");
            assert!(got == *want, "{name} reads differently");
            // A range starting and ending inside blocks.
            if want.len() > 4200 {
                let got = fs.read(&path, 4099, 101).expect("ranged read");
                assert_eq!(got, want[4099..4200], "{name} range");
            }
        }
        let _ = std::fs::remove_file(IMG);
    }
}
