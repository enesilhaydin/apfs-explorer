//! Writing to a container whose chunk-info blocks are reached through
//! address blocks.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + diskutil; formats a
//! 9 TiB sparse image).
//!
//! Above roughly 8 TiB the space manager's array no longer lists the
//! chunk-info blocks but chunk-info address blocks, each listing chunk-info
//! blocks in turn. A write has to follow that indirection to reach a chunk's
//! description, and when it copies a chunk-info block it has to copy the
//! address block that names it as well: a physical object's identifier is its
//! address, so the new copy needs a new entry, and the address block carries
//! the transaction id of the newest chunk-info block under it. fsck_apfs
//! checks all of that, and macOS reading the files back is the other half of
//! the verdict.
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
mod cab_container_write {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{unlink, write_file};
    use apfs_write::txn::Transaction;
    use std::io::{Read, Seek, SeekFrom};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-cab-container-write.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// 9 TiB: about 590 chunk-info blocks, more than one address block lists,
    /// so the formatter has to use the indirection.
    const IMAGE_BYTES: u64 = 9 * 1024 * 1024 * 1024 * 1024;
    const OBJECT_TYPE_SPACEMAN_CAB: u32 = 0x6;
    const OBJECT_TYPE_SPACEMAN_CIB: u32 = 0x7;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| ((i as u64 * 7 + u64::from(seed) * 131) % 251) as u8)
            .collect()
    }

    /// A sparse file formatted by macOS itself.
    fn make_image() {
        let _ = std::fs::remove_file(IMG);
        let f = std::fs::File::create(IMG).expect("create image");
        f.set_len(IMAGE_BYTES).expect("sparse extend");
        drop(f);
        let disk = attach(&["-nomount"]);
        let part = Command::new("diskutil")
            .args(["partitionDisk", &disk, "GPT", "APFS", "Huge", "100%"])
            .output()
            .expect("diskutil partitionDisk");
        detach(&disk);
        assert!(
            part.status.success(),
            "diskutil partitionDisk failed:\n{}",
            String::from_utf8_lossy(&part.stderr)
        );
    }

    /// Attach the image and return the whole-disk node.
    fn attach(extra: &[&str]) -> String {
        attach_output(extra)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string()
    }

    fn attach_output(extra: &[&str]) -> String {
        let mut args = vec![
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nobrowse",
        ];
        args.extend_from_slice(extra);
        args.push(IMG);
        let out = Command::new("hdiutil")
            .args(&args)
            .output()
            .expect("hdiutil attach");
        assert!(
            out.status.success(),
            "hdiutil attach failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn detach(disk: &str) {
        for attempt in 0..8 {
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

    /// One address block as the independent decoder sees it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Cab {
        paddr: u64,
        xid: u64,
        index: u32,
        cib_count: u32,
        /// (address, transaction id) of every chunk-info block it lists.
        cibs: Vec<(u64, u64)>,
    }

    /// The newest checkpoint's space manager, decoded with no writer code:
    /// `(checkpoint xid, spaceman bytes, address blocks)`.
    fn newest_spaceman() -> (u64, Vec<u8>, Vec<Cab>) {
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
            if u32le(&b, 0x18) & 0xFFFF == 1 && u32le(&b, 32) == 0x4253_584E {
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
        let mut mapping = None;
        'desc: for i in 0..desc_len {
            let b = rd(desc_base + (desc_index + i) % desc_blocks, bsz);
            if u32le(&b, 0x18) & 0xFFFF != 0x0C {
                continue;
            }
            for j in 0..u32le(&b, 0x24) as usize {
                let e = 0x28 + j * 40;
                if u32le(&b, e) & 0xFFFF == 5 && u64le(&b, e + 24) == sm_oid {
                    mapping = Some((u64le(&b, e + 32), u32le(&b, e + 8) as usize));
                    break 'desc;
                }
            }
        }
        let (paddr, size) = mapping.expect("newest checkpoint has no spaceman mapping");
        let mut sm = Vec::with_capacity(size);
        for i in 0..(size / bsz) as u64 {
            let slot = (paddr - data_base + i) % data_blocks;
            sm.extend_from_slice(&rd(data_base + slot, bsz));
        }
        // sm_dev[0]: sm_cab_count @68, sm_addr_offset @80.
        let cab_count = u32le(&sm, 68) as usize;
        let addr_offset = u32le(&sm, 80) as usize;
        let mut cabs = Vec::with_capacity(cab_count);
        for i in 0..cab_count {
            let cab_paddr = u64le(&sm, addr_offset + i * 8);
            let b = rd(cab_paddr, bsz);
            assert_eq!(
                u32le(&b, 0x18) & 0xFFFF,
                OBJECT_TYPE_SPACEMAN_CAB,
                "entry {i} of the address array at block {cab_paddr} is not an address block"
            );
            let cib_count = u32le(&b, 36);
            let cibs = (0..cib_count as usize)
                .map(|k| {
                    let cib_paddr = u64le(&b, 40 + k * 8);
                    let cib = rd(cib_paddr, bsz);
                    assert_eq!(
                        u32le(&cib, 0x18) & 0xFFFF,
                        OBJECT_TYPE_SPACEMAN_CIB,
                        "address block {i} entry {k} at block {cib_paddr} is not a chunk-info block"
                    );
                    (cib_paddr, u64le(&cib, 16))
                })
                .collect();
            cabs.push(Cab {
                paddr: cab_paddr,
                xid: u64le(&b, 16),
                index: u32le(&b, 32),
                cib_count,
                cibs,
            });
        }
        (xid, sm, cabs)
    }

    /// What fsck_apfs demands of an address block, applied by hand.
    fn check_cabs(label: &str, sm_xid: u64, cabs: &[Cab]) {
        let cibs_per_cab = u32le(&newest_spaceman().1, 44);
        for (i, cab) in cabs.iter().enumerate() {
            assert_eq!(
                cab.index, i as u32,
                "{label}: address block {i} carries index {}",
                cab.index
            );
            assert!(
                cab.xid <= sm_xid,
                "{label}: address block {i} is newer ({}) than the space manager ({sm_xid})",
                cab.xid
            );
            let newest_cib = cab.cibs.iter().map(|(_, x)| *x).max().unwrap_or(0);
            assert_eq!(
                cab.xid, newest_cib,
                "{label}: address block {i} must carry the newest of its chunk-info blocks' xids"
            );
            assert!(
                cab.cib_count <= cibs_per_cab,
                "{label}: address block {i} overfull"
            );
            if i + 1 < cabs.len() {
                assert_eq!(
                    cab.cib_count, cibs_per_cab,
                    "{label}: only the last address block may be partly filled"
                );
            }
        }
    }

    fn run_fsck(label: &str) {
        let disk = attach(&["-nomount"]);
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

    /// macOS mounts the image read-only and reads every file back.
    fn macos_reads(expected: &[(&str, Vec<u8>)]) {
        let out = attach_output(&["-readonly"]);
        let disk = out
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
        let mount = out
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .expect("no mount point");
        let mut failures = Vec::new();
        for (name, body) in expected {
            match std::fs::read(format!("{mount}/{name}")) {
                Ok(got) if &got == body => {}
                Ok(got) => failures.push(format!(
                    "{name}: {} bytes, expected {}",
                    got.len(),
                    body.len()
                )),
                Err(e) => failures.push(format!("{name}: {e}")),
            }
        }
        let listing: Vec<String> = std::fs::read_dir(&mount)
            .expect("read_dir")
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| !n.starts_with('.'))
            .collect();
        detach(&disk);
        assert!(
            failures.is_empty(),
            "macOS could not read back:\n{}",
            failures.join("\n")
        );
        assert_eq!(listing.len(), expected.len(), "macOS lists {listing:?}");
    }

    #[test]
    #[ignore = "scratch-image gated; formats a 9 TiB sparse image on macOS"]
    fn files_are_written_through_the_address_blocks() {
        if !gate() {
            return;
        }
        make_image();

        let (xid0, sm0, cabs0) = newest_spaceman();
        assert!(
            cabs0.len() >= 2,
            "the formatter must have used address blocks for this test to mean anything, \
             got {} (sm_cab_count {})",
            cabs0.len(),
            u32le(&sm0, 68)
        );
        check_cabs("as formatted", xid0, &cabs0);
        run_fsck("as formatted");

        // Five transactions: three files, one removal, one more file. The
        // allocations land in the first chunks, whose chunk-info block sits
        // under the first address block; every commit copies that block anew.
        let small = pattern(24 * 1024, 1);
        let big = pattern(1024 * 1024 + 12_345, 2);
        let mid = pattern(200 * 1024, 3);
        let last = pattern(64 * 1024, 4);
        in_txn(|t, v, o| {
            write_file(t, v, o, 2, "small.bin", &small).expect("write small");
        });
        in_txn(|t, v, o| {
            write_file(t, v, o, 2, "big.bin", &big).expect("write big");
        });
        in_txn(|t, v, o| {
            write_file(t, v, o, 2, "mid.bin", &mid).expect("write mid");
        });
        in_txn(|t, v, o| {
            unlink(t, v, o, 2, "big.bin").expect("unlink big");
        });
        in_txn(|t, v, o| {
            write_file(t, v, o, 2, "last.bin", &last).expect("write last");
        });

        let (xid1, sm1, cabs1) = newest_spaceman();
        assert!(
            xid1 > xid0,
            "five commits must advance the checkpoint ({xid0} -> {xid1})"
        );
        assert_eq!(
            u32le(&sm1, 68),
            u32le(&sm0, 68),
            "the address block count must not change"
        );
        check_cabs("after five commits", xid1, &cabs1);
        let rewritten: Vec<usize> = cabs1
            .iter()
            .enumerate()
            .filter(|(i, c)| c.xid == xid1 && cabs0[*i].paddr != c.paddr)
            .map(|(i, _)| i)
            .collect();
        assert!(
            !rewritten.is_empty(),
            "an address block naming a copied chunk-info block must itself have been copied \
             under the newest transaction: before {cabs0:?}\nafter {cabs1:?}"
        );
        for (i, c) in cabs1.iter().enumerate() {
            if c.xid != xid1 {
                assert_eq!(
                    c, &cabs0[i],
                    "an untouched address block must be left as it was"
                );
            }
        }

        let mut fs = apfs::FsView::open(open_dev()).expect("FsView::open");
        for (name, body) in [
            ("small.bin", &small),
            ("mid.bin", &mid),
            ("last.bin", &last),
        ] {
            let got = fs.read(&format!("/{name}"), 0, body.len()).expect("read");
            assert!(got == *body, "{name} must read back through our reader");
        }
        assert!(
            fs.getattr("/big.bin").expect("getattr").is_none(),
            "big.bin was removed"
        );
        drop(fs);

        run_fsck("after five commits");
        macos_reads(&[
            ("small.bin", small.clone()),
            ("mid.bin", mid.clone()),
            ("last.bin", last.clone()),
        ]);
        run_fsck("after macOS read it");
        println!(
            "cab-container-write PASS: {} address blocks, {} rewritten, checkpoint {xid0} -> {xid1}",
            cabs1.len(),
            rewritten.len()
        );
        let _ = std::fs::remove_file(IMG);
    }
}
