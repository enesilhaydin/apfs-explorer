//! A reader built on an open transaction sees the transaction's changes
//! before they are committed.
//!
//! A host that groups many operations into one checkpoint still has to
//! answer lookups between them: the file just created has to be found, the
//! bytes just written read back. The reader is opened on the transaction
//! itself (its reads lay staged blocks over the device) and rebound to the
//! superblock the transaction last staged after each operation. After the
//! commit a reader on the plain device sees the same tree, and the volume
//! checks clean.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]`: runs fsck.
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --features write \
//!     --test read_through_open_txn -- --ignored --nocapture
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
mod open_txn {
    use apfs::FsView;
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, mkdir, overwrite_existing_file, rename, unlink};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-read-open-txn.img";
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

    fn names(fs: &mut FsView<impl BlockDevice>, dir: &str) -> Vec<String> {
        let mut n: Vec<String> = fs
            .read_dir(dir)
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .filter(|n| !n.starts_with('.'))
            .collect();
        n.sort();
        n
    }

    #[test]
    #[ignore = "fsck-gated"]
    fn a_reader_on_an_open_transaction_sees_its_changes() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let (ok, out) = xtask(&["gen-scratch", IMG, "64"]);
        assert!(ok, "gen-scratch: {out}");

        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let vsb_oid = u64::from_le_bytes(vsb[8..16].try_into().unwrap());
        let mut fs = FsView::open(Transaction::begin(dev).unwrap()).unwrap();
        let body: Vec<u8> = (0..9000u32).map(|i| (i % 253) as u8).collect();

        // A first group of operations, then lookups between them.
        {
            let txn = fs.dev_mut();
            create_file(txn, &vsb, &om, ROOT_INO, "a.txt").unwrap();
            overwrite_existing_file(txn, &vsb, &om, ROOT_INO, "a.txt", &body).unwrap();
            let d = mkdir(txn, &vsb, &om, ROOT_INO, "d").unwrap();
            create_file(txn, &vsb, &om, d, "b.txt").unwrap();
        }
        let staged = fs.dev_mut().staged_volume_block(vsb_oid).unwrap();
        fs.rebind(staged).unwrap();
        assert_eq!(names(&mut fs, "/"), ["a.txt", "d"]);
        assert_eq!(names(&mut fs, "/d"), ["b.txt"]);
        assert_eq!(fs.read("/a.txt", 0, 20_000).unwrap(), body);

        // More operations in the same transaction, building on the first.
        {
            let txn = fs.dev_mut();
            rename(txn, &vsb, &om, ROOT_INO, "a.txt", "c.txt", false).unwrap();
            unlink(txn, &vsb, &om, ROOT_INO, "d").unwrap_err();
        }
        let staged = fs.dev_mut().staged_volume_block(vsb_oid).unwrap();
        fs.rebind(staged).unwrap();
        assert_eq!(names(&mut fs, "/"), ["c.txt", "d"]);
        assert_eq!(fs.read("/c.txt", 0, 20_000).unwrap(), body);

        // Commit; a reader on the plain device sees the same.
        let dev = fs.into_dev().commit().unwrap();
        let (ok, out) = xtask(&["verify-scratch", IMG]);
        assert!(ok, "the volume is not fsck-clean:\n{out}");
        let mut fs = FsView::open(dev).unwrap();
        assert_eq!(names(&mut fs, "/"), ["c.txt", "d"]);
        assert_eq!(names(&mut fs, "/d"), ["b.txt"]);
        assert_eq!(fs.read("/c.txt", 0, 20_000).unwrap(), body);
        let _ = std::fs::remove_file(IMG);
    }
}
