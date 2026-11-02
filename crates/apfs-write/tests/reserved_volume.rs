//! Writing into a volume that has a reserve.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch, diskutil
//! and fsck).
//!
//! A volume created with a reserve size (Disk Utility's "Reserve Size",
//! `diskutil apfs addVolume -reserve`) records it in its superblock, and the
//! space manager keeps two sums over such volumes: the reserved blocks and
//! the blocks they hold. A commit that changes a reserved volume's block
//! count has to move the space manager's sum by the same amount, or the
//! checker reports the space manager.
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
mod reserved_volume {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{unlink, write_file};
    use apfs_write::txn::Transaction;
    use std::io::{Read, Seek, SeekFrom};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-reserved-volume.img";
    const IMAGE_MB: &str = "128";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// `apfs_superblock_t`: `apfs_fs_reserve_block_count` and `apfs_fs_alloc_count`.
    const VSB_RESERVE_BLOCK_COUNT: usize = 0x48;
    const VSB_ALLOC_COUNT: usize = 0x58;
    /// `spaceman_phys`: `sm_fs_reserve_block_count` and `sm_fs_reserve_alloc_count`.
    const SM_FS_RESERVE_BLOCK_COUNT: usize = 184;
    const SM_FS_RESERVE_ALLOC_COUNT: usize = 192;
    const FILE_BYTES: usize = 2 * 1024 * 1024;

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
    fn whole_disk(attached: &str) -> String {
        attached
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string()
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
    fn attach_nomount() -> (String, String) {
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
        (disk, part)
    }
    /// macOS formats the container's one volume with a reserve (a scratch
    /// container this small allows a single volume, so the reserve goes on
    /// it at format time rather than on an added one).
    fn add_reserved_volume() {
        let (disk, part) = attach_nomount();
        let out = Command::new("newfs_apfs")
            .args(["-r", "16m", "-v", "Reserved", &part])
            .output()
            .expect("newfs_apfs");
        let _ = Command::new("sync").status();
        detach(&disk);
        assert!(
            out.status.success(),
            "newfs_apfs -r failed:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
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
    /// The volume superblock and omap of the volume named `name`.
    fn read_vsb_omap(name: &str) -> (Vec<u8>, Vec<u8>) {
        let mut dev = open_dev();
        let c = Container::open(&mut dev).unwrap();
        let nx = &c.superblock;
        let bsz = nx.block_size as usize;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).unwrap();
        for fs_oid in &nx.fs_oids {
            let vp = omap
                .resolve(&mut dev, *fs_oid, nx.obj.xid, nx.block_size)
                .unwrap()
                .unwrap();
            let mut vsb = vec![0u8; bsz];
            dev.read_at(vp * bsz as u64, &mut vsb).unwrap();
            let v = VolumeSuperblock::parse(&vsb).unwrap();
            if v.name != name {
                continue;
            }
            let mut om = vec![0u8; bsz];
            dev.read_at(v.omap_oid * bsz as u64, &mut om).unwrap();
            return (vsb, om);
        }
        panic!("no volume named {name}");
    }
    fn in_txn(name: &str, f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8])) {
        let (vsb, om) = read_vsb_omap(name);
        let mut txn = Transaction::begin(open_dev()).expect("begin");
        f(&mut txn, &vsb, &om);
        txn.commit().expect("commit");
    }
    fn u32le(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
    }
    fn u64le(b: &[u8], o: usize) -> u64 {
        u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
    }
    /// The newest checkpoint's spaceman reserve sums, decoded with no writer
    /// code: `(sm_fs_reserve_block_count, sm_fs_reserve_alloc_count)`.
    fn spaceman_reserve() -> (u64, u64) {
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
        let data_blocks = u64::from(u32le(&nx0, 0x6C));
        let desc_base = u64le(&nx0, 0x70);
        let data_base = u64le(&nx0, 0x78);
        let mut newest: Option<(u64, Vec<u8>)> = None;
        for i in 0..desc_blocks {
            let b = rd(desc_base + i, bsz);
            if u32le(&b, 0x18) & 0xFFFF == 1
                && u32le(&b, 32) == 0x4253_584E
                && apfs_core::checksum::verify_block(&b).is_ok()
            {
                let xid = u64le(&b, 16);
                if newest.as_ref().is_none_or(|(x, _)| xid > *x) {
                    newest = Some((xid, b));
                }
            }
        }
        let (_, nx) = newest.expect("a superblock in the ring");
        let desc_index = u64::from(u32le(&nx, 0x88));
        let desc_len = u64::from(u32le(&nx, 0x8C));
        let sm_oid = u64le(&nx, 0x98);
        for i in 0..desc_len {
            let b = rd(desc_base + (desc_index + i) % desc_blocks, bsz);
            if u32le(&b, 0x18) & 0xFFFF != 0xC {
                continue;
            }
            for j in 0..u32le(&b, 0x24) as usize {
                let e = 0x28 + j * 40;
                if u32le(&b, e) & 0xFFFF == 5 && u64le(&b, e + 24) == sm_oid {
                    let (paddr, size) = (u64le(&b, e + 32), u32le(&b, e + 8) as usize);
                    let mut sm = Vec::with_capacity(size);
                    for k in 0..(size / bsz) as u64 {
                        let slot = (paddr - data_base + k) % data_blocks;
                        sm.extend_from_slice(&rd(data_base + slot, bsz));
                    }
                    return (
                        u64le(&sm, SM_FS_RESERVE_BLOCK_COUNT),
                        u64le(&sm, SM_FS_RESERVE_ALLOC_COUNT),
                    );
                }
            }
        }
        panic!("newest checkpoint has no spaceman mapping");
    }
    fn run_fsck(label: &str) {
        let (disk, part) = attach_nomount();
        let rdev = part.replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        detach(&disk);
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
    #[ignore = "scratch-image gated; needs xtask gen-scratch, diskutil and fsck_apfs on macOS"]
    fn a_reserved_volumes_block_count_moves_the_space_managers_sum() {
        if !gate() {
            return;
        }
        gen_scratch();
        add_reserved_volume();
        run_fsck("as macOS made it");
        let (vsb, _) = read_vsb_omap("Reserved");
        let reserve = u64le(&vsb, VSB_RESERVE_BLOCK_COUNT);
        let alloc0 = u64le(&vsb, VSB_ALLOC_COUNT);
        let (sm_reserve0, sm_alloc0) = spaceman_reserve();
        println!(
            "reserved-volume: reserve {reserve} blocks, volume holds {alloc0}; space manager sums \
             {sm_reserve0} reserved / {sm_alloc0} held"
        );
        assert!(
            reserve > 0,
            "the volume must carry a reserve for this test to mean anything"
        );
        assert_eq!(
            sm_reserve0, reserve,
            "the space manager's reserve sum is this volume's reserve"
        );
        assert_eq!(
            sm_alloc0, alloc0,
            "the space manager's held sum is this volume's count"
        );

        let body: Vec<u8> = (0..FILE_BYTES).map(|i| (i % 251) as u8).collect();
        in_txn("Reserved", |t, v, o| {
            write_file(t, v, o, 2, "held.bin", &body).expect("write");
        });
        let (vsb, _) = read_vsb_omap("Reserved");
        let alloc1 = u64le(&vsb, VSB_ALLOC_COUNT);
        let (sm_reserve1, sm_alloc1) = spaceman_reserve();
        assert!(alloc1 > alloc0, "the file takes blocks from the volume");
        assert_eq!(sm_reserve1, reserve, "the reserve itself does not move");
        assert_eq!(
            sm_alloc1, alloc1,
            "the space manager's held sum must follow the volume's count ({alloc0} -> {alloc1})"
        );
        run_fsck("after writing into the reserved volume");

        in_txn("Reserved", |t, v, o| {
            unlink(t, v, o, 2, "held.bin").expect("unlink");
        });
        let (vsb, _) = read_vsb_omap("Reserved");
        let alloc2 = u64le(&vsb, VSB_ALLOC_COUNT);
        let (_, sm_alloc2) = spaceman_reserve();
        assert_eq!(sm_alloc2, alloc2, "and follow it back down");
        run_fsck("after removing the file again");
        let _ = std::fs::remove_file(IMG);
    }
}
