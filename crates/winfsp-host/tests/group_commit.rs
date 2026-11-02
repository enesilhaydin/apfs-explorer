//! Write operations grouped into one checkpoint, the way the mount runs them.
//!
//! Lookups between grouped operations see them before they are committed; an
//! operation that fails leaves the group's other operations; after the
//! commit a fresh reader sees everything and the volume checks clean. A
//! group dropped without its commit (a power cut) leaves the volume at the
//! checkpoint before it, still clean.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]`: runs fsck.
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p winfsp-host --test group_commit \
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

use apfs::{FsView, VolumeSelector};
use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
use apfs_write::file::{create_file, mkdir, overwrite_existing_file, rename, unlink};
use std::process::Command;
use winfsp_host::group::{
    commit_group, write_in_group, write_in_group_keeping_names, GroupClock, GroupDev,
};

const SECTOR: u64 = 512;
#[rustfmt::skip]
const APFS_GPT_TYPE: [u8; 16] = [
    0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
    0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
];
const ROOT_INO: u64 = 2;

type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;

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

fn open_dev(img: &str) -> Dev {
    let mut head = vec![0u8; 64 * 1024];
    std::io::Read::read_exact(&mut std::fs::File::open(img).unwrap(), &mut head).unwrap();
    let hdr = &head[512..1024];
    let lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
    let n = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
    let sz = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
    let base = (lba * SECTOR) as usize;
    for i in 0..n {
        let e = &head[base + i * sz..base + (i + 1) * sz];
        if e[..16] == APFS_GPT_TYPE {
            let s = u64::from_le_bytes(e[32..40].try_into().unwrap());
            let en = u64::from_le_bytes(e[40..48].try_into().unwrap());
            let inner = FileBlockDeviceRw::open_rw(img).unwrap();
            return WritableOffsetBlockDevice::new(inner, s * SECTOR, (en - s + 1) * SECTOR);
        }
    }
    panic!("no APFS partition in {img}");
}

fn names(fs: &mut FsView<impl BlockDevice>, dir: &str) -> Vec<String> {
    let mut n: Vec<String> = fs
        .read_dir(dir)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .filter(|n| !n.starts_with('.'))
        .collect();
    n.sort();
    n
}

fn mount(img: &str) -> FsView<GroupDev<Dev>> {
    FsView::open_selected(GroupDev::Plain(open_dev(img)), VolumeSelector::Default).unwrap()
}

#[test]
#[ignore = "fsck-gated"]
fn grouped_writes_are_seen_at_once_and_land_together() {
    if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
        return;
    }
    let img = "/tmp/apfs-group-commit.img";
    let (ok, out) = xtask(&["gen-scratch", img, "64"]);
    assert!(ok, "gen-scratch: {out}");
    let mut fs = mount(img);
    let mut clock = GroupClock::default();
    let body: Vec<u8> = (0..20_000u32).map(|i| (i % 241) as u8).collect();

    let d = write_in_group(&mut fs, &mut clock, |t, v, o| {
        mkdir(t, v, o, ROOT_INO, "dir")
    })
    .unwrap();
    for i in 0..50 {
        let name = format!("f{i:02}.txt");
        write_in_group(&mut fs, &mut clock, |t, v, o| {
            create_file(t, v, o, d, &name)
        })
        .unwrap();
        // The reader finds the file at once, before any commit.
        assert!(fs.getattr(&format!("/dir/{name}")).unwrap().is_some());
        // A write changes no name: the reader keeps its cached paths, yet
        // sees the new content and size at once.
        write_in_group_keeping_names(&mut fs, &mut clock, |t, v, o| {
            overwrite_existing_file(t, v, o, d, &name, &body)
        })
        .unwrap();
        let path = format!("/dir/{name}");
        assert_eq!(fs.getattr(&path).unwrap().unwrap().size, body.len() as u64);
        assert_eq!(fs.read(&path, 0, 50_000).unwrap(), body);
    }
    assert_eq!(fs.read("/dir/f07.txt", 0, 50_000).unwrap(), body);
    write_in_group(&mut fs, &mut clock, |t, v, o| {
        rename(t, v, o, d, "f00.txt", "renamed.txt", false)
    })
    .unwrap();
    // A failing operation reports its error and takes nothing with it.
    write_in_group(&mut fs, &mut clock, |t, v, o| {
        unlink(t, v, o, ROOT_INO, "dir")
    })
    .unwrap_err();
    write_in_group(&mut fs, &mut clock, |t, v, o| unlink(t, v, o, d, "f01.txt")).unwrap();
    assert_eq!(clock.ops, 1 + 100 + 2);
    assert_eq!(names(&mut fs, "/dir").len(), 49);

    let (committed, reopened) = commit_group(fs, VolumeSelector::Default, &mut clock);
    committed.unwrap();
    let mut fs = reopened.unwrap();
    assert!(!clock.is_open());
    let listed = names(&mut fs, "/dir");
    assert_eq!(listed.len(), 49);
    assert!(listed.contains(&"renamed.txt".to_string()));
    assert!(!listed.contains(&"f01.txt".to_string()));
    assert_eq!(fs.read("/dir/renamed.txt", 0, 50_000).unwrap(), body);
    drop(fs);
    let (ok, out) = xtask(&["verify-scratch", img]);
    assert!(ok, "not fsck-clean after the grouped commit:\n{out}");

    // A group that never commits leaves the volume as it was.
    let mut fs = mount(img);
    let mut clock = GroupClock::default();
    for i in 0..20 {
        let name = format!("lost{i}.txt");
        write_in_group(&mut fs, &mut clock, |t, v, o| {
            create_file(t, v, o, ROOT_INO, &name)
        })
        .unwrap();
    }
    drop(fs);
    let (ok, out) = xtask(&["verify-scratch", img]);
    assert!(ok, "not fsck-clean after a dropped group:\n{out}");
    let mut fs = mount(img);
    assert_eq!(names(&mut fs, "/"), ["dir"]);
    let _ = std::fs::remove_file(img);
}
