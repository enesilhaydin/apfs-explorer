//! Speed of our reader and writer against macOS on the same image.
//!
//! Gate: `APFS_PERF=1` (requires macOS: hdiutil, ditto). Run it in a release
//! build, single-threaded:
//!
//! `APFS_PERF=1 cargo test --release -p apfs-write --features write --test perf_bench -- --include-ignored --nocapture --test-threads=1`
//!
//! macOS populates an image (a directory of small files, a large file, a
//! compressed file); then each workload runs once through macOS on the
//! mounted volume and once through our code on the detached image, and the
//! two times are printed side by side with the device reads we issued. The
//! write workloads follow the mount host: a file is created in one
//! transaction and its content written at close in another.
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
mod perf_bench {
    use apfs_core::block_device::{
        BlockDevice, BlockError, FileBlockDeviceRw, WritableBlockDevice, WritableOffsetBlockDevice,
    };
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, overwrite_existing_file, rename, unlink, write_range};
    use apfs_write::txn::Transaction;
    use std::io::Read;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    const IMG: &str = "/tmp/apfs-perf-bench.img";
    const IMAGE_SIZE: &str = "2g";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// Small files macOS writes into `/many`.
    const SMALL_FILES: usize = 5000;
    const SMALL_SIZE: usize = 4096;
    /// The large file, in MiB.
    const BIG_MB: usize = 200;
    /// Files our writer creates, writes, renames and removes.
    const WRITE_FILES: usize = 300;
    /// Bytes per read call, the way a host reads a file.
    const CHUNK: usize = 1 << 20;

    fn gate() -> bool {
        std::env::var("APFS_PERF").as_deref() == Ok("1")
    }

    static READS: AtomicU64 = AtomicU64::new(0);
    static READ_BYTES: AtomicU64 = AtomicU64::new(0);
    static WRITES: AtomicU64 = AtomicU64::new(0);
    static FLUSHES: AtomicU64 = AtomicU64::new(0);
    static READ_NS: AtomicU64 = AtomicU64::new(0);
    static WRITE_NS: AtomicU64 = AtomicU64::new(0);
    static FLUSH_NS: AtomicU64 = AtomicU64::new(0);

    /// Adds the time `f` took to `acc`.
    fn clocked<T>(acc: &AtomicU64, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let r = f();
        acc.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        r
    }

    /// Counts what reaches the device, and the time spent there.
    struct Counting<D> {
        inner: D,
    }
    impl<D: WritableBlockDevice> BlockDevice for Counting<D> {
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            READS.fetch_add(1, Ordering::Relaxed);
            READ_BYTES.fetch_add(buf.len() as u64, Ordering::Relaxed);
            clocked(&READ_NS, || self.inner.read_at(offset, buf))
        }
    }
    impl<D: WritableBlockDevice> WritableBlockDevice for Counting<D> {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
            WRITES.fetch_add(1, Ordering::Relaxed);
            clocked(&WRITE_NS, || self.inner.write_at(offset, buf))
        }
        fn invalidate_read_cache(&mut self) {
            self.inner.invalidate_read_cache();
        }
        fn flush_data(&mut self) -> Result<(), BlockError> {
            FLUSHES.fetch_add(1, Ordering::Relaxed);
            clocked(&FLUSH_NS, || self.inner.flush_data())
        }
    }
    type Dev = Counting<WritableOffsetBlockDevice<FileBlockDeviceRw>>;

    #[derive(Default, Clone, Copy)]
    struct Io {
        reads: u64,
        read_bytes: u64,
        writes: u64,
        flushes: u64,
        read_ns: u64,
        write_ns: u64,
        flush_ns: u64,
    }

    fn open_dev() -> Dev {
        let mut head = vec![0u8; 64 * 1024];
        std::fs::File::open(IMG)
            .unwrap()
            .read_exact(&mut head)
            .unwrap();
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
                return Counting {
                    inner: WritableOffsetBlockDevice::new(inner, s * SECTOR, (en - s + 1) * SECTOR),
                };
            }
        }
        panic!("no APFS partition");
    }

    fn io_now() -> Io {
        Io {
            reads: READS.load(Ordering::Relaxed),
            read_bytes: READ_BYTES.load(Ordering::Relaxed),
            writes: WRITES.load(Ordering::Relaxed),
            flushes: FLUSHES.load(Ordering::Relaxed),
            read_ns: READ_NS.load(Ordering::Relaxed),
            write_ns: WRITE_NS.load(Ordering::Relaxed),
            flush_ns: FLUSH_NS.load(Ordering::Relaxed),
        }
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
        let out = Command::new("hdiutil").args(&args).output().unwrap();
        assert!(out.status.success(), "hdiutil attach failed");
        let s = String::from_utf8_lossy(&out.stdout).into_owned();
        let mount = s
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .unwrap_or_default();
        let disk = s
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .unwrap();
        (mount, disk)
    }

    fn detach(disk: &str) {
        for i in 0..8u64 {
            if Command::new("hdiutil")
                .args(["detach", disk])
                .output()
                .unwrap()
                .status
                .success()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(250 * (i + 1)));
        }
        let _ = Command::new("hdiutil")
            .args(["detach", "-force", disk])
            .status();
    }

    fn build_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-perf-bench.dmg");
        let st = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                IMAGE_SIZE,
                "-layout",
                "GPTSPUD",
                "-fs",
                "APFS",
                "-volname",
                "PERF",
                "/tmp/apfs-perf-bench",
            ])
            .output()
            .unwrap();
        assert!(st.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-perf-bench.dmg", IMG).unwrap();
        let (m, disk) = attach(true);
        std::fs::create_dir(format!("{m}/many")).unwrap();
        for i in 0..SMALL_FILES {
            let body: Vec<u8> = (0..SMALL_SIZE)
                .map(|j| ((i * 31 + j) % 251) as u8)
                .collect();
            std::fs::write(format!("{m}/many/f{i:05}.bin"), body).unwrap();
        }
        let big: Vec<u8> = (0..BIG_MB << 20).map(|j| (j % 253) as u8).collect();
        std::fs::write(format!("{m}/big.bin"), &big).unwrap();
        // A compressed file: text that compresses, written through ditto.
        let src = "/tmp/apfs-perf-bench-src";
        let _ = std::fs::remove_dir_all(src);
        std::fs::create_dir_all(src).unwrap();
        let text: String = (0..800_000)
            .map(|i| format!("line {i} of a compressible file\n"))
            .collect();
        std::fs::write(format!("{src}/text.txt"), text).unwrap();
        let st = Command::new("ditto")
            .args(["--hfsCompression", src, &m])
            .status()
            .unwrap();
        assert!(st.success(), "ditto failed");
        let _ = std::fs::remove_dir_all(src);
        let _ = Command::new("sync").status();
        detach(&disk);
    }

    fn read_vsb_omap(dev: &mut Dev) -> (Vec<u8>, Vec<u8>) {
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

    /// One operation in one committed transaction, the device carried over.
    fn in_txn<T>(
        dev: Dev,
        f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<T, apfs_write::txn::TxnError>,
    ) -> Dev {
        let mut dev = dev;
        let (vsb, om) = read_vsb_omap(&mut dev);
        let mut txn = Transaction::begin(dev).unwrap();
        f(&mut txn, &vsb, &om).unwrap();
        txn.commit().unwrap()
    }

    fn row(name: &str, macos: Duration, ours: Duration, io: Io) {
        let ratio = ours.as_secs_f64() / macos.as_secs_f64().max(1e-9);
        println!(
            "perf | {name:<34} | macOS {:>9.1} ms | ours {:>9.1} ms | x{ratio:>7.1} | reads {:>8} ({:>7.1} MiB) writes {:>7} flushes {:>5} | device ms r {:.0} w {:.0} f {:.0}",
            macos.as_secs_f64() * 1e3,
            ours.as_secs_f64() * 1e3,
            io.reads,
            io.read_bytes as f64 / (1 << 20) as f64,
            io.writes,
            io.flushes,
            io.read_ns as f64 / 1e6,
            io.write_ns as f64 / 1e6,
            io.flush_ns as f64 / 1e6
        );
    }

    fn timed<T>(f: impl FnOnce() -> T) -> (Duration, T) {
        let t = Instant::now();
        let r = f();
        (t.elapsed(), r)
    }

    fn diff(a: Io, b: Io) -> Io {
        Io {
            reads: b.reads - a.reads,
            read_bytes: b.read_bytes - a.read_bytes,
            writes: b.writes - a.writes,
            flushes: b.flushes - a.flushes,
            read_ns: b.read_ns - a.read_ns,
            write_ns: b.write_ns - a.write_ns,
            flush_ns: b.flush_ns - a.flush_ns,
        }
    }

    fn read_all_ours(fs: &mut apfs::FsView<Dev>, path: &str) -> usize {
        let mut off = 0u64;
        loop {
            let got = fs.read(path, off, CHUNK).unwrap();
            if got.is_empty() {
                return off as usize;
            }
            off += got.len() as u64;
        }
    }

    #[test]
    #[ignore = "performance gate: APFS_PERF=1, release build"]
    fn our_speed_against_macos() {
        if !gate() {
            return;
        }
        build_image();

        // ---- macOS, on the mounted volume (a fresh mount per run).
        let (m, disk) = attach(true);
        let (mac_list, n) = timed(|| {
            let mut n = 0;
            for e in std::fs::read_dir(format!("{m}/many")).unwrap() {
                let e = e.unwrap();
                let _ = e.metadata().unwrap().len();
                n += 1;
            }
            n
        });
        assert_eq!(n, SMALL_FILES);
        let (mac_small, _) = timed(|| {
            let mut total = 0;
            for i in 0..SMALL_FILES {
                total += std::fs::read(format!("{m}/many/f{i:05}.bin"))
                    .unwrap()
                    .len();
            }
            total
        });
        let (mac_big, big_len) = timed(|| std::fs::read(format!("{m}/big.bin")).unwrap().len());
        let (mac_comp, comp_len) = timed(|| std::fs::read(format!("{m}/text.txt")).unwrap().len());
        let (mac_stat, _) = timed(|| {
            for i in (0..SMALL_FILES).step_by(5) {
                let _ = std::fs::metadata(format!("{m}/many/f{i:05}.bin")).unwrap();
            }
        });
        // Writes: create, write, rename, remove; durable at the end (sync).
        let (mac_create, _) = timed(|| {
            for i in 0..WRITE_FILES {
                std::fs::write(format!("{m}/w{i}.bin"), vec![7u8; SMALL_SIZE]).unwrap();
            }
            let _ = Command::new("sync").status();
        });
        let (mac_rename, _) = timed(|| {
            for i in 0..WRITE_FILES {
                std::fs::rename(format!("{m}/w{i}.bin"), format!("{m}/r{i}.bin")).unwrap();
            }
            let _ = Command::new("sync").status();
        });
        let (mac_unlink, _) = timed(|| {
            for i in 0..WRITE_FILES {
                std::fs::remove_file(format!("{m}/r{i}.bin")).unwrap();
            }
            let _ = Command::new("sync").status();
        });
        let big_write: Vec<u8> = (0..64usize << 20).map(|j| (j % 241) as u8).collect();
        let (mac_bigw, _) = timed(|| {
            std::fs::write(format!("{m}/bigw.bin"), &big_write).unwrap();
            let _ = Command::new("sync").status();
        });
        std::fs::remove_file(format!("{m}/bigw.bin")).unwrap();
        let _ = Command::new("sync").status();
        detach(&disk);

        // ---- ours, on the detached image.
        let dev = open_dev();
        let (t_open, fs) = timed(|| apfs::FsView::open(dev).unwrap());
        let mut fs = fs;
        println!("perf | open: {:.1} ms", t_open.as_secs_f64() * 1e3);
        let io0 = io_now();
        let (our_list, n) = timed(|| {
            let entries = fs.read_dir("/many").unwrap();
            for e in &entries {
                let _ = fs.getattr(&format!("/many/{}", e.name)).unwrap();
            }
            entries.len()
        });
        assert_eq!(n, SMALL_FILES);
        let io1 = io_now();
        row("readdir + stat 5000", mac_list, our_list, diff(io0, io1));
        let (our_small, _) = timed(|| {
            for i in 0..SMALL_FILES {
                let p = format!("/many/f{i:05}.bin");
                let got = fs.read(&p, 0, CHUNK).unwrap();
                assert_eq!(got.len(), SMALL_SIZE);
            }
        });
        let io2 = io_now();
        row("read 5000 x 4 KiB", mac_small, our_small, diff(io1, io2));
        let (our_big, n) = timed(|| read_all_ours(&mut fs, "/big.bin"));
        assert_eq!(n, big_len);
        let io3 = io_now();
        row(
            "read 200 MiB sequential (1 MiB)",
            mac_big,
            our_big,
            diff(io2, io3),
        );
        let (our_comp, n) = timed(|| read_all_ours(&mut fs, "/text.txt"));
        assert_eq!(n, comp_len);
        let io4 = io_now();
        row("read compressed file", mac_comp, our_comp, diff(io3, io4));
        let (our_stat, _) = timed(|| {
            for i in (0..SMALL_FILES).step_by(5) {
                let _ = fs.getattr(&format!("/many/f{i:05}.bin")).unwrap();
            }
        });
        let io5 = io_now();
        row("stat 1000 paths", mac_stat, our_stat, diff(io4, io5));
        let mut dev = fs.into_dev();

        // Writes the way the mount host does them.
        let a = io_now();
        let (our_create, d) = timed(|| {
            let mut d = dev;
            for i in 0..WRITE_FILES {
                let name = format!("w{i}.bin");
                d = in_txn(d, |t, v, o| create_file(t, v, o, 2, &name));
                d = in_txn(d, |t, v, o| {
                    overwrite_existing_file(t, v, o, 2, &name, &[7u8; SMALL_SIZE])
                });
            }
            d
        });
        dev = d;
        row(
            "create + write 300 x 4 KiB",
            mac_create,
            our_create,
            diff(a, io_now()),
        );
        let a = io_now();
        let (our_rename, d) = timed(|| {
            let mut d = dev;
            for i in 0..WRITE_FILES {
                let (from, to) = (format!("w{i}.bin"), format!("r{i}.bin"));
                d = in_txn(d, |t, v, o| rename(t, v, o, 2, &from, &to, false));
            }
            d
        });
        dev = d;
        row("rename 300", mac_rename, our_rename, diff(a, io_now()));
        let a = io_now();
        let (our_unlink, d) = timed(|| {
            let mut d = dev;
            for i in 0..WRITE_FILES {
                let name = format!("r{i}.bin");
                d = in_txn(d, |t, v, o| unlink(t, v, o, 2, &name));
            }
            d
        });
        dev = d;
        row("unlink 300", mac_unlink, our_unlink, diff(a, io_now()));
        let a = io_now();
        let (our_bigw, d) = timed(|| {
            let mut d = in_txn(dev, |t, v, o| create_file(t, v, o, 2, "bigw.bin"));
            d = in_txn(d, |t, v, o| {
                overwrite_existing_file(t, v, o, 2, "bigw.bin", &big_write)
            });
            d
        });
        dev = d;
        row("write 64 MiB file", mac_bigw, our_bigw, diff(a, io_now()));

        // The same metadata work grouped the way macOS groups it: many
        // operations, one checkpoint.
        let a = io_now();
        let (grouped_create, d) = timed(|| {
            in_txn(dev, |t, v, o| {
                for i in 0..WRITE_FILES {
                    let name = format!("g{i}.bin");
                    create_file(t, v, o, 2, &name)?;
                    overwrite_existing_file(t, v, o, 2, &name, &[7u8; SMALL_SIZE])?;
                }
                Ok(())
            })
        });
        dev = d;
        row(
            "create + write 300, one checkpoint",
            mac_create,
            grouped_create,
            diff(a, io_now()),
        );
        let a = io_now();
        let (grouped_rename, d) = timed(|| {
            in_txn(dev, |t, v, o| {
                for i in 0..WRITE_FILES {
                    let (from, to) = (format!("g{i}.bin"), format!("h{i}.bin"));
                    rename(t, v, o, 2, &from, &to, false)?;
                }
                Ok(())
            })
        });
        dev = d;
        row(
            "rename 300, one checkpoint",
            mac_rename,
            grouped_rename,
            diff(a, io_now()),
        );
        let a = io_now();
        let (grouped_unlink, d) = timed(|| {
            in_txn(dev, |t, v, o| {
                for i in 0..WRITE_FILES {
                    unlink(t, v, o, 2, &format!("h{i}.bin"))?;
                }
                Ok(())
            })
        });
        dev = d;
        row(
            "unlink 300, one checkpoint",
            mac_unlink,
            grouped_unlink,
            diff(a, io_now()),
        );
        // Ranges written the way the Windows host writes them: a file grown
        // in 1 MiB writes, then one 4 KiB write into its middle.
        let a = io_now();
        let (ranged, d) = timed(|| {
            in_txn(dev, |t, v, o| {
                create_file(t, v, o, 2, "ranged.bin")?;
                for (i, chunk) in big_write.chunks(1 << 20).enumerate() {
                    write_range(t, v, o, 2, "ranged.bin", (i << 20) as u64, chunk)?;
                }
                Ok(())
            })
        });
        dev = d;
        row(
            "write 64 MiB as 1 MiB ranges",
            mac_bigw,
            ranged,
            diff(a, io_now()),
        );
        let a = io_now();
        let (patched, d) = timed(|| {
            in_txn(dev, |t, v, o| {
                write_range(t, v, o, 2, "ranged.bin", 32 << 20, &[9u8; 4096]).map(|_| ())
            })
        });
        dev = d;
        row(
            "write 4 KiB into a 64 MiB file",
            mac_bigw,
            patched,
            diff(a, io_now()),
        );
        // The grouped work landed: nothing of it is left behind.
        let mut fs = apfs::FsView::open(dev).unwrap();
        let left = fs
            .read_dir("/")
            .unwrap()
            .iter()
            .filter(|e| e.name.starts_with('g') || e.name.starts_with('h'))
            .count();
        assert_eq!(left, 0, "grouped renames and removals did not all land");
        let dev = fs.into_dev();
        drop(dev);
        let _ = std::fs::remove_file(IMG);
    }
}
