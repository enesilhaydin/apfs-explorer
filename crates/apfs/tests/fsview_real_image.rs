//! FsView proven against real APFS images: the synthetic apfs-large.img
//! fixture (deterministic) and, if present, the real-USB snapshot
//! /tmp/apfs-rt.img (best-effort, skipped when absent).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use apfs::FsView;
use apfs_core::block_device::FileBlockDevice;
use std::collections::BTreeSet;
use std::path::Path;

#[test]
fn fsview_apfs_large_fixture() {
    let img = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/apfs-large.img"
    );
    if !Path::new(img).exists() {
        eprintln!("skip: fixture missing — run `cargo run -p xtask -- gen-fixture --large`");
        return;
    }
    let dev = FileBlockDevice::open(img).expect("open fixture");
    let mut fs = FsView::open(dev).expect("open FsView");

    let root: BTreeSet<String> = fs
        .read_dir("/")
        .expect("read_dir /")
        .into_iter()
        .map(|e| e.name)
        .collect();
    if !root.contains("dir00") {
        eprintln!("skip: size-only fixture — regenerate gen-fixture --large");
        return;
    }
    for d in 0..16 {
        assert!(
            root.contains(&format!("dir{d:02}")),
            "root missing dir{d:02}"
        );
    }

    let kids: BTreeSet<String> = fs
        .read_dir("/dir00")
        .expect("read_dir /dir00")
        .into_iter()
        .map(|e| e.name)
        .collect();
    for f in 0..64 {
        assert!(
            kids.contains(&format!("f{f:03}.bin")),
            "dir00 missing f{f:03}.bin"
        );
    }

    let da = fs
        .getattr("/dir00")
        .expect("getattr dir00")
        .expect("dir00 exists");
    assert!(da.is_dir, "dir00 is a directory");
    let fa = fs
        .getattr("/dir00/f000.bin")
        .expect("getattr f000")
        .expect("f000 exists");
    assert!(!fa.is_dir);
    assert_eq!(fa.size, 4096);

    let full = fs.read("/dir00/f000.bin", 0, 4096).expect("read full");
    assert_eq!(full.len(), 4096);
    assert!(full.iter().all(|&b| b == b'A'));
    let mid = fs.read("/dir00/f000.bin", 100, 50).expect("read mid");
    assert_eq!(mid.len(), 50);
    assert!(mid.iter().all(|&b| b == b'A'));
    assert!(fs
        .read("/dir00/f000.bin", 99999, 10)
        .expect("past eof")
        .is_empty());

    assert!(fs.lookup("/nope/x").expect("lookup absent").is_none());
    assert!(fs.getattr("/nope").expect("getattr absent").is_none());
    assert_eq!(fs.lookup("").expect("lookup root"), Some(2));
    let again: BTreeSet<String> = fs
        .read_dir("/dir00")
        .expect("read_dir /dir00 again (cached)")
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(again, kids, "cached listing must match");
}

#[test]
fn fsview_real_usb_snapshot_smoke() {
    let img = "/tmp/apfs-rt.img";
    if !Path::new(img).exists() {
        eprintln!("skip: /tmp/apfs-rt.img absent (real-USB snapshot optional)");
        return;
    }
    let dev = FileBlockDevice::open(img).expect("open rt image");
    let mut fs = match FsView::open(dev) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("skip: real-USB image not a complete APFS container ({e})");
            return;
        }
    };
    let root = fs.read_dir("/").expect("read_dir / on real USB image");
    assert!(!root.is_empty(), "real USB volume root should have entries");
    if let Some(a) = fs.getattr("/man/man1/ls.1").ok().flatten() {
        assert!(!a.is_dir);
        let head = fs.read("/man/man1/ls.1", 0, 64).expect("read head");
        assert!(head.len() <= 64);
    }
}
