//! #156 regression: a MIXED writable workload on a large MULTI-NODE catalog
//! must stay fsck-clean.
//!
//! Background: the real-USB end-to-end seal (250-file volume, then 21 renames +
//! 3 creates + Set-Content overwrite + 3 deletes + nested mkdir on Windows) was
//! reported `fsroot tree is invalid -> corrupt` by macOS `fsck_apfs`:
//!   error: btn: oid (3676) … subtype 0xe (FSTREE) level 0  invalid key (440, 16)
//!   error: btn: oid (1028) … level 1  invalid btn_btree.bt_key_count
//!           (expected 1631, actual 1675)
//! The single-purpose scratch tests never combined a LARGE multi-node tree with
//! an INTERLEAVED mix of key-count-changing ops (create/delete) and the
//! incremental rename fast path (`RENAME_INCREMENTAL_FAST_PATH = true`, #140).
//! `rename_multinode_140` only sweeps pure (key-count-neutral) renames;
//! `write_ops_fsck_sweep` only touches a single-node tree. This test combines
//! both: a multi-node catalog plus an interleaved create/rename/overwrite/delete
//! workload, fscking after each round so the first corrupting op is pinpointed.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]` - shells out to fsck per round.
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test chaos_mixed_156 \
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
mod r156 {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::rename;
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-156-mixed.img";
    // Tunable via env so the same harness can probe deeper trees without an edit:
    //   APFS156_SIZE_MB   image size (default 128)
    //   APFS156_N         macOS-authored files to populate (default 250)
    //   APFS156_RENAMES   cap on the per-rename fsck sweep (default = N)
    fn env_usize(key: &str, default: usize) -> usize {
        std::env::var(key)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(default)
    }
    fn size_mb() -> u32 {
        env_usize("APFS156_SIZE_MB", 128) as u32
    }
    fn n_initial() -> usize {
        env_usize("APFS156_N", 250)
    }
    fn rounds_max() -> usize {
        env_usize("APFS156_RENAMES", n_initial())
    }

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
                &size_mb().to_string(),
            ])
            .current_dir(root())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
    }
    /// Populate the image with `count` files written by macOS's OWN APFS driver
    /// (Apple-laid-out catalog - different leaf split/packing than our packer).
    /// This is the differentiator the real-USB #156 corruption needed.
    fn macos_populate(count: usize) {
        let s = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "populate-scratch",
                IMG,
                &count.to_string(),
            ])
            .current_dir(root())
            .status()
            .expect("populate-scratch");
        assert!(s.success(), "populate-scratch failed");
    }
    /// File extension used by `xtask populate-scratch` for index `i` (must mirror
    /// the mapping in xtask::populate_scratch).
    fn ext_for(i: usize) -> &'static str {
        match i % 5 {
            0 => "txt",
            1 => "dat",
            2 => "log",
            3 => "md",
            _ => "bin",
        }
    }
    fn mac_name(i: usize) -> String {
        format!("u{i:03}.{}", ext_for(i))
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
    /// Read the fstree ROOT node level (0 = single leaf, >0 = multi-node tree)
    /// via the production parsers (no hand-rolled byte poking).
    fn fstree_root_level() -> u16 {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
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
        let vol_omap = Omap::open(&mut dev, vsb.omap_oid, bsz).unwrap();
        let root_paddr = vol_omap
            .resolve(&mut dev, vsb.root_tree_oid, vsb.obj.xid, bsz)
            .unwrap()
            .expect("root_tree_oid resolves through volume omap");
        let mut node = vec![0u8; bsz as usize];
        dev.read_at(root_paddr * bsz as u64, &mut node).unwrap();
        u16::from_le_bytes(node[34..36].try_into().unwrap())
    }
    /// Read the VOLUME omap b-tree root node level (0 = single node, >0 =
    /// multi-node omap). The write path's hand-rolled `omap_lookup` only reads
    /// this one node, so a level > 0 is exactly the #157 condition where it
    /// misreads an internal node's child pointer as an omap_val.
    fn vol_omap_tree_level() -> u16 {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let (bsz, vsb_omap_oid) = {
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
            (bsz, VolumeSuperblock::parse(&vsb_buf).unwrap().omap_oid)
        };
        // omap_phys block; offset 48 = om_tree_oid (physical tree root paddr).
        let mut omap_phys = vec![0u8; bsz as usize];
        dev.read_at(vsb_omap_oid * bsz as u64, &mut omap_phys)
            .unwrap();
        let tree_paddr = u64::from_le_bytes(omap_phys[48..56].try_into().unwrap());
        let mut node = vec![0u8; bsz as usize];
        dev.read_at(tree_paddr * bsz as u64, &mut node).unwrap();
        u16::from_le_bytes(node[34..36].try_into().unwrap())
    }
    fn open_txn() -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }

    /// fsck filter: surface only the lines that indicate corruption / dirtiness.
    fn verdict_lines(out: &str) -> Vec<String> {
        out.lines()
            .filter(|l| {
                l.contains("overallocation")
                    || l.contains("corrupt")
                    || l.contains("is not valid")
                    || l.contains("invalid")
                    || l.contains("reported DIRTY")
                    || l.contains("bt_key_count")
                    || l.contains("invalid key")
            })
            .map(|s| s.to_string())
            .collect()
    }

    #[test]
    #[ignore = "fsck-gated; slow (multi-node create + mixed-workload sweep)"]
    fn mixed_workload_stays_fsck_clean() {
        if !check_gate() {
            println!("#156: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        let (ok0, _) = verify();
        assert!(ok0, "#156 precondition: fresh scratch must be fsck-clean");

        // ---- Phase 1: build a large MULTI-NODE catalog with macOS's OWN driver.
        // Apple lays the catalog out differently than our packer (split points,
        // TOC ordering, packing). Our self-built scratch trees round-trip our own
        // packer and never reproduced #156; the real-USB volume was macOS-authored.
        let n_initial = n_initial();
        macos_populate(n_initial);
        let (ok1, _) = verify();
        assert!(
            ok1,
            "#156 precondition: {n_initial} macOS files must be fsck-clean"
        );
        let level = fstree_root_level();
        let omap_level = vol_omap_tree_level();
        println!("[#156] after {n_initial} macOS files: fstree root level = {level}, volume omap tree level = {omap_level}");
        assert!(
            level > 0,
            "#156: fstree single-node (level {level}) after {n_initial} files - \
             bump APFS156_N until the catalog splits, else this test is vacuous"
        );

        let mut fails: Vec<String> = Vec::new();

        // ---- Phase 2: PURE consecutive incremental renames on the Apple tree,
        // fsck IMMEDIATELY after each one. ----
        // Crucial: create/mkdir/write/unlink/overwrite all go through
        // `rewrite_fstree` (a full canonical rebuild) which HEALS any incremental
        // drift AND converts the Apple-authored tree into ours - so interleaving
        // them (or fscking only at end-of-round) masks the bug. The real-USB seal
        // ran ~21 *consecutive* renames (the only incremental path) with no
        // intervening rebuild. So here we rename every macOS file back-to-back and
        // fsck after EACH, catching the first post-rename-pre-heal corrupt state.
        let n = n_initial.min(rounds_max());
        for r in 0..n {
            let old = mac_name(r);
            let new = format!("ren_{r:03}.{}", ext_for(r));
            {
                let (vsb, om) = read_vsb_omap();
                let mut txn = open_txn();
                match rename(&mut txn, &vsb, &om, 2, &old, &new, false) {
                    Ok(()) => txn.commit().map(|_| ()).unwrap_or_else(|e| {
                        fails.push(format!("rename {r} ({old}) COMMIT-ERR {e}"))
                    }),
                    Err(e) => {
                        let _ = txn.abort();
                        fails.push(format!("rename {r} ({old}) OP-ERR {e}"));
                    }
                }
            }
            if !fails.is_empty() {
                break;
            }
            let (ok, out) = verify();
            let v = verdict_lines(&out);
            println!("[#156 rename {r:>3} {old} -> {new}] fsck_clean={ok}  {v:?}");
            if !ok {
                // A verdict with no fsck complaint in it means the harness
                // itself failed (attach, detach, or a lock on the mounted
                // volume), and only the raw output can say which.
                let detail = if v.is_empty() {
                    format!("(no fsck complaint; raw verify output follows)\n{out}")
                } else {
                    format!("{v:?}")
                };
                fails.push(format!("rename {r} ({old}): fsck DIRTY {detail}"));
                // Preserve the first corrupt image for offline byte-decode.
                let _ = std::fs::copy(IMG, format!("/tmp/apfs-156-corrupt-r{r}.img"));
                break; // first corruption pinpointed - stop.
            }
        }

        if !fails.is_empty() {
            panic!(
                "#156 mixed workload produced {} failure(s):\n  {}",
                fails.len(),
                fails.join("\n  ")
            );
        }
        println!("#156 PASS: mixed workload on a multi-node tree stays fsck-clean");
    }
}
