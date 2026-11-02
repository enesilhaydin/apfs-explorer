//! Deleting a snapshot must release the WHOLE pinned extentref tree.
//!
//! `create_snapshot` hands the populated extentref tree to the snapshot and
//! gives the live volume a fresh empty one. `delete_snapshot` then merges the
//! pinned records back into the live tree and releases the pinned tree. Once
//! that tree has grown past a single node, releasing only its root block
//! strands every other node: allocated, with nothing left referring to it.
//! The leak is invisible in a single-node tree, so the test first proves the
//! tree really is multi-node before asserting anything.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]` - shells out to fsck.
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --features write \
//!     --test snapshot_extref_destroy -- --ignored --nocapture --test-threads=1
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
mod extref_destroy {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{create_file, overwrite_existing_file};
    use apfs_write::snapshot::{create_snapshot, delete_snapshot};
    use apfs_write::txn::{Transaction, TxnError};
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-extref-destroy.img";
    const SIZE_MB: u32 = 256;
    const SECTOR: u64 = 512;
    /// `apfs_extentref_tree_oid` in the volume superblock.
    const VSBI_EXTENTREF_TREE_OID: usize = 0x90;
    /// Enough one-extent files to push the extentref tree past a single node.
    const N_FILES: usize = 300;

    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF,0x57,0x34,0x7C,0x00,0x00,0xAA,0x11,0xAA,0x11,0x00,0x30,0x65,0x43,0xEC,0xAC,
    ];

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn root_dir() -> &'static str {
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
            .current_dir(root_dir())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
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
    /// Independent decode: the b-tree node level lives at byte 34 of the block
    /// (`obj_phys` is 32 bytes, then `btn_flags`), so a non-zero level proves
    /// the tree has children below the root.
    fn extref_root_level(img: &str, root_paddr: u64) -> u16 {
        let raw = std::fs::read(img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).unwrap();
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let c = Container::open(&mut dev).unwrap();
        let bsz = c.superblock.block_size as usize;
        let mut node = vec![0u8; bsz];
        dev.read_at(root_paddr * bsz as u64, &mut node).unwrap();
        u16::from_le_bytes(node[34..36].try_into().unwrap())
    }
    fn open_txn(img: &str) -> Transaction<Dev> {
        let raw = std::fs::read(img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }
    fn free_count(img: &str) -> u64 {
        open_txn(img).sm.free_count
    }
    fn commit_op<F, T>(label: &str, f: F) -> T
    where
        F: FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<T, TxnError>,
    {
        let (vsb, om) = read_vsb_omap(IMG);
        let mut txn = open_txn(IMG);
        let out = f(&mut txn, &vsb, &om).unwrap_or_else(|e| panic!("{label}: {e}"));
        txn.commit()
            .unwrap_or_else(|e| panic!("commit {label}: {e}"));
        out
    }

    fn run_fsck(img: &str) {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-nomount",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                img,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut apfs_dev: Option<String> = None;
        let mut parent_dev: Option<String> = None;
        for line in stdout.lines() {
            let dev = line.split_whitespace().next().unwrap_or("");
            if !dev.starts_with("/dev/disk") {
                continue;
            }
            // hdiutil can list disk10 before disk9: the image's own node is the
            // lowest-numbered whole disk (shorter name, then smaller).
            if !dev.trim_start_matches("/dev/disk").contains('s')
                && parent_dev
                    .as_deref()
                    .is_none_or(|p: &str| (p.len(), p) > (dev.len(), dev))
            {
                parent_dev = Some(dev.to_string());
            }
            if apfs_dev.is_none() && line.contains("Apple_APFS") {
                apfs_dev = Some(dev.to_string());
            }
        }
        let parent = parent_dev.expect("no whole-disk node");
        let apfs = apfs_dev.expect("no Apple_APFS partition");
        let rdev = apfs.replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        let _ = Command::new("hdiutil").args(["detach", &parent]).status();
        let fsck = fsck.expect("fsck_apfs spawn");
        let report = String::from_utf8_lossy(&fsck.stdout).into_owned()
            + &String::from_utf8_lossy(&fsck.stderr);
        assert!(
            fsck.status.success(),
            "fsck_apfs reported corruption:\n{report}"
        );
        // fsck exits 0 for repairable damage and only prints it as a warning;
        // leaked blocks land exactly there, so warnings are failures here.
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
            complaints.is_empty(),
            "fsck_apfs flagged inconsistencies:\n{}",
            complaints.join("\n")
        );
    }

    #[test]
    #[ignore = "fsck-gated; slow"]
    fn deleting_a_snapshot_releases_every_pinned_extref_node() {
        if !gate() {
            println!("F052: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

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

        // The tree the snapshot is about to pin must really be multi-node,
        // otherwise a root-only release would be correct and the test vacuous.
        let (vsb, _) = read_vsb_omap(IMG);
        let extref_root = u64::from_le_bytes(
            vsb[VSBI_EXTENTREF_TREE_OID..VSBI_EXTENTREF_TREE_OID + 8]
                .try_into()
                .unwrap(),
        );
        let level = extref_root_level(IMG, extref_root);
        assert!(
            level > 0,
            "extentref tree is still single-node after {N_FILES} files - \
             the leak this test targets cannot occur, so raise N_FILES"
        );

        let before = free_count(IMG);
        let snap_xid = commit_op("create_snapshot", |t, v, o| {
            create_snapshot(t, v, o, "snap")
        });
        let after_create = free_count(IMG);
        commit_op("delete_snapshot", |t, v, o| {
            delete_snapshot(t, v, o, snap_xid)
        });
        let after_delete = free_count(IMG);
        println!(
            "F052: extref root level={level}, free_count {before} -> {after_create} \
             (create) -> {after_delete} (delete)"
        );

        run_fsck(IMG);
    }
}
