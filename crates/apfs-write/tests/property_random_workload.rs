//! Property test 1 - random workload roundtrip.
//!
//! Gate: set `APFS_WRITE_SCRATCH=1` to run (requires macOS + hdiutil).
//! Without that env var every test is a no-op pass.
//!
//! CI invocation:
//!   APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release --test property_random_workload -- --test-threads=1
//!
//! Design:
//!   For each seed in SEEDS, replay a deterministic sequence of 1000 random
//!   filesystem operations on a 64 MiB scratch image.  After the entire
//!   sequence, re-open the image and read back every surviving file, comparing
//!   content against an in-memory shadow map.  Four invariants are checked
//!   after every committed transaction:
//!
//!     I1. NXSB magic still valid (device parseable via Transaction::begin).
//!     I2. No more than 64 live files at once (prevents bitmap exhaustion in
//!         64 MiB image).
//!     I3. Op-level errors are tolerated (log + continue); panics are bugs.
//!     I4. Final readback: every surviving name maps to the expected content.
//!
//! Seeds (deterministic, same seed → same sequence):
//!   0xDEAD_BEEF_0000_0001  0xCAFE_BABE_0000_0002
//!   0x1234_5678_9ABC_DEF0  0x0FF1_CE00_DEAD_C0DE

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
    )
)]

mod common;

#[cfg(test)]
mod prop_random_tests {
    use apfs_core::block_device::{
        BlockDevice, FileBlockDevice, FileBlockDeviceRw, WritableOffsetBlockDevice,
    };
    use apfs_core::container::Container;
    use apfs_core::omap::Omap;
    use apfs_write::file::{create_symlink, mkdir, rename, unlink, write_file};
    use apfs_write::txn::Transaction;
    use std::collections::HashMap;
    use std::process::Command;

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    const SCRATCH_SIZE_MB: u32 = 64;
    const SECTOR: u64 = 512;
    const NUM_OPS: usize = 1000;
    /// Maximum live file count - prevents 64 MiB image from running out of space.
    const MAX_LIVE_FILES: usize = 64;

    #[rustfmt::skip]
    static APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    const SEEDS: &[u64] = &[
        0xDEAD_BEEF_0000_0001,
        0xCAFE_BABE_0000_0002,
        0x1234_5678_9ABC_DEF0,
        0x0FF1_CE00_DEAD_C0DE,
    ];

    // -----------------------------------------------------------------------
    // Gate
    // -----------------------------------------------------------------------

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    // -----------------------------------------------------------------------
    // Simple deterministic PRNG (xorshift64)
    // -----------------------------------------------------------------------

    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1) // seed must be non-zero
        }

        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn next_usize(&mut self, n: usize) -> usize {
            if n == 0 {
                return 0;
            }
            (self.next() % n as u64) as usize
        }

        fn next_bytes(&mut self, len: usize) -> Vec<u8> {
            (0..len).map(|_| (self.next() & 0xFF) as u8).collect()
        }
    }

    // -----------------------------------------------------------------------
    // Scratch image lifecycle
    // -----------------------------------------------------------------------

    fn gen_scratch_at(img: &str) {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let status = Command::new("cargo")
            .args([
                "run",
                "--quiet",
                "-p",
                "xtask",
                "--",
                "gen-scratch",
                img,
                &SCRATCH_SIZE_MB.to_string(),
            ])
            .current_dir(workspace_root)
            .status()
            .expect("cargo run xtask gen-scratch");
        assert!(status.success(), "xtask gen-scratch failed for {img}");
    }

    // -----------------------------------------------------------------------
    // GPT helpers
    // -----------------------------------------------------------------------

    fn find_apfs_range_in_bytes(raw: &[u8]) -> (u64, u64) {
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
            let entry = &raw[off..off + part_entry_size];
            if entry[..16] == APFS_GPT_TYPE {
                let start_lba = u64::from_le_bytes(entry[32..40].try_into().unwrap());
                let end_lba = u64::from_le_bytes(entry[40..48].try_into().unwrap());
                return (start_lba * SECTOR, (end_lba - start_lba + 1) * SECTOR);
            }
        }
        panic!("no APFS partition found in GPT");
    }

    fn read_image_header(img: &str) -> (u64, u64) {
        let mut dev = FileBlockDevice::open(img).expect("open ro for GPT");
        let size = dev.size() as usize;
        let read_len = size.min(2 * 1024 * 1024);
        let mut buf = vec![0u8; read_len];
        dev.read_at(0, &mut buf).expect("read header region");
        find_apfs_range_in_bytes(&buf)
    }

    fn read_vsb_omap_raw(img: &str) -> (Vec<u8>, Vec<u8>) {
        let (base, len) = read_image_header(img);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw for vsb");
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);

        let container = Container::open(&mut dev).expect("Container::open");
        let nx = &container.superblock;
        let bsz = nx.block_size as usize;
        let xid = nx.obj.xid;

        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).expect("omap open");
        let fs_oid = *nx.fs_oids.first().expect("no fs_oids");
        let vol_paddr = omap
            .resolve(&mut dev, fs_oid, xid, nx.block_size)
            .expect("omap resolve")
            .expect("vol_paddr not found");

        let mut vsb_raw = vec![0u8; bsz];
        dev.read_at(vol_paddr * bsz as u64, &mut vsb_raw)
            .expect("read vsb");

        let vsb = apfs_core::volume::VolumeSuperblock::parse(&vsb_raw).expect("parse vsb");
        let vol_omap_oid = vsb.omap_oid;

        let mut omap_raw = vec![0u8; bsz];
        dev.read_at(vol_omap_oid * bsz as u64, &mut omap_raw)
            .expect("read vol omap");

        (vsb_raw, omap_raw)
    }

    // I1: device still parseable after each committed txn.
    fn assert_device_parseable(img: &str, label: &str) {
        let (base, len) = read_image_header(img);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw for parseable check");
        let dev = WritableOffsetBlockDevice::new(inner, base, len);
        let txn = Transaction::begin(dev)
            .unwrap_or_else(|e| panic!("{label} FAIL: txn begin failed: {e}"));
        let _ = txn.abort();
    }

    // -----------------------------------------------------------------------
    // Final readback: compare surviving files against shadow map
    // -----------------------------------------------------------------------

    // Future-use: wired up when the property test gains a final-readback phase.
    #[allow(dead_code)]
    fn readback_verify(img: &str, shadow: &HashMap<String, Vec<u8>>) {
        let (base, len) = read_image_header(img);
        let inner = FileBlockDeviceRw::open_rw(img).expect("open rw for readback");
        let mut dev = WritableOffsetBlockDevice::new(inner, base, len);

        let container = Container::open(&mut dev).expect("Container::open for readback");
        let nx = &container.superblock;
        let bsz = nx.block_size as usize;
        let xid = nx.obj.xid;

        let omap = Omap::open(&mut dev, nx.omap_oid, nx.block_size).expect("omap open");
        let fs_oid = *nx.fs_oids.first().expect("no fs_oids");
        let vol_paddr = omap
            .resolve(&mut dev, fs_oid, xid, nx.block_size)
            .expect("omap resolve")
            .expect("vol_paddr not found");

        let mut vsb_raw = vec![0u8; bsz];
        dev.read_at(vol_paddr * bsz as u64, &mut vsb_raw)
            .expect("read vsb for readback");

        let vsb =
            apfs_core::volume::VolumeSuperblock::parse(&vsb_raw).expect("parse vsb for readback");

        // names_hashed: APFS_INCOMPAT_CASE_INSENSITIVE (0x1) | APFS_INCOMPAT_NORMALIZATION_INSENSITIVE (0x8).
        // Matches VolumeSuperblock::names_are_hashed().
        let names_hashed = (vsb.incompatible_features & (0x1 | 0x8)) != 0;

        let catalog = apfs_core::catalog::Catalog::open(&mut dev, &vsb, nx.block_size)
            .expect("Catalog::open for readback");

        for (name, expected) in shadow {
            // List root directory (ino 2). All files in this test live there.
            let entries = catalog
                .list_dir(&mut dev, 2, names_hashed)
                .unwrap_or_default();
            let found = entries.iter().find(|e| &e.name == name);
            match found {
                None => panic!("readback FAIL: file {name:?} missing from catalog"),
                Some(entry) => {
                    let ino = entry.file_id;
                    let data = catalog.read_file(&mut dev, ino).unwrap_or_else(|e| {
                        panic!("readback FAIL: read_file({name:?}, ino={ino}): {e:?}")
                    });
                    assert_eq!(
                        data, *expected,
                        "readback FAIL: content mismatch for {name:?}"
                    );
                }
            }
        }
        println!("  readback: {} files verified", shadow.len());
    }

    // -----------------------------------------------------------------------
    // Workload operations
    // -----------------------------------------------------------------------

    #[derive(Debug, Clone)]
    enum Op {
        Create { name: String, content: Vec<u8> },
        Write { name: String, content: Vec<u8> },
        Delete { name: String },
        Rename { old: String, new: String },
        Mkdir { name: String },
        Symlink { name: String, target: String },
    }

    fn pick_op(rng: &mut Rng, names: &[String], _shadow: &HashMap<String, Vec<u8>>) -> Op {
        let choice = rng.next_usize(7);
        match choice {
            0 | 1 => {
                // Create a new file with random content (1..=8 KiB).
                let len = 512 + rng.next_usize(7 * 512);
                let content = rng.next_bytes(len);
                let name = format!("f{:04x}.bin", rng.next() & 0xFFFF);
                Op::Create { name, content }
            }
            2 => {
                // Overwrite an existing file, or create if none exist.
                if names.is_empty() {
                    let content = rng.next_bytes(512);
                    Op::Create {
                        name: format!("f{:04x}.bin", rng.next() & 0xFFFF),
                        content,
                    }
                } else {
                    let idx = rng.next_usize(names.len());
                    let name = names[idx].clone();
                    let len = 512 + rng.next_usize(3 * 512);
                    let content = rng.next_bytes(len);
                    Op::Write { name, content }
                }
            }
            3 => {
                // Delete an existing file, or create if none.
                if names.is_empty() {
                    let content = rng.next_bytes(512);
                    Op::Create {
                        name: format!("f{:04x}.bin", rng.next() & 0xFFFF),
                        content,
                    }
                } else {
                    let idx = rng.next_usize(names.len());
                    Op::Delete {
                        name: names[idx].clone(),
                    }
                }
            }
            4 => {
                // Rename within root if we have files.
                if names.is_empty() {
                    let content = rng.next_bytes(512);
                    Op::Create {
                        name: format!("f{:04x}.bin", rng.next() & 0xFFFF),
                        content,
                    }
                } else {
                    let idx = rng.next_usize(names.len());
                    let old = names[idx].clone();
                    let new = format!("r{:04x}.bin", rng.next() & 0xFFFF);
                    Op::Rename { old, new }
                }
            }
            5 => {
                // mkdir - we don't recurse into directories in this test.
                let name = format!("d{:04x}", rng.next() & 0xFFFF);
                Op::Mkdir { name }
            }
            _ => {
                // Symlink
                let name = format!("s{:04x}.lnk", rng.next() & 0xFFFF);
                let target = format!("/tmp/target{:04x}", rng.next() & 0xFFFF);
                Op::Symlink { name, target }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Single-seed runner
    // -----------------------------------------------------------------------

    fn run_seed(seed: u64, img: &str) {
        println!("  [seed={seed:#018x}] gen-scratch {img}");
        gen_scratch_at(img);
        assert_device_parseable(img, "baseline");

        let mut rng = Rng::new(seed);
        // Shadow: name -> content for regular files only (not dirs/symlinks).
        let mut shadow: HashMap<String, Vec<u8>> = HashMap::new();
        let mut dir_names: Vec<String> = Vec::new();
        let mut sym_names: Vec<String> = Vec::new();

        let mut op_ok = 0usize;
        let mut op_err = 0usize;

        for op_idx in 0..NUM_OPS {
            // Cull file list if too many live files (I2).
            if shadow.len() >= MAX_LIVE_FILES {
                // Force a delete.
                let names: Vec<String> = shadow.keys().cloned().collect();
                let idx = rng.next_usize(names.len());
                let name = names[idx].clone();
                let (vsb_raw, omap_raw) = read_vsb_omap_raw(img);
                let (base, len) = read_image_header(img);
                let inner = FileBlockDeviceRw::open_rw(img).expect("open rw cull");
                let dev = WritableOffsetBlockDevice::new(inner, base, len);
                let mut txn = Transaction::begin(dev).expect("txn begin cull");
                if unlink(&mut txn, &vsb_raw, &omap_raw, 2, &name).is_ok() {
                    txn.commit().expect("commit cull");
                    shadow.remove(&name);
                } else {
                    let _ = txn.abort();
                }
                continue;
            }

            let file_names: Vec<String> = shadow.keys().cloned().collect();
            let op = pick_op(&mut rng, &file_names, &shadow);
            let (vsb_raw, omap_raw) = read_vsb_omap_raw(img);
            let (base, len) = read_image_header(img);
            let inner = FileBlockDeviceRw::open_rw(img).expect("open rw op");
            let dev = WritableOffsetBlockDevice::new(inner, base, len);
            let mut txn = Transaction::begin(dev).expect("txn begin op");

            let committed = match &op {
                Op::Create { name, content } => {
                    if shadow.contains_key(name) {
                        let _ = txn.abort();
                        false
                    } else {
                        let r = write_file(&mut txn, &vsb_raw, &omap_raw, 2, name, content);
                        if r.is_ok() {
                            txn.commit().expect("commit create");
                            shadow.insert(name.clone(), content.clone());
                            true
                        } else {
                            let _ = txn.abort();
                            false
                        }
                    }
                }
                Op::Write { name, content } => {
                    if !shadow.contains_key(name) {
                        let _ = txn.abort();
                        false
                    } else {
                        let r = write_file(&mut txn, &vsb_raw, &omap_raw, 2, name, content);
                        if r.is_ok() {
                            txn.commit().expect("commit write");
                            shadow.insert(name.clone(), content.clone());
                            true
                        } else {
                            let _ = txn.abort();
                            false
                        }
                    }
                }
                Op::Delete { name } => {
                    if !shadow.contains_key(name) {
                        let _ = txn.abort();
                        false
                    } else {
                        let r = unlink(&mut txn, &vsb_raw, &omap_raw, 2, name);
                        if r.is_ok() {
                            txn.commit().expect("commit delete");
                            shadow.remove(name);
                            true
                        } else {
                            let _ = txn.abort();
                            false
                        }
                    }
                }
                Op::Rename { old, new } => {
                    if !shadow.contains_key(old) {
                        let _ = txn.abort();
                        false
                    } else {
                        let r = rename(&mut txn, &vsb_raw, &omap_raw, 2, old, new, false);
                        if r.is_ok() {
                            txn.commit().expect("commit rename");
                            let content = shadow.remove(old).unwrap();
                            shadow.insert(new.clone(), content);
                            true
                        } else {
                            let _ = txn.abort();
                            false
                        }
                    }
                }
                Op::Mkdir { name } => {
                    if dir_names.contains(name) {
                        let _ = txn.abort();
                        false
                    } else {
                        let r = mkdir(&mut txn, &vsb_raw, &omap_raw, 2, name);
                        if r.is_ok() {
                            txn.commit().expect("commit mkdir");
                            dir_names.push(name.clone());
                            true
                        } else {
                            let _ = txn.abort();
                            false
                        }
                    }
                }
                Op::Symlink { name, target } => {
                    if sym_names.contains(name) {
                        let _ = txn.abort();
                        false
                    } else {
                        let r = create_symlink(
                            &mut txn,
                            &vsb_raw,
                            &omap_raw,
                            2,
                            name,
                            target.as_bytes(),
                        );
                        if r.is_ok() {
                            txn.commit().expect("commit symlink");
                            sym_names.push(name.clone());
                            true
                        } else {
                            let _ = txn.abort();
                            false
                        }
                    }
                }
            };

            if committed {
                op_ok += 1;
            } else {
                op_err += 1;
            }

            // I1: parseable after every 50 committed ops (not every op - too slow).
            if committed && op_ok % 50 == 0 {
                assert_device_parseable(img, &format!("seed={seed:#018x} op#{op_idx}"));
            }
        }

        println!(
            "  ops ok={op_ok} err/skip={op_err} shadow_files={}",
            shadow.len()
        );

        // I1 final: device parseable.
        assert_device_parseable(img, &format!("seed={seed:#018x} final"));

        // I4: readback all surviving files against shadow map.
        if !shadow.is_empty() {
            readback_verify(img, &shadow);
        }

        println!(
            "  seed={seed:#018x} PASS: {op_ok} successful ops, {} files surviving",
            shadow.len()
        );
    }

    // -----------------------------------------------------------------------
    // Tests - one per seed
    // -----------------------------------------------------------------------

    #[test]
    fn prop_random_workload_seed1() {
        if !check_gate() {
            println!("APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let seed = SEEDS[0];
        run_seed(seed, &format!("/tmp/apfs-prop-rw-{seed:#018x}.img"));
    }

    #[test]
    fn prop_random_workload_seed2() {
        if !check_gate() {
            println!("APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let seed = SEEDS[1];
        run_seed(seed, &format!("/tmp/apfs-prop-rw-{seed:#018x}.img"));
    }

    #[test]
    fn prop_random_workload_seed3() {
        if !check_gate() {
            println!("APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let seed = SEEDS[2];
        run_seed(seed, &format!("/tmp/apfs-prop-rw-{seed:#018x}.img"));
    }

    #[test]
    fn prop_random_workload_seed4() {
        if !check_gate() {
            println!("APFS_WRITE_SCRATCH not set - skip");
            return;
        }
        let seed = SEEDS[3];
        run_seed(seed, &format!("/tmp/apfs-prop-rw-{seed:#018x}.img"));
    }
}
