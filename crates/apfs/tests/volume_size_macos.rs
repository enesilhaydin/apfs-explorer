//! The volume size and free space we report, against what macOS reports.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, python3).
//!
//! macOS answers statfs from the container: the total is the container's
//! block count, the free space is the space manager's free-block count. A
//! figure derived from the raw device size and one volume's allocation count
//! overstates both, so a Windows copy that trusts the free space runs the
//! volume out of blocks before the number reaches zero.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
    )
)]

// macOS formats and reports the volumes (hdiutil, statvfs); the raw disk is
// read with Unix file calls.
#[cfg(all(test, target_os = "macos"))]
mod volume_size_macos {
    use apfs_core::block_device::{FileBlockDeviceRw, WritableOffsetBlockDevice};
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-volume-size.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
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

    /// Attach the image read-only, so that macOS writes nothing and what it
    /// reports describes exactly the state on disk, and take statfs of the
    /// given volumes.
    fn macos_statfs_read_only(img: &str, mounts: &[&str]) -> Vec<(u64, u64)> {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                "-readonly",
                img,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach -readonly failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        let reported: Vec<(u64, u64)> = mounts.iter().map(|m| macos_statfs(m)).collect();
        let st = Command::new("hdiutil")
            .args(["detach", &parent])
            .status()
            .expect("detach");
        assert!(st.success(), "hdiutil detach failed");
        reported
    }

    /// What macOS reports for the mounted volume: (total bytes, free bytes).
    fn macos_statfs(mount: &str) -> (u64, u64) {
        let out = Command::new("python3")
            .args([
                "-c",
                "import os, sys\n\
                 s = os.statvfs(sys.argv[1])\n\
                 print(s.f_blocks * s.f_frsize, s.f_bavail * s.f_frsize)",
                mount,
            ])
            .output()
            .expect("python3");
        assert!(out.status.success(), "statvfs failed");
        let text = String::from_utf8_lossy(&out.stdout);
        let mut it = text.split_whitespace().map(|v| v.parse::<u64>().unwrap());
        (it.next().unwrap(), it.next().unwrap())
    }

    /// Build an image, let macOS fill part of it, and record what it reports.
    fn build_image() -> (u64, u64) {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-volume-size.dmg");
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
                "VOLSIZE",
                "/tmp/apfs-volume-size",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-volume-size.dmg", IMG).expect("rename");

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

        let filler: Vec<u8> = (0..10 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::write(format!("{mount}/filler.bin"), filler).expect("write filler");
        let _ = Command::new("sync").status();
        let st = Command::new("hdiutil")
            .args(["detach", &parent])
            .status()
            .expect("hdiutil detach");
        assert!(st.success(), "hdiutil detach failed");
        // Unmounting writes a last checkpoint (and leaves frees queued), so
        // the figures are taken from a read-only mount of the final state.
        macos_statfs_read_only(IMG, &[&mount])[0]
    }

    const QUOTA_IMG: &str = "/tmp/apfs-volume-quota.img";

    /// A container with two volumes, the second under a 20 MB quota: macOS
    /// reports the quota as that volume's total and what is left of it as
    /// free, while the first volume sees the whole container.
    fn build_quota_image() -> Vec<(u64, u64)> {
        let _ = std::fs::remove_file(QUOTA_IMG);
        let _ = std::fs::remove_file("/tmp/apfs-volume-quota.dmg");
        let out = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "1g",
                "-layout",
                "GPTSPUD",
                "-fs",
                "APFS",
                "-volname",
                "QBASE",
                "/tmp/apfs-volume-quota",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-volume-quota.dmg", QUOTA_IMG).expect("rename");

        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                QUOTA_IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        let container = stdout
            .lines()
            .find(|l| l.contains("EF57347C"))
            .and_then(|l| l.split_whitespace().next())
            .expect("no container node")
            .to_string();
        let st = Command::new("diskutil")
            .args([
                "apfs",
                "addVolume",
                &container,
                "APFS",
                "QUOTA",
                "-quota",
                "20m",
            ])
            .status()
            .expect("diskutil");
        assert!(st.success(), "diskutil apfs addVolume failed");
        std::fs::write("/Volumes/QUOTA/q.bin", vec![7u8; 5 * 1024 * 1024]).expect("write");
        std::fs::write("/Volumes/QBASE/b.bin", vec![9u8; 3 * 1024 * 1024]).expect("write");
        let _ = Command::new("sync").status();
        let st = Command::new("hdiutil")
            .args(["detach", &parent])
            .status()
            .expect("detach");
        assert!(st.success(), "hdiutil detach failed");
        macos_statfs_read_only(QUOTA_IMG, &["/Volumes/QBASE", "/Volumes/QUOTA"])
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn a_quota_bounds_what_the_volume_reports() {
        if !gate() {
            return;
        }
        let want = build_quota_image();
        let raw = std::fs::read(QUOTA_IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let mut got = Vec::new();
        for index in 0..2 {
            let inner = FileBlockDeviceRw::open_rw(QUOTA_IMG).expect("open rw");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut fs = apfs::FsView::open_selected(dev, apfs::VolumeSelector::Index(index))
                .expect("FsView::open_selected");
            got.push(fs.volume_size().expect("volume_size"));
        }
        println!("macOS reports {want:?}; we report {got:?}");
        assert_eq!(
            got, want,
            "(total, free) per volume must be what macOS reports"
        );
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn total_and_free_space_match_what_macos_reports() {
        if !gate() {
            return;
        }
        let (want_total, want_free) = build_image();
        let raw = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let mut fs = apfs::FsView::open(dev).expect("FsView::open");
        let (total, free) = fs.volume_size().expect("volume_size");
        println!("macOS reports total {want_total} free {want_free}; we report total {total} free {free}");
        assert_eq!(total, want_total, "total bytes must be what macOS reports");
        assert_eq!(free, want_free, "free bytes must be what macOS reports");
    }

    /// A raw disk node, read in whole sectors (a character device refuses
    /// anything else), from the start of the APFS partition.
    struct RawDisk {
        f: std::fs::File,
        base: u64,
        size: u64,
    }

    impl apfs_core::block_device::BlockDevice for RawDisk {
        fn size(&self) -> u64 {
            self.size
        }
        fn read_at(
            &mut self,
            offset: u64,
            buf: &mut [u8],
        ) -> Result<(), apfs_core::block_device::BlockError> {
            use std::os::unix::fs::FileExt;
            let abs = self.base + offset;
            let start = abs / 4096 * 4096;
            let end = (abs + buf.len() as u64).div_ceil(4096) * 4096;
            let mut tmp = vec![0u8; (end - start) as usize];
            self.f.read_exact_at(&mut tmp, start)?;
            let skip = (abs - start) as usize;
            buf.copy_from_slice(&tmp[skip..skip + buf.len()]);
            Ok(())
        }
    }

    const BIG: &str = "/tmp/apfs-volume-4t";

    /// A 4 TB container, as macOS formats it, kept as a sparse image. Its
    /// space manager spans two blocks, checksummed together: read as one,
    /// it failed the checksum, and the mount showed the drive as 1 GB with
    /// 512 MB free (#23).
    #[test]
    #[ignore = "macOS: hdiutil"]
    fn a_4_tb_container_reports_its_size() {
        if !gate() {
            return;
        }
        let img = format!("{BIG}.sparseimage");
        let _ = std::fs::remove_file(&img);
        let out = Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "4t", "-type", "SPARSE"])
            .args([
                "-layout", "GPTSPUD", "-fs", "APFS", "-volname", "BIG4T", BIG,
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");

        // What macOS reports, from a read-only mount.
        let out = Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-readonly", &img])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let parent = |text: &str| {
            text.lines()
                .filter_map(|l| l.split_whitespace().next())
                .filter_map(|d| d.strip_prefix("/dev/disk"))
                .filter(|n| !n.contains('s'))
                .filter_map(|n| n.parse::<u32>().ok())
                .min()
                .expect("no whole-disk node")
        };
        let (want_total, _) = macos_statfs(&mount);
        let n = parent(&stdout);
        assert!(Command::new("hdiutil")
            .args(["detach", &format!("/dev/disk{n}")])
            .status()
            .expect("detach")
            .success());

        // What we report, from the raw disk.
        let out = Command::new("hdiutil")
            .args(["attach", "-nomount", "-readonly", &img])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach -nomount failed");
        let n = parent(&String::from_utf8_lossy(&out.stdout));
        let raw = format!("/dev/rdisk{n}");
        let mut f = std::fs::File::open(&raw).expect("open raw disk");
        let mut head = vec![0u8; 64 * 1024];
        std::io::Read::read_exact(&mut f, &mut head).expect("read GPT");
        let (base, len) = find_apfs_range(&head);
        let result = apfs::FsView::open(RawDisk { f, base, size: len })
            .map_err(|e| format!("{e:?}"))
            .and_then(|mut fs| fs.volume_size().map_err(|e| format!("{e:?}")));
        let _ = Command::new("hdiutil")
            .args(["detach", &format!("/dev/disk{n}")])
            .status();
        let _ = std::fs::remove_file(&img);
        let (total, free) = result.expect("volume_size on a 4 TB container");
        println!("4 TB container: macOS total {want_total}; we report total {total} free {free}");
        assert_eq!(total, want_total, "total bytes must be what macOS reports");
        // An empty volume: all but the metadata is free. (macOS caps the
        // free space of a sparse image at what its host has left, so its
        // own figure is not the one to compare with here.)
        assert!(free > total / 100 * 99, "free {free} of {total}");
    }
}
