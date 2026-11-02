//! Removing or replacing a file whose extents span several reference records.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, cp -c, dd, fsck_apfs).
//!
//! When macOS rewrites part of a clone, it splits the shared physical extent's
//! reference records at the rewritten range: the original file still has one
//! extent, but that extent now spans three records (shared, no longer shared,
//! shared). Releasing the original's blocks has to walk the records the extent
//! covers, decrementing the shared ones and freeing the one only it holds. A
//! release that looks up a single record by the extent's first block touches
//! one third of the range and leaves the rest with a reference count nothing
//! holds, which fsck reports and which keeps the blocks allocated for good.
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
mod extref_partial_overlap {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{clone_file, overwrite_existing_file, unlink};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-extref-partial-overlap.img";
    const SECTOR: u64 = 512;
    const BLOCK: usize = 4096;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    /// The original: 64 blocks, one extent as macOS writes it.
    const BLOCKS: usize = 64;
    /// The clone gets these blocks rewritten by macOS.
    const REWRITE_AT: usize = 10;
    const REWRITE_BLOCKS: usize = 5;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn pattern(len: usize, seed: u64) -> Vec<u8> {
        (0..len)
            .map(|i| ((i as u64 * 7 + seed * 131) % 251) as u8)
            .collect()
    }

    fn original(seed: u64) -> Vec<u8> {
        pattern(BLOCKS * BLOCK, seed)
    }

    /// The clone after macOS rewrote `REWRITE_BLOCKS` blocks in its middle.
    fn rewritten_clone(seed: u64) -> Vec<u8> {
        let mut c = original(seed);
        let piece = pattern(REWRITE_BLOCKS * BLOCK, seed + 50);
        c[REWRITE_AT * BLOCK..(REWRITE_AT + REWRITE_BLOCKS) * BLOCK].copy_from_slice(&piece);
        c
    }

    fn run(tool: &str, args: &[&str]) {
        let st = Command::new(tool).args(args).status().expect(tool);
        assert!(st.success(), "{tool} {args:?} failed");
    }

    /// macOS writes two originals, clones each, and rewrites part of each clone.
    fn build_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-extref-partial-overlap.dmg");
        let out = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                "64m",
                "-layout",
                "GPTSPUD",
                "-fs",
                "APFS",
                "-volname",
                "EXTREF",
                "/tmp/apfs-extref-partial-overlap",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-extref-partial-overlap.dmg", IMG).expect("rename");

        let (m, parent) = attach(true);
        assert!(!m.is_empty(), "the fresh image must mount");
        for (name, clone, seed) in [("a.bin", "b.bin", 1), ("c.bin", "d.bin", 2)] {
            std::fs::write(format!("{m}/{name}"), original(seed)).expect("write original");
            run(
                "cp",
                &["-c", &format!("{m}/{name}"), &format!("{m}/{clone}")],
            );
            let piece = format!("/tmp/apfs-extref-partial-overlap-piece-{seed}.bin");
            std::fs::write(&piece, pattern(REWRITE_BLOCKS * BLOCK, seed + 50)).unwrap();
            run(
                "dd",
                &[
                    &format!("if={piece}"),
                    &format!("of={m}/{clone}"),
                    &format!("bs={BLOCK}"),
                    &format!("seek={REWRITE_AT}"),
                    "conv=notrunc",
                    "status=none",
                ],
            );
            let back = std::fs::read(format!("{m}/{clone}")).expect("read clone");
            assert!(
                back == rewritten_clone(seed),
                "{clone}: macOS must hold the rewritten clone"
            );
        }
        let _ = Command::new("sync").status();
        detach(&parent);
    }

    fn attach(mount_it: bool) -> (String, String) {
        let mut args = vec![
            "attach",
            "-imagekey",
            "diskimage-class=CRawDiskImage",
            "-nobrowse",
        ];
        if !mount_it {
            args.push("-nomount");
        }
        args.push(IMG);
        let out = Command::new("hdiutil")
            .args(&args)
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mount = stdout
            .lines()
            .find_map(|l| l.find("/Volumes/").map(|p| l[p..].trim().to_string()))
            .unwrap_or_default();
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
        (mount, parent)
    }

    fn detach(parent: &str) {
        for attempt in 0..8 {
            let st = Command::new("hdiutil")
                .args(["detach", parent])
                .output()
                .expect("detach");
            if st.status.success() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(250 * (attempt + 1)));
        }
        let st = Command::new("hdiutil")
            .args(["detach", "-force", parent])
            .status()
            .expect("detach");
        assert!(st.success(), "hdiutil detach failed");
    }

    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let hdr = &raw[512..1024];
        let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let base = (lba * SECTOR) as usize;
        for i in 0..n {
            let off = base + i * sz;
            let e = match raw.get(off..off + sz) {
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

    fn open_txn() -> Transaction<Dev> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }

    fn open_fs() -> apfs::FsView<Dev> {
        let raw = std::fs::read(IMG).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
        apfs::FsView::open(WritableOffsetBlockDevice::new(inner, base, len)).expect("FsView::open")
    }

    fn with_txn<T>(
        f: impl FnOnce(&mut Transaction<Dev>, &[u8], &[u8]) -> Result<T, apfs_write::txn::TxnError>,
    ) -> T {
        let (vsb, om) = read_vsb_omap();
        let mut txn = open_txn();
        let r = f(&mut txn, &vsb, &om).expect("operation");
        txn.commit().expect("commit");
        r
    }

    /// Strict: any inconsistency fsck_apfs mentions counts, not just its exit code.
    fn run_fsck(label: &str) {
        let (_, parent) = attach(false);
        let rdev = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        detach(&parent);
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

    fn assert_content(label: &str, path: &str, want: &[u8]) {
        let mut fs = open_fs();
        let got = fs.read(path, 0, want.len() + 1).expect("read");
        assert!(got == want, "{label}: {path} content differs");
    }

    /// macOS mounts the result and reads the survivors itself.
    fn macos_reads(paths: &[(&str, Vec<u8>)]) {
        let (m, parent) = attach(true);
        let mut problems = Vec::new();
        for (path, want) in paths {
            match std::fs::read(format!("{m}{path}")) {
                Ok(got) if got == *want => {}
                Ok(_) => problems.push(format!("{path}: macOS read different bytes")),
                Err(e) => problems.push(format!("{path}: macOS could not read it: {e}")),
            }
        }
        detach(&parent);
        assert!(
            problems.is_empty(),
            "macOS disagrees:\n  {}",
            problems.join("\n  ")
        );
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn removing_the_original_after_macos_rewrote_part_of_its_clone() {
        if !gate() {
            return;
        }
        build_image();
        run_fsck("as macOS left it");
        with_txn(|t, v, o| unlink(t, v, o, 2, "a.bin"));
        run_fsck("after removing the original");
        assert_content("after removing the original", "/b.bin", &rewritten_clone(1));
        macos_reads(&[("/b.bin", rewritten_clone(1))]);
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn replacing_the_original_after_macos_rewrote_part_of_its_clone() {
        if !gate() {
            return;
        }
        build_image();
        let fresh = pattern(20 * BLOCK + 17, 9);
        with_txn(|t, v, o| overwrite_existing_file(t, v, o, 2, "c.bin", &fresh));
        run_fsck("after replacing the original");
        assert_content("after replacing the original", "/c.bin", &fresh);
        assert_content(
            "after replacing the original",
            "/d.bin",
            &rewritten_clone(2),
        );
        macos_reads(&[("/c.bin", fresh), ("/d.bin", rewritten_clone(2))]);
    }

    /// Cloning takes a reference on every record the source's extent spans:
    /// the shared head and tail and the middle only the original holds.
    #[test]
    #[ignore = "scratch-image gated"]
    fn cloning_the_original_after_macos_rewrote_part_of_its_clone() {
        if !gate() {
            return;
        }
        build_image();
        with_txn(|t, v, o| clone_file(t, v, o, 2, "a.bin", 2, "e.bin"));
        run_fsck("after cloning the original");
        assert_content("after cloning the original", "/e.bin", &original(1));
        // Each of the three names lets go of its share in turn.
        with_txn(|t, v, o| unlink(t, v, o, 2, "a.bin"));
        run_fsck("after removing the original");
        assert_content("after removing the original", "/e.bin", &original(1));
        assert_content("after removing the original", "/b.bin", &rewritten_clone(1));
        macos_reads(&[("/e.bin", original(1)), ("/b.bin", rewritten_clone(1))]);
    }

    #[test]
    #[ignore = "scratch-image gated"]
    fn removing_the_rewritten_clone_keeps_the_original() {
        if !gate() {
            return;
        }
        build_image();
        with_txn(|t, v, o| unlink(t, v, o, 2, "b.bin"));
        run_fsck("after removing the rewritten clone");
        assert_content("after removing the rewritten clone", "/a.bin", &original(1));
        with_txn(|t, v, o| unlink(t, v, o, 2, "a.bin"));
        run_fsck("after removing both");
        macos_reads(&[("/c.bin", original(2)), ("/d.bin", rewritten_clone(2))]);
    }
}
