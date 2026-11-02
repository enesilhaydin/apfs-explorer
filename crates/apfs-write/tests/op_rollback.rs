//! An operation that fails half way leaves a shared transaction as it was.
//!
//! A host that groups operations into one transaction cannot throw the
//! transaction away when one of them fails: the operations before it were
//! already reported done. A write larger than the free space takes blocks
//! before it runs out; run through `Transaction::atomically`, everything it
//! took and staged goes back, the operations around it land, and the volume
//! checks clean with none of the failed write's blocks still allocated.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]`: runs fsck.
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --features write \
//!     --test op_rollback -- --ignored --nocapture
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
mod rollback {
    use apfs::FsView;
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, overwrite_existing_file, write_range};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-op-rollback.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ROOT_INO: u64 = 2;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;

    fn xtask(args: &[&str]) -> (bool, String) {
        let o = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--"])
            .args(args)
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .output()
            .expect("xtask");
        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status.success(), s)
    }

    fn open_dev() -> Dev {
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
                return WritableOffsetBlockDevice::new(inner, s * SECTOR, (en - s + 1) * SECTOR);
            }
        }
        panic!("no APFS partition");
    }

    fn vsb_omap(dev: &mut Dev) -> (Vec<u8>, Vec<u8>) {
        let c = Container::open(dev).unwrap();
        let nx = &c.superblock;
        let bsz = nx.block_size as usize;
        let omap = Omap::open(dev, nx.omap_oid, nx.block_size).unwrap();
        let fs_oid = *nx.fs_oids.first().unwrap();
        let vp = omap
            .resolve(dev, fs_oid, nx.obj.xid, nx.block_size)
            .unwrap()
            .unwrap();
        let mut vsb = vec![0u8; bsz];
        dev.read_at(vp * bsz as u64, &mut vsb).unwrap();
        let v = VolumeSuperblock::parse(&vsb).unwrap();
        let mut om = vec![0u8; bsz];
        dev.read_at(v.omap_oid * bsz as u64, &mut om).unwrap();
        (vsb, om)
    }

    #[test]
    #[ignore = "fsck-gated"]
    fn a_failed_operation_leaves_the_others_of_its_transaction() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let (ok, out) = xtask(&["gen-scratch", IMG, "64"]);
        assert!(ok, "gen-scratch: {out}");
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut txn = Transaction::begin(dev).unwrap();
        let small = vec![3u8; 10_000];
        let too_big = vec![9u8; 96 << 20];

        txn.atomically(|t| {
            create_file(t, &vsb, &om, ROOT_INO, "before.txt")?;
            overwrite_existing_file(t, &vsb, &om, ROOT_INO, "before.txt", &small)
        })
        .unwrap();
        let failed = txn.atomically(|t| {
            create_file(t, &vsb, &om, ROOT_INO, "big.bin")?;
            overwrite_existing_file(t, &vsb, &om, ROOT_INO, "big.bin", &too_big)
        });
        assert!(failed.is_err(), "a write larger than the volume must fail");
        txn.atomically(|t| create_file(t, &vsb, &om, ROOT_INO, "after.txt"))
            .unwrap();
        let dev = txn.commit().unwrap();

        let (ok, out) = xtask(&["verify-scratch", IMG]);
        assert!(ok, "the volume is not fsck-clean:\n{out}");
        let mut fs = FsView::open(dev).unwrap();
        let mut names: Vec<String> = fs
            .read_dir("/")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        assert_eq!(names, ["after.txt", "before.txt"]);
        assert_eq!(fs.read("/before.txt", 0, 20_000).unwrap(), small);
        let _ = std::fs::remove_file(IMG);
    }

    /// Data a range write puts on the device before the commit is put back
    /// when the operation that rewrote it fails: the file keeps what the
    /// operation before wrote. And an open transaction holds no file data
    /// in memory, however much it wrote.
    #[test]
    #[ignore = "fsck-gated"]
    fn a_failed_range_write_puts_back_the_blocks_it_rewrote() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let img_ok = xtask(&["gen-scratch", IMG, "64"]);
        assert!(img_ok.0, "gen-scratch: {}", img_ok.1);
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut txn = Transaction::begin(dev).unwrap();
        let first = vec![1u8; 8192];
        txn.atomically(|t| {
            create_file(t, &vsb, &om, ROOT_INO, "fresh.bin")?;
            write_range(t, &vsb, &om, ROOT_INO, "fresh.bin", 0, &first).map(|_| ())
        })
        .unwrap();
        let failed = txn.atomically(|t| {
            write_range(t, &vsb, &om, ROOT_INO, "fresh.bin", 0, &[2u8; 4096])?;
            write_range(
                t,
                &vsb,
                &om,
                ROOT_INO,
                "fresh.bin",
                4096,
                &vec![3u8; 96 << 20],
            )
            .map(|_| ())
        });
        assert!(failed.is_err(), "a write larger than the volume must fail");
        txn.atomically(|t| {
            create_file(t, &vsb, &om, ROOT_INO, "large.bin")?;
            for i in 0..4u64 {
                write_range(
                    t,
                    &vsb,
                    &om,
                    ROOT_INO,
                    "large.bin",
                    i << 20,
                    &vec![5u8; 1 << 20],
                )?;
            }
            Ok(())
        })
        .unwrap();
        let held = txn.staged_bytes();
        assert!(
            held < 1 << 20,
            "4 MiB written, {held} bytes held in memory by the transaction"
        );
        // Small writes gather in a bounded buffer before they reach the
        // device in runs.
        txn.atomically(|t| {
            for i in 0..2000 {
                let name = format!("s{i}");
                create_file(t, &vsb, &om, ROOT_INO, &name)?;
                write_range(t, &vsb, &om, ROOT_INO, &name, 0, &[7u8; 4096])?;
            }
            Ok(())
        })
        .unwrap();
        let held = txn.staged_bytes();
        assert!(
            held < 6 << 20,
            "8 MiB of small writes, {held} bytes held in memory by the transaction"
        );
        let dev = txn.commit().unwrap();
        let (ok, out) = xtask(&["verify-scratch", IMG]);
        assert!(ok, "the volume is not fsck-clean:\n{out}");
        let mut fs = FsView::open(dev).unwrap();
        assert_eq!(fs.read("/fresh.bin", 0, 20_000).unwrap(), first);
        assert_eq!(
            fs.read("/large.bin", 0, 4 << 20).unwrap(),
            vec![5u8; 4 << 20]
        );
        let _ = std::fs::remove_file(IMG);
    }
}
