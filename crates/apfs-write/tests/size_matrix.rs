//! Containers of every size class macOS lays out differently, formatted by
//! macOS itself, read and written through the library.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, diskutil, fsck).
//! `SIZE_MATRIX_GIB=4096,8192` runs a subset.
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --features write \
//!     --test size_matrix -- --include-ignored --nocapture
//!
//! The space manager's shape follows the container's size: its internal
//! pool bitmap spans more blocks from about 2 TB, the object itself spans
//! two blocks at 3-4 TB (the chunk-info addresses outgrow the first), the
//! addresses move into address blocks (CABs) from about 8 TB, and block
//! numbers pass 2^32 from 16 TiB. The test suite used images of 64 MB, and
//! a two-block space manager went unnoticed on the read side until a 4 TB
//! drive showed as 1 GB (#23). Each size here first checks it lands in the
//! class it stands for, then checks both sides: what the reader reports
//! against macOS, and what the writer leaves against fsck_apfs and macOS
//! reading it back. The images are sparse files: a few hundred MB each.
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
mod size_matrix {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{mkdir, rename, unlink, write_file};
    use apfs_write::snapshot::{create_snapshot, delete_snapshot};
    use apfs_write::txn::{Transaction, TxnError};
    use std::io::Read;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-size-matrix.img";
    const SECTOR: u64 = 512;
    const GIB: u64 = 1 << 30;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ROOT: u64 = 2;

    /// `spaceman_phys_t`: the main device's chunk-info address block count,
    /// and the internal pool bitmap's length in blocks.
    const SM_MAIN_CAB_COUNT: usize = 48 + 20;
    const SM_IP_BM_SIZE_IN_BLOCKS: usize = 160;

    /// What a container of this size is in the matrix for.
    #[derive(Clone, Copy, Debug)]
    enum Class {
        /// One-block space manager, one-block internal pool bitmap.
        Baseline,
        /// The internal pool bitmap spans several blocks.
        PoolBitmapBlocks,
        /// The space manager spans two blocks.
        TwoBlockSpaceman,
        /// Chunk-info addresses live in address blocks.
        AddressBlocks,
        /// Block numbers past 2^32.
        Past32Bits,
        /// A two-block space manager with address blocks and 2^32 blocks.
        All,
    }

    const SIZES: &[(u64, Class)] = &[
        (1024, Class::Baseline),
        (2048, Class::PoolBitmapBlocks),
        (4096, Class::TwoBlockSpaceman),
        (8192, Class::AddressBlocks),
        (32768, Class::Past32Bits),
        (65536, Class::All),
    ];

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn run(cmd: &str, args: &[&str]) -> String {
        let out = Command::new(cmd).args(args).output().expect(cmd);
        assert!(
            out.status.success(),
            "{cmd} {args:?} failed:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Attach the image; with `mount`, return the volume's mount point too.
    fn attach(mount: bool, read_only: bool) -> (String, Option<String>) {
        let mut args = vec![
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nobrowse",
        ];
        if !mount {
            args.push("-nomount");
        }
        if read_only {
            args.push("-readonly");
        }
        args.push(IMG);
        let out = run("hdiutil", &args);
        let disk = out
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        let mnt = out
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()));
        (disk, mnt)
    }

    fn detach(disk: &str) {
        let _ = Command::new("hdiutil").args(["detach", disk]).status();
    }

    fn find_apfs_range() -> (u64, u64) {
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
            let Some(e) = head.get(base + i * sz..base + (i + 1) * sz) else {
                break;
            };
            if e[..16] == APFS_GPT_TYPE {
                let s = u64::from_le_bytes(e[32..40].try_into().unwrap());
                let en = u64::from_le_bytes(e[40..48].try_into().unwrap());
                return (s * SECTOR, (en - s + 1) * SECTOR);
            }
        }
        panic!("no APFS partition");
    }

    fn open_dev() -> Dev {
        let (base, len) = find_apfs_range();
        WritableOffsetBlockDevice::new(FileBlockDeviceRw::open_rw(IMG).unwrap(), base, len)
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

    /// One operation in a transaction of its own.
    fn one<R>(
        what: &str,
        op: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<R, TxnError>,
    ) -> R {
        let mut dev = open_dev();
        let (v, o) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap_or_else(|e| panic!("{what}: begin: {e:?}"));
        let r = t
            .atomically(|t| op(t, &v, &o))
            .unwrap_or_else(|e| panic!("{what}: {e:?}"));
        t.commit()
            .unwrap_or_else(|e| panic!("{what}: commit: {e:?}"));
        r
    }

    fn body(seed: u64, len: usize) -> Vec<u8> {
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn statvfs_total(mount: &str) -> u64 {
        let out = run(
            "python3",
            &[
                "-I",
                "-c",
                "import os,sys; s=os.statvfs(sys.argv[1]); print(s.f_blocks*s.f_frsize)",
                mount,
            ],
        );
        out.trim().parse().unwrap()
    }

    fn fsck_complaints() -> Vec<String> {
        let (disk, _) = attach(false, true);
        let rdev = format!("{disk}s2").replacen("/dev/disk", "/dev/rdisk", 1);
        let out = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        detach(&disk);
        let out = out.expect("fsck_apfs");
        let report = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        let mut bad: Vec<String> = report
            .lines()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("warning:")
                    || l.contains("error")
                    || l.contains("is not valid")
                    || l.contains("overallocation")
                    || l.contains("corrupt")
            })
            .map(str::to_string)
            .collect();
        if !out.status.success() {
            bad.push(format!("fsck_apfs exit {:?}", out.status.code()));
        }
        bad
    }

    /// Check one size; the failures found, empty when it passed.
    fn check(gib: u64, class: Class) -> Vec<String> {
        let mut fails = Vec::new();
        let label = format!("{gib} GiB ({class:?})");
        let _ = std::fs::remove_file(IMG);
        std::fs::File::create(IMG)
            .unwrap()
            .set_len(gib * GIB)
            .unwrap();
        let (disk, _) = attach(false, false);
        run(
            "diskutil",
            &["partitionDisk", &disk, "GPT", "APFS", "MATRIX", "100%"],
        );
        detach(&disk);

        // macOS writes first; its figures for the volume, from a mount.
        let (disk, mnt) = attach(true, false);
        let mnt = mnt.expect("no mount point");
        let theirs: Vec<(String, Vec<u8>)> = (0..40u64)
            .map(|i| {
                (
                    format!("mac/m{i}.bin"),
                    body(1000 + i, 1 + (i as usize * 7919) % 3_000_000),
                )
            })
            .collect();
        std::fs::create_dir_all(format!("{mnt}/mac")).unwrap();
        for (p, d) in &theirs {
            std::fs::write(format!("{mnt}/{p}"), d).unwrap();
        }
        let want_total = statvfs_total(&mnt);
        detach(&disk);

        // The class this size stands for, read from the disk.
        let mut dev = open_dev();
        let c = Container::open(&mut dev).unwrap();
        let nx = c.superblock.clone();
        let sm = apfs_core::checkpoint::ephemeral_object(&mut dev, &nx, nx.spaceman_oid)
            .unwrap()
            .unwrap();
        let u32_at = |at: usize| u32::from_le_bytes(sm[at..at + 4].try_into().unwrap());
        let (sm_blocks, cabs, ip_bm) = (
            sm.len() / nx.block_size as usize,
            u32_at(SM_MAIN_CAB_COUNT),
            u32_at(SM_IP_BM_SIZE_IN_BLOCKS),
        );
        let past_32 = nx.block_count > u64::from(u32::MAX);
        let is_class = match class {
            Class::Baseline => sm_blocks == 1 && cabs == 0 && ip_bm == 1,
            Class::PoolBitmapBlocks => ip_bm > 1,
            Class::TwoBlockSpaceman => sm_blocks == 2,
            Class::AddressBlocks => cabs > 0,
            Class::Past32Bits => past_32,
            Class::All => sm_blocks == 2 && cabs > 0 && past_32,
        };
        println!(
            "{label}: space manager {sm_blocks} block(s), {cabs} address blocks, \
             pool bitmap {ip_bm} block(s), {} blocks",
            nx.block_count
        );
        if !is_class {
            fails.push(format!("{label}: macOS laid it out outside its class"));
        }

        // The reader against macOS.
        let mut fs = apfs::FsView::open(open_dev()).expect("FsView::open");
        match fs.volume_size() {
            Ok((total, _)) if total == want_total => {}
            Ok((total, _)) => fails.push(format!("{label}: total {total}, macOS {want_total}")),
            Err(e) => fails.push(format!("{label}: volume_size: {e:?}")),
        }
        for (p, d) in &theirs {
            match fs.read(&format!("/{p}"), 0, d.len() + 1) {
                Ok(got) if &got == d => {}
                Ok(got) => fails.push(format!("{label}: /{p} read {} bytes, differs", got.len())),
                Err(e) => fails.push(format!("{label}: /{p}: {e:?}")),
            }
        }
        drop(fs);

        // The writer, over several transactions: files spanning chunks, a
        // snapshot over them, a rename and a delete under it, the snapshot
        // deleted, and the space it gave back taken again.
        let d = one("mkdir", |t, v, o| mkdir(t, v, o, ROOT, "ours"));
        let mut ours: Vec<(String, Vec<u8>)> = Vec::new();
        for (i, len) in [5_000usize, 9 << 20, 140 << 20].into_iter().enumerate() {
            let name = format!("f{i}.bin");
            let data = body(i as u64 + 1, len);
            one("write", |t, v, o| {
                write_file(t, v, o, d, &name, &data).map(|_| ())
            });
            ours.push((format!("ours/{name}"), data));
        }
        let snap = one("snapshot", |t, v, o| create_snapshot(t, v, o, "matrix"));
        one("rename", |t, v, o| {
            rename(t, v, o, d, "f0.bin", "renamed.bin", false)
        });
        ours[0].0 = "ours/renamed.bin".into();
        one("unlink", |t, v, o| unlink(t, v, o, d, "f1.bin"));
        ours.remove(1);
        one("delete snapshot", |t, v, o| delete_snapshot(t, v, o, snap));
        let again = body(77, 30 << 20);
        one("write again", |t, v, o| {
            write_file(t, v, o, d, "again.bin", &again).map(|_| ())
        });
        ours.push(("ours/again.bin".into(), again));

        for c in fsck_complaints() {
            fails.push(format!("{label}: fsck_apfs: {c}"));
        }

        // macOS reads back what we wrote, and its own files.
        let (disk, mnt) = attach(true, true);
        let mnt = mnt.expect("no mount point");
        for (p, d) in ours.iter().chain(theirs.iter()) {
            match std::fs::read(format!("{mnt}/{p}")) {
                Ok(got) if &got == d => {}
                Ok(got) => fails.push(format!(
                    "{label}: macOS read {p}: {} bytes, differs",
                    got.len()
                )),
                Err(e) => fails.push(format!("{label}: macOS read {p}: {e}")),
            }
        }
        detach(&disk);
        let _ = std::fs::remove_file(IMG);
        println!(
            "{label}: {}",
            if fails.is_empty() { "ok" } else { "FAILED" }
        );
        fails
    }

    #[test]
    #[ignore = "scratch-image gated; formats sparse images of 1-64 TiB on macOS"]
    fn every_size_class_reads_and_writes_as_macos_does() {
        if !gate() {
            return;
        }
        let only: Option<Vec<u64>> = std::env::var("SIZE_MATRIX_GIB")
            .ok()
            .map(|s| s.split(',').filter_map(|v| v.trim().parse().ok()).collect());
        let mut fails = Vec::new();
        for &(gib, class) in SIZES {
            if only.as_ref().is_some_and(|o| !o.contains(&gib)) {
                continue;
            }
            fails.extend(check(gib, class));
        }
        assert!(fails.is_empty(), "size matrix:\n{}", fails.join("\n"));
    }
}
