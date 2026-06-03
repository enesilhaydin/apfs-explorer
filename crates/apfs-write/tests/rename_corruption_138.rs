//! #138 diagnostic: localize when sequential individual-commit renames corrupt
//! the catalog. After EACH rename we (a) record the rename Result and (b) run
//! the fsck arbiter (xtask verify-scratch), so we can tell whether corruption
//! lands on disk at a specific rename, or whether a later rename merely fails to
//! find an entry that a prior rename silently lost.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. Diagnostic only - `#[ignore]` so it never runs
//! in the default suite (it shells out to fsck per iteration and is slow).
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test rename_corruption_138 \
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
mod r138 {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, rename};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-138-repro.img";
    const SIZE_MB: u32 = 64;
    const N: usize = 12;

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
    fn read_vsb_omap(img: &str) -> (Vec<u8>, Vec<u8>) {
        let raw = std::fs::read(img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).unwrap();
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
    fn open_txn(img: &str) -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }

    #[test]
    #[ignore = "diagnostic; slow (fsck per rename)"]
    fn localize_sequential_rename_corruption() {
        if !check_gate() {
            println!("#138 diag: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        for i in 0..N {
            let (vsb, om) = read_vsb_omap(IMG);
            let mut txn = open_txn(IMG);
            let name = format!("f_{i}.txt");
            create_file(&mut txn, &vsb, &om, 2, &name).expect("create_file");
            txn.commit().expect("commit create");
        }
        let (ok, _) = verify();
        println!("[after {N} creates] fsck_clean={ok}");
        assert!(ok, "#138 precondition: {N} creates must be fsck-clean");

        let mut first_dirty: Option<usize> = None;
        for i in 0..N {
            let old = format!("f_{i}.txt");
            let new = format!("r_{i}.txt");
            let (vsb, om) = read_vsb_omap(IMG);
            let mut txn = open_txn(IMG);
            let rres = rename(&mut txn, &vsb, &om, 2, &old, &new, false);
            let rdesc = match &rres {
                Ok(_) => "ok".to_string(),
                Err(e) => format!("RENAME-ERR: {e}"),
            };
            let cdesc = if rres.is_ok() {
                match txn.commit() {
                    Ok(_) => "ok".to_string(),
                    Err(e) => format!("COMMIT-ERR: {e}"),
                }
            } else {
                let _ = txn.abort();
                "skipped".to_string()
            };
            if i == 0 {
                // Preserve the cleanest post-rename-0 corruption for offline decode.
                let _ = std::fs::copy(IMG, "/tmp/apfs-138-r0.img");
            }
            let (ok, out) = verify();
            // Extract only the fsck verdict lines to keep output tight.
            let verdict: Vec<&str> = out
                .lines()
                .filter(|l| {
                    l.contains("overallocation")
                        || l.contains("appears to be OK")
                        || l.contains("corrupt")
                        || l.contains("is not valid")
                        || l.contains("reported DIRTY")
                })
                .collect();
            println!("[rename {i:>2}] rename={rdesc} commit={cdesc} fsck_clean={ok}  {verdict:?}");
            if !ok && first_dirty.is_none() {
                first_dirty = Some(i);
            }
        }

        // RED for #138: the rename COW path corrupts the volume (fsck reports the
        // volume "found to be corrupt and cannot be repaired", first observed at
        // rename 0). Will go GREEN when #138 is fixed.
        assert!(
            first_dirty.is_none(),
            "#138: rename #{} produced an fsck-DIRTY/corrupt volume",
            first_dirty.unwrap()
        );
    }
}
