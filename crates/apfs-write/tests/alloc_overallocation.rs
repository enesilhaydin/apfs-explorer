//! #136 regression: a single `create_file` + `commit` on a fresh scratch
//! volume must leave the container fsck-CLEAN (no allocator overallocation,
//! no leaked blocks).
//!
//! Gate: `APFS_WRITE_SCRATCH=1` (requires macOS + hdiutil + fsck_apfs).
//! Without that env var the test is a no-op pass.
//!
//! Invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release \
//!     --test alloc_overallocation -- --nocapture --test-threads=1
//!
//! The empirical arbiter is `xtask verify-scratch`, which attaches the image
//! and runs `fsck_apfs -n` on the synthesized container device, treating any
//! "overallocation" / "alloc_count" warning as DIRTY (the APFS spec).
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
mod alloc_oa_tests {
    use apfs_core::block_device::{BlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::create_file;
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-oa-repro.img";
    const SIZE_MB: u32 = 64;

    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    const SECTOR: u64 = 512;

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn manifest_root() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }

    fn gen_scratch() {
        let status = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                IMG,
                &SIZE_MB.to_string(),
            ])
            .current_dir(manifest_root())
            .status()
            .expect("cargo run xtask gen-scratch");
        assert!(status.success(), "xtask gen-scratch failed");
    }

    /// Run `xtask verify-scratch` and return combined stdout+stderr.
    fn verify_scratch() -> (bool, String) {
        let out = Command::new("cargo")
            .args(["run", "--quiet", "-p", "xtask", "--", "verify-scratch", IMG])
            .current_dir(manifest_root())
            .output()
            .expect("cargo run xtask verify-scratch");
        let mut s = String::new();
        s.push_str(&String::from_utf8_lossy(&out.stdout));
        s.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), s)
    }

    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let mbr = &raw[..512];
        assert_eq!(&mbr[510..512], &[0x55u8, 0xAA], "no MBR signature");
        let hdr = &raw[512..1024];
        assert_eq!(&hdr[..8], b"EFI PART", "no GPT signature");
        let part_entry_lba = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let num_parts = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let part_entry_size = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let entries_off = (part_entry_lba * SECTOR) as usize;
        for i in 0..num_parts {
            let off = entries_off + i * part_entry_size;
            let entry = match raw.get(off..off + part_entry_size) {
                Some(e) => e,
                None => break,
            };
            if entry.get(..16) == Some(&APFS_GPT_TYPE[..]) {
                let start_lba = u64::from_le_bytes(entry[32..40].try_into().unwrap());
                let end_lba = u64::from_le_bytes(entry[40..48].try_into().unwrap());
                return (start_lba * SECTOR, (end_lba - start_lba + 1) * SECTOR);
            }
        }
        panic!("no APFS partition in GPT");
    }

    fn read_vsb_omap_raw(img: &str) -> (Vec<u8>, Vec<u8>) {
        let raw_img = std::fs::read(img).expect("read img");
        let (base, len) = find_apfs_range(&raw_img);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw");
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let container = Container::open(&mut dev).expect("Container::open");
        let nx = &container.superblock;
        let bsz = nx.block_size as usize;
        let xid = nx.obj.xid;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).expect("container omap");
        let fs_oid = *nx.fs_oids.first().expect("no fs_oids");
        let vol_paddr = omap
            .resolve(&mut dev, fs_oid, xid, nx.block_size)
            .expect("omap resolve")
            .expect("volume not found");
        let mut vsb_raw = vec![0u8; bsz];
        dev.read_at(vol_paddr * bsz as u64, &mut vsb_raw)
            .expect("read vsb");
        let vsb = VolumeSuperblock::parse(&vsb_raw).expect("parse vsb");
        let mut omap_raw = vec![0u8; bsz];
        dev.read_at(vsb.omap_oid * bsz as u64, &mut omap_raw)
            .expect("read vol omap");
        (vsb_raw, omap_raw)
    }

    fn open_txn(img: &str) -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(img).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open_rw");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        Transaction::begin(dev).expect("Transaction::begin")
    }

    /// One empty `create_file` must keep the container fsck-CLEAN.
    #[test]
    fn create_file_leaves_no_overallocation() {
        if !check_gate() {
            println!("alloc-oa: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // Baseline: a freshly-formatted scratch must verify clean.
        let (base_ok, base_out) = verify_scratch();
        println!("===== BASELINE verify-scratch (ok={base_ok}) =====\n{base_out}");
        assert!(
            base_ok,
            "freshly-generated scratch is not clean:\n{base_out}"
        );

        // One create_file + commit.
        {
            let (vsb, omap) = read_vsb_omap_raw(IMG);
            let mut txn = open_txn(IMG);
            create_file(&mut txn, &vsb, &omap, 2, "oa_probe.txt").expect("create_file");
            txn.commit().expect("commit create");
        }

        // After: must still verify clean (RED until #136 is fixed).
        let (after_ok, after_out) = verify_scratch();
        println!("===== AFTER create_file verify-scratch (ok={after_ok}) =====\n{after_out}");
        assert!(
            after_ok,
            "create_file introduced an allocator fault (overallocation / leaked blocks):\n{after_out}"
        );
    }
}
