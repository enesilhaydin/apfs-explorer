//! macOS working on what we wrote keeps the volume consistent.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, fsck_apfs).
//!
//! fsck right after our commit checks what we wrote against itself. What we
//! leave behind can still be wrong in a way only the next writer shows: macOS
//! adjusts the volume's counters by what it removes (a total we never raised
//! goes below what it subtracts), and it edits our tree nodes with its own
//! assumptions about their layout. Each case here writes with our code, lets
//! macOS remove everything it can see, and runs fsck.
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
mod macos_after_us {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        clone_file, create_file, mkdir, overwrite_existing_file, rename, truncate_file_fast,
        unlink, unshare_stream, write_range,
    };
    use apfs_write::txn::{Transaction, TxnError};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-macos-after-us.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ROOT: u64 = 2;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
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
            .unwrap()
            .to_string();
        (mount, parent)
    }

    fn detach(parent: &str) {
        for _ in 0..5 {
            if Command::new("hdiutil")
                .args(["detach", parent])
                .status()
                .is_ok_and(|s| s.success())
            {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
        let _ = Command::new("hdiutil")
            .args(["detach", "-force", parent])
            .status();
    }

    fn fresh_image(seed: &dyn Fn(&str)) {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-macos-after-us.dmg");
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
                "AFTERUS",
                "/tmp/apfs-macos-after-us",
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "hdiutil create");
        std::fs::rename("/tmp/apfs-macos-after-us.dmg", IMG).unwrap();
        let (mount, parent) = attach(true);
        seed(&mount);
        let _ = Command::new("sync").status();
        detach(&parent);
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

    fn ours(f: &mut Op<'_>) {
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).expect("begin");
        f(&mut t, &vsb, &om).expect("operation");
        t.commit().expect("commit");
    }

    /// fsck complaints (empty when clean).
    fn fsck() -> Vec<String> {
        let (_, parent) = attach(false);
        let dev = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let out = Command::new("fsck_apfs")
            .args(["-n", &dev])
            .output()
            .unwrap();
        detach(&parent);
        let report = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        let mut bad: Vec<String> = report
            .lines()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("warning:") || l.contains("error:") || l.contains("is not valid")
            })
            .map(str::to_string)
            .collect();
        if !out.status.success() {
            bad.push(format!("fsck exit {:?}", out.status.code()));
        }
        bad
    }

    /// macOS removes everything visible at the volume root.
    fn macos_removes_all() {
        let (mount, parent) = attach(true);
        for e in std::fs::read_dir(&mount).unwrap() {
            let p = e.unwrap().path();
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if p.is_dir() {
                std::fs::remove_dir_all(&p).unwrap();
            } else {
                std::fs::remove_file(&p).unwrap();
            }
        }
        let _ = Command::new("sync").status();
        detach(&parent);
    }

    fn case(label: &str, seed: &dyn Fn(&str), f: &mut Op<'_>) -> Vec<String> {
        fresh_image(seed);
        ours(f);
        let after_us = fsck();
        assert!(
            after_us.is_empty(),
            "{label}: fsck after our commit: {after_us:?}"
        );
        macos_removes_all();
        let after_macos = fsck();
        println!("case {label}: {after_macos:?}");
        after_macos
    }

    fn no_seed(_: &str) {}

    fn seeded(mount: &str) {
        for i in 0..5 {
            std::fs::write(format!("{mount}/m{i}"), vec![i as u8; 9000]).unwrap();
        }
        std::fs::write(format!("{mount}/big"), vec![1u8; 4 << 20]).unwrap();
        let _ = Command::new("cp")
            .args(["-c", &format!("{mount}/big"), &format!("{mount}/big_clone")])
            .status();
    }

    #[test]
    #[ignore = "macOS only: hdiutil + fsck_apfs"]
    fn macos_removing_what_we_wrote_leaves_the_counters_right() {
        if !gate() {
            println!("macos_after_us: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let mut failed: Vec<(String, Vec<String>)> = Vec::new();
        let mut run = |label: &str, seed: &dyn Fn(&str), f: &mut Op<'_>| {
            let bad = case(label, seed, f);
            if !bad.is_empty() {
                failed.push((label.to_string(), bad));
            }
        };
        run("create + write_range", &no_seed, &mut |t, v, o| {
            for i in 0..5 {
                let name = format!("f{i}");
                create_file(t, v, o, ROOT, &name)?;
                write_range(t, v, o, ROOT, &name, 0, &vec![2u8; 9000])?;
            }
            Ok(())
        });
        run("create + overwrite", &no_seed, &mut |t, v, o| {
            for i in 0..5 {
                let name = format!("f{i}");
                create_file(t, v, o, ROOT, &name)?;
                overwrite_existing_file(t, v, o, ROOT, &name, &vec![2u8; 9000])?;
            }
            Ok(())
        });
        run("mkdir + files", &no_seed, &mut |t, v, o| {
            let d = mkdir(t, v, o, ROOT, "dir")?;
            for i in 0..5 {
                create_file(t, v, o, d, &format!("f{i}"))?;
            }
            Ok(())
        });
        run("unlink macOS files", &seeded, &mut |t, v, o| {
            unlink(t, v, o, ROOT, "m0")?;
            unlink(t, v, o, ROOT, "m1")
        });
        run("rename macOS files", &seeded, &mut |t, v, o| {
            rename(t, v, o, ROOT, "m0", "n0", false)?;
            rename(t, v, o, ROOT, "m1", "m2", true)
        });
        run("edit macOS files", &seeded, &mut |t, v, o| {
            write_range(t, v, o, ROOT, "m0", 100, b"edit")?;
            write_range(t, v, o, ROOT, "m1", 9000, &vec![3u8; 20_000])?;
            truncate_file_fast(t, v, o, ROOT, "m2", 4000, 9000).map(|_| ())
        });
        run("unshare a macOS clone", &seeded, &mut |t, v, o| {
            unshare_stream(t, v, o, ROOT, "big_clone")?;
            write_range(t, v, o, ROOT, "big_clone", 0, b"mine").map(|_| ())
        });
        run("clone_file", &seeded, &mut |t, v, o| {
            clone_file(t, v, o, ROOT, "m3", ROOT, "m3_copy").map(|_| ())
        });
        // The real-USB workload's shape: many files in one directory, some
        // removed and some renamed in the same transaction. With tables of
        // contents cut to their exact record count, macOS removing these
        // moved records between nodes without moving the separator above
        // them, left files it could list but not open, and fsck found the
        // tree invalid.
        run(
            "many files, removed and renamed",
            &seeded,
            &mut |t, v, o| {
                let d = mkdir(t, v, o, ROOT, "ours")?;
                for i in 0..1500 {
                    let name = format!("f{i}");
                    create_file(t, v, o, d, &name)?;
                    write_range(t, v, o, d, &name, 0, &[i as u8; 4096])?;
                }
                for i in 0..400 {
                    rename(t, v, o, d, &format!("f{i}"), &format!("r{i}"), false)?;
                }
                for i in 400..800 {
                    unlink(t, v, o, d, &format!("f{i}"))?;
                }
                Ok(())
            },
        );
        let _ = std::fs::remove_file(IMG);
        assert!(failed.is_empty(), "inconsistent after macOS: {failed:#?}");
    }
}
