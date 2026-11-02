//! A commit that loses power right after its checkpoint became durable.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! The commit writes its blocks, flushes, writes the superblock, flushes again,
//! and only then writes whatever it held back. Everything the sealed checkpoint
//! names has to be on the media by the time the superblock is: a block it names
//! that is still unwritten makes a torn container, and the previous checkpoint
//! cannot help, because the newest one is valid and is the one macOS reads.
//!
//! The device below drops every write after the commit's second flush, which
//! is the instant the power goes out. An independent decoder then checks the
//! allocator blocks the newest checkpoint names, fsck_apfs judges the whole
//! container, and the reader has to find both files.
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
mod torn_commit {
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
    const IMG: &str = "/tmp/apfs-torn-commit.img";
    const IMAGE_MB: &str = "64";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const TYPE_NX_SUPERBLOCK: u32 = 0x1;
    const TYPE_SPACEMAN: u32 = 0x5;
    const TYPE_SPACEMAN_CIB: u32 = 0x7;
    const TYPE_CHECKPOINT_MAP: u32 = 0xC;
    const NX_MAGIC: u32 = 0x4253_584E;

    /// The media of a machine that loses power after the commit's `cut_after`th
    /// flush: every later write is acknowledged and never lands.
    struct TornDevice<D: WritableBlockDevice> {
        inner: D,
        flushes: usize,
        cut_after: usize,
        dropped: usize,
    }

    impl<D: WritableBlockDevice> TornDevice<D> {
        fn new(inner: D, cut_after: usize) -> Self {
            Self {
                inner,
                flushes: 0,
                cut_after,
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
            if self.flushes >= self.cut_after {
                self.dropped += 1;
                return Ok(());
            }
            self.inner.write_at(offset, buf)
        }
        fn flush_data(&mut self) -> Result<(), BlockError> {
            self.inner.flush_data()?;
            self.flushes += 1;
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

    fn u32le(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }
    fn u64le(b: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }

    /// What the newest checkpoint's allocator blocks look like on the media,
    /// decoded with no writer code: one line per problem.
    fn allocator_problems() -> Vec<String> {
        let (base, _) = find_apfs_range();
        let mut f = std::fs::File::open(IMG).unwrap();
        let mut rd = |bno: u64, bsz: usize| {
            let mut b = vec![0u8; bsz];
            f.seek(SeekFrom::Start(base + bno * bsz as u64)).unwrap();
            f.read_exact(&mut b).unwrap();
            b
        };
        let (xid, sm, bsz) = newest_spaceman(&mut rd);
        let mut problems = Vec::new();
        let chunks_per_cib = u32le(&sm, 40) as usize;
        let cib_count = u32le(&sm, 64) as usize;
        assert_eq!(
            u32le(&sm, 68),
            0,
            "a small container lists its chunk-info blocks directly"
        );
        let addr_offset = u32le(&sm, 80) as usize;
        for i in 0..cib_count {
            let cib_bno = u64le(&sm, addr_offset + i * 8);
            let cib = rd(cib_bno, bsz);
            if apfs_core::checksum::verify_block(&cib).is_err() {
                problems.push(format!(
                    "chunk-info block {i} at {cib_bno}: bad checksum (the checkpoint names a block \
                     that was never written)"
                ));
                continue;
            }
            if u32le(&cib, 0x18) & 0xFFFF != TYPE_SPACEMAN_CIB {
                problems.push(format!("chunk-info block {i} at {cib_bno}: wrong type"));
                continue;
            }
            let cib_xid = u64le(&cib, 16);
            if cib_xid > xid {
                problems.push(format!(
                    "chunk-info block {i} at {cib_bno}: newer than the checkpoint"
                ));
            }
            for k in 0..chunks_per_cib {
                let ci = 40 + k * 32;
                let block_count = u32le(&cib, ci + 16) as usize;
                if block_count == 0 {
                    break;
                }
                let free_count = u32le(&cib, ci + 20) as usize;
                let bitmap = u64le(&cib, ci + 24);
                if bitmap == 0 {
                    continue;
                }
                let bm = rd(bitmap, bsz);
                let used: usize = bm
                    .iter()
                    .take(block_count.div_ceil(8))
                    .map(|b| b.count_ones() as usize)
                    .sum();
                if block_count - used != free_count {
                    problems.push(format!(
                        "chunk {k} of chunk-info block {i}: free count {free_count} but the bitmap \
                         at {bitmap} has {} clear bits",
                        block_count - used
                    ));
                }
            }
        }
        problems
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
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        // The scratch image has one partition and it is the APFS one.
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
    fn a_checkpoint_that_became_durable_names_only_written_blocks() {
        if !gate() {
            return;
        }
        gen_scratch();
        let first = pattern(24 * 1024, 1);
        let second = pattern(40 * 1024, 2);

        // A whole commit first, so the second one copies allocator blocks the
        // first one made and has to release them.
        {
            let (vsb, om) = read_vsb_omap();
            let mut t = Transaction::begin(open_dev()).expect("begin");
            write_file(&mut t, &vsb, &om, 2, "first.bin", &first).expect("write first");
            t.commit().expect("commit first");
        }
        let problems = allocator_problems();
        assert!(
            problems.is_empty(),
            "after a whole commit:\n{}",
            problems.join("\n")
        );
        run_fsck("after a whole commit");

        // The second commit loses power the moment its checkpoint is durable.
        let (vsb, om) = read_vsb_omap();
        let mut t = Transaction::begin(TornDevice::new(open_dev(), 2)).expect("begin");
        write_file(&mut t, &vsb, &om, 2, "second.bin", &second).expect("write second");
        let media = t
            .commit()
            .expect("the commit reports success; the loss is on the media");
        println!(
            "torn-commit: {} flushes seen, {} writes lost after the checkpoint was sealed",
            media.flushes, media.dropped
        );
        drop(media);

        let problems = allocator_problems();
        assert!(
            problems.is_empty(),
            "the sealed checkpoint names allocator blocks that never reached the media:\n{}",
            problems.join("\n")
        );
        run_fsck("after the torn commit");

        let mut fs = apfs::FsView::open(open_dev()).expect("FsView::open");
        for (name, body) in [("first.bin", &first), ("second.bin", &second)] {
            let got = fs.read(&format!("/{name}"), 0, body.len()).expect("read");
            assert!(
                got == *body,
                "{name} must read back from the sealed checkpoint"
            );
            // And the allocator the sealed checkpoint describes has to know
            // the file's blocks are taken, or the next writer overwrites them.
            for ext in fs.file_extents(&format!("/{name}")).expect("extents") {
                if ext.phys_block_num == 0 {
                    continue;
                }
                let unmarked = unmarked_blocks(ext.phys_block_num, ext.len / 4096);
                assert!(
                    unmarked.is_empty(),
                    "{name}: blocks {unmarked:?} are in use but the sealed checkpoint's bitmap \
                     says they are free"
                );
            }
        }
        let _ = std::fs::remove_file(IMG);
    }

    /// Which of `count` blocks from `first` the newest checkpoint's bitmaps
    /// leave clear.
    fn unmarked_blocks(first: u64, count: u64) -> Vec<u64> {
        let (base, _) = find_apfs_range();
        let mut f = std::fs::File::open(IMG).unwrap();
        let mut rd = |bno: u64, bsz: usize| {
            let mut b = vec![0u8; bsz];
            f.seek(SeekFrom::Start(base + bno * bsz as u64)).unwrap();
            f.read_exact(&mut b).unwrap();
            b
        };
        let (_, sm, bsz) = newest_spaceman(&mut rd);
        let blocks_per_chunk = u64::from(u32le(&sm, 36));
        let chunks_per_cib = u32le(&sm, 40) as usize;
        let addr_offset = u32le(&sm, 80) as usize;
        let mut unmarked = Vec::new();
        for bno in first..first + count {
            let chunk = (bno / blocks_per_chunk) as usize;
            let cib = rd(u64le(&sm, addr_offset + (chunk / chunks_per_cib) * 8), bsz);
            let ci = 40 + (chunk % chunks_per_cib) * 32;
            let ci_addr = u64le(&cib, ci + 8);
            let bitmap = u64le(&cib, ci + 24);
            if bitmap == 0 {
                unmarked.push(bno);
                continue;
            }
            let bm = rd(bitmap, bsz);
            let bit = (bno - ci_addr) as usize;
            if bm[bit / 8] & (1 << (bit % 8)) == 0 {
                unmarked.push(bno);
            }
        }
        unmarked
    }

    /// The newest checkpoint's transaction id, its spaceman bytes and the
    /// block size.
    fn newest_spaceman(rd: &mut impl FnMut(u64, usize) -> Vec<u8>) -> (u64, Vec<u8>, usize) {
        let nx0 = rd(0, 4096);
        let bsz = u32le(&nx0, 36) as usize;
        let desc_blocks = (u32le(&nx0, 0x68) & 0x7FFF_FFFF) as u64;
        let data_blocks = u64::from(u32le(&nx0, 0x6C));
        let desc_base = u64le(&nx0, 0x70);
        let data_base = u64le(&nx0, 0x78);
        let mut newest: Option<(u64, Vec<u8>)> = None;
        for i in 0..desc_blocks {
            let b = rd(desc_base + i, bsz);
            if u32le(&b, 0x18) & 0xFFFF == TYPE_NX_SUPERBLOCK
                && u32le(&b, 32) == NX_MAGIC
                && apfs_core::checksum::verify_block(&b).is_ok()
            {
                let xid = u64le(&b, 16);
                if newest.as_ref().is_none_or(|(x, _)| xid > *x) {
                    newest = Some((xid, b));
                }
            }
        }
        let (xid, nx) = newest.expect("a superblock in the ring");
        let desc_index = u64::from(u32le(&nx, 0x88));
        let desc_len = u64::from(u32le(&nx, 0x8C));
        let sm_oid = u64le(&nx, 0x98);
        for i in 0..desc_len {
            let b = rd(desc_base + (desc_index + i) % desc_blocks, bsz);
            if u32le(&b, 0x18) & 0xFFFF != TYPE_CHECKPOINT_MAP {
                continue;
            }
            for j in 0..u32le(&b, 0x24) as usize {
                let e = 0x28 + j * 40;
                if u32le(&b, e) & 0xFFFF == TYPE_SPACEMAN && u64le(&b, e + 24) == sm_oid {
                    let (paddr, size) = (u64le(&b, e + 32), u32le(&b, e + 8) as usize);
                    let mut sm = Vec::with_capacity(size);
                    for k in 0..(size / bsz) as u64 {
                        let slot = (paddr - data_base + k) % data_blocks;
                        sm.extend_from_slice(&rd(data_base + slot, bsz));
                    }
                    return (xid, sm, bsz);
                }
            }
        }
        panic!("newest checkpoint has no spaceman mapping");
    }
}
