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
use apfs_core::block_device::{
    BlockDevice, BlockError, FileBlockDeviceRw, WritableBlockDevice, WritableOffsetBlockDevice,
};
use apfs_write::file::{create_file, mkdir, overwrite_existing_file, rename, unlink};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use winfsp_host::cache::CachingBlockDevice;
use winfsp_host::group::{
    commit_group, write_in_group, write_in_group_keeping_names, write_in_group_removing,
    GroupClock, GroupDev,
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

/// A device that counts the reads that reach it.
struct Counted<D> {
    inner: D,
    reads: Arc<AtomicU64>,
}

impl<D: BlockDevice> BlockDevice for Counted<D> {
    fn size(&self) -> u64 {
        self.inner.size()
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_at(offset, buf)
    }
}

impl<D: WritableBlockDevice> WritableBlockDevice for Counted<D> {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        self.inner.write_at(offset, buf)
    }
    fn invalidate_read_cache(&mut self) {
        self.inner.invalidate_read_cache();
    }
    fn flush_data(&mut self) -> Result<(), BlockError> {
        self.inner.flush_data()
    }
}

/// The mount's stack: the read cache over a disk whose reads are counted.
type CachedDev = CachingBlockDevice<Counted<Dev>>;

fn mount_cached(img: &str, reads: &Arc<AtomicU64>) -> FsView<GroupDev<CachedDev>> {
    let dev = CachingBlockDevice::new(Counted {
        inner: open_dev(img),
        reads: Arc::clone(reads),
    });
    FsView::open_selected(GroupDev::Plain(dev), VolumeSelector::Default).unwrap()
}

#[test]
#[ignore = "fsck-gated"]
fn a_commit_keeps_the_read_cache() {
    if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
        return;
    }
    let img = "/tmp/apfs-group-cache.img";
    let (ok, out) = xtask(&["gen-scratch", img, "64"]);
    assert!(ok, "gen-scratch: {out}");
    let reads = Arc::new(AtomicU64::new(0));
    let mut fs = mount_cached(img, &reads);
    let mut clock = GroupClock::default();
    let d = write_in_group(&mut fs, &mut clock, |t, v, o| {
        mkdir(t, v, o, ROOT_INO, "big")
    })
    .unwrap();
    for i in 0..2000 {
        let name = format!("entry-{i:05}");
        write_in_group(&mut fs, &mut clock, |t, v, o| {
            create_file(t, v, o, d, &name)
        })
        .unwrap();
    }
    write_in_group(&mut fs, &mut clock, |t, v, o| {
        create_file(t, v, o, ROOT_INO, "data.bin")
    })
    .unwrap();
    let (committed, reopened) = commit_group(fs, VolumeSelector::Default, &mut clock);
    committed.unwrap();
    let mut fs = reopened.unwrap();

    // What a reader on a cold cache reads to list the directory.
    let cold_reads = Arc::new(AtomicU64::new(0));
    let mut cold = mount_cached(img, &cold_reads);
    assert_eq!(cold.read_dir("/big").unwrap().len(), 2000);
    let cold = cold_reads.load(Ordering::Relaxed);
    drop(cold_reads);

    // Rounds of: list the directory, rewrite a file (its old blocks are
    // freed and may be taken again), commit. Every round reads back what it
    // wrote, though the cache outlives the commits.
    let mut after_commit = Vec::new();
    for round in 0..12u8 {
        let body: Vec<u8> = (0..40_000u32)
            .map(|i| (i as u8) ^ round.wrapping_mul(37))
            .collect();
        let before = reads.load(Ordering::Relaxed);
        assert_eq!(fs.read_dir("/big").unwrap().len(), 2000);
        after_commit.push(reads.load(Ordering::Relaxed) - before);
        write_in_group_keeping_names(&mut fs, &mut clock, |t, v, o| {
            overwrite_existing_file(t, v, o, ROOT_INO, "data.bin", &body)
        })
        .unwrap();
        let (committed, reopened) = commit_group(fs, VolumeSelector::Default, &mut clock);
        committed.unwrap();
        fs = reopened.unwrap();
        assert_eq!(
            fs.read("/data.bin", 0, 50_000).unwrap(),
            body,
            "round {round}"
        );
    }
    // A directory the commits did not touch is listed from the cache: a
    // cache dropped at every commit read it all again each round.
    let worst = after_commit.iter().skip(1).max().copied().unwrap_or(0);
    assert!(
        worst * 2 < cold,
        "listing after a commit read {worst} times; a cold listing reads {cold} \
         (per round: {after_commit:?})"
    );
    drop(fs);
    let (ok, out) = xtask(&["verify-scratch", img]);
    assert!(ok, "not fsck-clean after commits over a kept cache:\n{out}");
    let _ = std::fs::remove_file(img);
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

type CountedView = FsView<GroupDev<Counted<Dev>>>;

/// Delete every file of `dir` the way a recursive delete on Windows does:
/// WinFsp lists a page after the last name it gave out, the program takes
/// a name or two from it and deletes them, and the delete makes WinFsp
/// drop the page, so the next name is listed again. A group commits every
/// 250 deletes. Gives the names seen and the reads the listings took.
fn delete_while_listing(
    fs: &mut Option<CountedView>,
    clock: &mut GroupClock,
    reads: &Arc<AtomicU64>,
    dir: &str,
    keep_listing: bool,
) -> (Vec<String>, u64) {
    let view = fs.as_mut().unwrap();
    let d = view.getattr(dir).unwrap().unwrap().inode;
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    let mut listing_reads = 0;
    loop {
        let view = fs.as_mut().unwrap();
        let before = reads.load(Ordering::Relaxed);
        let mut page = view.read_dir_after(dir, after.as_deref(), 64).unwrap();
        listing_reads += reads.load(Ordering::Relaxed) - before;
        page.truncate(2);
        let Some(last) = page.last() else { break };
        after = Some(last.name.clone());
        for e in page {
            let view = fs.as_mut().unwrap();
            let path = format!("{dir}/{}", e.name);
            // Another spelling of the name, resolved and cached first.
            let alias = format!("{dir}/{}", e.name.to_uppercase());
            let _ = view.getattr(&alias).unwrap();
            let unlink_it = |t: &mut _, v: &[u8], o: &[u8]| unlink(t, v, o, d, &e.name);
            if keep_listing {
                write_in_group_removing(view, clock, d, &e.name, unlink_it).unwrap();
            } else {
                write_in_group(view, clock, unlink_it).unwrap();
            }
            assert!(view.getattr(&path).unwrap().is_none(), "{path} still found");
            assert!(
                view.getattr(&alias).unwrap().is_none(),
                "{alias} still found"
            );
            seen.push(e.name);
            if seen.len() % 250 == 0 {
                let (committed, reopened) =
                    commit_group(fs.take().unwrap(), VolumeSelector::Default, clock);
                committed.unwrap();
                *fs = Some(reopened.unwrap());
            }
        }
    }
    (seen, listing_reads)
}

#[test]
#[ignore = "fsck-gated"]
fn deleting_a_folder_while_listing_it() {
    if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
        return;
    }
    let img = "/tmp/apfs-group-delete.img";
    let (ok, out) = xtask(&["gen-scratch", img, "64"]);
    assert!(ok, "gen-scratch: {out}");
    let reads = Arc::new(AtomicU64::new(0));
    let dev = Counted {
        inner: open_dev(img),
        reads: Arc::clone(&reads),
    };
    let mut fs =
        Some(FsView::open_selected(GroupDev::Plain(dev), VolumeSelector::Default).unwrap());
    let mut clock = GroupClock::default();
    const FILES: usize = 1500;
    let mut want = Vec::new();
    for dir in ["kept", "dropped"] {
        let view = fs.as_mut().unwrap();
        let d = write_in_group(view, &mut clock, |t, v, o| mkdir(t, v, o, ROOT_INO, dir)).unwrap();
        for i in 0..FILES {
            let name = format!("item-{i:05}.dat");
            write_in_group(view, &mut clock, |t, v, o| create_file(t, v, o, d, &name)).unwrap();
        }
    }
    for i in 0..FILES {
        want.push(format!("item-{i:05}.dat"));
    }
    let (committed, reopened) =
        commit_group(fs.take().unwrap(), VolumeSelector::Default, &mut clock);
    committed.unwrap();
    fs = Some(reopened.unwrap());

    let (seen, kept_reads) = delete_while_listing(&mut fs, &mut clock, &reads, "/kept", true);
    assert_eq!(
        seen, want,
        "every name seen once, in order, keeping the listing"
    );
    let (seen, dropped_reads) =
        delete_while_listing(&mut fs, &mut clock, &reads, "/dropped", false);
    assert_eq!(
        seen, want,
        "every name seen once, in order, reading it again"
    );
    eprintln!("listing reads: keeping the listing {kept_reads}, reading it again {dropped_reads}");
    // Kept, the folder is read once; read again, once for every two names.
    assert!(
        kept_reads * 20 < dropped_reads,
        "keeping the listing read {kept_reads} times, reading it again {dropped_reads}"
    );

    let mut view = fs.take().unwrap();
    assert!(names(&mut view, "/kept").is_empty());
    let (committed, reopened) = commit_group(view, VolumeSelector::Default, &mut clock);
    committed.unwrap();
    drop(reopened);
    let mut fresh = mount(img);
    assert!(names(&mut fresh, "/kept").is_empty());
    assert!(names(&mut fresh, "/dropped").is_empty());
    drop(fresh);
    let (ok, out) = xtask(&["verify-scratch", img]);
    assert!(ok, "not fsck-clean after deleting while listing:\n{out}");
    let _ = std::fs::remove_file(img);
}

#[test]
#[ignore = "fsck-gated"]
fn a_kept_listing_matches_the_disk() {
    if std::env::var("APFS_WRITE_SCRATCH").as_deref() != Ok("1") {
        return;
    }
    let img = "/tmp/apfs-group-listing.img";
    let (ok, out) = xtask(&["gen-scratch", img, "64"]);
    assert!(ok, "gen-scratch: {out}");
    let mut fs = mount(img);
    let mut clock = GroupClock::default();
    let d = write_in_group(&mut fs, &mut clock, |t, v, o| {
        mkdir(t, v, o, ROOT_INO, "dir")
    })
    .unwrap();
    for i in 0..300 {
        let name = format!("n{i:03}");
        write_in_group(&mut fs, &mut clock, |t, v, o| {
            create_file(t, v, o, d, &name)
        })
        .unwrap();
    }
    let sub = write_in_group(&mut fs, &mut clock, |t, v, o| mkdir(t, v, o, d, "sub")).unwrap();
    write_in_group(&mut fs, &mut clock, |t, v, o| {
        create_file(t, v, o, sub, "inner")
    })
    .unwrap();
    let (committed, reopened) = commit_group(fs, VolumeSelector::Default, &mut clock);
    committed.unwrap();
    let mut fs = reopened.unwrap();
    assert_eq!(names(&mut fs, "/dir").len(), 301);
    assert_eq!(names(&mut fs, "/dir/sub"), ["inner"]);
    // Every third name removed, with commits between, over a listing kept
    // across them; then a directory emptied and removed.
    for i in (0..300).step_by(3) {
        let name = format!("n{i:03}");
        write_in_group_removing(&mut fs, &mut clock, d, &name, |t, v, o| {
            unlink(t, v, o, d, &name)
        })
        .unwrap();
        if i % 60 == 0 {
            let (committed, reopened) = commit_group(fs, VolumeSelector::Default, &mut clock);
            committed.unwrap();
            fs = reopened.unwrap();
        }
    }
    write_in_group_removing(&mut fs, &mut clock, sub, "inner", |t, v, o| {
        unlink(t, v, o, sub, "inner")
    })
    .unwrap();
    write_in_group_removing(&mut fs, &mut clock, d, "sub", |t, v, o| {
        unlink(t, v, o, d, "sub")
    })
    .unwrap();
    assert!(
        fs.read_dir("/dir/sub").is_err(),
        "the removed directory is gone"
    );
    let cached = names(&mut fs, "/dir");
    assert_eq!(cached.len(), 200);
    let (committed, reopened) = commit_group(fs, VolumeSelector::Default, &mut clock);
    committed.unwrap();
    let mut fs = reopened.unwrap();
    assert_eq!(names(&mut fs, "/dir"), cached);
    drop(fs);
    let mut fresh = mount(img);
    assert_eq!(
        names(&mut fresh, "/dir"),
        cached,
        "the cached listing is what the disk holds"
    );
    drop(fresh);
    let (ok, out) = xtask(&["verify-scratch", img]);
    assert!(ok, "not fsck-clean:\n{out}");
    let _ = std::fs::remove_file(img);
}
