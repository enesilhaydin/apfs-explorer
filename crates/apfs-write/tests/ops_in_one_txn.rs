//! Several operations in one transaction each see what the ones before them
//! did.
//!
//! Every operation takes the volume superblock and object-map header read
//! before the transaction began. When an earlier operation in the same
//! transaction has staged a newer superblock, a later one working from the
//! old copy built its catalog from the state before the transaction, and the
//! last superblock staged won: every earlier change was lost, and the blocks
//! the earlier changes released could still be named by the tree that won.
//! The operations here run in one transaction from the same pre-transaction
//! copies; all of them must land, and the volume must check clean.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]`: runs fsck.
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --features write \
//!     --test ops_in_one_txn -- --ignored --nocapture --test-threads=1
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
mod one_txn {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, overwrite_existing_file, rename, set_xattr, unlink};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-ops-one-txn.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ROOT_INO: u64 = 2;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;

    fn root() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }

    fn xtask(args: &[&str]) -> (bool, String) {
        let o = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--"])
            .args(args)
            .current_dir(root())
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
    fn every_operation_of_a_transaction_lands() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let (ok, out) = xtask(&["gen-scratch", IMG, "64"]);
        assert!(ok, "gen-scratch: {out}");

        // Before: two files, one with content.
        {
            let mut dev = open_dev();
            let (vsb, om) = vsb_omap(&mut dev);
            let mut txn = Transaction::begin(dev).unwrap();
            create_file(&mut txn, &vsb, &om, ROOT_INO, "keep.txt").unwrap();
            txn.commit().unwrap();
        }
        {
            let mut dev = open_dev();
            let (vsb, om) = vsb_omap(&mut dev);
            let mut txn = Transaction::begin(dev).unwrap();
            create_file(&mut txn, &vsb, &om, ROOT_INO, "gone.txt").unwrap();
            txn.commit().unwrap();
        }
        {
            let mut dev = open_dev();
            let (vsb, om) = vsb_omap(&mut dev);
            let mut txn = Transaction::begin(dev).unwrap();
            overwrite_existing_file(&mut txn, &vsb, &om, ROOT_INO, "gone.txt", &[5u8; 9000])
                .unwrap();
            txn.commit().unwrap();
        }

        // One transaction, every operation given the copies read before it.
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut txn = Transaction::begin(dev).unwrap();
        let new_ino = create_file(&mut txn, &vsb, &om, ROOT_INO, "new.txt").unwrap();
        overwrite_existing_file(&mut txn, &vsb, &om, ROOT_INO, "new.txt", &[7u8; 5000]).unwrap();
        set_xattr(&mut txn, &vsb, &om, new_ino, "user.note", b"hello").unwrap();
        rename(&mut txn, &vsb, &om, ROOT_INO, "keep.txt", "kept.txt", false).unwrap();
        unlink(&mut txn, &vsb, &om, ROOT_INO, "gone.txt").unwrap();
        txn.commit().unwrap();

        let (ok, out) = xtask(&["verify-scratch", IMG]);
        assert!(ok, "the volume is not fsck-clean:\n{out}");

        let mut fs = apfs::FsView::open(open_dev()).unwrap();
        let mut names: Vec<String> = fs
            .read_dir("/")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .filter(|n| n.ends_with(".txt"))
            .collect();
        names.sort();
        assert_eq!(names, ["kept.txt", "new.txt"], "not every operation landed");
        assert_eq!(fs.read("/new.txt", 0, 10_000).unwrap(), vec![7u8; 5000]);
        assert_eq!(
            fs.get_xattr("/new.txt", "user.note").unwrap().as_deref(),
            Some(&b"hello"[..])
        );
        let _ = std::fs::remove_file(IMG);
    }

    /// The volume superblock and its object-map header an earlier operation
    /// of the transaction staged are written again where they are: a run of
    /// renames, whose tree nodes are rewritten in place too, takes no new
    /// blocks after the first, so the blocks a transaction holds stay
    /// bounded by what it changes and data written between operations stays
    /// contiguous.
    #[test]
    #[ignore = "fsck-gated"]
    fn later_operations_take_no_new_blocks_for_the_superblock() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let (ok, out) = xtask(&["gen-scratch", IMG, "64"]);
        assert!(ok, "gen-scratch: {out}");
        {
            let mut dev = open_dev();
            let (vsb, om) = vsb_omap(&mut dev);
            let mut txn = Transaction::begin(dev).unwrap();
            create_file(&mut txn, &vsb, &om, ROOT_INO, "r0.txt").unwrap();
            txn.commit().unwrap();
        }

        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut txn = Transaction::begin(dev).unwrap();
        rename(&mut txn, &vsb, &om, ROOT_INO, "r0.txt", "r1.txt", false).unwrap();
        let after_first = txn.blocks_allocated();
        for i in 1..50 {
            let (from, to) = (format!("r{i}.txt"), format!("r{}.txt", i + 1));
            rename(&mut txn, &vsb, &om, ROOT_INO, &from, &to, false).unwrap();
        }
        let later = txn.blocks_allocated() - after_first;
        txn.commit().unwrap();

        let (ok, out) = xtask(&["verify-scratch", IMG]);
        assert!(ok, "the volume is not fsck-clean:\n{out}");
        let mut fs = apfs::FsView::open(open_dev()).unwrap();
        assert!(
            fs.lookup("/r50.txt").unwrap().is_some(),
            "the last rename landed"
        );
        assert_eq!(
            later, 0,
            "49 renames after the first took {later} new blocks"
        );
        let _ = std::fs::remove_file(IMG);
    }
}
