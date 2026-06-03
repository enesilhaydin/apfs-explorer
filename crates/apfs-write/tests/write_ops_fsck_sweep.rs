//! Systematic fsck sweep over the core write operations.
//!
//! Motivation (#138 lesson): the write fast paths were historically validated
//! only by the lenient in-process `assert_device_parseable` check, never by
//! `fsck_apfs`. #138 proved that a path can pass the parser yet produce a volume
//! fsck calls "corrupt". This sweep runs the fsck arbiter (xtask verify-scratch)
//! after EVERY core operation, so any parser-clean-but-fsck-invalid op is caught.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` + `#[ignore]` (slow: an fsck attach per step).
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test write_ops_fsck_sweep \
//!     -- --ignored --nocapture --test-threads=1
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
mod sweep {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        clone_file, create_file, mkdir, overwrite_existing_file, rename, set_xattr,
        truncate_file_fast, unlink, write_file,
    };
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-sweep.img";
    const SIZE_MB: u32 = 64;

    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const SECTOR: u64 = 512;

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn root() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }
    fn gen_scratch() {
        let s = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                IMG,
                &SIZE_MB.to_string(),
            ])
            .current_dir(root())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
    }
    fn verify() -> (bool, String) {
        let o = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--", "verify-scratch", IMG])
            .current_dir(root())
            .output()
            .expect("verify-scratch");
        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status.success(), s)
    }
    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let hdr = &raw[512..1024];
        let pe = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let np = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let ps = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let eo = (pe * SECTOR) as usize;
        for i in 0..np {
            let off = eo + i * ps;
            let e = match raw.get(off..off + ps) {
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
        let xid = nx.obj.xid;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).unwrap();
        let fs_oid = *nx.fs_oids.first().unwrap();
        let vp = omap
            .resolve(&mut dev, fs_oid, xid, nx.block_size)
            .unwrap()
            .unwrap();
        let mut vsb = vec![0u8; bsz];
        dev.read_at(vp * bsz as u64, &mut vsb).unwrap();
        let v = VolumeSuperblock::parse(&vsb).unwrap();
        let mut om = vec![0u8; bsz];
        dev.read_at(v.omap_oid * bsz as u64, &mut om).unwrap();
        (vsb, om)
    }
    fn open_txn() -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }

    /// Run `op` (which performs one or more staged mutations on the txn it is
    /// given) in its own transaction, commit, then fsck. Records the first
    /// fsck-DIRTY step.
    fn step<F>(label: &str, fails: &mut Vec<String>, op: F)
    where
        F: FnOnce(
            &mut Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>>,
            &[u8],
            &[u8],
        ) -> Result<(), String>,
    {
        let (vsb, om) = read_vsb_omap();
        let mut txn = open_txn();
        let opres = op(&mut txn, &vsb, &om);
        let commit_ok = match opres {
            Ok(()) => match txn.commit() {
                Ok(_) => true,
                Err(e) => {
                    fails.push(format!("{label}: COMMIT-ERR {e}"));
                    false
                }
            },
            Err(e) => {
                let _ = txn.abort();
                fails.push(format!("{label}: OP-ERR {e}"));
                false
            }
        };
        if !commit_ok {
            return;
        }
        let (ok, out) = verify();
        let verdict: Vec<&str> = out
            .lines()
            .filter(|l| {
                l.contains("overallocation")
                    || l.contains("corrupt")
                    || l.contains("is not valid")
                    || l.contains("invalid")
            })
            .collect();
        println!("[{label:<22}] fsck_clean={ok}  {verdict:?}");
        if !ok {
            fails.push(format!("{label}: fsck DIRTY {verdict:?}"));
        }
    }

    #[test]
    #[ignore = "slow fsck sweep; gated by APFS_WRITE_SCRATCH=1"]
    fn all_core_write_ops_stay_fsck_clean() {
        if !check_gate() {
            println!("sweep: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        let (ok, _) = verify();
        assert!(ok, "precondition: fresh scratch must be fsck-clean");

        let mut fails: Vec<String> = Vec::new();

        step("create_file", &mut fails, |t, v, o| {
            create_file(t, v, o, 2, "a.txt")
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        step("write_small", &mut fails, |t, v, o| {
            write_file(t, v, o, 2, "small.txt", b"hello world")
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        step("write_large_40k", &mut fails, |t, v, o| {
            write_file(t, v, o, 2, "big.txt", &vec![0xABu8; 40 * 1024])
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        step("mkdir", &mut fails, |t, v, o| {
            mkdir(t, v, o, 2, "d1")
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        step("truncate_shrink_big", &mut fails, |t, v, o| {
            truncate_file_fast(t, v, o, 2, "big.txt", 8 * 1024, 40 * 1024)
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        step("overwrite_small", &mut fails, |t, v, o| {
            overwrite_existing_file(t, v, o, 2, "small.txt", b"replaced content here")
                .map_err(|e| e.to_string())
        });
        step("set_xattr_embedded", &mut fails, |t, v, o| {
            // a.txt is inode-id 16 on a fresh volume (first user create); resolve by
            // reading would be ideal, but set_xattr keys by inode id - use the known id.
            set_xattr(t, v, o, 16, "user.tag", b"v1").map_err(|e| e.to_string())
        });
        step("rename", &mut fails, |t, v, o| {
            rename(t, v, o, 2, "a.txt", "a_renamed.txt", false).map_err(|e| e.to_string())
        });
        step("unlink", &mut fails, |t, v, o| {
            unlink(t, v, o, 2, "big.txt").map_err(|e| e.to_string())
        });
        // NOTE: clone_file is intentionally NOT swept here - it is known to corrupt
        // the fsroot (orphan/invalid dstream_id + file_extent objects). Reproduced
        // separately by `clone_file_corrupts_fsroot_141` (task #141). clone is not
        // user-reachable on Windows (FSCTL_DUPLICATE_EXTENTS -> NOT_SUPPORTED).

        if !fails.is_empty() {
            panic!(
                "write-ops fsck sweep found {} failing step(s):\n  {}",
                fails.len(),
                fails.join("\n  ")
            );
        }
        println!("SWEEP PASS: all core write ops stay fsck-clean");
    }

    /// #141 (GREEN since the shared-dstream fix): `clone_file` must leave the
    /// volume fsck-clean. Apple's clonefile (IDA apfs.kext `clone_item`, an audit pass)
    /// SHARES the source's data stream - the clone inode's private_id points at
    /// the source's data-stream id, both inodes are marked WAS_CLONED, and the
    /// source dstream_id refcnt goes 1->2; NO new dstream_id / file_extent record
    /// and NO extentref change. The previous impl fabricated independent records
    /// under a new id (fsck "orphan dstream id / file extent") and read the source
    /// inode's mode/uid/gid from the wrong offsets (fsck "Update inode objects").
    /// Not user-reachable on Windows (FSCTL_DUPLICATE_EXTENTS -> NOT_SUPPORTED).
    #[test]
    #[ignore = "slow fsck (#141 regression); gated by APFS_WRITE_SCRATCH=1"]
    fn clone_file_shared_dstream_fsck_clean_141() {
        if !check_gate() {
            println!("#141: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, "src.txt", b"clone me please").expect("write src");
            txn.commit().expect("commit src");
        }
        let (ok0, _) = verify();
        assert!(ok0, "#141 precondition: source write must be fsck-clean");
        let clone_ino = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let ino = clone_file(&mut txn, &vsb, &om, 2, "src.txt", 2, "dst.txt").expect("clone");
            txn.commit().expect("commit clone");
            ino
        };
        let (ok, out) = verify();
        let v: Vec<&str> = out
            .lines()
            .filter(|l| l.contains("corrupt") || l.contains("orphan") || l.contains("invalid"))
            .collect();
        println!("#141 clone ino={clone_ino} fsck_clean={ok}  {v:?}");
        assert!(ok, "#141: clone_file must leave the volume fsck-clean");
    }
}
