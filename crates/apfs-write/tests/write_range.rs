//! Writing a byte range of an existing file without rewriting the file.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, cp -c, truncate,
//! fsck_apfs).
//!
//! macOS writes the files: a plain one, a short one, a sparse one, an empty
//! one and a clone pair sharing one data stream. Ranges are then written
//! into them one transaction at a time and, in a second phase, many to a
//! transaction (small appends to one file, scattered overwrites of
//! another). After each phase fsck must pass without warnings, and both our
//! reader and macOS must read every file as a model of the writes says.
//! Only the blocks a write touches are written: the rest of the file keeps
//! its blocks, a clone made by `clone_file` keeps its content, small
//! appends inside one transaction extend one extent, and a stream macOS
//! shares between two files is declined (the caller rewrites the file).
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
mod write_range {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        clone_file, create_file, truncate_file_fast, unshare_stream, write_range, RangeWrite,
        TruncateOutcome,
    };
    use apfs_write::snapshot::create_snapshot;
    use apfs_write::txn::{Transaction, TxnError};
    use std::collections::BTreeMap;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-write-range.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ROOT: u64 = 2;
    const BIG: usize = 1 << 20;
    const SPARSE: usize = 3 << 20;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn pattern(n: usize, seed: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 7 + seed * 131) % 251) as u8).collect()
    }

    /// What every file should hold.
    type Model = BTreeMap<&'static str, Vec<u8>>;

    /// Apply a write to the model the way POSIX does: a gap reads as zeros.
    fn model_write(m: &mut Model, name: &'static str, offset: usize, data: &[u8]) {
        let f = m.entry(name).or_default();
        if f.len() < offset + data.len() {
            f.resize(offset + data.len(), 0);
        }
        f[offset..offset + data.len()].copy_from_slice(data);
    }

    fn build_image() -> Model {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-write-range.dmg");
        let out = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "64m",
                "-layout",
                "GPTSPUD",
                "-fs",
                "APFS",
                "-volname",
                "RANGE",
                "/tmp/apfs-write-range",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-write-range.dmg", IMG).expect("rename");

        let mut m = Model::new();
        let (mount, parent) = attach(true);
        let put = |m: &mut Model, name: &'static str, body: Vec<u8>| {
            std::fs::write(format!("{mount}/{name}"), &body).expect("write");
            m.insert(name, body);
        };
        put(&mut m, "big.bin", pattern(BIG, 1));
        put(&mut m, "short.txt", b"hello world".to_vec());
        put(&mut m, "empty.bin", Vec::new());
        put(&mut m, "shared_src.bin", pattern(200 * 1024, 2));
        // A sparse file: macOS records the holes as extents without blocks.
        let st = Command::new("truncate")
            .args(["-s", &SPARSE.to_string(), &format!("{mount}/sparse.bin")])
            .status()
            .expect("truncate");
        assert!(st.success(), "truncate failed");
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(format!("{mount}/sparse.bin"))
                .expect("open sparse");
            f.seek(SeekFrom::Start(1 << 20)).expect("seek");
            f.write_all(&pattern(4096, 3)).expect("write sparse");
        }
        let mut sparse = vec![0u8; SPARSE];
        sparse[1 << 20..(1 << 20) + 4096].copy_from_slice(&pattern(4096, 3));
        m.insert("sparse.bin", sparse);
        let st = Command::new("cp")
            .args([
                "-c",
                &format!("{mount}/shared_src.bin"),
                &format!("{mount}/shared.bin"),
            ])
            .status()
            .expect("cp -c");
        assert!(st.success(), "cp -c failed");
        m.insert("shared.bin", m["shared_src.bin"].clone());
        let _ = Command::new("sync").status();
        detach(&parent);
        m
    }

    fn attach(mount_it: bool) -> (String, String) {
        let mut args = vec![
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nobrowse",
        ];
        if !mount_it {
            args.push("-nomount");
        }
        args.push(IMG);
        let out = Command::new("hdiutil")
            .args(&args)
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .unwrap_or_default();
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
        (mount, parent)
    }

    fn detach(parent: &str) {
        for _ in 0..5 {
            if Command::new("hdiutil")
                .args(["detach", parent])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let _ = Command::new("hdiutil")
            .args(["detach", "-force", parent])
            .status();
    }

    fn open_dev() -> Dev {
        let mut head = vec![0u8; 64 * 1024];
        std::io::Read::read_exact(&mut std::fs::File::open(IMG).unwrap(), &mut head).unwrap();
        let hdr = &head[512..1024];
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let e = &head[base + i * sz..base + (i + 1) * sz];
            if e[..16] == APFS_GPT_TYPE {
                let s = u64::from_le_bytes(e[32..40].try_into().unwrap());
                let en = u64::from_le_bytes(e[40..48].try_into().unwrap());
                let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
                return WritableOffsetBlockDevice::new(inner, s * SECTOR, (en - s + 1) * SECTOR);
            }
        }
        panic!("no APFS partition");
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

    type Op<'a> = dyn FnMut(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<(), TxnError> + 'a;

    /// Run `f` in one committed transaction.
    fn txn(f: &mut Op<'_>) {
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).expect("begin");
        f(&mut t, &vsb, &om).expect("operation");
        t.commit().expect("commit");
    }

    fn written(r: Result<RangeWrite, TxnError>) {
        assert_eq!(r.expect("write_range"), RangeWrite::Written);
    }

    fn fsck(label: &str) {
        let (_, parent) = attach(false);
        let dev = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let out = Command::new("fsck_apfs").args(["-n", &dev]).output();
        detach(&parent);
        let out = out.expect("fsck_apfs");
        let report = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
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
            out.status.success() && complaints.is_empty(),
            "{label}: fsck_apfs:\n{report}"
        );
    }

    /// Our reader and macOS both read every file of `m` as `m` says.
    fn check(m: &Model, label: &str) {
        fsck(label);
        let mut fs = apfs::FsView::open(open_dev()).expect("FsView::open");
        for (name, want) in m {
            let path = format!("/{name}");
            let size = fs.getattr(&path).expect("getattr").expect("present").size;
            assert_eq!(size as usize, want.len(), "{label}: our size of {name}");
            let got = fs.read(&path, 0, want.len()).expect("read");
            assert!(got == *want, "{label}: our reader differs on {name}");
        }
        let (mount, parent) = attach(true);
        for (name, want) in m {
            let got = std::fs::read(format!("{mount}/{name}")).expect("macOS read");
            assert!(got == *want, "{label}: macOS reads {name} differently");
        }
        detach(&parent);
    }

    fn extents(name: &str) -> usize {
        let mut fs = apfs::FsView::open(open_dev()).expect("FsView::open");
        fs.file_extents(&format!("/{name}")).expect("extents").len()
    }

    #[test]
    #[ignore = "macOS only: hdiutil + fsck_apfs"]
    fn ranges_land_where_they_were_written() {
        if !gate() {
            println!("write_range: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let _ = std::fs::remove_file(IMG);
        let mut m = build_image();

        // One write to a transaction.
        let writes: Vec<(&'static str, usize, Vec<u8>)> = vec![
            // Inside one extent, across a block boundary, unaligned.
            ("big.bin", 100_000, pattern(4096, 4)),
            // One byte past the end.
            ("big.bin", BIG, vec![0xA5]),
            // Past the end with a gap: the gap reads as zeros.
            ("big.bin", 2 * BIG + 3, pattern(10, 5)),
            // Inside the last, partly used block; then past the end inside it.
            ("short.txt", 3, b"XY".to_vec()),
            ("short.txt", 20, b"Z".to_vec()),
            // Into a hole, across its edge with the data, and at the start.
            ("sparse.bin", (2 << 20) - 4096, pattern(8192, 6)),
            ("sparse.bin", (1 << 20) - 100, pattern(300, 7)),
            ("sparse.bin", 0, pattern(10, 8)),
            // A file with no stream yet.
            ("empty.bin", 0, pattern(5000, 9)),
        ];
        for (name, offset, data) in &writes {
            txn(&mut |t, vsb, om| {
                written(write_range(t, vsb, om, ROOT, name, *offset as u64, data));
                Ok(())
            });
            model_write(&mut m, name, *offset, data);
        }

        // A clone made here shares big.bin's blocks; writing into it leaves
        // big.bin as it is.
        txn(&mut |t, vsb, om| {
            clone_file(t, vsb, om, ROOT, "big.bin", ROOT, "copy.bin").map(|_| ())
        });
        m.insert("copy.bin", m["big.bin"].clone());
        txn(&mut |t, vsb, om| {
            written(write_range(
                t,
                vsb,
                om,
                ROOT,
                "copy.bin",
                500_000,
                &pattern(20_000, 10),
            ));
            Ok(())
        });
        model_write(&mut m, "copy.bin", 500_000, &pattern(20_000, 10));

        // A stream macOS shares between two files is declined; given a
        // stream of its own (sharing the blocks by reference, nothing
        // copied) the file takes the write, and the other keeps its content.
        txn(&mut |t, vsb, om| {
            let r = write_range(t, vsb, om, ROOT, "shared.bin", 10, b"no").expect("write_range");
            assert_eq!(r, RangeWrite::Declined);
            Ok(())
        });
        txn(&mut |t, vsb, om| {
            assert!(
                unshare_stream(t, vsb, om, ROOT, "shared.bin")?,
                "the stream was shared"
            );
            assert!(
                !unshare_stream(t, vsb, om, ROOT, "shared.bin")?,
                "and is no longer"
            );
            written(write_range(
                t,
                vsb,
                om,
                ROOT,
                "shared.bin",
                70_000,
                &pattern(9000, 12),
            ));
            Ok(())
        });
        model_write(&mut m, "shared.bin", 70_000, &pattern(9000, 12));
        check(&m, "one write to a transaction");

        // Many to a transaction: small appends to a new file, and scattered
        // overwrites of another.
        txn(&mut |t, vsb, om| {
            create_file(t, vsb, om, ROOT, "appended.bin").map(|_| ())?;
            for i in 0..300usize {
                let chunk = pattern(512, 20 + i);
                written(write_range(
                    t,
                    vsb,
                    om,
                    ROOT,
                    "appended.bin",
                    (i * 512) as u64,
                    &chunk,
                ));
            }
            for i in 0..50usize {
                let chunk = pattern(100, 400 + i);
                let off = (i * 19_997) % (BIG - 100);
                written(write_range(t, vsb, om, ROOT, "big.bin", off as u64, &chunk));
            }
            Ok(())
        });
        let mut appended = Vec::new();
        for i in 0..300usize {
            appended.extend_from_slice(&pattern(512, 20 + i));
        }
        m.insert("appended.bin", appended);
        for i in 0..50usize {
            let off = (i * 19_997) % (BIG - 100);
            model_write(&mut m, "big.bin", off, &pattern(100, 400 + i));
        }
        check(&m, "many writes to a transaction");
        // The first append's block is taken before that operation copies the
        // tree nodes the checkpoint holds; every later one follows on from
        // the block before it, since the nodes are then rewritten in place.
        assert!(
            extents("appended.bin") <= 2,
            "appends in one transaction extend one extent, got {}",
            extents("appended.bin")
        );

        // Cuts inside an extent are made in place too: the blocks past the
        // new end go, the block holding it keeps nothing past it (growing
        // the file again reads zeros there), a clone keeps its content.
        let cuts: Vec<(&'static str, usize)> = vec![
            ("big.bin", 700_001),
            ("sparse.bin", (1 << 20) + 100),
            ("copy.bin", 123_457),
            ("big.bin", 650_000),
            // A macOS clone given its own stream above.
            ("shared.bin", 50_001),
        ];
        for (name, new_size) in &cuts {
            let current = m[name].len() as u64;
            txn(&mut |t, vsb, om| {
                let r = truncate_file_fast(t, vsb, om, ROOT, name, *new_size as u64, current)?;
                assert_eq!(r, TruncateOutcome::Applied, "cut of {name} to {new_size}");
                Ok(())
            });
            m.get_mut(name).unwrap().truncate(*new_size);
        }
        txn(&mut |t, vsb, om| {
            let r = truncate_file_fast(t, vsb, om, ROOT, "big.bin", 900_000, 650_000)?;
            assert_eq!(r, TruncateOutcome::Applied);
            Ok(())
        });
        m.get_mut("big.bin").unwrap().resize(900_000, 0);
        check(&m, "cuts inside extents");

        // Under a snapshot: the blocks a write replaces stay with the
        // snapshot, which macOS still reads as it was.
        txn(&mut |t, vsb, om| create_snapshot(t, vsb, om, "range").map(|_| ()));
        let held = m["big.bin"].clone();
        txn(&mut |t, vsb, om| {
            written(write_range(
                t,
                vsb,
                om,
                ROOT,
                "big.bin",
                300_000,
                &pattern(9000, 60),
            ));
            written(write_range(
                t,
                vsb,
                om,
                ROOT,
                "appended.bin",
                300 * 512,
                b"tail",
            ));
            Ok(())
        });
        model_write(&mut m, "big.bin", 300_000, &pattern(9000, 60));
        model_write(&mut m, "appended.bin", 300 * 512, b"tail");
        check(&m, "under a snapshot");
        assert!(
            snapshot_file("range", "big.bin") == held,
            "the snapshot no longer reads big.bin as it was"
        );
        println!("write_range PASS");
    }

    /// `name` as macOS reads it in snapshot `snap`.
    fn snapshot_file(snap: &str, name: &str) -> Vec<u8> {
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
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let first = |pred: &dyn Fn(&str) -> bool| {
            stdout
                .lines()
                .filter_map(|l| l.split_whitespace().next())
                .find(|d| pred(d))
                .map(str::to_string)
        };
        let parent = first(&|d| {
            d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
        })
        .expect("no whole-disk node");
        // The volume is on the container disk hdiutil synthesizes, typed
        // with the APFS volume GUID; the partition itself does not mount.
        let dev = stdout
            .lines()
            .find(|l| l.contains("41504653-0000-11AA"))
            .and_then(|l| l.split_whitespace().next())
            .expect("no APFS volume node")
            .to_string();
        let dir = "/tmp/apfs-write-range-snap";
        let _ = std::fs::create_dir_all(dir);
        let st = Command::new("mount_apfs")
            .args(["-o", "rdonly", "-s", snap, &dev, dir])
            .status()
            .expect("mount_apfs");
        assert!(st.success(), "mount_apfs -s {snap} failed");
        let got = std::fs::read(format!("{dir}/{name}"));
        let _ = Command::new("umount").arg(dir).status();
        detach(&parent);
        got.expect("read from the snapshot")
    }
}
