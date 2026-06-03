//! COW-1 regression test: the transaction commit MUST issue a durability
//! barrier (`flush_data`) both before and after writing the NX superblock.
//!
//! Without the barrier, an unclean stop (power loss / process kill / USB yank)
//! can let the NXSB - the checkpoint commit point - reach the platter before
//! the blocks it references, leaving the container empty or corrupt. This is
//! the data-integrity fix verified in `docs/refs/di-tier-verification-2026-05.md`
//! against linux-apfs-rw `apfs_checkpoint_end` (filemap_write_and_wait brackets
//! the superblock write).
//!
//! Gated on `APFS_WRITE_SCRATCH=1` like the other write integration tests
//! (needs a real macOS-formatted scratch image via `xtask gen-scratch`).

#![cfg(feature = "write")]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod cow1 {
    use apfs_core::block_device::{
        BlockDevice, BlockError, FileBlockDeviceRw, WritableBlockDevice, WritableOffsetBlockDevice,
    };
    use apfs_write::txn::Transaction;
    use std::process::Command;

    const IMG: &str = "/tmp/apfs-cow1-flush.img";
    const SIZE_MB: u32 = 64;
    const SECTOR: u64 = 512;

    #[rustfmt::skip]
    const APFS_GPT_TYPE: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
        0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];

    fn check_gate() -> bool {
        std::env::var("APFS_WRITE_SCRATCH").as_deref() == Ok("1")
    }

    fn gen_scratch() {
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
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
            .current_dir(manifest)
            .status()
            .expect("cargo run xtask gen-scratch");
        assert!(status.success(), "xtask gen-scratch failed");
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

    /// Ordered device event for commit barrier assertions.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ev {
        Write { block: u64 },
        Flush,
    }

    /// Records every `write_at` (block-relative) and `flush_data` so the test
    /// can assert the COW-1 ordering. Delegates all I/O to the inner device.
    struct RecordingDev<D: WritableBlockDevice> {
        inner: D,
        events: Vec<Ev>,
    }

    impl<D: WritableBlockDevice> RecordingDev<D> {
        fn new(inner: D) -> Self {
            Self {
                inner,
                events: Vec::new(),
            }
        }
    }

    impl<D: WritableBlockDevice> BlockDevice for RecordingDev<D> {
        fn size(&self) -> u64 {
            self.inner.size()
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            self.inner.read_at(offset, buf)
        }
    }

    impl<D: WritableBlockDevice> WritableBlockDevice for RecordingDev<D> {
        fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
            let r = self.inner.write_at(offset, buf);
            if r.is_ok() {
                self.events.push(Ev::Write {
                    block: offset / 4096,
                });
            }
            r
        }
        fn flush_data(&mut self) -> Result<(), BlockError> {
            let r = self.inner.flush_data();
            if r.is_ok() {
                self.events.push(Ev::Flush);
            }
            r
        }
        fn invalidate_read_cache(&mut self) {
            self.inner.invalidate_read_cache();
        }
    }

    /// The NX superblock is written to container-relative block 0 (and to a ring
    /// slot). Block 0 is the unambiguous commit-point marker.
    #[test]
    fn cow1_commit_flushes_around_nxsb_write() {
        if !check_gate() {
            eprintln!("SKIP cow1_commit_flushes_around_nxsb_write (set APFS_WRITE_SCRATCH=1)");
            return;
        }
        gen_scratch();

        let raw = std::fs::read(IMG).expect("read img");
        let (base, len) = find_apfs_range(&raw);
        let inner = FileBlockDeviceRw::open_rw(IMG).expect("open_rw");
        let offset_dev = WritableOffsetBlockDevice::new(inner, base, len);
        let rec = RecordingDev::new(offset_dev);

        // A bare begin -> commit still writes spaceman + checkpoint map + NXSB,
        // so it fully exercises both COW-1 barriers without any file mutation.
        let txn = Transaction::begin(rec).expect("Transaction::begin");
        let rec_back = txn.commit().expect("commit");
        let ev = rec_back.events;

        let total_flushes = ev.iter().filter(|e| **e == Ev::Flush).count();
        let first_nxsb = ev
            .iter()
            .position(|e| *e == Ev::Write { block: 0 })
            .expect("commit must write the NX superblock to block 0");

        // Barrier #1: at least one flush BEFORE the NXSB write - every block the
        // new checkpoint references is durable before the commit point.
        assert!(
            ev[..first_nxsb].iter().any(|e| *e == Ev::Flush),
            "COW-1: no flush_data before the NXSB write (block 0). Events: {ev:?}"
        );

        // Barrier #2: at least one flush AFTER the NXSB write - the commit point
        // is durable before Phase B reclaim clears any allocation bits.
        assert!(
            ev[first_nxsb + 1..].iter().any(|e| *e == Ev::Flush),
            "COW-1: no flush_data after the NXSB write (block 0). Events: {ev:?}"
        );

        assert!(
            total_flushes >= 2,
            "COW-1: expected >= 2 flush barriers per commit, got {total_flushes}. Events: {ev:?}"
        );

        println!(
            "COW-1 PASS: {total_flushes} flush barriers bracket the NXSB write ({} total writes)",
            ev.iter().filter(|e| matches!(e, Ev::Write { .. })).count()
        );
    }
}
