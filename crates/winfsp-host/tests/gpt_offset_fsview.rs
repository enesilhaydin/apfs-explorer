//! End-to-end (macOS-runnable): a GPT-prefixed copy of the APFS fixture is
//! parsed by `gpt`, rebased by `OffsetBlockDevice`, and read by
//! `apfs::FsView` - proving the M4b cross-platform stack without Windows.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::path::PathBuf;

use apfs::FsView;
use apfs_core::block_device::FileBlockDevice;
use winfsp_host::gpt::find_apfs_container;
use winfsp_host::offset_dev::OffsetBlockDevice;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/apfs-large.img")
}

/// Build a temp disk image: [protective MBR][GPT hdr][GPT table][... pad to
/// container LBA ...][raw apfs-large.img bytes]. Container firstLBA chosen
/// so the container is 1 MiB into the disk (well past the GPT table).
#[test]
fn fsview_over_gpt_wrapped_fixture() {
    let fx = fixture_path();
    if !fx.exists() {
        eprintln!("SKIP: fixture {} absent", fx.display());
        return;
    }
    let mut apfs_bytes = Vec::new();
    std::fs::File::open(&fx)
        .unwrap()
        .read_to_end(&mut apfs_bytes)
        .unwrap();

    let first_lba: u64 = 2048; // 1 MiB
    let sector = 512u64;
    let cont_off = (first_lba * sector) as usize;
    let last_lba = first_lba + (apfs_bytes.len() as u64 / sector) - 1;

    let mut disk = vec![0u8; cont_off + apfs_bytes.len()];
    disk[510] = 0x55;
    disk[511] = 0xAA;
    disk[512..520].copy_from_slice(b"EFI PART");
    disk[512 + 72..512 + 80].copy_from_slice(&2u64.to_le_bytes());
    disk[512 + 80..512 + 84].copy_from_slice(&128u32.to_le_bytes());
    disk[512 + 84..512 + 88].copy_from_slice(&128u32.to_le_bytes());
    let ent = 2 * 512;
    let apfs_guid: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC,
        0xAC,
    ];
    disk[ent..ent + 16].copy_from_slice(&apfs_guid);
    disk[ent + 32..ent + 40].copy_from_slice(&first_lba.to_le_bytes());
    disk[ent + 40..ent + 48].copy_from_slice(&last_lba.to_le_bytes());
    disk[cont_off..cont_off + apfs_bytes.len()].copy_from_slice(&apfs_bytes);

    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(&disk).unwrap();
    tmp.flush().unwrap();

    let mut dev = FileBlockDevice::open(tmp.path()).unwrap();
    let range = find_apfs_container(&mut dev, sector as u64).unwrap();
    assert_eq!(range.offset, first_lba * sector);

    let dev = FileBlockDevice::open(tmp.path()).unwrap();
    let rebased = OffsetBlockDevice::new(dev, range.offset, range.length);
    let mut fs = FsView::open(rebased).unwrap();

    // Same assertions as the M4a FsView fixture test: root listing exists
    // and a known file reads back its 4096 'A' bytes.
    let root = fs.read_dir("/").unwrap();
    assert!(!root.is_empty(), "root dir must list entries");
    let attr = fs.getattr("/dir00/f000.bin").unwrap().unwrap();
    assert_eq!(attr.size, 4096);
    assert!(!attr.is_dir);
    let data = fs.read("/dir00/f000.bin", 0, 4096).unwrap();
    assert_eq!(data.len(), 4096);
    assert!(data.iter().all(|&b| b == b'A'));
}
