//! Committing on a container whose spaceman is larger than one block.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, diskutil, fsck).
//!
//! The spaceman carries the address of every chunk-info block, and a
//! container of a few terabytes has more of those than fit in one block
//! behind the fixed fields. The formatter then makes the spaceman two blocks
//! long, says so in the checkpoint mapping's size field, checksums the whole
//! object, and counts `nx_xp_data_len` in blocks. Reading one block of it
//! loses the tail of the address array; writing one block back and declaring
//! it whole destroys the allocator metadata of the newest checkpoint, which
//! is the one nothing can fall back from.
//!
//! The image is a sparse file, so the container costs a few megabytes of real
//! disk however large it says it is.
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
mod multiblock_spaceman {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::write_file;
    use apfs_write::txn::Transaction;
    use std::io::{Read, Seek, SeekFrom};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-multiblock-spaceman.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// 3.5 TiB: about 28000 chunks, 223 chunk-info blocks, and a spaceman the
    /// formatter has to make two blocks long.
    const IMAGE_BYTES: u64 = 3500 * 1024 * 1024 * 1024;
    const NAME: &str = "on_a_large_container.bin";

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    /// A sparse file formatted by macOS itself.
    fn make_image() {
        let _ = std::fs::remove_file(IMG);
        let f = std::fs::File::create(IMG).expect("create image");
        f.set_len(IMAGE_BYTES).expect("sparse extend");
        drop(f);
        let disk = attach_nomount();
        let part = Command::new("diskutil")
            .args(["partitionDisk", &disk, "GPT", "APFS", "Large", "100%"])
            .output()
            .expect("diskutil partitionDisk");
        detach(&disk);
        assert!(
            part.status.success(),
            "diskutil partitionDisk failed:\n{}",
            String::from_utf8_lossy(&part.stderr)
        );
    }

    fn attach_nomount() -> String {
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
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string()
    }

    fn detach(disk: &str) {
        let _ = Command::new("hdiutil").args(["detach", disk]).status();
    }

    /// The image is far too large to read whole; the partition table sits in
    /// the first few sectors.
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
    fn open_txn() -> Transaction<Dev> {
        Transaction::begin(open_dev()).unwrap()
    }
    fn open_fs() -> apfs::FsView<Dev> {
        apfs::FsView::open(open_dev()).expect("FsView::open")
    }

    /// The newest checkpoint's spaceman mapping: `(cme_size, nx_xp_data_len)`.
    /// Decoded independently of the writer.
    fn newest_spaceman_mapping() -> (u32, u32) {
        let (base, _) = find_apfs_range();
        let mut f = std::fs::File::open(IMG).unwrap();
        let mut rd = |bno: u64, bsz: usize| {
            let mut b = vec![0u8; bsz];
            f.seek(SeekFrom::Start(base + bno * bsz as u64)).unwrap();
            f.read_exact(&mut b).unwrap();
            b
        };
        let u32le = |b: &[u8], o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        let u64le = |b: &[u8], o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let nx0 = rd(0, 4096);
        let bsz = u32le(&nx0, 36) as usize;
        let desc_blocks = (u32le(&nx0, 0x68) & 0x7FFF_FFFF) as u64;
        let desc_base = u64le(&nx0, 0x70);
        let mut newest: Option<(u64, Vec<u8>)> = None;
        for i in 0..desc_blocks {
            let b = rd(desc_base + i, bsz);
            if u32le(&b, 0x18) & 0xFFFF == 1 && u32le(&b, 32) == 0x4253_584E {
                let xid = u64le(&b, 16);
                if newest.as_ref().is_none_or(|(x, _)| xid > *x) {
                    newest = Some((xid, b));
                }
            }
        }
        let (_, nx) = newest.expect("a superblock in the ring");
        let data_len = u32le(&nx, 0x94);
        let desc_index = u64::from(u32le(&nx, 0x88));
        let desc_len = u64::from(u32le(&nx, 0x8C));
        let sm_oid = u64le(&nx, 0x98);
        for i in 0..desc_len {
            let b = rd(desc_base + (desc_index + i) % desc_blocks, bsz);
            if u32le(&b, 0x18) & 0xFFFF != 0x0C {
                continue;
            }
            for j in 0..u32le(&b, 0x24) as usize {
                let e = 0x28 + j * 40;
                if u32le(&b, e) & 0xFFFF == 5 && u64le(&b, e + 24) == sm_oid {
                    return (u32le(&b, e + 8), data_len);
                }
            }
        }
        panic!("newest checkpoint has no spaceman mapping");
    }

    fn run_fsck(label: &str) {
        let disk = attach_nomount();
        let rdev = format!("{disk}s2").replacen("/dev/disk", "/dev/rdisk", 1);
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
    #[ignore = "scratch-image gated; formats a 3.5 TiB sparse image on macOS"]
    fn a_two_block_spaceman_survives_a_commit() {
        if !gate() {
            println!("multiblock-spaceman: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        make_image();

        let (size_before, len_before) = newest_spaceman_mapping();
        assert!(
            size_before > 4096,
            "the formatter must have produced a multi-block spaceman for this test to \
             mean anything, got {size_before} bytes"
        );
        run_fsck("as formatted");

        let body = vec![0x5Au8; 24 * 1024];
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, NAME, &body).expect("write");
            txn.commit().expect("commit");
        }

        // The mapping we wrote must describe the object we wrote: the same
        // size the formatter used, and a data length counted in blocks.
        let (size_after, len_after) = newest_spaceman_mapping();
        assert_eq!(
            size_after, size_before,
            "the spaceman mapping must keep the object's real size"
        );
        assert_eq!(
            len_after, len_before,
            "nx_xp_data_len counts blocks, and the same objects were written"
        );

        let mut fs = open_fs();
        let got = fs.read(&format!("/{NAME}"), 0, body.len()).expect("read");
        assert_eq!(got, body, "the file must read back");
        drop(fs);

        run_fsck("after one commit");
        println!("multiblock-spaceman PASS: spaceman of {size_after} bytes survived a commit");
    }
}
