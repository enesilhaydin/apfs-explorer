//! A small read of a file edited in many places reads one path down the
//! tree to the extents it touches.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil).
//!
//! Every edit of a file splits its extent, so a file written into in a
//! thousand places has thousands of extent records. A read gathered all of
//! them first, so on Windows, where an application reads such a file 4 KiB
//! at a time, every request walked the file's whole extent list.
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
mod fragmented_read {
    use apfs_core::block_device::{
        BlockDevice, BlockError, FileBlockDeviceRw, WritableOffsetBlockDevice,
    };
    use apfs_core::container::Container;
    use apfs_core::gpt::find_apfs_container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, write_range, RangeWrite};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-fragmented-read";
    const ROOT: u64 = 2;
    const BLOCK: usize = 4096;
    /// Blocks in the file; every other one is rewritten, so each rewrite
    /// is an extent of its own between two pieces of the original.
    const BLOCKS: usize = 2400;
    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;

    /// Counts the reads that reach the image.
    struct Counted {
        inner: Dev,
        reads: usize,
    }

    impl BlockDevice for Counted {
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            self.reads += 1;
            self.inner.read_at(offset, buf)
        }
    }

    fn open_dev() -> Dev {
        let mut disk = FileBlockDeviceRw::open_rw(format!("{IMG}.dmg")).unwrap();
        let r = find_apfs_container(&mut disk, 512).unwrap();
        WritableOffsetBlockDevice::new(disk, r.offset, r.length)
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

    fn write(offset: usize, data: &[u8]) {
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        let r = write_range(&mut t, &vsb, &om, ROOT, "frag.bin", offset as u64, data).unwrap();
        assert_eq!(r, RangeWrite::Written);
        t.commit().unwrap();
    }

    #[test]
    fn a_small_read_of_a_fragmented_file_reads_one_path() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            println!("APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let _ = std::fs::remove_file(format!("{IMG}.dmg"));
        let st = Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "64m", "-layout", "GPTSPUD"])
            .args(["-fs", "APFS", "-volname", "FRAGREAD", IMG])
            .status()
            .unwrap();
        assert!(st.success(), "hdiutil create failed");
        {
            let mut dev = open_dev();
            let (vsb, om) = vsb_omap(&mut dev);
            let mut t = Transaction::begin(dev).unwrap();
            create_file(&mut t, &vsb, &om, ROOT, "frag.bin").unwrap();
            t.commit().unwrap();
        }
        let mut want: Vec<u8> = (0..BLOCKS * BLOCK).map(|i| (i % 251) as u8).collect();
        write(0, &want);
        for b in (1..BLOCKS).step_by(2) {
            let block = vec![(b % 253) as u8 ^ 0x5a; BLOCK];
            write(b * BLOCK, &block);
            want[b * BLOCK..(b + 1) * BLOCK].copy_from_slice(&block);
        }

        let mut fs = apfs::FsView::open(open_dev()).unwrap();
        let extents = fs.file_extents("/frag.bin").unwrap().len();
        assert!(extents >= BLOCKS - 1, "only {extents} extents");
        // Every block, and reads that cross from one extent into the next,
        // read back what was written.
        for b in 0..BLOCKS {
            let got = fs.read("/frag.bin", (b * BLOCK) as u64, BLOCK).unwrap();
            assert!(got == want[b * BLOCK..(b + 1) * BLOCK], "block {b} differs");
        }
        for at in [BLOCK - 100, 777 * BLOCK + 5, (BLOCKS - 1) * BLOCK - 10] {
            let got = fs.read("/frag.bin", at as u64, 3 * BLOCK).unwrap();
            let end = (at + 3 * BLOCK).min(want.len());
            assert!(got == want[at..end], "read at {at} differs");
        }
        drop(fs);

        // From a cold start: the lookup of the file, then one read in the
        // middle. The tree is a few levels deep; its extent leaves are
        // dozens.
        let mut fs = apfs::FsView::open(Counted {
            inner: open_dev(),
            reads: 0,
        })
        .unwrap();
        fs.getattr("/frag.bin").unwrap().unwrap();
        let before = fs.dev_mut().reads;
        let mid = BLOCKS / 2 * BLOCK;
        let got = fs.read("/frag.bin", mid as u64, BLOCK).unwrap();
        assert!(got == want[mid..mid + BLOCK]);
        let reads = fs.dev_mut().reads - before;
        assert!(reads <= 8, "a 4 KiB read reached the image {reads} times");
        let _ = std::fs::remove_file(format!("{IMG}.dmg"));
    }
}
