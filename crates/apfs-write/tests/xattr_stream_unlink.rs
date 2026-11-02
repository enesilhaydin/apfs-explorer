//! Removing a file whose extended attribute lives in its own stream.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! An attribute value too large to sit inline is stored as a stream of its
//! own, keyed by an id that is neither the file's nor its content stream's.
//! Removing the file has to remove that stream and release its blocks too;
//! gathering only what is keyed by the file's id leaves the attribute's
//! stream and blocks behind, unreachable but allocated.
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
mod xattr_stream_unlink {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{set_xattr, unlink, write_file};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-xattr-stream-unlink.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const NAME: &str = "with_large_attribute.bin";
    const XATTR: &str = "user.large";

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn gen_scratch() {
        let _ = std::fs::remove_file(IMG);
        let s = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                IMG,
                "64",
            ])
            .current_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .status()
            .expect("gen-scratch");
        assert!(s.success(), "gen-scratch failed");
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

    /// fsck_apfs exits 0 on damage it considers repairable; a leaked stream is
    /// exactly that, so the warnings have to count.
    fn run_fsck(label: &str) {
        let out = Command::new("hdiutil")
            .args([
                "attach",
                "-imagekey",
                "diskimage-class=CRawDiskImage",
                "-nobrowse",
                "-nomount",
                IMG,
            ])
            .output()
            .expect("hdiutil attach");
        assert!(out.status.success(), "hdiutil attach failed");
        let parent = String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .find(|d| {
                d.starts_with("/dev/disk") && !d.trim_start_matches("/dev/disk").contains('s')
            })
            .expect("no whole-disk node")
            .to_string();
        let rdev = format!("{parent}s1").replacen("/dev/disk", "/dev/rdisk", 1);
        let fsck = Command::new("fsck_apfs").args(["-n", &rdev]).output();
        let _ = Command::new("hdiutil").args(["detach", &parent]).status();
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

    #[test]
    #[ignore = "scratch-image gated"]
    fn removing_a_file_takes_its_stream_attribute_with_it() {
        if !gate() {
            println!("xattr-stream-unlink: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let content = vec![0x3Cu8; 8 * 1024];
        let value: Vec<u8> = (0..100 * 1024).map(|i| (i % 199) as u8).collect();
        let ino = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let ino = write_file(&mut txn, &vsb, &om, 2, NAME, &content).expect("write");
            txn.commit().expect("commit write");
            ino
        };
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            set_xattr(&mut txn, &vsb, &om, ino, XATTR, &value).expect("set xattr");
            txn.commit().expect("commit xattr");
        }
        run_fsck("with the attribute in place");
        {
            let mut fs = open_fs();
            let names = fs.list_xattrs(&format!("/{NAME}")).expect("list xattrs");
            assert!(
                names.iter().any(|n| n == XATTR),
                "the attribute must be there: {names:?}"
            );
        }

        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            unlink(&mut txn, &vsb, &om, 2, NAME).expect("unlink");
            txn.commit().expect("commit unlink");
        }
        {
            let mut fs = open_fs();
            assert!(
                fs.lookup(&format!("/{NAME}")).expect("lookup").is_none(),
                "the file must be gone"
            );
        }
        run_fsck("after removing the file");
        println!("xattr-stream-unlink PASS: the attribute's stream went with the file");
    }
}
