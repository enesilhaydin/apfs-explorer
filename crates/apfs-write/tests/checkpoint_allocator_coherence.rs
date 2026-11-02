//! The checkpoint before the newest one has to describe a coherent container.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! A checkpoint is a complete description of the container at one transaction:
//! its superblock names a spaceman, that spaceman names chunk-info blocks and a
//! copy of the internal-pool bitmap, and those have to agree with each other.
//! APFS keeps the previous checkpoint precisely so a machine that loses power
//! mid-commit can fall back to it, which only works if it was never written
//! through.
//!
//! `fsck_apfs` reads the newest checkpoint and nothing else, so it cannot see a
//! previous one that has been mutated underneath. This decodes it directly and
//! checks the invariants apfsck applies to the newest: a chunk's free count is
//! the number of clear bits in its bitmap, a chunk-info block's transaction id
//! is the newest of the chunks it holds, and every block of the pool that is in
//! use is marked used in that checkpoint's own copy of the pool bitmap.
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
mod checkpoint_allocator_coherence {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::write_file;
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-checkpoint-coherence.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// Four chunks of 128 MiB, only the first of which the formatter gives a
    /// bitmap - so filling past 128 MiB makes the allocator touch the pool.
    const IMAGE_MB: &str = "512";
    const FILE_BYTES: usize = 32 * 1024 * 1024;
    const FILE_COUNT: usize = 5;

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

    // --- an independent decoder, sharing no code with the writer ---

    const NX_MAGIC: u32 = 0x4253_584E;
    const TYPE_NX_SUPERBLOCK: u32 = 0x0001;
    const TYPE_CHECKPOINT_MAP: u32 = 0x000C;
    const TYPE_SPACEMAN: u32 = 0x0005;

    struct Image {
        part: Vec<u8>,
        bsz: usize,
    }

    impl Image {
        fn load() -> Self {
            let raw = std::fs::read(IMG).unwrap();
            let (base, len) = find_apfs_range(&raw);
            let part = raw[base as usize..(base + len) as usize].to_vec();
            let bsz = u32le(&part, 36) as usize;
            assert!(bsz >= 512 && bsz.is_power_of_two(), "block size {bsz}");
            Self { part, bsz }
        }
        fn block(&self, bno: u64) -> &[u8] {
            let off = bno as usize * self.bsz;
            &self.part[off..off + self.bsz]
        }
    }

    fn u16le(b: &[u8], o: usize) -> u16 {
        u16::from_le_bytes(b[o..o + 2].try_into().unwrap())
    }
    fn u32le(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }
    fn u64le(b: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }

    /// One checkpoint: the superblock that seals it and the spaceman it names.
    struct Checkpoint {
        xid: u64,
        spaceman_bno: u64,
        spaceman: Vec<u8>,
    }

    /// Every checkpoint still readable in the descriptor ring, newest first.
    fn checkpoints(img: &Image) -> Vec<Checkpoint> {
        let nx0 = img.block(0);
        let desc_blocks = u32le(nx0, 0x68) & 0x7FFF_FFFF;
        let desc_base = u64le(nx0, 0x70);
        let mut found: Vec<(u64, Vec<u8>)> = Vec::new();
        for i in 0..desc_blocks as u64 {
            let b = img.block(desc_base + i);
            if u32le(b, 0x18) & 0xFFFF != TYPE_NX_SUPERBLOCK || u32le(b, 32) != NX_MAGIC {
                continue;
            }
            found.push((u64le(b, 16), b.to_vec()));
        }
        found.sort_by_key(|(xid, _)| std::cmp::Reverse(*xid));
        found
            .into_iter()
            .filter_map(|(xid, nx)| {
                spaceman_of(img, &nx).map(|(spaceman_bno, spaceman)| Checkpoint {
                    xid,
                    spaceman_bno,
                    spaceman,
                })
            })
            .collect()
    }

    /// Walk a checkpoint's own descriptor slots for the map entry that places
    /// its spaceman in the data ring.
    fn spaceman_of(img: &Image, nx: &[u8]) -> Option<(u64, Vec<u8>)> {
        let desc_blocks = u32le(nx, 0x68) & 0x7FFF_FFFF;
        let desc_base = u64le(nx, 0x70);
        let desc_index = u32le(nx, 0x88);
        let desc_len = u32le(nx, 0x8C);
        let sm_oid = u64le(nx, 0x98);
        for i in 0..desc_len {
            let slot = (desc_index + i) % desc_blocks;
            let b = img.block(desc_base + slot as u64);
            if u32le(b, 0x18) & 0xFFFF != TYPE_CHECKPOINT_MAP {
                continue;
            }
            let count = u32le(b, 0x24) as usize;
            for j in 0..count {
                let e = 0x28 + j * 40;
                if e + 40 > img.bsz {
                    break;
                }
                if u32le(b, e) & 0xFFFF == TYPE_SPACEMAN && u64le(b, e + 24) == sm_oid {
                    let bno = u64le(b, e + 32);
                    return Some((bno, img.block(bno).to_vec()));
                }
            }
        }
        None
    }

    fn bit_is_set(bitmap: &[u8], bit: usize) -> bool {
        bitmap[bit / 8] & (1u8 << (bit % 8)) != 0
    }

    /// Everything the newest checkpoint has to satisfy, applied to any
    /// checkpoint. Panics with the failing detail.
    fn assert_coherent(img: &Image, cp: &Checkpoint, label: &str) {
        let sm = &cp.spaceman;
        let sm_xid = u64le(sm, 16);
        let addr_offset = u32le(sm, 80) as usize;
        let cib_count = u32le(sm, 64) as usize;
        let chunks_per_cib = u32le(sm, 40) as usize;
        let ip_block_count = u64le(sm, 152);
        let ip_bm_size = u32le(sm, 160) as usize;
        let ip_bm_base = u64le(sm, 168);
        let ip_base = u64le(sm, 176);
        let ip_bitmap_offset = u32le(sm, 328) as usize;

        // This checkpoint's own copy of the pool bitmap.
        assert_eq!(
            ip_bm_size, 1,
            "{label}: expected a single-block pool bitmap"
        );
        let pool_slot = u16le(sm, ip_bitmap_offset);
        let pool_bitmap = img.block(ip_bm_base + u64::from(pool_slot)).to_vec();
        let in_pool = |b: u64| b >= ip_base && b < ip_base + ip_block_count;
        let pool_says_used = |b: u64| bit_is_set(&pool_bitmap, (b - ip_base) as usize);

        for cib_idx in 0..cib_count {
            let cib_bno = u64le(sm, addr_offset + cib_idx * 8);
            if cib_bno == 0 {
                continue;
            }
            assert!(
                in_pool(cib_bno) && pool_says_used(cib_bno),
                "{label} (xid {}): chunk-info block {cib_bno} is not marked used in the \
                 pool bitmap this checkpoint names (ring slot {pool_slot}); the block it \
                 depends on is free as far as this checkpoint is concerned",
                cp.xid
            );
            let cib = img.block(cib_bno);
            let cib_xid = u64le(cib, 16);
            assert!(
                cib_xid <= sm_xid,
                "{label} (xid {}): chunk-info block {cib_bno} carries xid {cib_xid}, newer \
                 than the spaceman that names it ({sm_xid})",
                cp.xid
            );
            let chunk_count = u32le(cib, 36) as usize;
            assert!(chunk_count <= chunks_per_cib, "{label}: too many chunks");
            let mut max_chunk_xid = 0u64;
            for k in 0..chunk_count {
                let o = 40 + k * 32;
                let ci_xid = u64le(cib, o);
                let ci_addr = u64le(cib, o + 8);
                let ci_block_count = u32le(cib, o + 16);
                let ci_free_count = u32le(cib, o + 20);
                let ci_bitmap_addr = u64le(cib, o + 24);
                max_chunk_xid = max_chunk_xid.max(ci_xid);
                assert!(
                    ci_xid <= cib_xid,
                    "{label} (xid {}): chunk at {ci_addr} carries xid {ci_xid}, newer than \
                     its chunk-info block ({cib_xid})",
                    cp.xid
                );
                if ci_bitmap_addr == 0 {
                    assert_eq!(
                        ci_free_count, ci_block_count,
                        "{label} (xid {}): chunk at {ci_addr} has no bitmap, which states \
                         that all {ci_block_count} of its blocks are free, but its free \
                         count is {ci_free_count}",
                        cp.xid
                    );
                    continue;
                }
                assert!(
                    in_pool(ci_bitmap_addr) && pool_says_used(ci_bitmap_addr),
                    "{label} (xid {}): the bitmap of the chunk at {ci_addr} lives at \
                     {ci_bitmap_addr}, which the pool bitmap this checkpoint names (ring \
                     slot {pool_slot}) reports as free - a later transaction may hand that \
                     block to a file and destroy this checkpoint's allocation state",
                    cp.xid
                );
                let bitmap = img.block(ci_bitmap_addr);
                let used: u32 = bitmap.iter().map(|b| b.count_ones()).sum();
                assert_eq!(
                    ci_free_count,
                    ci_block_count - used,
                    "{label} (xid {}): the chunk at {ci_addr} claims {ci_free_count} free \
                     blocks, its bitmap at {ci_bitmap_addr} shows {}",
                    cp.xid,
                    ci_block_count - used
                );
            }
            assert_eq!(
                cib_xid, max_chunk_xid,
                "{label} (xid {}): chunk-info block {cib_bno} carries xid {cib_xid}, but the \
                 newest chunk it holds is from {max_chunk_xid}; a chunk-info block only \
                 changes when one of its chunks does",
                cp.xid
            );
        }
    }

    /// Every block a checkpoint's allocator state is made of: the spaceman, the
    /// chunk-info blocks it names, their chunks' bitmaps, and its copy of the
    /// internal-pool bitmap.
    fn allocator_blocks(img: &Image, cp: &Checkpoint) -> Vec<(u64, Vec<u8>)> {
        let sm = &cp.spaceman;
        let addr_offset = u32le(sm, 80) as usize;
        let cib_count = u32le(sm, 64) as usize;
        let ip_bm_base = u64le(sm, 168);
        let ip_bitmap_offset = u32le(sm, 328) as usize;
        let ip_bm_size = u32le(sm, 160) as usize;

        let mut out = vec![(cp.spaceman_bno, sm.clone())];
        for i in 0..ip_bm_size {
            let slot = u16le(sm, ip_bitmap_offset + i * 2);
            let bno = ip_bm_base + u64::from(slot);
            out.push((bno, img.block(bno).to_vec()));
        }
        for cib_idx in 0..cib_count {
            let cib_bno = u64le(sm, addr_offset + cib_idx * 8);
            if cib_bno == 0 {
                continue;
            }
            let cib = img.block(cib_bno);
            out.push((cib_bno, cib.to_vec()));
            for k in 0..u32le(cib, 36) as usize {
                let bitmap = u64le(cib, 40 + k * 32 + 24);
                if bitmap != 0 {
                    out.push((bitmap, img.block(bitmap).to_vec()));
                }
            }
        }
        out
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn a_commit_leaves_the_previous_checkpoints_allocator_blocks_alone() {
        if !gate() {
            println!("checkpoint-coherence: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let body = vec![0xA7u8; 64 * 1024];
        let write = |name: &str| {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, name, &body).expect("write");
            txn.commit().expect("commit");
        };

        // Two commits, so that the checkpoint sealed by the first is a real
        // predecessor rather than the formatter's.
        write("first.bin");
        write("second.bin");

        let img = Image::load();
        let before = {
            let cps = checkpoints(&img);
            let cp = cps.into_iter().next().expect("a newest checkpoint");
            (cp.xid, allocator_blocks(&img, &cp))
        };

        // The next commit must not reach into it. The previous checkpoint is
        // what a machine reads when the newest superblock is torn, so a block
        // it depends on changing underneath it means the fallback describes a
        // container that no longer exists.
        write("third.bin");

        let after = Image::load();
        let mut changed = Vec::new();
        for (bno, was) in &before.1 {
            if after.block(*bno) != was.as_slice() {
                changed.push(*bno);
            }
        }
        assert!(
            changed.is_empty(),
            "the commit rewrote blocks belonging to checkpoint {}: {changed:?}",
            before.0
        );
        println!(
            "checkpoint-untouched PASS: {} allocator blocks of xid {} survived the next commit",
            before.1.len(),
            before.0
        );
    }

    #[test]
    #[ignore = "scratch-image gated; writes 160 MiB"]
    fn the_checkpoint_before_the_newest_still_describes_a_coherent_container() {
        if !gate() {
            println!("checkpoint-coherence: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // Filling past the first chunk is what makes the allocator create a
        // bitmap out of the internal pool. Only one transaction in this run
        // does that, and it is the one after which its predecessor breaks, so
        // the check has to happen after every commit rather than at the end.
        let body: Vec<u8> = (0..FILE_BYTES).map(|i| (i % 251) as u8).collect();
        let mut checked = 0usize;
        for i in 0..FILE_COUNT {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, &format!("fill{i:02}.bin"), &body).expect("write");
            txn.commit().expect("commit");

            let img = Image::load();
            let cps = checkpoints(&img);
            assert!(
                cps.len() >= 2,
                "the ring must still hold the previous checkpoint, found {}",
                cps.len()
            );
            assert_coherent(&img, &cps[0], "newest checkpoint");
            assert_coherent(&img, &cps[1], "previous checkpoint");
            checked += 1;
        }
        assert_eq!(checked, FILE_COUNT);
        println!("checkpoint-coherence PASS: every commit left its predecessor coherent");
    }
}
