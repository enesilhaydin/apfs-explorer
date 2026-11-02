//! Shrinking a file to a size that is not a block boundary.
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS: xtask gen-scratch + fsck).
//!
//! A file cut to 100 bytes has 100 bytes. Rounding the recorded size up to the
//! block that holds the cut leaves the file claiming bytes nobody wrote, and
//! every reader returns them.
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
mod truncate_size {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::{truncate_file_fast, write_file, TruncateOutcome};
    use apfs_write::txn::Transaction;
    use std::process::Command;

    type Dev = WritableOffsetBlockDevice<FileBlockDeviceRw>;
    const IMG: &str = "/tmp/apfs-truncate-size.img";
    const SECTOR: u64 = 512;
    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const NAME: &str = "trunc.bin";

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn gen_scratch() {
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
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
            .current_dir(manifest)
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

    /// Cutting inside a block: the recorded size has to be the cut, whatever
    /// the fast path decides to do about the blocks.
    #[test]
    #[ignore = "scratch-image gated"]
    fn a_cut_inside_a_block_keeps_the_requested_size() {
        if !gate() {
            println!("truncate-size: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        let body: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            write_file(&mut txn, &vsb, &om, 2, NAME, &body).expect("write");
            txn.commit().expect("commit write");
        }

        let outcome = {
            let (vsb, om) = read_vsb_omap();
            let mut txn = open_txn();
            let o = truncate_file_fast(&mut txn, &vsb, &om, 2, NAME, 100, body.len() as u64)
                .expect("truncate");
            if matches!(o, TruncateOutcome::Applied) {
                txn.commit().expect("commit truncate");
            } else {
                let _ = txn.abort();
            }
            o
        };
        println!("truncate-size: fast path said {outcome:?}");

        // Whatever it decided, if it says it applied the cut then the cut is
        // what the file must report - and read back.
        if matches!(outcome, TruncateOutcome::Applied) {
            let mut fs = open_fs();
            let path = format!("/{NAME}");
            let attr = fs.getattr(&path).expect("getattr").expect("file");
            assert_eq!(
                attr.size, 100,
                "a file cut to 100 bytes reports 100, not the block that holds the cut"
            );
            let got = fs.read(&path, 0, 100).expect("read");
            assert_eq!(
                got,
                &body[..100],
                "the surviving bytes must be the first 100"
            );
        }
    }
}
