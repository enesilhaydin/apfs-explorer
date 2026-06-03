//! Write-enable Faz 2: differential oracle.
//!
//! We write files via OUR stack, then read them back through **macOS's own APFS
//! driver** (the independent authority, the APFS spec) via `xtask diff-scratch`, and
//! assert the content matches byte-for-byte (sha256 + size). fsck proves the
//! tree is structurally valid; THIS proves the file CONTENT is correct - the
//! third-party-implementation guarantee that bytes we wrote are the bytes Apple
//! reads back.
//!
//! Gate: `APFS_WRITE_SCRATCH=1`. `#[ignore]` - shells out to hdiutil + fsck.
//!
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --features write \
//!     --test differential_macos -- --ignored --nocapture --test-threads=1
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
mod diff {
    use apfs_core::block_device::BlockDevice;
    use apfs_core::block_device::{FileBlockDeviceRw, WritableOffsetBlockDevice};
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_core::volume::VolumeSuperblock;
    use apfs_write::file::write_file;
    use apfs_write::txn::Transaction;
    use std::collections::HashMap;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-diff-oracle.img";
    const SIZE_MB: u32 = 96;
    const SECTOR: u64 = 512;

    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    fn gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }
    fn root() -> &'static str {
        concat!(env!("CARGO_MANIFEST_DIR"), "/../..")
    }
    fn xtask(args: &[&str]) -> std::process::Output {
        let mut a = vec!["run", "--quiet", "-p", "xtask", "--"];
        a.extend_from_slice(args);
        Command::new("cargo")
            .args(&a)
            .current_dir(root())
            .output()
            .expect("xtask")
    }
    fn gen_scratch() {
        let s = xtask(&["gen-scratch", IMG, &SIZE_MB.to_string()]);
        assert!(s.status.success(), "gen-scratch failed");
    }
    fn fsck_clean() -> bool {
        xtask(&["verify-scratch", IMG]).status.success()
    }
    /// Run `xtask diff-scratch` (macOS read-back) → map relpath → (size, sha256).
    fn macos_manifest() -> HashMap<String, (u64, String)> {
        let o = xtask(&["diff-scratch", IMG]);
        assert!(
            o.status.success(),
            "diff-scratch failed: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        let mut m = HashMap::new();
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let cols: Vec<&str> = line.split('\t').collect();
            if cols.len() == 3 {
                if let Ok(sz) = cols[1].parse::<u64>() {
                    m.insert(cols[0].to_string(), (sz, cols[2].to_string()));
                }
            }
        }
        m
    }
    /// sha256 of `content` via the SAME `shasum` tool the oracle uses.
    fn sha256(content: &[u8]) -> String {
        let tmp = format!("/tmp/apfs-diff-exp-{}", content.len());
        std::fs::write(&tmp, content).unwrap();
        let o = Command::new("shasum")
            .args(["-a", "256", &tmp])
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&tmp);
        String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .next()
            .unwrap_or("?")
            .to_string()
    }

    fn find_apfs_range(raw: &[u8]) -> (u64, u64) {
        let hdr = &raw[512..1024];
        let pe = u64::from_le_bytes(hdr[72..80].try_into().unwrap());
        let np = u32::from_le_bytes(hdr[80..84].try_into().unwrap()) as usize;
        let ps = u32::from_le_bytes(hdr[84..88].try_into().unwrap()) as usize;
        let eo = (pe * SECTOR) as usize;
        for i in 0..np {
            let off = eo + i * ps;
            let e = match raw.get(off..off + ps) {
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
    fn read_vsb_omap(img: &str) -> (Vec<u8>, Vec<u8>) {
        let raw = std::fs::read(img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).unwrap();
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);
        let c = Container::open(&mut dev).unwrap();
        let nx = &c.superblock;
        let bsz = nx.block_size as usize;
        let xid = nx.obj.xid;
        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).unwrap();
        let fs_oid = *nx.fs_oids.first().unwrap();
        let vp = omap
            .resolve(&mut dev, fs_oid, xid, nx.block_size)
            .unwrap()
            .unwrap();
        let mut vsb = vec![0u8; bsz];
        dev.read_at(vp * bsz as u64, &mut vsb).unwrap();
        let v = VolumeSuperblock::parse(&vsb).unwrap();
        let mut om = vec![0u8; bsz];
        dev.read_at(v.omap_oid * bsz as u64, &mut om).unwrap();
        (vsb, om)
    }
    fn open_txn(img: &str) -> Transaction<WritableOffsetBlockDevice<FileBlockDeviceRw>> {
        let raw = std::fs::read(img).unwrap();
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(img).unwrap();
        Transaction::begin(WritableOffsetBlockDevice::new(inner, base, len)).unwrap()
    }

    #[test]
    #[ignore = "fsck + macOS-mount differential; slow"]
    fn macos_reads_back_what_we_wrote() {
        if !gate() {
            println!("diff-oracle: APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        gen_scratch();

        // Deterministic content: single-block text, an exact-block file, and a
        // multi-block (multi-extent) binary, to exercise the extent layout.
        let files: Vec<(&str, Vec<u8>)> = vec![
            ("small.txt", b"hello apfs differential oracle\n".to_vec()),
            ("exact_block.bin", vec![0xA5u8; 4096]),
            (
                "multi.bin",
                (0..12_288u32)
                    .map(|i| (i.wrapping_mul(31) & 0xff) as u8)
                    .collect(),
            ),
        ];

        for (name, content) in &files {
            let (vsb, om) = read_vsb_omap(IMG);
            let mut txn = open_txn(IMG);
            write_file(&mut txn, &vsb, &om, 2, name, content)
                .unwrap_or_else(|e| panic!("write_file {name}: {e}"));
            txn.commit()
                .unwrap_or_else(|e| panic!("commit {name}: {e}"));
        }

        assert!(fsck_clean(), "fsck must be clean after writes");

        let manifest = macos_manifest();
        println!("[diff] macOS read-back manifest: {manifest:?}");
        for (name, content) in &files {
            let (size, hash) = manifest
                .get(*name)
                .unwrap_or_else(|| panic!("macOS did not read back {name}"));
            assert_eq!(*size, content.len() as u64, "size mismatch for {name}");
            assert_eq!(*hash, sha256(content), "CONTENT mismatch for {name}");
            println!("[diff] {name}: size={size} sha256 OK (macOS == ours)");
        }
    }
}
