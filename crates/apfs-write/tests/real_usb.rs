//! The writer on a real USB drive macOS formatted.
//!
//! Gate: `APFS_USB_DEV=/dev/rdiskNsM`, the raw node of the Apple_APFS
//! partition of a removable USB disk whose volume is named `APFSUSB`, and
//! writable by the user running the test. EVERYTHING ON THAT VOLUME IS
//! OVERWRITTEN. The device is checked to be a removable USB partition of
//! type Apple_APFS before anything is written.
//!
//!   APFS_USB_DEV=/dev/rdisk9s2 cargo test --release -p apfs-write \
//!     --features write --test real_usb -- --ignored --nocapture --test-threads=1
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
    )
)]

#[cfg(all(test, target_os = "macos"))]
mod real_usb {
    use apfs_core::block_device::{
        BlockDevice, BlockError, FileBlockDeviceRw, WritableBlockDevice, WritableOffsetBlockDevice,
    };
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        create_file, mkdir, overwrite_existing_file, rename, truncate_file_fast, unlink,
        unshare_stream, write_range, RangeWrite, TruncateOutcome,
    };
    use apfs_write::txn::{Transaction, TxnError};
    use std::os::fd::AsRawFd;
    use std::process::Command;

    /// A raw macOS disk node. Writes go straight to the driver; the commit
    /// barrier flushes the drive's write cache with DKIOCSYNCHRONIZE, since
    /// the F_FULLFSYNC `File::sync_data` issues fails on device nodes
    /// (ENOTTY).
    struct Dev {
        inner: WritableOffsetBlockDevice<FileBlockDeviceRw>,
        node: std::fs::File,
        /// Whole blocks read before, as the Windows mount's block cache
        /// keeps them; a write drops the blocks it covers.
        cache: std::collections::HashMap<u64, Vec<u8>>,
    }

    const CACHE_BLOCK: u64 = 4096;
    const CACHE_BLOCKS: usize = 16 * 1024;

    /// `<sys/disk.h>` `dk_synchronize_t`.
    #[repr(C)]
    struct DkSynchronize {
        offset: u64,
        length: u64,
        options: u32,
        reserved: [u8; 4],
    }

    /// `_IOW('d', 22, dk_synchronize_t)`: IOC_IN | sizeof << 16 | 'd' << 8 | 22.
    const DKIOCSYNCHRONIZE: libc::c_ulong = 0x8000_0000
        | ((std::mem::size_of::<DkSynchronize>() as libc::c_ulong) << 16)
        | ((b'd' as libc::c_ulong) << 8)
        | 22;

    impl BlockDevice for Dev {
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let one_block = offset % CACHE_BLOCK == 0 && buf.len() as u64 == CACHE_BLOCK;
            if one_block {
                if let Some(b) = self.cache.get(&offset) {
                    buf.copy_from_slice(b);
                    return Ok(());
                }
            }
            let started = std::time::Instant::now();
            let out = self.inner.read_at(offset, buf);
            count(&READS, &READ_US, started);
            if one_block && out.is_ok() {
                if self.cache.len() >= CACHE_BLOCKS {
                    self.cache.clear();
                }
                self.cache.insert(offset, buf.to_vec());
            }
            out
        }
    }

    impl WritableBlockDevice for Dev {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
            let first = offset / CACHE_BLOCK * CACHE_BLOCK;
            let mut b = first;
            while b < offset + buf.len() as u64 {
                self.cache.remove(&b);
                b += CACHE_BLOCK;
            }
            let started = std::time::Instant::now();
            let out = self.inner.write_at(offset, buf);
            count(&WRITES, &WRITE_US, started);
            out
        }
        fn flush_data(&mut self) -> Result<(), BlockError> {
            let started = std::time::Instant::now();
            let out = self.synchronize();
            count(&FLUSHES, &FLUSH_US, started);
            out
        }
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    /// Device calls so far and the time they took, for the timing report.
    static FLUSHES: AtomicU64 = AtomicU64::new(0);
    static FLUSH_US: AtomicU64 = AtomicU64::new(0);
    static READS: AtomicU64 = AtomicU64::new(0);
    static READ_US: AtomicU64 = AtomicU64::new(0);
    static WRITES: AtomicU64 = AtomicU64::new(0);
    static WRITE_US: AtomicU64 = AtomicU64::new(0);

    fn count(n: &AtomicU64, us: &AtomicU64, started: std::time::Instant) {
        n.fetch_add(1, Ordering::Relaxed);
        us.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    }

    /// (flushes, ms, reads, ms, writes, ms) so far.
    type Io = [u64; 6];

    fn flushes() -> Io {
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        [
            get(&FLUSHES),
            get(&FLUSH_US) / 1000,
            get(&READS),
            get(&READ_US) / 1000,
            get(&WRITES),
            get(&WRITE_US) / 1000,
        ]
    }

    fn io_since(a: Io, b: Io) -> String {
        format!(
            "flushes {} ({} ms), reads {} ({} ms), writes {} ({} ms)",
            b[0] - a[0],
            b[1] - a[1],
            b[2] - a[2],
            b[3] - a[3],
            b[4] - a[4],
            b[5] - a[5]
        )
    }

    impl Dev {
        fn synchronize(&mut self) -> Result<(), BlockError> {
            // Offset and length zero: the whole medium.
            let mut req = DkSynchronize {
                offset: 0,
                length: 0,
                options: 0,
                reserved: [0; 4],
            };
            // SAFETY: an ioctl on a descriptor this struct owns, with a
            // properly sized and initialised argument the call only reads.
            let rc = unsafe {
                libc::ioctl(
                    self.node.as_raw_fd(),
                    DKIOCSYNCHRONIZE,
                    &mut req as *mut DkSynchronize,
                )
            };
            if rc != 0 {
                return Err(BlockError::Io(std::io::Error::last_os_error()));
            }
            Ok(())
        }
    }
    const VOLUME: &str = "APFSUSB";
    const ROOT: u64 = 2;

    /// The raw partition node, its whole disk (`diskN`) and its size.
    struct Usb {
        raw: String,
        disk: String,
        size: u64,
    }

    fn diskutil(args: &[&str]) -> String {
        let out = Command::new("diskutil")
            .args(args)
            .output()
            .expect("diskutil");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn field<'a>(info: &'a str, name: &str) -> Option<&'a str> {
        info.lines()
            .find_map(|l| l.trim().strip_prefix(name)?.trim().strip_prefix(':'))
            .map(str::trim)
    }

    /// The device `APFS_USB_DEV` names, if it is a removable USB Apple_APFS
    /// partition; `None` (skip) when the variable is unset.
    fn usb() -> Option<Usb> {
        let raw = std::env::var("APFS_USB_DEV").ok()?;
        let info = diskutil(&["info", &raw]);
        assert_eq!(field(&info, "Protocol"), Some("USB"), "{raw} is not on USB");
        assert_eq!(
            field(&info, "Removable Media"),
            Some("Removable"),
            "{raw} is not removable"
        );
        assert_eq!(
            field(&info, "Partition Type"),
            Some("Apple_APFS"),
            "{raw} is not an APFS partition"
        );
        let disk = field(&info, "Part of Whole")
            .expect("whole disk")
            .to_string();
        let size = field(&info, "Disk Size")
            .and_then(|s| s.split('(').nth(1))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .expect("partition size");
        Some(Usb { raw, disk, size })
    }

    fn unmount(usb: &Usb) {
        let out = Command::new("diskutil")
            .args(["unmountDisk", &usb.disk])
            .output()
            .expect("diskutil unmountDisk");
        assert!(out.status.success(), "unmountDisk failed");
    }

    /// Mount the volume and return its mount point.
    fn mount(usb: &Usb) -> String {
        let out = Command::new("diskutil")
            .args(["mountDisk", &usb.disk])
            .output()
            .expect("diskutil mountDisk");
        assert!(out.status.success(), "mountDisk failed");
        let point = format!("/Volumes/{VOLUME}");
        assert!(std::path::Path::new(&point).exists(), "{point} not mounted");
        point
    }

    fn open_dev(usb: &Usb) -> Dev {
        // macOS may have mounted the volume again (diskutil does after a
        // verify, now and then a moment later); the node is busy while it is.
        let mut tries = 0;
        let inner = loop {
            match FileBlockDeviceRw::open_rw_with_size(&usb.raw, usb.size) {
                Ok(d) => break d,
                Err(BlockError::Io(e)) if e.raw_os_error() == Some(16) && tries < 5 => {
                    tries += 1;
                    unmount(usb);
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                Err(e) => panic!("open raw: {e:?}"),
            }
        };
        Dev {
            inner: WritableOffsetBlockDevice::new(inner, 0, usb.size),
            node: std::fs::File::open(&usb.raw).expect("open raw for flush"),
            cache: std::collections::HashMap::new(),
        }
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

    fn txn(usb: &Usb, f: &mut Op<'_>) {
        let mut dev = open_dev(usb);
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).expect("begin");
        f(&mut t, &vsb, &om).expect("operation");
        t.commit().expect("commit");
    }

    /// fsck_apfs on the unmounted container, through diskutil (which runs it
    /// as root, `-n -x`): clean, with no warning.
    fn fsck(usb: &Usb, label: &str) {
        let info = diskutil(&["info", &usb.raw]);
        let container = field(&info, "APFS Container").expect("APFS container disk");
        let out = Command::new("diskutil")
            .args(["verifyVolume", container])
            .output()
            .expect("diskutil verifyVolume");
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
        // diskutil mounts the volume again after verifying it, sometimes a
        // moment later; the raw node cannot be opened for writing while it is.
        std::thread::sleep(std::time::Duration::from_secs(2));
        unmount(usb);
    }

    /// A range write that has to land, not be declined.
    fn written(r: Result<RangeWrite, TxnError>) -> Result<(), TxnError> {
        assert_eq!(r?, RangeWrite::Written);
        Ok(())
    }

    fn pattern(n: usize, seed: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 7 + seed * 131) % 251) as u8).collect()
    }

    fn timed<T>(f: impl FnOnce() -> T) -> (std::time::Duration, T) {
        let t = std::time::Instant::now();
        let r = f();
        (t.elapsed(), r)
    }

    fn sync() {
        assert!(Command::new("sync").status().expect("sync").success());
    }

    /// What every file under the test directory holds, by path from the
    /// volume root.
    type Model = std::collections::BTreeMap<String, Vec<u8>>;

    fn put(m: &mut Model, path: &str, offset: usize, data: &[u8]) {
        let f = m.entry(path.to_string()).or_default();
        if f.len() < offset + data.len() {
            f.resize(offset + data.len(), 0);
        }
        f[offset..offset + data.len()].copy_from_slice(data);
    }

    fn ino(usb: &Usb, path: &str) -> u64 {
        let mut fs = apfs::FsView::open(open_dev(usb)).expect("FsView::open");
        fs.lookup(path).expect("lookup").expect("present")
    }

    const SMALL_FILES: usize = 2000;
    const SMALL: usize = 4096;
    const BIG: usize = 128 << 20;
    const MIB: usize = 1 << 20;

    /// macOS writes a tree, we write the same kind of work and edit what
    /// macOS wrote, macOS reads all of it and writes some more, we read
    /// that: strict fsck between every step. The time each side takes for
    /// the same work is printed.
    #[test]
    #[ignore = "real USB drive only"]
    fn a_workload_written_here_reads_back_under_macos() {
        let Some(usb) = usb() else {
            println!("real_usb: APFS_USB_DEV not set - skip");
            return;
        };
        let mut m = Model::new();

        // macOS: small files, a big file, a compressed one, a clone and a
        // sparse file, timed.
        let point = mount(&usb);
        let _ = std::fs::remove_dir_all(format!("{point}/t"));
        std::fs::create_dir_all(format!("{point}/t/mac")).unwrap();
        let (mac_small, ()) = timed(|| {
            for i in 0..SMALL_FILES {
                std::fs::write(format!("{point}/t/mac/f{i}"), pattern(SMALL, i)).unwrap();
            }
            sync();
        });
        for i in 0..SMALL_FILES {
            m.insert(format!("t/mac/f{i}"), pattern(SMALL, i));
        }
        let big = pattern(BIG, 7);
        let (mac_big, ()) = timed(|| {
            std::fs::write(format!("{point}/t/big.bin"), &big).unwrap();
            sync();
        });
        m.insert("t/big.bin".into(), big);
        let text: Vec<u8> = (0..MIB).map(|i| b"compress me "[i % 12]).collect();
        std::fs::write(format!("{point}/t/text.txt"), &text).unwrap();
        let st = Command::new("ditto")
            .args([
                "--hfsCompression",
                &format!("{point}/t/text.txt"),
                &format!("{point}/t/comp.txt"),
            ])
            .status()
            .unwrap();
        assert!(st.success(), "ditto --hfsCompression");
        m.insert("t/text.txt".into(), text.clone());
        m.insert("t/comp.txt".into(), text);
        let st = Command::new("cp")
            .args([
                "-c",
                &format!("{point}/t/big.bin"),
                &format!("{point}/t/clone.bin"),
            ])
            .status()
            .unwrap();
        assert!(st.success(), "cp -c");
        m.insert("t/clone.bin".into(), m["t/big.bin"].clone());
        {
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::File::create(format!("{point}/t/sparse.bin")).unwrap();
            f.set_len(8 * MIB as u64).unwrap();
            f.seek(SeekFrom::Start(3 * MIB as u64)).unwrap();
            f.write_all(&pattern(SMALL, 3)).unwrap();
        }
        let mut sparse = vec![0u8; 8 * MIB];
        sparse[3 * MIB..3 * MIB + SMALL].copy_from_slice(&pattern(SMALL, 3));
        m.insert("t/sparse.bin".into(), sparse);
        sync();
        unmount(&usb);
        fsck(&usb, "macOS seed");

        // Ours, grouped the way the Windows mount groups: small files 256 to
        // a transaction, the big file 64 MiB to one.
        let t_ino = ino(&usb, "/t");
        txn(&usb, &mut |t, v, o| {
            mkdir(t, v, o, t_ino, "ours").map(|_| ())
        });
        let ours = ino(&usb, "/t/ours");
        let f0 = flushes();
        let (our_small, ()) = timed(|| {
            for chunk in (0..SMALL_FILES).collect::<Vec<_>>().chunks(256) {
                txn(&usb, &mut |t, v, o| {
                    for &i in chunk {
                        let name = format!("f{i}");
                        create_file(t, v, o, ours, &name)?;
                        written(write_range(
                            t,
                            v,
                            o,
                            ours,
                            &name,
                            0,
                            &pattern(SMALL, 1000 + i),
                        ))?;
                    }
                    Ok(())
                });
            }
        });
        for i in 0..SMALL_FILES {
            m.insert(format!("t/ours/f{i}"), pattern(SMALL, 1000 + i));
        }
        let ours_big = pattern(BIG, 8);
        let f1 = flushes();
        let (our_big, ()) = timed(|| {
            txn(&usb, &mut |t, v, o| {
                create_file(t, v, o, t_ino, "ours_big.bin").map(|_| ())
            });
            for group in ours_big.chunks(64 * MIB).enumerate() {
                let (g, bytes) = group;
                txn(&usb, &mut |t, v, o| {
                    for (k, chunk) in bytes.chunks(MIB).enumerate() {
                        let off = (g * 64 * MIB + k * MIB) as u64;
                        written(write_range(t, v, o, t_ino, "ours_big.bin", off, chunk))?;
                    }
                    Ok(())
                });
            }
        });
        m.insert("t/ours_big.bin".into(), ours_big);
        let f2 = flushes();

        // Edits of what macOS wrote.
        let (our_edits, ()) = timed(|| {
            txn(&usb, &mut |t, v, o| {
                // big.bin and clone.bin share one stream (cp -c): the clone
                // takes its own first, and then each can be edited.
                let r = write_range(t, v, o, t_ino, "big.bin", 3, b"no");
                assert_eq!(r?, RangeWrite::Declined, "a shared stream is declined");
                assert!(unshare_stream(t, v, o, t_ino, "clone.bin")?);
                let off = 50 * MIB as u64 + 3;
                written(write_range(
                    t,
                    v,
                    o,
                    t_ino,
                    "big.bin",
                    off,
                    &pattern(MIB, 9),
                ))?;
                written(write_range(
                    t,
                    v,
                    o,
                    t_ino,
                    "big.bin",
                    BIG as u64,
                    b"appended",
                ))?;
                let cut = 100 * MIB as u64 + 12_345;
                let r = truncate_file_fast(t, v, o, t_ino, "big.bin", cut, BIG as u64 + 8)?;
                assert_eq!(r, TruncateOutcome::Applied);
                let off = 10 * MIB as u64;
                written(write_range(
                    t,
                    v,
                    o,
                    t_ino,
                    "clone.bin",
                    off,
                    &pattern(65_536, 10),
                ))?;
                let off = 5 * MIB as u64;
                written(write_range(
                    t,
                    v,
                    o,
                    t_ino,
                    "sparse.bin",
                    off,
                    &pattern(SMALL, 11),
                ))?;
                for i in 0..500 {
                    rename(t, v, o, ours, &format!("f{i}"), &format!("r{i}"), false)?;
                }
                for i in 500..1000 {
                    unlink(t, v, o, ours, &format!("f{i}"))?;
                }
                Ok(())
            });
        });
        put(&mut m, "t/big.bin", 50 * MIB + 3, &pattern(MIB, 9));
        put(&mut m, "t/big.bin", BIG, b"appended");
        m.get_mut("t/big.bin").unwrap().truncate(100 * MIB + 12_345);
        put(&mut m, "t/clone.bin", 10 * MIB, &pattern(65_536, 10));
        put(&mut m, "t/sparse.bin", 5 * MIB, &pattern(SMALL, 11));
        for i in 0..500 {
            let body = m.remove(&format!("t/ours/f{i}")).unwrap();
            m.insert(format!("t/ours/r{i}"), body);
        }
        for i in 500..1000 {
            m.remove(&format!("t/ours/f{i}"));
        }
        // The compressed file: read back through the compression, written
        // plain, then patched in place.
        let comp = {
            let mut fs = apfs::FsView::open(open_dev(&usb)).unwrap();
            let a = fs.getattr("/t/comp.txt").unwrap().unwrap();
            assert!(
                a.bsd_flags & apfs_core::inode::UF_COMPRESSED != 0,
                "ditto left comp.txt compressed"
            );
            fs.read("/t/comp.txt", 0, a.size as usize).unwrap()
        };
        assert!(comp == m["t/comp.txt"], "our reader decompresses comp.txt");
        txn(&usb, &mut |t, v, o| {
            overwrite_existing_file(t, v, o, t_ino, "comp.txt", &comp)?;
            written(write_range(t, v, o, t_ino, "comp.txt", 100, b"PATCH"))
        });
        put(&mut m, "t/comp.txt", 100, b"PATCH");
        fsck(&usb, "our writes");

        // macOS reads every file, then writes some more.
        let point = mount(&usb);
        for (path, want) in &m {
            let got = std::fs::read(format!("{point}/{path}")).expect("macOS read");
            assert!(got == *want, "macOS reads {path} differently");
        }
        std::fs::write(
            format!("{point}/t/mac_after.txt"),
            b"macOS wrote this after",
        )
        .unwrap();
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(format!("{point}/t/ours/r0"))
                .unwrap();
            f.write_all(b" + macOS tail").unwrap();
        }
        put(&mut m, "t/mac_after.txt", 0, b"macOS wrote this after");
        let r0_len = m["t/ours/r0"].len();
        put(&mut m, "t/ours/r0", r0_len, b" + macOS tail");
        sync();
        unmount(&usb);
        fsck(&usb, "macOS after us");

        // Our reader reads what macOS wrote.
        let mut fs = apfs::FsView::open(open_dev(&usb)).unwrap();
        for path in ["t/mac_after.txt", "t/ours/r0"] {
            let want = &m[path];
            let got = fs.read(&format!("/{path}"), 0, want.len() + 10).unwrap();
            assert!(got == *want, "our reader reads {path} differently");
        }

        let ms = |d: std::time::Duration| d.as_secs_f64() * 1e3;
        println!(
            "usb | {SMALL_FILES} x 4 KiB files | macOS {:>8.0} ms | ours {:>8.0} ms | {}",
            ms(mac_small),
            ms(our_small),
            io_since(f0, f1)
        );
        println!(
            "usb | {} MiB file              | macOS {:>8.0} ms | ours {:>8.0} ms | {}",
            BIG / MIB,
            ms(mac_big),
            ms(our_big),
            io_since(f1, f2)
        );
        println!(
            "usb | edits + 500 renames + 500 removals | ours {:>8.0} ms",
            ms(our_edits)
        );
        println!("real_usb workload PASS");
    }

    /// Does macOS see a commit made under it while the volume was unmounted
    /// but the container stayed attached?
    #[test]
    #[ignore = "real USB drive only"]
    fn macos_sees_a_commit_made_while_unmounted() {
        let Some(usb) = usb() else {
            println!("real_usb: APFS_USB_DEV not set - skip");
            return;
        };
        let point = mount(&usb);
        let _ = std::fs::remove_file(format!("{point}/probe.txt"));
        unmount(&usb);
        fsck(&usb, "before");
        txn(&usb, &mut |t, vsb, om| {
            create_file(t, vsb, om, ROOT, "probe.txt")?;
            write_range(t, vsb, om, ROOT, "probe.txt", 0, b"written under macOS").map(|_| ())
        });
        fsck(&usb, "after our commit");
        let point = mount(&usb);
        let got = std::fs::read(format!("{point}/probe.txt"));
        unmount(&usb);
        assert_eq!(
            got.expect("macOS reads probe.txt"),
            b"written under macOS",
            "macOS reads our file"
        );
        println!("real_usb probe PASS");
    }
}
