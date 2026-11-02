//! Hundreds of removals behind a snapshot, then the snapshot's deletion.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! While a snapshot exists the blocks of a removed file stay with the
//! snapshot; the live volume only drops the names. Deleting the snapshot then
//! releases everything at once. Apple's fsck judges the container after each
//! stage, and the test prints the shape of the space manager's main free
//! queue at the two points where it could have grown, so a change in how
//! releases are queued shows up here.
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
mod free_queue_growth {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{unlink, write_file};
    use apfs_write::snapshot::{create_snapshot, delete_snapshot};
    use apfs_write::txn::Transaction;
    use std::io::Read;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-free-queue-growth.img";
    const IMAGE_MB: &str = "128";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// Enough removals to fill more than one queue leaf if each were queued.
    const FILES: usize = 320;
    const FILE_BYTES: usize = 8 * 1024;

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
    fn in_txn(f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8])) {
        let (vsb, om) = read_vsb_omap();
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

    /// The main free queue of the newest checkpoint, decoded with no writer
    /// code: `(sfq_count, root level, root key count)`.
    fn main_free_queue() -> (u64, u16, u32) {
        const TYPE_NX_SUPERBLOCK: u32 = 0x1;
        const TYPE_SPACEMAN: u32 = 0x5;
        const TYPE_CHECKPOINT_MAP: u32 = 0xC;
        const NX_MAGIC: u32 = 0x4253_584E;
        // spaceman_phys: sm_fq[SFQ_MAIN] at 240 (sfq_count, then sfq_tree_oid).
        const SFQ_MAIN_COUNT: usize = 240;
        const SFQ_MAIN_TREE_OID: usize = 248;
        let (base, _) = find_apfs_range();
        let mut f = std::fs::File::open(IMG).unwrap();
        let mut rd = |bno: u64, bsz: usize| {
            use std::io::{Seek, SeekFrom};
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
        let (_, nx) = newest.expect("a superblock in the ring");
        let desc_index = u64::from(u32le(&nx, 0x88));
        let desc_len = u64::from(u32le(&nx, 0x8C));
        let sm_oid = u64le(&nx, 0x98);
        // Every ephemeral mapping of the checkpoint: oid -> (paddr, size).
        let mut mappings: Vec<(u64, u64, usize)> = Vec::new();
        for i in 0..desc_len {
            let b = rd(desc_base + (desc_index + i) % desc_blocks, bsz);
            if u32le(&b, 0x18) & 0xFFFF != TYPE_CHECKPOINT_MAP {
                continue;
            }
            for j in 0..u32le(&b, 0x24) as usize {
                let e = 0x28 + j * 40;
                mappings.push((
                    u64le(&b, e + 24),
                    u64le(&b, e + 32),
                    u32le(&b, e + 8) as usize,
                ));
            }
        }
        let mut read_ephemeral = |oid: u64| -> Vec<u8> {
            let (_, paddr, size) = *mappings
                .iter()
                .find(|(o, _, _)| *o == oid)
                .unwrap_or_else(|| panic!("no mapping for ephemeral {oid}"));
            let mut obj = Vec::with_capacity(size);
            for k in 0..(size / bsz) as u64 {
                let slot = (paddr - data_base + k) % data_blocks;
                obj.extend_from_slice(&rd(data_base + slot, bsz));
            }
            obj
        };
        let sm = read_ephemeral(sm_oid);
        assert_eq!(u32le(&sm, 0x18) & 0xFFFF, TYPE_SPACEMAN);
        let count = u64le(&sm, SFQ_MAIN_COUNT);
        let tree_oid = u64le(&sm, SFQ_MAIN_TREE_OID);
        let root = read_ephemeral(tree_oid);
        let level = u16::from_le_bytes(root[34..36].try_into().unwrap());
        let nkeys = u32le(&root, 36);
        (count, level, nkeys)
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
    fn removing_hundreds_of_files_behind_a_snapshot_and_deleting_it_stays_clean() {
        if !gate() {
            return;
        }
        gen_scratch();
        let body: Vec<u8> = (0..FILE_BYTES).map(|i| (i % 251) as u8).collect();
        for i in 0..FILES {
            let name = format!("f{i:03}.bin");
            in_txn(|t, v, o| {
                write_file(t, v, o, 2, &name, &body).expect("write");
            });
        }
        run_fsck("after the files");
        let mut snap_xid = 0;
        in_txn(|t, v, o| {
            snap_xid = create_snapshot(t, v, o, "pins-them").expect("create_snapshot");
        });
        run_fsck("after the snapshot");

        // Judge the container at several points along the removals.
        let checkpoints = [100usize, 160, 220, FILES];
        for i in 0..FILES {
            let name = format!("f{i:03}.bin");
            in_txn(|t, v, o| {
                unlink(t, v, o, 2, &name).unwrap_or_else(|e| panic!("unlink {name}: {e}"));
            });
            if checkpoints.contains(&(i + 1)) {
                run_fsck(&format!("after {} removals behind the snapshot", i + 1));
            }
        }

        let mut fs = apfs::FsView::open(open_dev()).expect("FsView::open");
        let left = fs.read_dir("/").expect("read_dir").len();
        drop(fs);
        assert_eq!(left, 0, "every file is gone from the live volume");
        let (count, level, root_keys) = main_free_queue();
        println!(
            "free-queue-growth: with the files gone the main queue holds {count} blocks, \
             root level {level} with {root_keys} keys"
        );

        in_txn(|t, v, o| {
            delete_snapshot(t, v, o, snap_xid).expect("delete_snapshot");
        });
        run_fsck("after deleting the snapshot");
        let (count, level, root_keys) = main_free_queue();
        println!(
            "free-queue-growth: with the snapshot gone the main queue holds {count} blocks, \
             root level {level} with {root_keys} keys"
        );
        // A further commit drains what the deletion released.
        in_txn(|t, v, o| {
            write_file(t, v, o, 2, "after.bin", &body).expect("write after");
        });
        run_fsck("after the queue drained");
        let _ = std::fs::remove_file(IMG);
    }
}
