//! #151 deterministic reproduction: the extentref tree must grow MULTI-NODE.
//!
//! Each file-with-content adds a `phys_ext` record to the volume's extentref
//! tree via `insert_catalog_records` -> `pack_cat_leaf`, which is SINGLE-NODE
//! only. Past ~250 files the records overflow one 4 KiB node and the write
//! fails with `SpacemanParse("node overflow")`. On a real mount this surfaces
//! at the async WinFsp Cleanup/flush, AFTER the app's write() returned success,
//! so the writes vanish silently. RED until the extentref tree splits.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]` - shells out to fsck.
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --features write \
//!     --test extref_multinode_151 -- --ignored --nocapture --test-threads=1
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
mod r151 {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, overwrite_existing_file};
    use apfs_write::txn::{Transaction, TxnError};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-151-extref.img";
    const SIZE_MB: u32 = 256;
    const SECTOR: u64 = 512;
    /// Enough files-with-content to overflow a single extentref node.
    const N_FILES: usize = 300;

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
    fn open_txn(img: &str) -> Transaction<Dev> {
        let raw = std::fs::read(img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }
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

    #[test]
    #[ignore = "fsck-gated repro; slow"]
    fn many_files_with_content_stay_fsck_clean() {
        if !gate() {
            println!("#151: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // Create N files, each with a small content extent → one phys_ext per
        // file in the extentref tree. Past one node, the un-fixed engine panics
        // here with "node overflow" (the bug); the fixed engine splits the tree.
        for i in 0..N_FILES {
            let name = format!("f{i}.txt");
            commit_op("create", |t, v, o| {
                create_file(t, v, o, 2, &name).map(|_| ())
            });
            let body = format!("content {i}\n");
            commit_op("write", |t, v, o| {
                overwrite_existing_file(t, v, o, 2, &name, body.as_bytes())
            });
        }

        let (ok, out) = verify();
        let verdict: Vec<&str> = out
            .lines()
            .filter(|l| {
                l.contains("extent")
                    || l.contains("invalid")
                    || l.contains("corrupt")
                    || l.contains("appears to be OK")
                    || l.contains("overflow")
            })
            .collect();
        println!("[#151] {N_FILES} files-with-content: fsck_clean={ok}\n  {verdict:?}");
        assert!(
            ok,
            "#151: extentref tree must stay fsck-clean past one node"
        );
    }
}
