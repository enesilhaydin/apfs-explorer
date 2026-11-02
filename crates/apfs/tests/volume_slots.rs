//! Volume indices in a container one of whose volumes was deleted.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: hdiutil + diskutil).
//!
//! macOS puts three volumes in a container and deletes the middle one. It
//! leaves that volume's slot in the container's volume array empty and the
//! third volume in slot 2 (`diskutil` keeps calling it `s3`). Selecting a
//! volume by index has to follow the slots, or index 1 opens the third
//! volume and index 2 opens nothing.
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
mod volume_slots {
    use apfs::{FsError, FsView, VolumeSelector};
    use apfs_core::block_device::{FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use std::io::Read;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-volume-slots.img";
    /// A container holds one volume per 512 MiB of its size; three need
    /// more than a gigabyte.
    const IMAGE_SIZE: &str = "1600m";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const MARKER: &str = "marker.txt";

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn diskutil(args: &[&str]) {
        let out = Command::new("diskutil")
            .args(args)
            .output()
            .expect("diskutil");
        assert!(
            out.status.success(),
            "diskutil {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The device node of the mounted volume named `name`.
    fn volume_node(name: &str) -> String {
        let out = Command::new("diskutil")
            .args(["info", &format!("/Volumes/{name}")])
            .output()
            .expect("diskutil info");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| {
                l.trim()
                    .strip_prefix("Device Node:")
                    .map(|v| v.trim().to_string())
            })
            .unwrap_or_else(|| panic!("no device node for volume {name}"))
    }

    /// Three volumes, each with a marker file naming it, then the middle one
    /// deleted by macOS.
    fn build_image() {
        let _ = std::fs::remove_file(IMG);
        let _ = std::fs::remove_file("/tmp/apfs-volume-slots.dmg");
        let out = Command::new("hdiutil")
            .args([
                "create",
                "-size",
                IMAGE_SIZE,
                "-layout",
                "GPTSPUD",
                "-fs",
                "APFS",
                "-volname",
                "SLOTA",
                "/tmp/apfs-volume-slots",
            ])
            .output()
            .expect("hdiutil create");
        assert!(out.status.success(), "hdiutil create failed");
        std::fs::rename("/tmp/apfs-volume-slots.dmg", IMG).expect("rename");

        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let parent = stdout
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .filter_map(|d| d.strip_prefix("/dev/disk"))
            .filter(|n| !n.contains('s'))
            .filter_map(|n| n.parse::<u32>().ok())
            .min()
            .map(|n| format!("/dev/disk{n}"))
            .expect("no whole-disk node");
        let first = volume_node("SLOTA");
        let container = first
            .rsplit_once('s')
            .map(|(c, _)| c.to_string())
            .expect("volume node names its container");
        for name in ["SLOTB", "SLOTC"] {
            diskutil(&["apfs", "addVolume", &container, "APFS", name]);
        }
        for name in ["SLOTA", "SLOTB", "SLOTC"] {
            std::fs::write(format!("/Volumes/{name}/{MARKER}"), name).expect("write marker");
        }
        let _ = Command::new("sync").status();
        let middle = volume_node("SLOTB");
        diskutil(&["apfs", "deleteVolume", &middle]);
        let _ = Command::new("sync").status();
        let st = Command::new("hdiutil")
            .args(["detach", &parent])
            .status()
            .expect("hdiutil detach");
        assert!(st.success(), "hdiutil detach failed");
    }

    fn open_dev() -> WritableOffsetBlockDevice<FileBlockDeviceRw> {
        let mut head = vec![0u8; 64 * 1024];
        std::fs::File::open(IMG)
            .unwrap()
            .read_exact(&mut head)
            .unwrap();
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
                let inner = FileBlockDeviceRw::open_rw(IMG).unwrap();
                return WritableOffsetBlockDevice::new(inner, s * SECTOR, (en - s + 1) * SECTOR);
            }
        }
        panic!("no APFS partition");
    }

    fn marker_at(index: usize) -> Result<Vec<u8>, FsError> {
        let mut fs = FsView::open_selected(open_dev(), VolumeSelector::Index(index))?;
        fs.read(&format!("/{MARKER}"), 0, 16)
    }

    #[test]
    #[ignore = "macOS-gated; needs hdiutil and diskutil"]
    fn a_volume_after_a_deleted_one_keeps_its_index() {
        if !gate() {
            return;
        }
        build_image();

        let mut dev = open_dev();
        let c = Container::open(&mut dev).expect("container");
        let listed: Vec<(u32, String)> = c
            .list_volumes(&mut dev)
            .expect("list volumes")
            .into_iter()
            .map(|v| (v.index, v.name))
            .collect();
        assert_eq!(
            listed,
            vec![(0, "SLOTA".to_string()), (2, "SLOTC".to_string())],
            "each volume is listed at its slot"
        );

        assert_eq!(marker_at(0).expect("slot 0"), b"SLOTA");
        assert_eq!(marker_at(2).expect("slot 2"), b"SLOTC");
        assert!(
            matches!(marker_at(1), Err(FsError::NoVolume)),
            "the emptied slot selects no volume"
        );
        let _ = std::fs::remove_file(IMG);
    }
}
