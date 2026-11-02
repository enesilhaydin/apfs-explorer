//! Looking at one record of an object reads only the part of the catalog
//! that holds it.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil).
//!
//! A directory's entries are keyed by the directory's own id, after its
//! inode record. Reading the inode (a stat) by walking every record of that
//! id read the whole directory: on Windows, where every create opens the
//! parent, filling a folder got slower with each file in it.
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
mod stat_reads {
    use apfs_core::block_device::{
        BlockDevice, BlockError, FileBlockDeviceRw, WritableOffsetBlockDevice,
    };
    use apfs_core::gpt::find_apfs_container;
    use std::process::Command;

    const SRC: &str = "/tmp/apfs-stat-reads-src";
    const IMG: &str = "/tmp/apfs-stat-reads";
    const ENTRIES: usize = 3000;

    /// Counts the reads that reach the image.
    struct Counted {
        inner: WritableOffsetBlockDevice<FileBlockDeviceRw>,
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

    #[test]
    fn stat_of_a_large_directory_reads_one_path_down_the_tree() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            println!("APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let _ = std::fs::remove_dir_all(SRC);
        std::fs::create_dir_all(format!("{SRC}/big")).unwrap();
        for i in 0..ENTRIES {
            std::fs::write(format!("{SRC}/big/entry{i:05}.txt"), b"x").unwrap();
        }
        let _ = std::fs::remove_file(format!("{IMG}.dmg"));
        let st = Command::new("hdiutil")
            .args([
                "create",
                "-quiet",
                "-srcfolder",
                SRC,
                "-fs",
                "APFS",
                "-layout",
                "GPTSPUD",
            ])
            .args(["-format", "UDRW", "-volname", "STATREADS", IMG])
            .status()
            .unwrap();
        assert!(st.success(), "hdiutil create failed");
        let _ = std::fs::remove_dir_all(SRC);

        let image = format!("{IMG}.dmg");
        let mut disk = FileBlockDeviceRw::open_rw(&image).unwrap();
        let range = find_apfs_container(&mut disk, 512).unwrap();
        let dev = Counted {
            inner: WritableOffsetBlockDevice::new(disk, range.offset, range.length),
            reads: 0,
        };
        let mut fs = apfs::FsView::open(dev).unwrap();
        // Resolve the path first, so what is counted is the stat.
        let ino = fs.lookup("/big").unwrap().expect("/big");
        let before = fs.dev_mut().reads;
        let attr = fs.getattr("/big").unwrap().expect("/big");
        let reads = fs.dev_mut().reads - before;
        assert!(attr.is_dir);
        assert_eq!(attr.inode, ino);
        assert_eq!(attr.nchildren_or_nlink as usize, ENTRIES);
        assert!(
            reads <= 8,
            "a stat of a directory of {ENTRIES} entries made {reads} device reads"
        );
        let _ = std::fs::remove_file(&image);
    }
}
