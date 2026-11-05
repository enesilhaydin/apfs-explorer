//! #139 regression: multi-snapshot retention must stay fsck-clean.
//!
//! Before #139 `create_snapshot` rebuilt a SINGLE-entry snap_meta tree and
//! om_snapshot tree on every call, silently dropping every earlier snapshot
//! and leaking the previous om_snapshot tree block (fsck "overallocation").
//! It also left the VSB/omap snapshot counters at N while the on-disk trees
//! held one entry, so fsck reported a snapshot count mismatch.
//!
//! This test creates two snapshots and asserts the fsck arbiter stays clean
//! with BOTH retained, then reverts the most-recent one and asserts the
//! earlier snapshot survives AND the volume is still fsck-clean.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` + `#[ignore]` (an fsck attach per checkpoint).
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_multi_139 \
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
mod multi {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::write_file;
    use apfs_write::snapshot::{create_snapshot, delete_snapshot, revert_to_snapshot};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-snap-multi-139.img";
    const SIZE_MB: u32 = 64;
    const SECTOR: u64 = 512;

    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

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
    fn fsck_clean(label: &str) {
        let (ok, out) = verify();
        let v: Vec<&str> = out
            .lines()
            .filter(|l| {
                l.contains("overallocation")
                    || l.contains("corrupt")
                    || l.contains("is not valid")
                    || l.contains("invalid")
                    || l.contains("snapshot")
            })
            .collect();
        println!("[{label}] fsck_clean={ok}  {v:?}");
        assert!(ok, "{label}: fsck reported the volume DIRTY {v:?}");
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
    fn snap_count() -> u32 {
        let (_, om) = read_vsb_omap();
        u32::from_le_bytes(om[36..40].try_into().unwrap())
    }

    #[test]
    #[ignore = "slow fsck; gated by APFS_WRITE_SCRATCH=1"]
    fn two_snapshots_retained_and_fsck_clean() {
        if !check_gate() {
            println!("#139: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();
        fsck_clean("baseline");

        // snap_1
        let snap1 = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let x = create_snapshot(&mut txn, &vsb, &om, "m139-snap-1").expect("snap1");
            txn.commit().expect("commit snap1");
            x
        };
        fsck_clean("after-snap1");
        assert_eq!(snap_count(), 1, "after snap1: count 1");

        // write between
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, "m139_between.txt", b"between").expect("write");
            txn.commit().expect("commit write");
        }
        fsck_clean("after-write");

        // snap_2 - both snapshots must now be retained AND fsck-clean.
        let snap2 = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let x = create_snapshot(&mut txn, &vsb, &om, "m139-snap-2").expect("snap2");
            txn.commit().expect("commit snap2");
            x
        };
        fsck_clean("after-snap2");
        assert_eq!(snap_count(), 2, "after snap2: BOTH snapshots retained");
        assert!(snap2 > snap1, "snap2 xid must exceed snap1");

        // revert most-recent (snap_2) - snap_1 must survive, snap_2 stays, still fsck-clean.
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            revert_to_snapshot(&mut txn, &vsb, &om, snap2).expect("revert snap2");
            txn.commit().expect("commit revert");
        }
        fsck_clean("after-revert-snap2");
        assert_eq!(
            snap_count(),
            2,
            "after revert(snap2): snap1 survives and snap2 stays, as on macOS"
        );

        println!(
            "#139 PASS: two snapshots retained, revert leaves earlier snapshot, all fsck-clean"
        );
    }

    #[test]
    #[ignore = "slow fsck; gated by APFS_WRITE_SCRATCH=1"]
    fn delete_older_of_two_keeps_newer_fsck_clean() {
        if !check_gate() {
            println!("#139-del: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // snap_1, write, snap_2 - both retained (proven fsck-clean above).
        let snap1 = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let x = create_snapshot(&mut txn, &vsb, &om, "del-snap-1").expect("snap1");
            txn.commit().expect("commit snap1");
            x
        };
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, "del_between.txt", b"x").expect("write");
            txn.commit().expect("commit write");
        }
        let _snap2 = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let x = create_snapshot(&mut txn, &vsb, &om, "del-snap-2").expect("snap2");
            txn.commit().expect("commit snap2");
            x
        };
        fsck_clean("del:after-snap2");
        assert_eq!(snap_count(), 2, "two snapshots present");

        // Delete the OLDER snapshot (snap_1) - snap_2 must survive, fsck-clean.
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            delete_snapshot(&mut txn, &vsb, &om, snap1).expect("delete snap1");
            txn.commit().expect("commit delete");
        }
        fsck_clean("del:after-delete-older");
        assert_eq!(snap_count(), 1, "snap2 survives delete of snap1");

        println!("#139-del PASS: deleting the older of two snapshots keeps the newer, fsck-clean");
    }
}
