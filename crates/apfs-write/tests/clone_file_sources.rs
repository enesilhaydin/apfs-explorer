//! Cloning a file macOS has already cloned, or linked.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil, cp -c, ln).
//!
//! A clone macOS made keeps its content under the original's stream id, and a
//! hard link's content is under an id its extra names do not carry. A copy
//! that gathers the source's extents by the source inode's id finds nothing
//! for the first and produces a file that claims a size it has no extents
//! for. The copy has to follow the id the inode names.
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
mod clone_file_sources {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::clone_file;
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-clone-file-sources.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const SIZE: usize = 200 * 1024;

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn body() -> Vec<u8> {
        (0..SIZE).map(|i| ((i * 13) % 251) as u8).collect()
    }

    /// Build an image where macOS writes `original.bin`, clones it as
    /// `cloned.bin` and links it as `linked.bin`.
    fn build_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-clone-file-sources.dmg");
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
                "CLSRC",
                "/tmp/apfs-clone-file-sources",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-clone-file-sources.dmg", IMG).expect("rename");

        let (mount, parent) = attach(true);
        std::fs::write(format!("{mount}/original.bin"), body()).expect("write original");
        // A sparse file: the first 10000 bytes written, the rest a hole up to
        // the same size as the others, the shape macOS gives a truncate.
        std::fs::write(format!("{mount}/sparse.bin"), &body()[..10_000]).expect("write sparse");
        let st = Command::new("truncate")
            .args(["-s", &SIZE.to_string(), &format!("{mount}/sparse.bin")])
            .status()
            .expect("truncate");
        assert!(st.success(), "truncate failed");
        for (tool, flag, target) in [("cp", Some("-c"), "cloned.bin"), ("ln", None, "linked.bin")] {
            let mut args: Vec<String> = Vec::new();
            if let Some(f) = flag {
                args.push(f.to_string());
            }
            args.push(format!("{mount}/original.bin"));
            args.push(format!("{mount}/{target}"));
            let st = Command::new(tool).args(&args).status().expect(tool);
            assert!(st.success(), "{tool} {target} failed");
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
        let _ = Command::new("hdiutil").args(["detach", parent]).status();
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

    fn run_fsck(label: &str) {
        let (_, parent) = attach(false);
        let apfs = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &apfs]).output();
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

    /// Clone `source` as `copy.bin`; the copy and every existing name must
    /// read the same bytes, through our reader and through macOS, fsck-clean.
    fn clone_and_check(source: &str) {
        build_image();
        let body = body();
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            clone_file(&mut txn, &vsb, &om, 2, source, 2, "copy.bin").expect("clone_file");
            txn.commit().expect("commit clone");
        }
        run_fsck(&format!("after cloning {source}"));
        {
            let mut fs = open_fs();
            for name in ["copy.bin", "original.bin", "cloned.bin", "linked.bin"] {
                let got = fs.read(&format!("/{name}"), 0, SIZE).expect("read");
                assert_eq!(
                    got, body,
                    "{name} must hold the content after cloning {source}"
                );
            }
        }
        let (mount, parent) = attach(true);
        let seen = std::fs::read(format!("{mount}/copy.bin"));
        detach(&parent);
        assert_eq!(seen.expect("macOS read"), body, "macOS must read the copy");
    }

    #[test]
    #[ignore = "fsck-gated; needs macOS to make the clone"]
    fn a_clone_macos_made_can_be_cloned_again() {
        if !gate() {
            println!("clone-file-sources: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        clone_and_check("cloned.bin");
        println!("clone-file-sources PASS: a macOS clone was cloned again");
    }

    #[test]
    #[ignore = "fsck-gated; needs macOS to make the link"]
    fn a_hard_linked_file_can_be_cloned() {
        if !gate() {
            println!("clone-file-sources: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        clone_and_check("linked.bin");
        println!("clone-file-sources PASS: a hard-linked file was cloned");
    }

    /// A sparse file's clone is sparse the same way: the hole, the sparse
    /// byte count and the sparse flag come along, or the checker reports the
    /// clone's inode.
    #[test]
    #[ignore = "fsck-gated; needs macOS to make the sparse file"]
    fn a_sparse_file_can_be_cloned() {
        if !gate() {
            println!("clone-file-sources: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        build_image();
        let mut expected = body()[..10_000].to_vec();
        expected.resize(SIZE, 0);
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            clone_file(&mut txn, &vsb, &om, 2, "sparse.bin", 2, "sparse-copy.bin")
                .expect("clone_file");
            txn.commit().expect("commit clone");
        }
        run_fsck("after cloning the sparse file");
        {
            let mut fs = open_fs();
            for name in ["sparse.bin", "sparse-copy.bin"] {
                let got = fs.read(&format!("/{name}"), 0, SIZE).expect("read");
                assert_eq!(got, expected, "{name} must hold the content with its hole");
            }
            let original = fs
                .inode_record("/sparse.bin")
                .expect("inode")
                .expect("sparse.bin");
            let copy = fs
                .inode_record("/sparse-copy.bin")
                .expect("inode")
                .expect("sparse-copy.bin");
            assert_eq!(
                copy.internal_flags & INODE_IS_SPARSE,
                original.internal_flags & INODE_IS_SPARSE,
                "the clone carries the sparse flag the original carries ({:#x} vs {:#x})",
                copy.internal_flags,
                original.internal_flags
            );
        }
        let (mount, parent) = attach(true);
        let seen = std::fs::read(format!("{mount}/sparse-copy.bin"));
        detach(&parent);
        assert_eq!(
            seen.expect("macOS read"),
            expected,
            "macOS must read the sparse copy"
        );
        println!("clone-file-sources PASS: a sparse file was cloned");
    }

    /// `INODE_IS_SPARSE` in `j_inode_val_t.internal_flags`.
    const INODE_IS_SPARSE: u64 = 0x200;
}
