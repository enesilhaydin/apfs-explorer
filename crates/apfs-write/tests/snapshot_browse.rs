//! Reading a volume as one of its snapshots holds it.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, mount_apfs).
//! `#[ignore]`: attaches images.
//!
//! A snapshot keeps the volume as it was: its own volume superblock (the
//! one frozen when it was taken), whose catalog is read through the live
//! object map at the snapshot's transaction. The view must show the files
//! as they were, not as they are, and macOS mounting the same snapshot
//! must read the same bytes.
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
mod snapshot_browse {
    use apfs::{FsView, SnapshotRef, VolumeSelector};
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::gpt::find_apfs_container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{overwrite_existing_file, unlink, write_file};
    use apfs_write::snapshot::create_snapshot;
    use apfs_write::txn::Transaction;
    use std::collections::BTreeMap;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-snapshot-browse.img";
    const ROOT: u64 = 2;

    fn root_dir() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
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

    /// Every regular file under `/` (no dot files) and its content.
    fn files(fs: &mut FsView<Dev>) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        for e in fs.read_dir("/").unwrap() {
            if e.name.starts_with('.') {
                continue;
            }
            let path = format!("/{}", e.name);
            let size = fs.getattr(&path).unwrap().unwrap().size as usize;
            out.insert(e.name.clone(), fs.read(&path, 0, size).unwrap());
        }
        out
    }

    /// The same, as macOS reads snapshot `name` mounted with `mount_apfs -s`.
    fn macos_files(name: &str) -> BTreeMap<String, Vec<u8>> {
        let out = Command::new("hdiutil")
            .args(["attach", "-nomount", "-imagekey"])
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
        let dev = text
            .lines()
            .find(|l| l.contains("41504653-0000-11AA"))
            .and_then(|l| l.split_whitespace().next())
            .expect("no APFS volume node")
            .to_string();
        let dir = "/tmp/apfs-snapshot-browse-mnt";
        let _ = std::fs::create_dir_all(dir);
        let st = Command::new("mount_apfs")
            .args(["-o", "rdonly", "-s", name, &dev, dir])
            .status()
            .unwrap();
        let mut got = BTreeMap::new();
        if st.success() {
            for e in std::fs::read_dir(dir).unwrap() {
                let e = e.unwrap();
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with('.') || !e.file_type().unwrap().is_file() {
                    continue;
                }
                got.insert(n, std::fs::read(e.path()).unwrap());
            }
            let _ = Command::new("umount").arg(dir).status();
        }
        let _ = Command::new("hdiutil").args(["detach", &whole]).status();
        assert!(st.success(), "mount_apfs -s {name} failed");
        got
    }

    #[test]
    #[ignore = "attaches images"]
    fn a_snapshot_reads_as_the_volume_was() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let o = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                IMG,
                "64",
            ])
            .current_dir(root_dir())
            .output()
            .unwrap();
        assert!(o.status.success(), "gen-scratch failed");

        let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        txn(|t, v, o| write_file(t, v, o, ROOT, "keep.txt", b"first")).unwrap();
        txn(|t, v, o| write_file(t, v, o, ROOT, "gone.txt", b"deleted later")).unwrap();
        txn(|t, v, o| write_file(t, v, o, ROOT, "big.bin", &big)).unwrap();
        let before = txn(|t, v, o| create_snapshot(t, v, o, "before")).unwrap();

        txn(|t, v, o| overwrite_existing_file(t, v, o, ROOT, "keep.txt", b"second")).unwrap();
        txn(|t, v, o| unlink(t, v, o, ROOT, "gone.txt")).unwrap();
        txn(|t, v, o| write_file(t, v, o, ROOT, "new.txt", b"added later")).unwrap();
        txn(|t, v, o| create_snapshot(t, v, o, "after")).unwrap();
        txn(|t, v, o| overwrite_existing_file(t, v, o, ROOT, "new.txt", b"changed again")).unwrap();

        let mut live = FsView::open(open_dev()).unwrap();
        let listed: Vec<String> = live
            .snapshots()
            .unwrap()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(listed, ["before", "after"]);
        let now = files(&mut live);
        assert_eq!(now["keep.txt"], b"second");
        assert_eq!(now["new.txt"], b"changed again");
        assert!(!now.contains_key("gone.txt"));
        drop(live);

        let mut then = FsView::open_snapshot(
            open_dev(),
            VolumeSelector::Default,
            &SnapshotRef::Name("before".into()),
        )
        .unwrap();
        let was = files(&mut then);
        assert_eq!(was["keep.txt"], b"first");
        assert_eq!(was["gone.txt"], b"deleted later");
        assert!(was["big.bin"] == big, "big.bin differs in the snapshot");
        assert!(!was.contains_key("new.txt"));
        assert!(
            then.read_vsb_omap_raw().is_err(),
            "a snapshot view handed out its superblock for writing"
        );
        drop(then);

        let mut by_xid = FsView::open_snapshot(
            open_dev(),
            VolumeSelector::Default,
            &SnapshotRef::Xid(before),
        )
        .unwrap();
        assert_eq!(files(&mut by_xid), was);
        drop(by_xid);

        let mut after = FsView::open_snapshot(
            open_dev(),
            VolumeSelector::Default,
            &SnapshotRef::Name("after".into()),
        )
        .unwrap();
        let mid = files(&mut after);
        assert_eq!(mid["keep.txt"], b"second");
        assert_eq!(mid["new.txt"], b"added later");
        drop(after);

        assert!(FsView::open_snapshot(
            open_dev(),
            VolumeSelector::Default,
            &SnapshotRef::Name("missing".into())
        )
        .is_err());

        assert_eq!(
            macos_files("before"),
            was,
            "macOS reads the snapshot otherwise"
        );
        assert_eq!(
            macos_files("after"),
            mid,
            "macOS reads the snapshot otherwise"
        );
        let _ = std::fs::remove_file(IMG);
    }
}
