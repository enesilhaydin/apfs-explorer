//! Reverting to a snapshot older than the latest.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, fsck_apfs,
//! mount_apfs). `#[ignore]`: runs fsck.
//!
//! macOS reverts to any snapshot: the snapshots taken after it are deleted
//! (apfs.kext `revert_to_snapshot` / `omap_revert_to_snapshot` record the
//! later transactions as a pending revert and delete the object map's
//! newer snapshots; the reaper deletes their records), and the snapshot
//! reverted to stays. Only the latest could be reverted to here, and the
//! revert deleted it.
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
mod snapshot_revert_older {
    use apfs::FsView;
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::gpt::find_apfs_container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{overwrite_existing_file, write_file};
    use apfs_write::snapshot::{create_snapshot, list_snapshots, revert_to_snapshot};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-snapshot-revert-older.img";
    const ROOT: u64 = 2;

    fn xtask(args: &[&str]) -> (bool, String) {
        let o = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--"])
            .args(args)
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .output()
            .expect("xtask");
        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status.success(), s)
    }

    fn open_dev() -> Dev {
        let mut disk = FileBlockDeviceRw::open_rw(IMG).unwrap();
        let r = find_apfs_container(&mut disk, 512).unwrap();
        WritableOffsetBlockDevice::new(disk, r.offset, r.length)
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

    fn txn<R>(op: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> R) -> R {
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        let r = op(&mut t, &vsb, &om);
        t.commit().unwrap();
        r
    }

    fn snapshot_names() -> Vec<String> {
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        list_snapshots(&mut t, &vsb, &om)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    fn snapshot_xid(name: &str) -> u64 {
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        list_snapshots(&mut t, &vsb, &om)
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
            .map(|s| s.xid)
            .unwrap_or_else(|| panic!("no snapshot {name}"))
    }

    fn files() -> Vec<(String, Vec<u8>)> {
        let mut fs = FsView::open(open_dev()).unwrap();
        let mut names: Vec<String> = fs
            .read_dir("/")
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .filter(|n| !n.starts_with('.'))
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|n| {
                let p = format!("/{n}");
                let size = fs.getattr(&p).unwrap().unwrap().size as usize;
                let body = fs.read(&p, 0, size).unwrap();
                (n, body)
            })
            .collect()
    }

    /// macOS mounts the volume read-only and reads `a.txt`.
    fn macos_reads_a() -> Vec<u8> {
        let out = Command::new("hdiutil")
            .args(["attach", "-readonly", "-nobrowse", "-imagekey"])
            .args(["diskimage-class=CRawDiskImage", IMG])
            .output()
            .unwrap();
        assert!(out.status.success(), "hdiutil attach failed");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let whole = text
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        let mount = text
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()));
        let got = mount.as_ref().map(|m| std::fs::read(format!("{m}/a.txt")));
        let _ = Command::new("hdiutil").args(["detach", &whole]).status();
        got.expect("macOS did not mount the volume")
            .expect("macOS cannot read a.txt")
    }

    #[test]
    #[ignore = "fsck-gated"]
    fn reverting_to_an_older_snapshot_drops_the_later_ones_and_keeps_it() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let (ok, out) = xtask(&["gen-scratch", IMG, "64"]);
        assert!(ok, "gen-scratch: {out}");

        txn(|t, v, o| write_file(t, v, o, ROOT, "a.txt", b"zero")).unwrap();
        txn(|t, v, o| create_snapshot(t, v, o, "s0")).unwrap();
        txn(|t, v, o| overwrite_existing_file(t, v, o, ROOT, "a.txt", b"one")).unwrap();
        let s1 = txn(|t, v, o| create_snapshot(t, v, o, "s1")).unwrap();
        txn(|t, v, o| overwrite_existing_file(t, v, o, ROOT, "a.txt", b"two")).unwrap();
        txn(|t, v, o| write_file(t, v, o, ROOT, "b.txt", &[7u8; 20_000])).unwrap();
        txn(|t, v, o| create_snapshot(t, v, o, "s2")).unwrap();
        txn(|t, v, o| overwrite_existing_file(t, v, o, ROOT, "a.txt", b"three")).unwrap();
        txn(|t, v, o| write_file(t, v, o, ROOT, "c.txt", b"after s2")).unwrap();
        txn(|t, v, o| create_snapshot(t, v, o, "s3")).unwrap();
        txn(|t, v, o| write_file(t, v, o, ROOT, "d.txt", b"after s3")).unwrap();
        assert_eq!(snapshot_names(), ["s0", "s1", "s2", "s3"]);

        txn(|t, v, o| revert_to_snapshot(t, v, o, s1))
            .unwrap_or_else(|e| panic!("reverting to s1: {e:?}"));

        assert_eq!(files(), [("a.txt".to_string(), b"one".to_vec())]);
        // The later ones are gone; the one reverted to stays, as on macOS.
        assert_eq!(snapshot_names(), ["s0", "s1"]);
        let (ok, out) = xtask(&["verify-scratch", IMG]);
        assert!(ok, "not fsck-clean after the revert:\n{out}");
        assert_eq!(macos_reads_a(), b"one");

        // Work on, and go back to s1 again: it still holds what it held.
        txn(|t, v, o| overwrite_existing_file(t, v, o, ROOT, "a.txt", b"four")).unwrap();
        txn(|t, v, o| write_file(t, v, o, ROOT, "e.txt", b"after the revert")).unwrap();
        let s1_again = snapshot_xid("s1");
        txn(|t, v, o| revert_to_snapshot(t, v, o, s1_again))
            .unwrap_or_else(|e| panic!("reverting to s1 again: {e:?}"));
        assert_eq!(files(), [("a.txt".to_string(), b"one".to_vec())]);
        assert_eq!(snapshot_names(), ["s0", "s1"]);
        let (ok, out) = xtask(&["verify-scratch", IMG]);
        assert!(ok, "not fsck-clean after reverting again:\n{out}");
        assert_eq!(macos_reads_a(), b"one");
        let _ = std::fs::remove_file(IMG);
    }
}
