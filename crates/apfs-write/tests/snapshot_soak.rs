//! Many mixed operations under a snapshot, the way a mount runs them: in
//! groups of operations per transaction, on files the snapshot holds and on
//! files made after it. Each kind of operation runs as its own phase, and
//! the volume is checked with `fsck_apfs` after each, so a phase that leaves
//! the volume's block count wrong is named. Then reverts: to the only
//! snapshot, to the newest and to the older of two, and the delete and
//! revert a revert to an older snapshot is made of, each on its own.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. macOS only (hdiutil, fsck_apfs).
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_soak \
//!     -- --ignored --nocapture
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
mod snapshot_soak {
    use apfs::FsView;
    use apfs_core::block_device::{
        BlockDevice, FileBlockDeviceRw, WritableBlockDevice, WritableOffsetBlockDevice,
    };
    use apfs_core::container::Container;
    use apfs_core::gpt::find_apfs_container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{
        create_file, overwrite_existing_file, rename, unlink, unshare_stream, write_file,
        write_range, RangeWrite,
    };
    use apfs_write::snapshot::create_snapshot;
    use apfs_write::txn::{Transaction, TxnError};
    use std::process::Command;

    const SRC: &str = "/tmp/apfs-snapshot-soak-src";
    const IMG: &str = "/tmp/apfs-snapshot-soak";
    const BASE_FILES: usize = 40;
    const MANY_BASE: usize = 5_000;
    const MANY_NEW: usize = 20_000;
    const GROUP: usize = 20;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;

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

    /// The volume superblock and object map header a transaction has
    /// staged for the volume whose superblock is `vsb`.
    fn staged_vsb_omap<D: WritableBlockDevice>(
        t: &mut Transaction<D>,
        vsb: &[u8],
    ) -> (Vec<u8>, Vec<u8>) {
        let oid = u64::from_le_bytes(vsb[8..16].try_into().unwrap());
        let p = t.staged_volume_block(oid).unwrap();
        let bsz = vsb.len();
        let mut v = vec![0u8; bsz];
        t.read_at(p * bsz as u64, &mut v).unwrap();
        let om_oid = VolumeSuperblock::parse(&v).unwrap().omap_oid;
        let mut om = vec![0u8; bsz];
        t.read_at(om_oid * bsz as u64, &mut om).unwrap();
        (v, om)
    }

    /// Run `n` operations in groups of `GROUP` per transaction, as the mount
    /// does.
    fn grouped<F>(n: usize, mut op: F)
    where
        F: FnMut(usize, &mut Transaction<Dev>, &[u8], &[u8]) -> Result<(), TxnError>,
    {
        let mut i = 0;
        while i < n {
            let mut dev = open_dev();
            let first = vsb_omap(&mut dev);
            let mut t = Transaction::begin(dev).unwrap();
            let mut cur = first;
            for _ in 0..GROUP {
                if i >= n {
                    break;
                }
                let (v, o) = cur.clone();
                t.atomically(|t| op(i, t, &v, &o)).unwrap();
                cur = staged_vsb_omap(&mut t, &v);
                i += 1;
            }
            t.commit().unwrap();
        }
    }

    fn fsck(phase: &str) -> Option<String> {
        let o = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--", "verify-scratch"])
            .arg(image())
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .output()
            .unwrap();
        let out = format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        let warnings: Vec<&str> = out.lines().filter(|l| l.contains("warning")).collect();
        println!(
            "{phase}: {}",
            if o.status.success() { "clean" } else { "DIRTY" }
        );
        for w in &warnings {
            println!("    {w}");
        }
        (!o.status.success()).then(|| format!("{phase}: {}", warnings.join(" | ")))
    }

    fn dir_ino(path: &str) -> u64 {
        let mut fs = FsView::open(open_dev()).unwrap();
        fs.getattr(path).unwrap().unwrap().inode
    }

    fn body(seed: usize, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| ((i * 31 + seed * 7) % 251) as u8)
            .collect()
    }

    #[test]
    #[ignore = "macOS scratch image + fsck"]
    fn mixed_operations_under_a_snapshot_keep_the_block_count() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let _ = std::fs::remove_dir_all(SRC);
        std::fs::create_dir_all(format!("{SRC}/d")).unwrap();
        for i in 0..BASE_FILES {
            let len = [900, 5_000, 70_000, 600_000, 2_500_000][i % 5];
            std::fs::write(format!("{SRC}/d/base{i:03}.bin"), body(i, len)).unwrap();
        }
        std::fs::create_dir_all(format!("{SRC}/many")).unwrap();
        for i in 0..MANY_BASE {
            std::fs::write(format!("{SRC}/many/held{i:05}.txt"), format!("held {i}")).unwrap();
        }
        let _ = std::fs::remove_file(image());
        let st = Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "256m", "-srcfolder", SRC])
            .args(["-fs", "APFS", "-layout", "GPTSPUD", "-format", "UDRW"])
            .args(["-volname", "SOAK", IMG])
            .status()
            .unwrap();
        assert!(st.success(), "hdiutil create failed");
        let _ = std::fs::remove_dir_all(SRC);
        let d = dir_ino("/d");
        let mut dirty = Vec::new();
        dirty.extend(fsck("before the snapshot"));

        let mut dev = open_dev();
        let (v, o) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        create_snapshot(&mut t, &v, &o, "soak").unwrap();
        t.commit().unwrap();
        dirty.extend(fsck("snapshot taken"));

        // Files made after the snapshot: created with content, half deleted.
        grouped(60, |i, t, v, o| {
            write_file(
                t,
                v,
                o,
                d,
                &format!("new{i:03}.bin"),
                &body(i + 100, 3_000 + i * 997),
            )
            .map(|_| ())
        });
        grouped(30, |i, t, v, o| {
            unlink(t, v, o, d, &format!("new{:03}.bin", i * 2))
        });
        dirty.extend(fsck("new files written and half deleted"));

        // Range writes into the new files, inside and past their ends.
        grouped(60, |i, t, v, o| {
            let name = format!("new{:03}.bin", (i % 30) * 2 + 1);
            let at = ((i * 7919) % 9_000) as u64;
            match write_range(t, v, o, d, &name, at, &body(i, 1 + (i * 613) % 20_000))? {
                RangeWrite::Written => Ok(()),
                RangeWrite::Declined => {
                    overwrite_existing_file(t, v, o, d, &name, &body(i, 12_345))
                }
            }
        });
        dirty.extend(fsck("range writes into new files"));

        // Range writes into files the snapshot holds.
        grouped(40, |i, t, v, o| {
            let name = format!("base{:03}.bin", i % BASE_FILES);
            let at = ((i * 104_729) % 50_000) as u64;
            let data = body(i + 7, 1 + (i * 389) % 9_000);
            if write_range(t, v, o, d, &name, at, &data)? == RangeWrite::Written {
                return Ok(());
            }
            if unshare_stream(t, v, o, d, &name)? {
                write_range(t, v, o, d, &name, at, &data)?;
            }
            Ok(())
        });
        dirty.extend(fsck("range writes into snapshot files"));

        // Whole rewrites of files the snapshot holds.
        grouped(15, |i, t, v, o| {
            overwrite_existing_file(
                t,
                v,
                o,
                d,
                &format!("base{:03}.bin", i + 20),
                &body(i, 4_321),
            )
        });
        dirty.extend(fsck("snapshot files rewritten"));

        // Renames of both kinds.
        grouped(20, |i, t, v, o| {
            let (from, to) = if i % 2 == 0 {
                (format!("base{i:03}.bin"), format!("moved-base{i:03}.bin"))
            } else {
                (format!("new{i:03}.bin"), format!("moved-new{i:03}.bin"))
            };
            rename(t, v, o, d, &from, &to, false)
        });
        dirty.extend(fsck("renames"));

        // Deletes of files the snapshot holds, some edited first.
        grouped(10, |i, t, v, o| {
            unlink(t, v, o, d, &format!("base{:03}.bin", 21 + i * 2))
        });
        grouped(5, |i, t, v, o| {
            unlink(t, v, o, d, &format!("moved-base{:03}.bin", i * 2))
        });
        dirty.extend(fsck("snapshot files deleted"));

        // Short-lived files: made and deleted in the same group.
        grouped(40, |i, t, v, o| {
            let name = format!("tmp{:03}.bin", i / 2);
            if i % 2 == 0 {
                create_file(t, v, o, d, &name)?;
                write_range(t, v, o, d, &name, 0, &body(i, 8_000)).map(|_| ())
            } else {
                unlink(t, v, o, d, &name)
            }
        });
        dirty.extend(fsck("files made and deleted in one group"));

        // At scale: a folder the snapshot holds emptied, and a large folder
        // made and emptied under the snapshot (B-tree nodes split, merge
        // and go).
        let many = dir_ino("/many");
        grouped(MANY_BASE, |i, t, v, o| {
            unlink(t, v, o, many, &format!("held{i:05}.txt"))
        });
        dirty.extend(fsck("a folder of snapshot files emptied"));
        let wide = {
            let mut dev = open_dev();
            let (v, o) = vsb_omap(&mut dev);
            let mut t = Transaction::begin(dev).unwrap();
            let ino = apfs_write::file::mkdir(&mut t, &v, &o, 2, "wide").unwrap();
            t.commit().unwrap();
            ino
        };
        grouped(MANY_NEW, |i, t, v, o| {
            create_file(t, v, o, wide, &format!("e{i}")).map(|_| ())
        });
        dirty.extend(fsck("a large folder made under the snapshot"));
        grouped(MANY_NEW, |i, t, v, o| {
            unlink(t, v, o, wide, &format!("e{i}"))
        });
        dirty.extend(fsck("the large folder emptied"));

        assert!(
            dirty.is_empty(),
            "phases that left the volume dirty:\n{}",
            dirty.join("\n")
        );
        let _ = std::fs::remove_file(image());
    }

    /// One operation in a transaction of its own.
    fn one<R>(op: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<R, TxnError>) -> R {
        let mut dev = open_dev();
        let (v, o) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        let r = t.atomically(|t| op(t, &v, &o)).unwrap();
        t.commit().unwrap();
        r
    }

    /// Two snapshots with writes after each, a revert to the first, and
    /// writes after the revert: the sequence run on a USB drive whose
    /// volume block count came out short.
    #[test]
    #[ignore = "macOS scratch image + fsck"]
    fn writes_around_a_revert_to_an_older_snapshot() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let _ = std::fs::remove_dir_all(SRC);
        std::fs::create_dir_all(format!("{SRC}/d")).unwrap();
        // Enough files that every tree the revert rebuilds has many levels.
        let users: usize = std::env::var("REVERT_FILES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20_000);
        for i in 0..users {
            std::fs::write(
                format!("{SRC}/d/user{i:05}.bin"),
                body(i, 1_000 + (i % 200) * 50),
            )
            .unwrap();
        }
        let _ = std::fs::remove_file(image());
        let st = Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "256m", "-srcfolder", SRC])
            .args(["-fs", "APFS", "-layout", "GPTSPUD", "-format", "UDRW"])
            .args(["-volname", "REVERT", IMG])
            .status()
            .unwrap();
        assert!(st.success(), "hdiutil create failed");
        let _ = std::fs::remove_dir_all(SRC);
        let mut dirty = Vec::new();

        let a = one(|t, v, o| create_snapshot(t, v, o, "rel-a"));
        dirty.extend(fsck("rel-a taken"));
        let r = one(|t, v, o| apfs_write::file::mkdir(t, v, o, 2, "rel-test"));
        grouped(20, |i, t, v, o| {
            write_file(
                t,
                v,
                o,
                r,
                &format!("small-{i}.bin"),
                &body(i, 4096 + 37 * i),
            )
            .map(|_| ())
        });
        one(|t, v, o| write_file(t, v, o, r, "big.bin", &body(99, 64 << 20)));
        grouped(5, |i, t, v, o| {
            unlink(t, v, o, r, &format!("small-{}.bin", 15 + i))
        });
        one(|t, v, o| rename(t, v, o, r, "small-0.bin", "renamed-0.bin", false));
        dirty.extend(fsck("first writes under rel-a"));

        one(|t, v, o| create_snapshot(t, v, o, "rel-b"));
        dirty.extend(fsck("rel-b taken"));
        one(|t, v, o| {
            let len = 4096 + 37;
            match write_range(t, v, o, r, "small-1.bin", len, b"appended after rel-b")? {
                RangeWrite::Written => Ok(()),
                RangeWrite::Declined => {
                    unshare_stream(t, v, o, r, "small-1.bin")?;
                    write_range(t, v, o, r, "small-1.bin", len, b"appended after rel-b").map(|_| ())
                }
            }
        });
        one(|t, v, o| unlink(t, v, o, r, "small-2.bin"));
        one(|t, v, o| write_file(t, v, o, r, "after-b.bin", &body(123, 10_000)).map(|_| ()));
        dirty.extend(fsck("writes under rel-b"));

        one(|t, v, o| apfs_write::snapshot::revert_to_snapshot(t, v, o, a));
        dirty.extend(fsck("reverted to rel-a"));

        let r = one(|t, v, o| apfs_write::file::mkdir(t, v, o, 2, "rel-test"));
        one(|t, v, o| write_file(t, v, o, r, "mac-check-1.bin", &body(501, 123_457)).map(|_| ()));
        one(|t, v, o| write_file(t, v, o, r, "mac-check-2.bin", &body(502, 8 << 20)).map(|_| ()));
        dirty.extend(fsck("writes after the revert"));

        assert!(
            dirty.is_empty(),
            "steps that left the volume dirty:\n{}",
            dirty.join("\n")
        );
        let _ = std::fs::remove_file(image());
    }

    fn small_image(name: &str) {
        let _ = std::fs::remove_dir_all(SRC);
        std::fs::create_dir_all(format!("{SRC}/d")).unwrap();
        for i in 0..50 {
            std::fs::write(format!("{SRC}/d/user{i:03}.bin"), body(i, 2_000 + i * 70)).unwrap();
        }
        let _ = std::fs::remove_file(image());
        let st = Command::new("hdiutil")
            .args(["create", "-quiet", "-size", "64m", "-srcfolder", SRC])
            .args(["-fs", "APFS", "-layout", "GPTSPUD", "-format", "UDRW"])
            .args(["-volname", name, IMG])
            .status()
            .unwrap();
        assert!(st.success(), "hdiutil create failed");
        let _ = std::fs::remove_dir_all(SRC);
    }

    /// The steps a revert to an older snapshot is made of, each in its own
    /// transaction and checked on its own.
    #[test]
    #[ignore = "macOS scratch image + fsck"]
    fn the_steps_of_a_revert() {
        if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
            return;
        }
        let mut dirty = Vec::new();
        let d = || dir_ino("/d");
        let write = |tag: &str| {
            let d = d();
            let tag = tag.to_string();
            one(move |t, v, o| write_file(t, v, o, d, &format!("{tag}.bin"), &body(7, 30_000)))
        };

        // (1) The only snapshot, reverted to.
        small_image("STEP1");
        let a = one(|t, v, o| create_snapshot(t, v, o, "a"));
        write("after-a");
        one(|t, v, o| apfs_write::snapshot::revert_to_snapshot(t, v, o, a));
        dirty.extend(fsck("(1) revert to the only snapshot"));

        // (2) The newest snapshot deleted, an older one kept.
        small_image("STEP2");
        one(|t, v, o| create_snapshot(t, v, o, "a"));
        write("after-a");
        let b = one(|t, v, o| create_snapshot(t, v, o, "b"));
        write("after-b");
        one(|t, v, o| apfs_write::snapshot::delete_snapshot(t, v, o, b));
        dirty.extend(fsck("(2) newest snapshot deleted, older kept"));

        // (3) Then a revert to the one left.
        let a = apfs_write_snapshots().into_iter().next().unwrap();
        one(|t, v, o| apfs_write::snapshot::revert_to_snapshot(t, v, o, a));
        dirty.extend(fsck("(3) then reverted to the one left"));

        // (4) A revert to the newest of two, the older kept.
        small_image("STEP4");
        one(|t, v, o| create_snapshot(t, v, o, "a"));
        write("after-a");
        let b = one(|t, v, o| create_snapshot(t, v, o, "b"));
        write("after-b");
        one(|t, v, o| apfs_write::snapshot::revert_to_snapshot(t, v, o, b));
        dirty.extend(fsck("(4) revert to the newest of two"));

        // (5) A revert straight to the older of two.
        small_image("STEP5");
        let a = one(|t, v, o| create_snapshot(t, v, o, "a"));
        write("after-a");
        one(|t, v, o| create_snapshot(t, v, o, "b"));
        write("after-b");
        one(|t, v, o| apfs_write::snapshot::revert_to_snapshot(t, v, o, a));
        dirty.extend(fsck("(5) revert to the older of two"));

        assert!(
            dirty.is_empty(),
            "steps that left the volume dirty:\n{}",
            dirty.join("\n")
        );
        let _ = std::fs::remove_file(image());
    }

    fn apfs_write_snapshots() -> Vec<u64> {
        let mut dev = open_dev();
        let (v, o) = vsb_omap(&mut dev);
        let mut t = Transaction::begin(dev).unwrap();
        let mut x: Vec<u64> = apfs_write::snapshot::list_snapshots(&mut t, &v, &o)
            .unwrap()
            .into_iter()
            .map(|s| s.xid)
            .collect();
        x.sort_unstable();
        x
    }
}
