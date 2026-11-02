//! List a directory of the APFS volume in a disk image, with sizes.
//!
//! `cargo run -p apfs --example list_dir -- disk.img /Users`
//!
//! The image may be a whole disk (a GPT partition table holding an APFS
//! container, as `hdiutil` or `dd` of a drive produce) or the container
//! alone. Nothing is written: the device is opened read-write only because
//! the offset wrapper takes a writable device.

use apfs::FsView;
use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
use apfs_core::gpt::find_apfs_container;

/// GPT logical block size of disk images.
const LBA_SIZE: u64 = 512;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let image = args.next().ok_or("usage: list_dir <image> [path]")?;
    let path = args.next().unwrap_or_else(|| "/".to_string());

    let mut disk = FileBlockDeviceRw::open_rw(&image)?;
    let (offset, length) = match find_apfs_container(&mut disk, LBA_SIZE) {
        Ok(range) => (range.offset, range.length),
        // No partition table: the image is the container itself.
        Err(_) => (0, disk.size()),
    };
    let mut fs = FsView::open(WritableOffsetBlockDevice::new(disk, offset, length))?;

    for entry in fs.read_dir(&path)? {
        let child = format!("{}/{}", path.trim_end_matches('/'), entry.name);
        match fs.getattr(&child)? {
            Some(a) if a.is_dir => println!("{:>12}  {}/", "-", entry.name),
            Some(a) => println!("{:>12}  {}", a.size, entry.name),
            None => println!("{:>12}  {}", "?", entry.name),
        }
    }
    Ok(())
}
