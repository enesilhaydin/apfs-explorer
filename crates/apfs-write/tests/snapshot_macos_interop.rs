//! Handing a volume with a snapshot back to macOS.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil).
//!
//! Writing under a snapshot copies metadata aside and records the old blocks
//! for later release. macOS reads those records too, and acts on them the
//! moment it writes anything. If they say a block is free while the snapshot's
//! frozen trees still point at it, macOS hands that block to something else and
//! the snapshot's copy of the file is gone - with no error anywhere.
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
mod snapshot_macos_interop {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::write_file;
    use apfs_write::snapshot::create_snapshot;
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-snap-interop.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const BEFORE: &str = "before_snapshot.bin";
    const UNDER: &str = "under_snapshot.bin";

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn root_dir() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }
    fn gen_scratch() {
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
            .current_dir(root_dir())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
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

    fn run_fsck(label: &str) {
        let (_, parent) = attach(false);
        let rdev = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        detach(&parent);
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
    #[ignore = "fsck-gated; mounts the image read-write on macOS"]
    fn macos_writing_after_our_snapshot_keeps_it_intact() {
        if !gate() {
            println!("snap-interop: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let before = vec![0xC1u8; 48 * 1024];
        let under = vec![0xC2u8; 48 * 1024];
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, BEFORE, &before).expect("write before");
            txn.commit().expect("commit before");
        }
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            create_snapshot(&mut txn, &vsb, &om, "interop").expect("snapshot");
            txn.commit().expect("commit snapshot");
        }
        // The write under the snapshot is what fills the free queue with blocks
        // the snapshot still needs.
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, UNDER, &under).expect("write under");
            txn.commit().expect("commit under");
        }
        run_fsck("before handing over");

        // macOS mounts it and writes. Anything it does with those records, it
        // does now.
        {
            let (mount, parent) = attach(true);
            assert!(!mount.is_empty(), "the volume must mount on macOS");
            std::fs::write(format!("{mount}/macos_wrote.txt"), b"macOS was here\n")
                .expect("macOS write");
            let _ = Command::new("sync").status();
            detach(&parent);
        }
        run_fsck("after macOS wrote");

        // Everything must still be there, ours and its.
        {
            let mut fs = open_fs();
            let got = fs
                .read(&format!("/{BEFORE}"), 0, before.len())
                .expect("read before");
            assert_eq!(got, before, "the pre-snapshot file must be intact");
            let got = fs
                .read(&format!("/{UNDER}"), 0, under.len())
                .expect("read under");
            assert_eq!(
                got, under,
                "the file written under the snapshot must be intact"
            );
            assert!(
                fs.lookup("/macos_wrote.txt").expect("lookup").is_some(),
                "macOS's own file must be there"
            );
        }

        // And macOS must still list the snapshot it was handed.
        // Ask through the mount point: the volume lives on the synthesized
        // disk the container produces, not on the image's own partition.
        let (mount, parent) = attach(true);
        let list = Command::new("diskutil")
            .args(["apfs", "listSnapshots", &mount])
            .output()
            .expect("diskutil listSnapshots");
        detach(&parent);
        let listed = String::from_utf8_lossy(&list.stdout).into_owned()
            + &String::from_utf8_lossy(&list.stderr);
        assert!(
            listed.contains("interop"),
            "macOS must still see the snapshot, got:\n{listed}"
        );
        println!("snap-interop PASS: macOS wrote to the volume and the snapshot survived");
    }
}
