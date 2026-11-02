//! A large range write does not copy its data.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, fsck_apfs).
//!
//! Windows hands a large file to the mount in writes of up to 32 MiB. The
//! range writer built a 4 KiB buffer for every block and then joined them
//! into one more buffer per run, so a write held about twice its own size
//! in memory. Blocks the data covers whole are written from the data.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
    )
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Bytes allocated now, and the most since the last reset.
struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged to the system allocator.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let now = LIVE.fetch_add(layout.size(), Ordering::SeqCst) + layout.size();
            PEAK.fetch_max(now, Ordering::SeqCst);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: forwarded unchanged to the system allocator.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::SeqCst);
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

#[cfg(test)]
mod memory {
    use super::{LIVE, PEAK};
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::gpt::find_apfs_container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, write_range, RangeWrite};
    use apfs_write::txn::Transaction;
    use std::process::Command;
    use std::sync::atomic::Ordering;

    const IMG: &str = "/tmp/apfs-write-range-memory";
    const ROOT: u64 = 2;
    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;

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

    fn pattern(n: usize, seed: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 7 + seed * 131) % 251) as u8).collect()
    }

    /// Run one range write in its own transaction and return the most
    /// memory it held above what was allocated before it.
    fn measured_write(offset: u64, data: &[u8]) -> usize {
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        let before = LIVE.load(Ordering::SeqCst);
        PEAK.store(before, Ordering::SeqCst);
        let r = write_range(&mut t, &vsb, &om, ROOT, "big.bin", offset, data).unwrap();
        let held = PEAK.load(Ordering::SeqCst) - before;
        assert_eq!(r, RangeWrite::Written);
        t.commit().unwrap();
        held
    }

    #[test]
    fn a_large_range_write_does_not_copy_its_data() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            println!("APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let _ = std::fs::remove_file(format!("{IMG}.dmg"));
        let st = Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "160m", "-layout", "GPTSPUD"])
            .args(["-fs", "APFS", "-volname", "RANGEMEM", IMG])
            .status()
            .unwrap();
        assert!(st.success(), "hdiutil create failed");
        {
            let mut dev = open_dev();
            let (vsb, om) = vsb_omap(&mut dev);
            let mut t = Transaction::begin(dev).unwrap();
            create_file(&mut t, &vsb, &om, ROOT, "big.bin").unwrap();
            t.commit().unwrap();
        }

        let first = pattern(32 << 20, 1);
        let held = measured_write(0, &first);
        assert!(held < 8 << 20, "a 32 MiB write held {} MiB", held >> 20);

        // Unaligned at both ends: the edge blocks mix old and new bytes.
        let second = pattern((16 << 20) + 5000, 2);
        let held = measured_write(1000, &second);
        assert!(
            held < 8 << 20,
            "a 16 MiB unaligned write held {} MiB",
            held >> 20
        );

        let mut want = first;
        want[1000..1000 + second.len()].copy_from_slice(&second);
        let mut fs = apfs::FsView::open(open_dev()).unwrap();
        let got = fs.read("/big.bin", 0, want.len()).unwrap();
        assert!(got == want, "the file reads back different");

        let attached = Command::new("hdiutil")
            .args(["attach", "-nomount", &format!("{IMG}.dmg")])
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&attached.stdout).into_owned();
        let part = out
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| d.starts_with("/dev/disk") && d.ends_with("s1"))
            .unwrap()
            .replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs")
            .args(["-n", &part])
            .output()
            .unwrap();
        let whole = part
            .trim_end_matches("s1")
            .replacen("/dev/rdisk", "/dev/disk", 1);
        let _ = Command::new("hdiutil").args(["detach", &whole]).status();
        assert!(
            fsck.status.success(),
            "fsck_apfs: {}",
            String::from_utf8_lossy(&fsck.stdout)
        );
        let _ = std::fs::remove_file(format!("{IMG}.dmg"));
    }
}
