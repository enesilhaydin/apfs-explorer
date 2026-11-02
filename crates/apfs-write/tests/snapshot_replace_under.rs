//! Replacing and removing files under a snapshot, then deleting the snapshot.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! A snapshot pins the blocks of every file it holds. Replacing a file's
//! content or removing the file afterwards records that the live volume no
//! longer references those blocks, but they stay allocated for the snapshot.
//! Deleting the snapshot is when they become free: the pinned reference and
//! the live volume's withdrawal cancel, the record goes, the blocks are
//! released, and the volume's block count drops by exactly their number.
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
mod snapshot_replace_under {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{clone_file, overwrite_existing_file, unlink, write_file, write_range};
    use apfs_write::snapshot::{create_snapshot, delete_snapshot};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-snapshot-replace-under.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

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
                "64",
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
    fn with_txn<T>(
        f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<T, apfs_write::txn::TxnError>,
    ) -> T {
        let (vsb, om) = read_vsb_omap();
        let mut txn = open_txn();
        let r = f(&mut txn, &vsb, &om).expect("operation");
        txn.commit().expect("commit");
        r
    }

    fn run_fsck(label: &str) {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                "-nomount",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let parent = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        let rdev = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        let _ = Command::new("hdiutil").args(["detach", &parent]).status();
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

    fn body(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(seed)).collect()
    }

    /// Two files (and, when asked, a clone of the second) exist before the
    /// snapshot; `mutate` runs under it; the snapshot is then deleted.
    fn scenario(label: &str, with_clone: bool, mutate: impl FnOnce()) {
        gen_scratch();
        with_txn(|t, v, o| write_file(t, v, o, 2, "replaced.bin", &body(20 * 1024, 3)));
        with_txn(|t, v, o| write_file(t, v, o, 2, "removed.bin", &body(5000, 5)));
        if with_clone {
            with_txn(|t, v, o| clone_file(t, v, o, 2, "removed.bin", 2, "kept_copy.bin"));
        }
        let snap = with_txn(|t, v, o| create_snapshot(t, v, o, "under"));
        run_fsck(&format!("{label}: snapshot taken"));
        mutate();
        run_fsck(&format!("{label}: mutated under the snapshot"));
        with_txn(|t, v, o| delete_snapshot(t, v, o, snap));
        run_fsck(&format!("{label}: snapshot deleted"));
        let mut fs = open_fs();
        if with_clone {
            let got = fs.read("/kept_copy.bin", 0, 5000).expect("read copy");
            assert_eq!(
                got,
                body(5000, 5),
                "{label}: the surviving copy must read whole"
            );
        }
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn replacing_a_file_under_a_snapshot() {
        if !gate() {
            return;
        }
        scenario("replace", false, || {
            with_txn(|t, v, o| {
                overwrite_existing_file(t, v, o, 2, "replaced.bin", &body(40 * 1024, 7))
            });
        });
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn removing_a_file_under_a_snapshot() {
        if !gate() {
            return;
        }
        scenario("remove", false, || {
            with_txn(|t, v, o| unlink(t, v, o, 2, "removed.bin"));
        });
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn removing_one_side_of_a_clone_under_a_snapshot() {
        if !gate() {
            return;
        }
        scenario("remove clone side", true, || {
            with_txn(|t, v, o| unlink(t, v, o, 2, "removed.bin"));
        });
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn replacing_and_removing_under_a_snapshot() {
        if !gate() {
            return;
        }
        scenario("both", true, || {
            with_txn(|t, v, o| {
                overwrite_existing_file(t, v, o, 2, "replaced.bin", &body(40 * 1024, 7))
            });
            with_txn(|t, v, o| unlink(t, v, o, 2, "removed.bin"));
        });
    }

    /// What Windows does for `WriteAllBytes` on an existing file: the open
    /// truncates it to nothing, the write then gives it its new content.
    #[test]
    #[ignore = "scratch-image gated"]
    fn truncating_then_rewriting_under_a_snapshot() {
        if !gate() {
            return;
        }
        scenario("truncate, rewrite", false, || {
            with_txn(|t, v, o| overwrite_existing_file(t, v, o, 2, "replaced.bin", &[]));
            with_txn(|t, v, o| overwrite_existing_file(t, v, o, 2, "replaced.bin", &[7u8; 200]));
        });
    }

    /// The same two steps in one transaction, as a mount's group commit does.
    #[test]
    #[ignore = "scratch-image gated"]
    fn truncating_then_rewriting_in_one_transaction_under_a_snapshot() {
        if !gate() {
            return;
        }
        scenario("truncate, rewrite, one txn", false, || {
            with_txn(|t, v, o| {
                overwrite_existing_file(t, v, o, 2, "replaced.bin", &[])?;
                overwrite_existing_file(t, v, o, 2, "replaced.bin", &[7u8; 200])
            });
        });
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn writing_inside_a_file_under_a_snapshot() {
        if !gate() {
            return;
        }
        scenario("write inside", false, || {
            with_txn(|t, v, o| {
                write_range(t, v, o, 2, "replaced.bin", 4096, &body(4096, 9)).map(|_| ())
            });
        });
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn appending_to_a_file_under_a_snapshot() {
        if !gate() {
            return;
        }
        scenario("append", false, || {
            with_txn(|t, v, o| {
                write_range(t, v, o, 2, "replaced.bin", 20 * 1024, &body(8192, 11)).map(|_| ())
            });
        });
    }
}
