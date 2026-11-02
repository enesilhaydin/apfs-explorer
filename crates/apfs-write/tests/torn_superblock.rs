//! A commit that loses power between its two superblock writes.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! A checkpoint is committed by writing its superblock into the descriptor
//! ring; block 0 holds a copy for bootstrapping. The two are written one after
//! the other, and a power loss in between leaves them disagreeing. What the
//! checker and macOS mount then is the newest valid superblock in the ring, so
//! that write has to come first, and our own reader has to make the same
//! choice: a checkpoint that only block 0 knows about is not a checkpoint.
//!
//! The device below drops every write after the first superblock write of the
//! commit. Apple's fsck judges the container, our reader and a read-only mount
//! by macOS are compared on which checkpoint they see.
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
mod torn_superblock {
    use apfs_core::block_device::{
        BlockDevice, BlockError, FileBlockDeviceRw, WritableBlockDevice, WritableOffsetBlockDevice,
    };
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::write_file;
    use apfs_write::txn::Transaction;
    use std::io::{Read, Seek, SeekFrom};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-torn-superblock.img";
    const IMAGE_MB: &str = "64";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const NX_MAGIC: u32 = 0x4253_584E;

    /// The media of a machine that loses power right after the commit's first
    /// superblock write: that write lands, every later write is acknowledged
    /// and lost.
    struct TornDevice<D: WritableBlockDevice> {
        inner: D,
        superblock_writes: usize,
        dropped: usize,
    }

    impl<D: WritableBlockDevice> TornDevice<D> {
        fn new(inner: D) -> Self {
            Self {
                inner,
                superblock_writes: 0,
                dropped: 0,
            }
        }
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
            if self.superblock_writes >= 1 {
                self.dropped += 1;
                return Ok(());
            }
            let is_superblock = buf.len() >= 36 && u32le(buf, 32) == NX_MAGIC;
            let r = self.inner.write_at(offset, buf);
            if r.is_ok() && is_superblock {
                self.superblock_writes += 1;
            }
            r
        }
        fn flush_data(&mut self) -> Result<(), BlockError> {
            self.inner.flush_data()
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

    fn u32le(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }
    fn u64le(b: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }

    /// `(block 0 xid, newest valid superblock xid in the ring)`, decoded with
    /// no writer code: the checker's own rule is magic + checksum + highest xid.
    fn superblock_xids() -> (u64, u64) {
        let (base, _) = find_apfs_range();
        let mut f = std::fs::File::open(IMG).unwrap();
        let mut rd = |bno: u64, bsz: usize| {
            let mut b = vec![0u8; bsz];
            f.seek(SeekFrom::Start(base + bno * bsz as u64)).unwrap();
            f.read_exact(&mut b).unwrap();
            b
        };
        let nx0 = rd(0, 4096);
        let bsz = u32le(&nx0, 36) as usize;
        let desc_blocks = (u32le(&nx0, 0x68) & 0x7FFF_FFFF) as u64;
        let desc_base = u64le(&nx0, 0x70);
        let mut ring_xid = 0;
        for i in 0..desc_blocks {
            let b = rd(desc_base + i, bsz);
            if u32le(&b, 32) == NX_MAGIC && apfs_core::checksum::verify_block(&b).is_ok() {
                ring_xid = ring_xid.max(u64le(&b, 16));
            }
        }
        (u64le(&nx0, 16), ring_xid)
    }

    /// `(fsck exit status, complaint lines)`.
    fn run_fsck() -> (bool, Vec<String>, String) {
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
        let disk = whole_disk(&attached);
        let part = attached
            .lines()
            .filter(|l| l.contains("Apple_APFS"))
            .filter_map(|l| l.split_whitespace().next())
            .next()
            .expect("no APFS partition node")
            .to_string();
        let rdev = part.replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        detach(&disk);
        let fsck = fsck.expect("fsck_apfs spawn");
        let report = String::from_utf8_lossy(&fsck.stdout).into_owned()
            + &String::from_utf8_lossy(&fsck.stderr);
        let complaints: Vec<String> = report
            .lines()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("warning:")
                    || l.contains("error:")
                    || l.contains("overallocation")
                    || l.contains("underallocation")
                    || l.contains("is not valid")
            })
            .map(str::to_owned)
            .collect();
        (fsck.status.success(), complaints, report)
    }

    fn whole_disk(attached: &str) -> String {
        attached
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node")
    }

    fn detach(disk: &str) {
        for attempt in 0..8u64 {
            let st = Command::new("hdiutil")
                .args(["detach", disk])
                .output()
                .expect("detach");
            if st.status.success() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
        }
        let _ = Command::new("hdiutil")
            .args(["detach", "-force", disk])
            .status();
    }

    /// The root listing macOS shows on a read-only mount.
    fn macos_root_listing() -> Vec<String> {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                "-readonly",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach (mount) failed");
        let attached = String::from_utf8_lossy(&out.stdout).into_owned();
        let disk = whole_disk(&attached);
        let mount = attached
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let mut names: Vec<String> = std::fs::read_dir(&mount)
            .expect("read_dir")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| !n.starts_with('.'))
            .collect();
        detach(&disk);
        names.sort();
        names
    }

    #[test]
    #[ignore = "scratch-image gated; needs xtask gen-scratch and fsck_apfs on macOS"]
    fn the_ring_decides_which_checkpoint_a_torn_commit_left() {
        if !gate() {
            return;
        }
        gen_scratch();
        let first = pattern(16 * 1024, 1);
        let second = pattern(24 * 1024, 2);
        {
            let (vsb, om) = read_vsb_omap();
            let mut t = Transaction::begin(open_dev()).expect("begin");
            write_file(&mut t, &vsb, &om, 2, "first.bin", &first).expect("write first");
            t.commit().expect("commit first");
        }
        let (block0_before, ring_before) = superblock_xids();
        assert_eq!(
            block0_before, ring_before,
            "a whole commit leaves both copies agreeing"
        );

        // The second commit loses power after its first superblock write.
        let (vsb, om) = read_vsb_omap();
        let mut t = Transaction::begin(TornDevice::new(open_dev())).expect("begin");
        write_file(&mut t, &vsb, &om, 2, "second.bin", &second).expect("write second");
        let media = t
            .commit()
            .expect("the commit reports success; the loss is on the media");
        println!(
            "torn-superblock: {} superblock write(s) landed, {} writes lost",
            media.superblock_writes, media.dropped
        );
        drop(media);

        let (block0, ring) = superblock_xids();
        println!("torn-superblock: block 0 says xid {block0}, the ring's newest valid is {ring}");
        assert_eq!(
            ring,
            ring_before + 1,
            "the ring slot is the commit point and has to be written first"
        );

        // Our reader has to see what the checker's rule selects.
        let ours = Container::open(&mut open_dev())
            .expect("Container::open")
            .superblock
            .obj
            .xid;
        let mut fs = apfs::FsView::open(open_dev()).expect("FsView::open");
        let mut names: Vec<String> = fs
            .read_dir("/")
            .expect("read_dir")
            .into_iter()
            .map(|e| e.name)
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        drop(fs);
        assert_eq!(
            ours, ring,
            "the reader must mount the newest checkpoint of the ring"
        );
        assert_eq!(
            names,
            vec!["first.bin".to_string(), "second.bin".to_string()]
        );

        // Apple's fsck and Apple's mount agree with that.
        let (ok, complaints, report) = run_fsck();
        assert!(ok, "fsck_apfs reported corruption:\n{report}");
        assert!(
            complaints.is_empty(),
            "fsck_apfs flagged inconsistencies:\n{}",
            complaints.join("\n")
        );
        let seen = macos_root_listing();
        assert_eq!(
            seen, names,
            "macOS must mount the same checkpoint as our reader"
        );

        let _ = std::fs::remove_file(IMG);
    }
}
