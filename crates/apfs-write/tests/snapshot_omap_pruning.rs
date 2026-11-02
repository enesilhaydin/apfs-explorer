//! Object-map versions no view can resolve once a snapshot is deleted.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! The volume's object map holds a mapping per version of each object. A
//! view (the volume, or a snapshot) resolves an object to its newest version
//! not past the view's transaction, so a version is reachable only while
//! some view falls between it and the next one. This takes a snapshot,
//! writes, takes a second snapshot and deletes the first; every version the
//! first snapshot alone could see must then be gone from the map, and the
//! image must still pass a strict fsck.
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
mod snapshot_omap_pruning {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::{Omap, OmapEntry};
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::write_file;
    use apfs_write::snapshot::{create_snapshot, delete_snapshot};
    use apfs_write::txn::Transaction;
    use std::io::Read;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-snapshot-omap-pruning.img";
    const IMAGE_MB: &str = "64";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const ROOT_INO: u64 = 2;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn root_dir() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }
    fn gen_scratch() {
        let _ = std::fs::remove_file(IMG);
        let s = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                IMG,
                IMAGE_MB,
            ])
            .current_dir(root_dir())
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
    }
    fn find_apfs_range() -> (u64, u64) {
        let mut f = std::fs::File::open(IMG).unwrap();
        let mut head = vec![0u8; 64 * 1024];
        f.read_exact(&mut head).unwrap();
        let hdr = &head[512..1024];
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let off = base + i * sz;
            let e = match head.get(off..off + sz) {
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
    fn open_dev() -> Dev {
        let (base, len) = find_apfs_range();
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        WritableOffsetBlockDevice::new(inner, base, len)
    }
    fn read_vsb_omap() -> (Vec<u8>, Vec<u8>) {
        let mut dev = open_dev();
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
    fn in_txn<T>(f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> T) -> T {
        let (vsb, om) = read_vsb_omap();
        let mut txn = Transaction::begin(open_dev()).expect("begin");
        let out = f(&mut txn, &vsb, &om);
        txn.commit().expect("commit");
        out
    }
    fn open_fs() -> apfs::FsView<Dev> {
        apfs::FsView::open(open_dev()).expect("FsView::open")
    }
    fn whole_disk(attached: &str) -> String {
        attached
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node")
    }
    fn detach(disk: &str) {
        for attempt in 0..8u64 {
            let st = Command::new("hdiutil")
                .args(["detach", disk])
                .output()
                .expect("detach");
            if st.status.success() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
        }
        let _ = Command::new("hdiutil")
            .args(["detach", "-force", disk])
            .status();
    }
    fn run_fsck(label: &str) {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                "-nomount",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let attached = String::from_utf8_lossy(&out.stdout).into_owned();
        let disk = whole_disk(&attached);
        let part = attached
            .lines()
            .filter(|l| l.contains("Apple_APFS"))
            .filter_map(|l| l.split_whitespace().next())
            .next()
            .expect("no APFS partition node")
            .to_string();
        let rdev = part.replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        detach(&disk);
        let fsck = fsck.expect("fsck_apfs spawn");
        let report = String::from_utf8_lossy(&fsck.stdout).into_owned()
            + &String::from_utf8_lossy(&fsck.stderr);
        assert!(
            fsck.status.success(),
            "{label}: fsck_apfs reported corruption:\n{report}"
        );
        let complaints: Vec<&str> = report
            .lines()
            .filter(|l| {
                let l = l.to_ascii_lowercase();
                l.contains("warning:")
                    || l.contains("error:")
                    || l.contains("overallocation")
                    || l.contains("underallocation")
                    || l.contains("is not valid")
            })
            .collect();
        assert!(
            complaints.is_empty(),
            "{label}: fsck_apfs flagged inconsistencies:\n{}",
            complaints.join("\n")
        );
    }

    /// Mappings no view among `views` (snapshot xids; the volume itself sees
    /// everything) resolves: each object's versions in order, a version is
    /// resolved by a view at or past it and before the next version, and the
    /// newest version by the volume.
    fn unresolved(entries: &[OmapEntry], views: &[u64]) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < entries.len() {
            let oid = entries[i].oid;
            let mut j = i;
            while j < entries.len() && entries[j].oid == oid {
                j += 1;
            }
            let versions = &entries[i..j];
            for (k, e) in versions.iter().enumerate() {
                let next = versions.get(k + 1).map(|n| n.xid);
                let seen = match next {
                    None => true,
                    Some(n) => views.iter().any(|&v| v >= e.xid && v < n),
                };
                if !seen {
                    out.push((e.oid, e.xid));
                }
            }
            i = j;
        }
        out
    }

    #[test]
    #[ignore = "scratch-image gated; needs xtask gen-scratch and fsck_apfs on macOS"]
    fn deleting_a_snapshot_drops_the_versions_only_it_could_see() {
        if !gate() {
            return;
        }
        gen_scratch();
        in_txn(|t, v, o| {
            write_file(t, v, o, ROOT_INO, "before.txt", b"before").expect("write");
        });
        let first = in_txn(|t, v, o| create_snapshot(t, v, o, "first").expect("first snapshot"));
        // Two writes between the snapshots: the catalog root and its
        // neighbours get a version the first snapshot pins and a newer one.
        in_txn(|t, v, o| {
            write_file(t, v, o, ROOT_INO, "between-1.txt", b"one").expect("write");
        });
        in_txn(|t, v, o| {
            write_file(t, v, o, ROOT_INO, "between-2.txt", b"two").expect("write");
        });
        let second = in_txn(|t, v, o| create_snapshot(t, v, o, "second").expect("second snapshot"));
        in_txn(|t, v, o| {
            write_file(t, v, o, ROOT_INO, "after.txt", b"after").expect("write");
        });
        let before_delete = open_fs().volume_omap_entries().expect("omap entries");
        assert!(
            unresolved(&before_delete, &[first, second]).is_empty(),
            "with both snapshots every version is somebody's: {before_delete:?}"
        );
        let pinned_by_first: Vec<(u64, u64)> = before_delete
            .iter()
            .filter(|e| e.xid <= first)
            .map(|e| (e.oid, e.xid))
            .collect();
        println!(
            "snapshot-omap-pruning: first {first}, second {second}; {} mappings, {} at or before \
             the first snapshot: {pinned_by_first:?}",
            before_delete.len(),
            pinned_by_first.len()
        );

        in_txn(|t, v, o| delete_snapshot(t, v, o, first).expect("delete first"));
        let after_delete = open_fs().volume_omap_entries().expect("omap entries");
        let leftover = unresolved(&after_delete, &[second]);
        println!(
            "snapshot-omap-pruning: {} mappings after the delete; unresolved: {leftover:?}",
            after_delete.len()
        );
        assert!(
            leftover.is_empty(),
            "versions only the deleted snapshot could see must leave the map: {leftover:?}"
        );
        assert!(
            after_delete.len() < before_delete.len(),
            "the map must have shed the first snapshot's versions"
        );
        // What the surviving views see is untouched.
        let mut fs = open_fs();
        for name in ["before.txt", "between-1.txt", "between-2.txt", "after.txt"] {
            assert!(
                fs.lookup(&format!("/{name}")).expect("lookup").is_some(),
                "{name} stays"
            );
        }
        drop(fs);
        run_fsck("after deleting the first of two snapshots");
        // `APFS_KEEP_IMAGE=1` leaves the image for a look with other tools.
        if std::env::var("APFS_KEEP_IMAGE").as_deref() != Ok("1") {
            let _ = std::fs::remove_file(IMG);
        }
    }
}
