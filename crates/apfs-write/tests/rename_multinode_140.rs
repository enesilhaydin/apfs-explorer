//! #140 regression: renames on a MULTI-NODE filesystem B-tree stay fsck-clean.
//!
//! Background: the incremental rename fast path (`incremental_rename_drec`,
//! gated off by `RENAME_INCREMENTAL_FAST_PATH = false`) is DEFERRED - re-enabling
//! it surfaced two independent corruption mechanisms on multi-node trees, both
//! masked by the proven full-rebuild path (`rewrite_fstree`):
//!   1. Stale root `bt_longest_key` / `bt_longest_val` footer when a rename
//!      changes the longest key/value length ("invalid bt_longest_key …
//!      fsroot tree is invalid").
//!   2. Spaceman under-allocation: the sparse alloc/free pattern of an
//!      incremental rename drops prior-transaction blocks from the chunk/bitmap
//!      accounting that the uniform full-rebuild alloc/free does not.
//! See `docs/refs/cross-check-matrix.md` (an audit pass) for the full root-cause
//! analysis and the concrete work re-enabling #140 requires.
//!
//! Until then rename uses the full rebuild, which this test proves stays
//! fsck-clean even when the catalog has split into a multi-node tree (the prior
//! rename regression `rename_corruption_138` only exercised a single-node
//! N=12 catalog). It:
//!   1. Creates enough files to split the catalog into a MULTI-NODE tree
//!      (asserts the fstree root level > 0 - otherwise the test is meaningless).
//!   2. Renames each file (own commit), running the fsck arbiter to prove the
//!      on-disk tree stays clean.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]` - shells out to fsck, slow.
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test rename_multinode_140 \
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
mod r140 {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, rename};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-140-multinode.img";
    const SIZE_MB: u32 = 96;
    // Enough files to overflow a single 4 KiB catalog node into a multi-node
    // tree. Verified at runtime via the root-node level assertion below.
    const N: usize = 140;

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
    /// Read the fstree ROOT node level (0 = single leaf, >0 = multi-node tree)
    /// by resolving the volume's root_tree_oid through the volume omap, using
    /// the production parsers (no hand-rolled byte poking).
    fn fstree_root_level(img: &str) -> u16 {
        let raw = std::fs::read(img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).unwrap();
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        // Resolve the Data volume superblock through the container omap.
        let (bsz, vsb) = {
            let c = Container::open(&mut dev).unwrap();
            let nx = &c.superblock;
            let bsz = nx.block_size;
            let omap = Omap::open(&mut dev, nx.omap_oid, bsz).unwrap();
            let fs_oid = *nx.fs_oids.first().unwrap();
            let vp = omap
                .resolve(&mut dev, fs_oid, nx.obj.xid, bsz)
                .unwrap()
                .unwrap();
            let mut vsb_buf = vec![0u8; bsz as usize];
            dev.read_at(vp * bsz as u64, &mut vsb_buf).unwrap();
            (bsz, VolumeSuperblock::parse(&vsb_buf).unwrap())
        };
        // Resolve root_tree_oid through the VOLUME omap → fstree root paddr.
        let vol_omap = Omap::open(&mut dev, vsb.omap_oid, bsz).unwrap();
        let root_paddr = vol_omap
            .resolve(&mut dev, vsb.root_tree_oid, vsb.obj.xid, bsz)
            .unwrap()
            .expect("root_tree_oid resolves through volume omap");
        let mut node = vec![0u8; bsz as usize];
        dev.read_at(root_paddr * bsz as u64, &mut node).unwrap();
        u16::from_le_bytes(node[34..36].try_into().unwrap())
    }
    fn open_txn(img: &str) -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }

    #[test]
    #[ignore = "fsck-gated; slow (multi-node create + rename sweep)"]
    fn multinode_rename_stays_fsck_clean() {
        if !check_gate() {
            println!("#140: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // 1. Create N files in the root dir, each its own commit.
        for i in 0..N {
            let (vsb, om) = read_vsb_omap(IMG);
            let mut txn = open_txn(IMG);
            let name = format!("file_{i:04}.dat");
            create_file(&mut txn, &vsb, &om, 2, &name).expect("create_file");
            txn.commit().expect("commit create");
        }
        let (ok, _) = verify();
        assert!(ok, "#140 precondition: {N} creates must be fsck-clean");

        // Precondition: the catalog MUST be multi-node, else the fast path's
        // parent-pivot invariant is never exercised and the test proves nothing.
        let level = fstree_root_level(IMG);
        println!("[#140] fstree root level after {N} creates = {level}");
        assert!(
            level > 0,
            "#140: fstree is single-node (level {level}) after {N} creates - \
             bump N until the catalog splits, otherwise this test is vacuous"
        );

        // 2. Rename every file, own commit, fsck the arbiter periodically and at
        //    the first rename (where #138 corruption first surfaced).
        for i in 0..N {
            let old = format!("file_{i:04}.dat");
            let new = format!("renamed_{i:04}.dat");
            let (vsb, om) = read_vsb_omap(IMG);
            let mut txn = open_txn(IMG);
            rename(&mut txn, &vsb, &om, 2, &old, &new, false)
                .unwrap_or_else(|e| panic!("rename {i} failed: {e}"));
            txn.commit()
                .unwrap_or_else(|e| panic!("commit rename {i} failed: {e}"));

            if i == 0 {
                let _ = std::fs::copy(IMG, "/tmp/apfs-140-r0.img");
            }
            if i == 0 || i == N / 2 || i == N - 1 {
                let (ok, out) = verify();
                let verdict: Vec<&str> = out
                    .lines()
                    .filter(|l| {
                        l.contains("overallocation")
                            || l.contains("appears to be OK")
                            || l.contains("corrupt")
                            || l.contains("is not valid")
                            || l.contains("invalid")
                            || l.contains("reported DIRTY")
                    })
                    .collect();
                println!("[#140 rename {i:>3}] fsck_clean={ok}  {verdict:?}");
                assert!(
                    ok,
                    "#140: rename #{i} produced an fsck-DIRTY/corrupt volume"
                );
            }
        }
    }
}
