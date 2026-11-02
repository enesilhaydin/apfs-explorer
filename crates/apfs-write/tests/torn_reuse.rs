//! A block freed by a transaction must not be handed out again by that same
//! transaction.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! Until the new checkpoint is sealed, the previous one is what a machine
//! falls back to after a power loss, and it still names every block it was
//! sealed with. A transaction that frees one of those blocks and reuses it
//! for new content overwrites the fallback: nothing on disk says so, no
//! checksum covers file data, and the file simply reads back wrong under the
//! checkpoint everyone then mounts. macOS routes such frees through the space
//! manager's free queue, one checkpoint late, for exactly this reason.
//!
//! The device below drops every write from the commit's first flush on, which
//! is a power loss before the checkpoint is sealed; the file's content under
//! the surviving checkpoint is then compared with what that checkpoint wrote.
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
mod torn_reuse {
    use apfs_core::block_device::{
        BlockDevice, BlockError, FileBlockDeviceRw, WritableBlockDevice, WritableOffsetBlockDevice,
    };
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{overwrite_existing_file, write_file};
    use apfs_write::txn::Transaction;
    use std::io::Read;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-torn-reuse.img";
    const IMAGE_MB: &str = "64";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const FILE_BYTES: usize = 40 * 1024;

    /// Power goes out at the commit's first flush: every write before it has
    /// landed, nothing after it does.
    struct TornDevice<D: WritableBlockDevice> {
        inner: D,
        cut: bool,
        dropped: usize,
    }

    impl<D: WritableBlockDevice> BlockDevice for TornDevice<D> {
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            self.inner.read_at(offset, buf)
        }
    }

    impl<D: WritableBlockDevice> WritableBlockDevice for TornDevice<D> {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
            if self.cut {
                self.dropped += 1;
                return Ok(());
            }
            self.inner.write_at(offset, buf)
        }
        fn flush_data(&mut self) -> Result<(), BlockError> {
            self.inner.flush_data()?;
            self.cut = true;
            Ok(())
        }
        fn invalidate_read_cache(&mut self) {
            self.inner.invalidate_read_cache();
        }
    }

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn root_dir() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
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
                IMAGE_MB,
            ])
            .current_dir(root_dir())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
    }
    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| ((i as u64 * 7 + u64::from(seed) * 131) % 251) as u8)
            .collect()
    }
    fn find_apfs_range() -> (u64, u64) {
        let mut f = std::fs::File::open(IMG).unwrap();
        let mut head = vec![0u8; 64 * 1024];
        f.read_exact(&mut head).unwrap();
        let hdr = &head[512..1024];
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let off = base + i * sz;
            let e = match head.get(off..off + sz) {
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
        let (base, len) = find_apfs_range();
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        WritableOffsetBlockDevice::new(inner, base, len)
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
        let attached = String::from_utf8_lossy(&out.stdout).into_owned();
        let disk = attached
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
        let part = attached
            .lines()
            .filter(|l| l.contains("Apple_APFS"))
            .filter_map(|l| l.split_whitespace().next())
            .next()
            .expect("no APFS partition node")
            .to_string();
        let rdev = part.replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        for attempt in 0..8u64 {
            let st = Command::new("hdiutil")
                .args(["detach", &disk])
                .output()
                .expect("detach");
            if st.status.success() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
        }
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
                    || l.contains("error:")
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
    #[ignore = "scratch-image gated; needs xtask gen-scratch and fsck_apfs on macOS"]
    fn a_torn_overwrite_leaves_the_previous_checkpoint_reading_its_own_bytes() {
        if !gate() {
            return;
        }
        gen_scratch();
        let first = pattern(FILE_BYTES, 1);
        let second = pattern(FILE_BYTES, 2);
        {
            let (vsb, om) = read_vsb_omap();
            let mut t = Transaction::begin(open_dev()).expect("begin");
            write_file(&mut t, &vsb, &om, 2, "data.bin", &first).expect("write");
            t.commit().expect("commit");
        }
        let sealed = Container::open(&mut open_dev())
            .expect("open")
            .superblock
            .obj
            .xid;
        run_fsck("after the whole commit");

        // The overwrite frees the file's blocks and allocates for the new
        // content in one transaction; the power goes out before it is sealed.
        let (vsb, om) = read_vsb_omap();
        let dev = TornDevice {
            inner: open_dev(),
            cut: false,
            dropped: 0,
        };
        let mut t = Transaction::begin(dev).expect("begin");
        overwrite_existing_file(&mut t, &vsb, &om, 2, "data.bin", &second).expect("overwrite");
        let outcome = t.commit();
        println!(
            "torn-reuse: commit {} (the media lost every write after the first flush)",
            if outcome.is_ok() {
                "reported success"
            } else {
                "reported failure"
            }
        );
        drop(outcome);

        let after = Container::open(&mut open_dev())
            .expect("open")
            .superblock
            .obj
            .xid;
        assert_eq!(
            after, sealed,
            "the previous checkpoint is the one that survives"
        );
        let mut fs = apfs::FsView::open(open_dev()).expect("FsView::open");
        let got = fs.read("/data.bin", 0, FILE_BYTES).expect("read");
        drop(fs);
        let changed = got
            .chunks(4096)
            .zip(first.chunks(4096))
            .filter(|(a, b)| a != b)
            .count();
        assert!(
            got == first,
            "the surviving checkpoint must read the bytes it was sealed with; {changed} of {} \
             blocks were overwritten by the transaction that never sealed",
            FILE_BYTES / 4096
        );
        run_fsck("after the torn overwrite");
        let _ = std::fs::remove_file(IMG);
    }
}
