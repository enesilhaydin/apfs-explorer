//! #149 deterministic reproduction: writing to a volume that HAS a snapshot.
//!
//! Real-USB writable proof failed fsck (sm_free_count off + sm_fq overlap) on a
//! volume with a pre-existing (macOS) snapshot. The engine's single-op write and
//! the snapshot tests are each fsck-clean, but the COMBINATION - a snapshot
//! present, then several subsequent write commits that COW metadata pinned by
//! that snapshot (routed to sm_fq by free_replaced_metadata) - is the suspect.
//!
//! This test creates a snapshot, then does N write commits while it is live, and
//! fsck-checks. RED until #149 is fixed; GREEN proves snapshotted-volume writes.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]` - shells out to fsck.
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --features write \
//!     --test snapshot_write_repro_149 -- --ignored --nocapture --test-threads=1
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

#[cfg(test)]
mod r149 {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, mkdir, overwrite_existing_file, rename, unlink};
    use apfs_write::snapshot::create_snapshot;
    use apfs_write::txn::{Transaction, TxnError};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    /// Run one op in its own fresh-begin transaction + commit (mirrors the live
    /// mount: each op re-reads vsb/omap, mutates, commits).
    fn commit_op<F>(label: &str, f: F)
    where
        F: FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<(), TxnError>,
    {
        let (vsb, om) = read_vsb_omap(IMG);
        let mut txn = open_txn(IMG);
        f(&mut txn, &vsb, &om).unwrap_or_else(|e| panic!("{label}: {e}"));
        txn.commit()
            .unwrap_or_else(|e| panic!("commit {label}: {e}"));
    }

    const IMG: &str = "/tmp/apfs-149-snapwrite.img";
    const SIZE_MB: u32 = 96;
    const SECTOR: u64 = 512;
    const N_WRITES: usize = 8;

    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF,0x57,0x34,0x7C,0x00,0x00,0xAA,0x11,0xAA,0x11,0x00,0x30,0x65,0x43,0xEC,0xAC,
    ];
    fn gate() -> bool {
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
    fn open_txn(img: &str) -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }

    #[test]
    #[ignore = "fsck-gated repro; slow"]
    fn writes_after_snapshot_stay_fsck_clean() {
        if !gate() {
            println!("#149: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // 1. PRE-SNAPSHOT dataset (mirrors the real USB: macOS wrote files first,
        //    then a snapshot pinned that whole catalog + the files' extents).
        for i in 0..6 {
            let name = format!("pre_{i}.txt");
            commit_op("create-pre", |t, v, o| {
                create_file(t, v, o, 2, &name).map(|_| ())
            });
            let body = format!("pre-snapshot data {i}\n").repeat(50);
            commit_op("write-pre", |t, v, o| {
                overwrite_existing_file(t, v, o, 2, &name, body.as_bytes())
            });
        }
        commit_op("mkdir-pre", |t, v, o| {
            mkdir(t, v, o, 2, "predir").map(|_| ())
        });

        // 2. Snapshot - pins everything above.
        commit_op("snapshot", |t, v, o| {
            create_snapshot(t, v, o, "snap1").map(|_| ())
        });
        let (ok, _) = verify();
        assert!(
            ok,
            "#149 precondition: fsck must be clean right after the snapshot"
        );

        // 3. Post-snapshot MIXED matrix - each op COWs catalog/omap/extents that
        //    the snapshot pins, routing the old blocks to sm_fq across commits.
        for i in 0..N_WRITES {
            let name = format!("new_{i}.txt");
            commit_op("create-new", |t, v, o| {
                create_file(t, v, o, 2, &name).map(|_| ())
            });
            let body = format!("post-snapshot file {i}\n").repeat(40);
            commit_op("write-new", |t, v, o| {
                overwrite_existing_file(t, v, o, 2, &name, body.as_bytes())
            });
        }
        // rename a pre-snapshot file (COWs the catalog the snapshot pins)
        commit_op("rename", |t, v, o| {
            rename(t, v, o, 2, "pre_0.txt", "pre_0_renamed.txt", false)
        });
        // overwrite a pre-snapshot file (COWs its snapshot-pinned extents)
        let grow = "x".repeat(9000);
        commit_op("overwrite-pinned", |t, v, o| {
            overwrite_existing_file(t, v, o, 2, "pre_1.txt", grow.as_bytes())
        });
        // delete pre-snapshot files (frees snapshot-pinned metadata → sm_fq)
        commit_op("unlink-2", |t, v, o| unlink(t, v, o, 2, "pre_2.txt"));
        commit_op("unlink-3", |t, v, o| unlink(t, v, o, 2, "pre_3.txt"));

        // 3. fsck - the decisive arbiter.
        let (ok, out) = verify();
        let verdict: Vec<&str> = out
            .lines()
            .filter(|l| {
                l.contains("sm_free_count")
                    || l.contains("free queue")
                    || l.contains("overlap")
                    || l.contains("appears to be OK")
                    || l.contains("corrupt")
                    || l.contains("invalid")
                    || l.contains("underalloc")
                    || l.contains("overalloc")
            })
            .collect();
        println!("[#149] after snapshot + {N_WRITES} writes: fsck_clean={ok}\n  {verdict:?}");
        assert!(ok, "#149: writing to a snapshotted volume left fsck-DIRTY");
    }
}
