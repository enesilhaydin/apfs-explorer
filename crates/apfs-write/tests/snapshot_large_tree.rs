//! Writing under a snapshot on a volume whose trees span many nodes.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch, hdiutil,
//! fsck_apfs).
//!
//! macOS fills the volume with a few thousand files, so the catalog, the
//! object map and the extent-reference tree each grow past one node. A
//! snapshot is taken, and the operations a Windows mount performs run under
//! it, several to a transaction as the mount's group commit does. After
//! every step `fsck_apfs` must find nothing, not even a block-count warning;
//! the snapshot is then deleted and the volume checked once more.
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
mod snapshot_large_tree {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{overwrite_existing_file, rename, unlink, write_file, write_range};
    use apfs_write::snapshot::{create_snapshot, delete_snapshot};
    use apfs_write::txn::{Transaction, TxnError};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-snapshot-large-tree.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const DIRS: usize = 10;
    const FILES_PER_DIR: usize = 300;
    const BIG_LEN: usize = 32 << 20;
    /// Operations per transaction, as a mount groups them before it commits.
    const GROUP: usize = 10;

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
                "256",
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
    fn open_dev() -> Dev {
        let mut head = vec![0u8; 64 * 1024];
        FileBlockDeviceRw::open_rw(IMG)
            .unwrap()
            .read_at(0, &mut head)
            .unwrap();
        let (base, len) = find_apfs_range(&head);
        WritableOffsetBlockDevice::new(FileBlockDeviceRw::open_rw(IMG).unwrap(), base, len)
    }
    fn read_vsb_omap() -> (Vec<u8>, Vec<u8>) {
        let mut dev = open_dev();
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
    fn with_txn<T>(
        f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<T, TxnError>,
    ) -> T {
        let (vsb, om) = read_vsb_omap();
        let mut txn = Transaction::begin(open_dev()).unwrap();
        let r = f(&mut txn, &vsb, &om).expect("operation");
        txn.commit().expect("commit");
        r
    }
    fn ino(path: &str) -> u64 {
        apfs::FsView::open(open_dev())
            .unwrap()
            .lookup(path)
            .unwrap()
            .unwrap_or_else(|| panic!("{path} not found"))
    }

    /// The lowest-numbered whole disk `hdiutil attach` reports: the image's
    /// own node. The synthesized container disk comes after it, but
    /// `hdiutil` can list disk10 before disk9.
    fn whole_disk(out: &str) -> String {
        out.lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node")
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
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .unwrap_or_default();
        (mount, whole_disk(&stdout))
    }
    fn detach(parent: &str) {
        for attempt in 0..8u64 {
            if Command::new("hdiutil")
                .args(["detach", parent])
                .output()
                .unwrap()
                .status
                .success()
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
        }
        panic!("hdiutil detach {parent} failed");
    }
    fn run_fsck(label: &str) {
        let (_, parent) = attach(false);
        let rdev = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        detach(&parent);
        let fsck = fsck.expect("fsck_apfs spawn");
        let report = String::from_utf8_lossy(&fsck.stdout).into_owned()
            + &String::from_utf8_lossy(&fsck.stderr);
        let complaints: Vec<&str> = report
            .lines()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("warning:")
                    || l.contains("error:")
                    || l.contains("overallocation")
                    || l.contains("underallocation")
                    || l.contains("is not valid")
            })
            .collect();
        assert!(
            fsck.status.success() && complaints.is_empty(),
            "{label}: fsck_apfs flagged inconsistencies:\n{}",
            complaints.join("\n")
        );
        eprintln!("fsck clean: {label}");
    }

    fn body(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u8).wrapping_mul(seed) ^ seed)
            .collect()
    }

    /// macOS writes the tree: `DIRS` directories of `FILES_PER_DIR` files of
    /// one to three blocks, and one large file.
    fn populate() {
        let (mount, parent) = attach(true);
        assert!(!mount.is_empty(), "the volume did not mount");
        for d in 0..DIRS {
            let dir = format!("{mount}/d{d:02}");
            std::fs::create_dir(&dir).unwrap();
            for f in 0..FILES_PER_DIR {
                let len = 1000 + (f % 3) * 4096;
                std::fs::write(format!("{dir}/f{f:04}.bin"), body(len, (f % 251) as u8 + 1))
                    .unwrap();
            }
        }
        std::fs::write(format!("{mount}/big.bin"), body(BIG_LEN, 13)).unwrap();
        let _ = Command::new("sync").status();
        detach(&parent);
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn group_commits_under_a_snapshot_on_a_large_tree() {
        if !gate() {
            return;
        }
        gen_scratch();
        populate();
        run_fsck("populated by macOS");
        let snap = with_txn(|t, v, o| create_snapshot(t, v, o, "under"));
        run_fsck("snapshot taken");

        let d03 = ino("/d03");
        for group in (0..FILES_PER_DIR).collect::<Vec<_>>().chunks(GROUP) {
            with_txn(|t, v, o| {
                for f in group {
                    unlink(t, v, o, d03, &format!("f{f:04}.bin"))?;
                }
                Ok(())
            });
        }
        with_txn(|t, v, o| unlink(t, v, o, 2, "d03"));
        run_fsck("a directory removed");

        let d04 = ino("/d04");
        for group in (0..50).collect::<Vec<_>>().chunks(GROUP) {
            with_txn(|t, v, o| {
                for f in group {
                    let name = format!("f{f:04}.bin");
                    overwrite_existing_file(t, v, o, d04, &name, &[])?;
                    overwrite_existing_file(t, v, o, d04, &name, &[7u8; 200])?;
                }
                Ok(())
            });
        }
        run_fsck("files truncated and rewritten");

        let chunk = body(64 * 1024, 5);
        for group in (0..(BIG_LEN >> 20) as u64)
            .collect::<Vec<_>>()
            .chunks(GROUP)
        {
            with_txn(|t, v, o| {
                for k in group {
                    write_range(t, v, o, 2, "big.bin", k << 20, &chunk)?;
                }
                Ok(())
            });
        }
        with_txn(|t, v, o| {
            write_range(t, v, o, 2, "big.bin", BIG_LEN as u64, &body(1 << 20, 9)).map(|_| ())
        });
        run_fsck("a large file edited and extended");

        with_txn(|t, v, o| rename(t, v, o, 2, "d05", "d05-moved", false));
        let d06 = ino("/d06");
        for group in (0..100).collect::<Vec<_>>().chunks(GROUP) {
            with_txn(|t, v, o| {
                for f in group {
                    write_file(t, v, o, d06, &format!("new{f:04}.bin"), &body(5000, 3))?;
                }
                Ok(())
            });
        }
        run_fsck("a directory renamed and files added");

        with_txn(|t, v, o| delete_snapshot(t, v, o, snap));
        run_fsck("snapshot deleted");
    }
}
