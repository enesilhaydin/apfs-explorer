//! Writing to a volume that has a snapshot changes the catalog along the
//! paths to the changed records, as without one.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, fsck_apfs,
//! mount_apfs). `#[ignore]`: runs fsck.
//!
//! Every operation under a snapshot rewrote the whole catalog and object
//! map: deleting 20 files under a snapshot on a drive of 120 000 took 11 s,
//! so the Windows mount refused to write to such a volume. The snapshot
//! shares the catalog nodes it froze; a write copies only the nodes on its
//! path and leaves the snapshot's versions mapped for it.
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
mod snapshot_write_paths {
    use apfs::FsView;
    use apfs_core::block_device::{
        BlockDevice, BlockError, FileBlockDeviceRw, WritableBlockDevice, WritableOffsetBlockDevice,
    };
    use apfs_core::container::Container;
    use apfs_core::gpt::find_apfs_container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        create_file, mkdir, rename, unlink, write_file, write_range, RangeWrite,
    };
    use apfs_write::snapshot::{create_snapshot, delete_snapshot, list_snapshots};
    use apfs_write::txn::Transaction;
    use std::collections::BTreeMap;
    use std::process::Command;

    const SRC: &str = "/tmp/apfs-snapshot-write-paths-src";
    const IMG: &str = "/tmp/apfs-snapshot-write-paths";
    const ROOT: u64 = 2;
    /// Enough files for a catalog of many nodes.
    const FILES: usize = 3000;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;

    /// Counts the blocks written to the image.
    struct Counted {
        inner: Dev,
        written: u64,
    }

    impl BlockDevice for Counted {
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            self.inner.read_at(offset, buf)
        }
    }

    impl WritableBlockDevice for Counted {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
            self.written += buf.len() as u64 / 4096;
            self.inner.write_at(offset, buf)
        }
        fn flush_data(&mut self) -> Result<(), BlockError> {
            self.inner.flush_data()
        }
    }

    fn image() -> String {
        format!("{IMG}.dmg")
    }

    fn open_dev() -> Dev {
        let mut disk = FileBlockDeviceRw::open_rw(image()).unwrap();
        let r = find_apfs_container(&mut disk, 512).unwrap();
        WritableOffsetBlockDevice::new(disk, r.offset, r.length)
    }

    fn vsb_omap<D: BlockDevice>(dev: &mut D) -> (Vec<u8>, Vec<u8>) {
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

    /// Run `op` in a transaction of its own; return its result and how many
    /// blocks the commit wrote.
    fn txn<R>(op: impl FnOnce(&mut Transaction<Counted>, &[u8], &[u8]) -> R) -> (R, u64) {
        let mut dev = Counted {
            inner: open_dev(),
            written: 0,
        };
        let (vsb, om) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        let r = op(&mut t, &vsb, &om);
        let dev = t.commit().unwrap();
        (r, dev.written)
    }

    fn fsck_clean(when: &str) {
        let o = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--", "verify-scratch"])
            .arg(image())
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "not fsck-clean {when}:\n{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
    }

    /// The files under `/sub` (name and content) as macOS reads them, at
    /// snapshot `snap` or now.
    fn macos_files(snap: Option<&str>) -> BTreeMap<String, Vec<u8>> {
        let out = Command::new("hdiutil")
            .args(["attach", "-nomount", "-imagekey"])
            .args(["diskimage-class=CRawDiskImage"])
            .arg(image())
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
        let dev = text
            .lines()
            .find(|l| l.contains("41504653-0000-11AA"))
            .and_then(|l| l.split_whitespace().next())
            .expect("no APFS volume node")
            .to_string();
        let dir = "/tmp/apfs-snapshot-write-paths-mnt";
        let _ = std::fs::create_dir_all(dir);
        let mut args = vec!["-o", "rdonly"];
        if let Some(s) = snap {
            args.extend(["-s", s]);
        }
        let st = Command::new("mount_apfs")
            .args(&args)
            .args([dev.as_str(), dir])
            .status()
            .unwrap();
        let mut got = BTreeMap::new();
        if st.success() {
            if let Ok(rd) = std::fs::read_dir(format!("{dir}/sub")) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        got.insert(n, std::fs::read(e.path()).unwrap());
                    }
                }
            }
            let _ = Command::new("umount").arg(dir).status();
        }
        let _ = Command::new("hdiutil").args(["detach", &whole]).status();
        assert!(st.success(), "mount_apfs {snap:?} failed");
        got
    }

    /// macOS mounts the volume read-write, adds a file to `/sub` and deletes
    /// one from it.
    fn macos_writes() {
        let out = Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-imagekey"])
            .args(["diskimage-class=CRawDiskImage"])
            .arg(image())
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
        let done = mount.as_ref().map(|m| {
            std::fs::write(format!("{m}/sub/from-macos.txt"), b"written by macOS")
                .and_then(|()| std::fs::remove_file(format!("{m}/sub/file00500.txt")))
        });
        let _ = Command::new("sync").status();
        let _ = Command::new("hdiutil").args(["detach", &whole]).status();
        done.expect("macOS did not mount the volume")
            .expect("macOS could not write");
    }

    /// The files under `/sub` as the library reads the volume now.
    fn our_files() -> BTreeMap<String, Vec<u8>> {
        let mut fs = FsView::open(open_dev()).unwrap();
        let mut got = BTreeMap::new();
        for e in fs.read_dir("/sub").unwrap() {
            let p = format!("/sub/{}", e.name);
            let a = fs.getattr(&p).unwrap().unwrap();
            if !a.is_dir {
                got.insert(e.name.clone(), fs.read(&p, 0, a.size as usize).unwrap());
            }
        }
        got
    }

    fn sub_ino() -> u64 {
        FsView::open(open_dev())
            .unwrap()
            .lookup("/sub")
            .unwrap()
            .unwrap()
    }

    #[test]
    #[ignore = "fsck-gated"]
    fn writes_under_a_snapshot_change_only_their_paths() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let _ = std::fs::remove_dir_all(SRC);
        std::fs::create_dir_all(format!("{SRC}/sub")).unwrap();
        for i in 0..FILES {
            std::fs::write(format!("{SRC}/sub/file{i:05}.txt"), format!("content {i}")).unwrap();
        }
        let _ = std::fs::remove_file(image());
        let st = Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "128m", "-srcfolder", SRC])
            .args(["-fs", "APFS", "-layout", "GPTSPUD", "-format", "UDRW"])
            .args(["-volname", "SNAPWRITE", IMG])
            .status()
            .unwrap();
        assert!(st.success(), "hdiutil create failed");
        let _ = std::fs::remove_dir_all(SRC);
        let sub = sub_ino();

        // A small write without a snapshot, for scale.
        let (_, plain) = txn(|t, v, o| create_file(t, v, o, sub, "probe-before.txt"));
        let before = our_files();

        txn(|t, v, o| create_snapshot(t, v, o, "frozen")).0.unwrap();

        // The same kinds of operation under the snapshot.
        let (r, created) = txn(|t, v, o| create_file(t, v, o, sub, "probe-after.txt"));
        r.unwrap();
        assert!(
            created <= plain * 3 + 16,
            "creating a file under a snapshot wrote {created} blocks, without one {plain}"
        );
        let (r, written) = txn(|t, v, o| write_range(t, v, o, sub, "file01500.txt", 3, b"EDITED"));
        assert_eq!(r.unwrap(), RangeWrite::Written);
        let (r, renamed) =
            txn(|t, v, o| rename(t, v, o, sub, "file00010.txt", "renamed.txt", false));
        r.unwrap();
        let (r, deleted) = txn(|t, v, o| unlink(t, v, o, sub, "file02999.txt"));
        r.unwrap();
        txn(|t, v, o| mkdir(t, v, o, ROOT, "newdir")).0.unwrap();
        txn(|t, v, o| write_file(t, v, o, sub, "new.bin", &[9u8; 50_000]))
            .0
            .unwrap();
        for (what, n) in [
            ("a range write", written),
            ("a rename", renamed),
            ("an unlink", deleted),
        ] {
            assert!(
                n <= plain * 3 + 16,
                "{what} under a snapshot wrote {n} blocks"
            );
        }
        println!(
            "blocks written: create {plain} without a snapshot; under it create {created}, \
             range write {written}, rename {renamed}, unlink {deleted}"
        );
        fsck_clean("after writing under the snapshot");

        // Many operations in one transaction, as the Windows mount groups
        // them: the same nodes are rewritten again within it.
        txn(|t, v, o| {
            for i in 0..40 {
                create_file(t, v, o, sub, &format!("grouped{i:02}.txt"))?;
            }
            write_range(t, v, o, sub, "file02000.txt", 0, b"GROUPED")?;
            for i in 0..20 {
                unlink(t, v, o, sub, &format!("grouped{i:02}.txt"))?;
            }
            unlink(t, v, o, sub, "file02001.txt")
        })
        .0
        .unwrap();
        fsck_clean("after a grouped transaction under the snapshot");

        // The snapshot still holds the files as they were; the volume now
        // holds the changes. macOS reads both the same way.
        let mut now = before.clone();
        now.insert("probe-after.txt".into(), Vec::new());
        // "content 1500" with bytes 3..9 written over.
        let mut edited = now["file01500.txt"].clone();
        edited[3..9].copy_from_slice(b"EDITED");
        now.insert("file01500.txt".into(), edited);
        let moved = now.remove("file00010.txt").unwrap();
        now.insert("renamed.txt".into(), moved);
        now.remove("file02999.txt");
        now.insert("new.bin".into(), vec![9u8; 50_000]);
        for i in 20..40 {
            now.insert(format!("grouped{i:02}.txt"), Vec::new());
        }
        now.get_mut("file02000.txt").unwrap()[0..7].copy_from_slice(b"GROUPED");
        now.remove("file02001.txt");
        assert!(our_files() == now, "the volume does not read as written");
        assert!(macos_files(None) == now, "macOS reads the volume otherwise");
        assert!(
            macos_files(Some("frozen")) == before,
            "macOS reads the snapshot changed"
        );

        // macOS writes to the volume after us, the snapshot still there.
        macos_writes();
        fsck_clean("after macOS wrote under the snapshot");
        now.insert("from-macos.txt".into(), b"written by macOS".to_vec());
        now.remove("file00500.txt");
        assert!(
            our_files() == now,
            "the volume reads otherwise after macOS wrote"
        );
        assert!(
            macos_files(Some("frozen")) == before,
            "the snapshot changed after macOS wrote"
        );

        // Deleting the snapshot afterwards leaves a clean volume.
        let xid = {
            let mut dev = open_dev();
            let (vsb, om) = vsb_omap(&mut dev);
            let mut t = Transaction::begin(dev).unwrap();
            list_snapshots(&mut t, &vsb, &om).unwrap()[0].xid
        };
        txn(|t, v, o| delete_snapshot(t, v, o, xid)).0.unwrap();
        fsck_clean("after deleting the snapshot");
        assert!(
            our_files() == now,
            "deleting the snapshot changed the volume"
        );
        let _ = std::fs::remove_file(image());
    }
}
