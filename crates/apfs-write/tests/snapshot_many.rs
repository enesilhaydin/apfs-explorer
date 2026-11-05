//! A volume with more snapshots than one node of the snapshot metadata tree
//! holds.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, diskutil,
//! fsck_apfs). `#[ignore]`: runs fsck.
//!
//! Each snapshot takes two records in the snapshot metadata tree (its
//! metadata and its name), about 150 bytes with a table entry each, so a
//! 4 KiB node holds some 25 snapshots. The tree was read and written as a
//! single node: past that, taking one more failed, and on a volume where
//! macOS had grown the tree (a Time Machine disk) listing, deleting and
//! reverting failed. macOS reads and grows it as any other B-tree, and so
//! does the object map's snapshot tree, which fills a node at 142.
//!
//! Checked: both trees go past one node, every step is fsck-clean, macOS
//! lists the same snapshots, and after macOS deletes one from the tree we
//! wrote, we read what it left.
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
mod snapshot_many {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::gpt::find_apfs_container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::write_file;
    use apfs_write::snapshot::{
        create_snapshot, delete_snapshot, list_snapshots, revert_to_snapshot,
    };
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-snapshot-many.img";
    const ROOT: u64 = 2;
    /// More than one node of either tree holds: the snapshot metadata tree
    /// fills a node at about 25 snapshots, the object map's snapshot tree
    /// (fixed 24-byte records) at 142.
    const SNAPSHOTS: usize = 150;
    /// `apfs_snap_meta_tree_oid` in the volume superblock.
    const VSB_SNAP_META_TREE: usize = 0x98;
    /// `om_snapshot_tree_oid` in the object map header.
    const OMAP_SNAPSHOT_TREE: usize = 56;

    /// The level of the root of the snapshot metadata tree and of the object
    /// map's snapshot tree.
    fn tree_levels() -> (u16, u16) {
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut level = |paddr: u64| {
            let mut node = vec![0u8; vsb.len()];
            dev.read_at(paddr * vsb.len() as u64, &mut node).unwrap();
            u16::from_le_bytes(node[34..36].try_into().unwrap())
        };
        let at = |b: &[u8], off: usize| u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
        (
            level(at(&vsb, VSB_SNAP_META_TREE)),
            level(at(&om, OMAP_SNAPSHOT_TREE)),
        )
    }

    fn root_dir() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }

    fn xtask(args: &[&str]) -> (bool, String) {
        let o = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--"])
            .args(args)
            .current_dir(root_dir())
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

    /// Run `op` in a transaction of its own and commit it.
    fn txn<R>(op: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> R) -> R {
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        let r = op(&mut t, &vsb, &om);
        t.commit().unwrap();
        r
    }

    fn names() -> Vec<String> {
        let mut dev = open_dev();
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        list_snapshots(&mut t, &vsb, &om)
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    fn name(i: usize) -> String {
        format!("snapshot-{i:03}-taken-by-the-test-suite")
    }

    fn fsck_clean(when: &str) {
        let (ok, out) = xtask(&["verify-scratch", IMG]);
        assert!(ok, "not fsck-clean {when}:\n{out}");
    }

    /// The snapshot names macOS lists for the volume, in its order. diskutil
    /// lists only a mounted volume's snapshots; the image is attached
    /// read-only, so macOS mounts it without writing to it.
    fn macos_names() -> Vec<String> {
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
        let volume = text
            .lines()
            .find(|l| l.contains("41504653-0000-11AA"))
            .and_then(|l| l.split_whitespace().next())
            .map(str::to_string);
        let listed = volume.as_ref().map(|v| {
            Command::new("diskutil")
                .args(["apfs", "listSnapshots", v])
                .output()
                .unwrap()
        });
        let _ = Command::new("hdiutil").args(["detach", &whole]).status();
        let listed = listed.expect("no APFS volume node");
        let text = String::from_utf8_lossy(&listed.stdout).into_owned();
        assert!(listed.status.success(), "diskutil listSnapshots: {text}");
        // "|   Name:        <name>" under each snapshot (no bar under the last).
        text.lines()
            .filter_map(|l| l.split_once("Name:"))
            .map(|(_, n)| n.trim().to_string())
            .collect()
    }

    #[test]
    #[ignore = "fsck-gated"]
    fn many_snapshots_are_taken_listed_deleted_and_reverted() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let (ok, out) = xtask(&["gen-scratch", IMG, "64"]);
        assert!(ok, "gen-scratch: {out}");

        let mut want: Vec<String> = Vec::new();
        for i in 0..SNAPSHOTS {
            if i % 10 == 0 {
                let body = vec![i as u8; 5000];
                txn(|t, v, o| write_file(t, v, o, ROOT, &format!("f{i}.bin"), &body)).unwrap();
            }
            let n = name(i);
            txn(|t, v, o| create_snapshot(t, v, o, &n))
                .unwrap_or_else(|e| panic!("taking snapshot {i}: {e:?}"));
            want.push(n);
        }
        assert_eq!(names(), want);
        let (meta_level, omap_level) = tree_levels();
        assert!(
            meta_level >= 1,
            "the snapshot metadata tree stayed one node"
        );
        assert!(
            omap_level >= 1,
            "the object map's snapshot tree stayed one node"
        );
        fsck_clean("after taking the snapshots");
        let mut by_macos = macos_names();
        by_macos.sort();
        assert_eq!(by_macos, want, "macOS lists other snapshots");

        // The oldest, one in the middle and the newest but one.
        let xids: Vec<u64> = {
            let mut dev = open_dev();
            let (vsb, om) = vsb_omap(&mut dev);
            let mut t = Transaction::begin(dev).unwrap();
            list_snapshots(&mut t, &vsb, &om)
                .unwrap()
                .into_iter()
                .map(|s| s.xid)
                .collect()
        };
        for at in [0, SNAPSHOTS / 2, SNAPSHOTS - 2] {
            let xid = xids[at];
            txn(|t, v, o| delete_snapshot(t, v, o, xid))
                .unwrap_or_else(|e| panic!("deleting snapshot {at}: {e:?}"));
            let gone = name(at);
            want.retain(|n| *n != gone);
        }
        assert_eq!(names(), want);
        fsck_clean("after deleting three snapshots");

        let latest = *xids.last().unwrap();
        txn(|t, v, o| revert_to_snapshot(t, v, o, latest))
            .unwrap_or_else(|e| panic!("reverting to the latest snapshot: {e:?}"));
        // The snapshot reverted to stays, as on macOS.
        assert_eq!(names(), want);
        fsck_clean("after reverting to the latest snapshot");
        let mut by_macos = macos_names();
        by_macos.sort();
        assert_eq!(
            by_macos, want,
            "macOS lists other snapshots after the revert"
        );

        // macOS deletes one from the tree we wrote, and we read what it left.
        let gone = name(100);
        macos_delete(&gone);
        want.retain(|n| *n != gone);
        assert_eq!(names(), want, "read back after macOS deleted a snapshot");
        fsck_clean("after macOS deleted a snapshot");
        let _ = std::fs::remove_file(IMG);
    }

    /// Let macOS delete snapshot `name`: the volume mounted read-write.
    fn macos_delete(name: &str) {
        let out = Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-imagekey"])
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
        let volume = text
            .lines()
            .find(|l| l.contains("41504653-0000-11AA"))
            .and_then(|l| l.split_whitespace().next())
            .map(str::to_string);
        let deleted = volume.as_ref().map(|v| {
            Command::new("diskutil")
                .args(["apfs", "deleteSnapshot", v, "-name", name])
                .output()
                .unwrap()
        });
        let _ = Command::new("hdiutil").args(["detach", &whole]).status();
        let deleted = deleted.expect("no APFS volume node");
        assert!(
            deleted.status.success(),
            "diskutil deleteSnapshot: {}{}",
            String::from_utf8_lossy(&deleted.stdout),
            String::from_utf8_lossy(&deleted.stderr)
        );
    }
}
