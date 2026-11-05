//! A whole drive from arbitrary bytes: find the container, open the volume,
//! walk every directory, read every file and attribute, list the snapshots
//! and open each. A drive plugged into Windows is parsed by a process that
//! runs as administrator, so a damaged or crafted one must give errors,
//! never a panic, a hang or memory without bound.
#![no_main]

use apfs::{FsView, SnapshotRef, VolumeSelector};
use apfs_core::block_device::{BlockDevice, BlockError};
use libfuzzer_sys::fuzz_target;

/// Bounds on the walk, so one input cannot run for long.
const MAX_ENTRIES: usize = 2000;
const MAX_DEPTH: usize = 16;
const READ_CAP: usize = 1 << 20;

struct Mem(Vec<u8>);

impl BlockDevice for Mem {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let len = buf.len() as u64;
        let src = usize::try_from(offset)
            .ok()
            .and_then(|o| self.0.get(o..o.checked_add(buf.len())?))
            .ok_or(BlockError::OutOfRange {
                offset,
                len,
                size: self.0.len() as u64,
            })?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

fn walk<D: BlockDevice>(fs: &mut FsView<D>, dir: &str, depth: usize, seen: &mut usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = fs.read_dir(dir) else {
        return;
    };
    for e in entries {
        *seen += 1;
        if *seen > MAX_ENTRIES {
            return;
        }
        let path = if dir == "/" {
            format!("/{}", e.name)
        } else {
            format!("{dir}/{}", e.name)
        };
        let Ok(Some(attr)) = fs.getattr(&path) else {
            continue;
        };
        if let Ok(names) = fs.list_xattrs(&path) {
            for n in names.iter().take(8) {
                let _ = fs.get_xattr(&path, n);
            }
        }
        if attr.is_dir {
            walk(fs, &path, depth + 1, seen);
        } else {
            let want = usize::try_from(attr.size).unwrap_or(usize::MAX).min(READ_CAP);
            let _ = fs.read(&path, 0, want);
            let _ = fs.read(&path, attr.size / 2, 4096);
            let _ = fs.read_symlink(&path);
        }
    }
}

fuzz_target!(|data: &[u8]| {
    let mut disk = Mem(data.to_vec());
    // A whole-disk image (GPT) or a bare container.
    let dev = match apfs_core::gpt::find_apfs_container(&mut disk, 512) {
        Ok(r) => {
            let start = usize::try_from(r.offset).unwrap_or(usize::MAX);
            let end = usize::try_from(r.offset.saturating_add(r.length)).unwrap_or(usize::MAX);
            match data.get(start..end.min(data.len())) {
                Some(part) => Mem(part.to_vec()),
                None => return,
            }
        }
        Err(_) => disk,
    };
    let Ok(mut fs) = FsView::open_selected(dev, VolumeSelector::Default) else {
        return;
    };
    let mut seen = 0usize;
    walk(&mut fs, "/", 0, &mut seen);
    let snaps = fs.snapshots().unwrap_or_default();
    let dev = fs.into_dev();
    for s in snaps.iter().take(4) {
        let Ok(mut snap) = FsView::open_snapshot(
            Mem(dev.0.clone()),
            VolumeSelector::Default,
            &SnapshotRef::Xid(s.xid),
        ) else {
            continue;
        };
        let mut seen = 0usize;
        walk(&mut snap, "/", 0, &mut seen);
    }
});
