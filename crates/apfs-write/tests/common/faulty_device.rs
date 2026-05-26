//! `FaultyBlockDevice` — test-only fault-injection wrapper for `WritableBlockDevice`.
//!
//! Wraps any `WritableBlockDevice` and injects controlled failures so the
//! chaos test matrix can verify ACID invariants without real hardware.
//!
//! All fault modes are independent and stackable (e.g. you can set both
//! `fail_after_n_writes` and `fail_on_block` simultaneously — the first
//! matching condition wins).

use apfs_core::block_device::{BlockDevice, BlockError, WritableBlockDevice};

/// A `WritableBlockDevice` wrapper that injects configurable write faults.
///
/// `D` is required to be `Debug` so that `Result<_, TxnError>` (which may
/// embed the device via `Transaction<FaultyBlockDevice<D>>`) can be unwrapped
/// in tests.  This is only a test helper so the bound is acceptable.
///
/// Production code MUST NOT depend on this type. It lives exclusively in
/// `tests/` (test-only). All `unwrap`/`expect`/`panic!` uses inside tests
/// that use this helper are intentional (test harness, not library code).
pub struct FaultyBlockDevice<D: WritableBlockDevice> {
    inner: D,

    /// Total number of `write_at` calls that have completed successfully.
    pub write_count: usize,

    /// If set, the device returns `BlockError::Io` after exactly this many
    /// successful writes. The N-th write succeeds; write N+1 fails.
    pub fail_after_n_writes: Option<usize>,

    /// If set, any `write_at` targeting a block whose `offset / 4096` equals
    /// this value returns an error (regardless of `fail_after_n_writes`).
    pub fail_on_block: Option<u64>,

    /// If `true`, the device simulates a crash on `drop` by skipping the inner
    /// device's `flush`-equivalent (i.e. the inner device is simply dropped
    /// without any explicit finalisation). This is structurally equivalent to a
    /// power loss before fsync completes; it is the default state — call
    /// `set_crash_before_flush(false)` to allow a clean close.
    ///
    /// In practice APFS `Transaction::commit` calls `write_at` for every
    /// pending block and then the device is dropped — there is no explicit
    /// `flush()` call in the trait.  This flag exists to document intent and
    /// can be inspected by tests.
    pub crash_before_flush: bool,

    /// Injected-fault log: each entry is `(write_count_at_injection, offset)`.
    /// Populated whenever a fault fires so tests can assert on exactly which
    /// write triggered the failure.
    pub fault_log: Vec<(usize, u64)>,
}

impl<D: WritableBlockDevice> FaultyBlockDevice<D> {
    /// Wrap `inner` with no faults enabled.  All writes pass through until a
    /// fault mode is configured via one of the `set_*` methods.
    pub fn new(inner: D) -> Self {
        Self {
            inner,
            write_count: 0,
            fail_after_n_writes: None,
            fail_on_block: None,
            crash_before_flush: true, // default: simulate crash-on-drop
            fault_log: Vec::new(),
        }
    }

    /// Fail on the `(n+1)`-th write (zero-indexed: after `n` successes).
    ///
    /// Setting `n = 0` means the very first write fails.
    pub fn set_fail_after_n_writes(&mut self, n: usize) {
        self.fail_after_n_writes = Some(n);
    }

    /// Fail any write whose block address (offset / 4096) equals `paddr`.
    pub fn set_fail_on_block(&mut self, paddr: u64) {
        self.fail_on_block = Some(paddr);
    }

    /// Control crash-on-drop simulation.  Pass `false` for a clean run-down.
    pub fn set_crash_before_flush(&mut self, crash: bool) {
        self.crash_before_flush = crash;
    }

    /// Clear all injected faults and the fault log. The write counter is
    /// preserved (callers may want to verify it).
    pub fn clear_faults(&mut self) {
        self.fail_after_n_writes = None;
        self.fail_on_block = None;
        self.fault_log.clear();
    }

    /// Consume the wrapper and return the inner device (for re-open /
    /// post-crash verification).  The device is returned as-is — no flush.
    pub fn into_inner(self) -> D {
        self.inner
    }
}

impl<D: WritableBlockDevice> BlockDevice for FaultyBlockDevice<D> {
    fn size(&self) -> u64 {
        self.inner.size()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.inner.read_at(offset, buf)
    }
}

impl<D: WritableBlockDevice> WritableBlockDevice for FaultyBlockDevice<D> {
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> Result<(), BlockError> {
        let block_addr = offset / 4096;

        // Check block-specific fault first.
        if let Some(bad_block) = self.fail_on_block {
            if block_addr == bad_block {
                self.fault_log.push((self.write_count, offset));
                return Err(BlockError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("FaultyBlockDevice: injected fault on block 0x{block_addr:x}"),
                )));
            }
        }

        // Check write-count threshold fault.
        if let Some(limit) = self.fail_after_n_writes {
            if self.write_count >= limit {
                self.fault_log.push((self.write_count, offset));
                return Err(BlockError::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!(
                        "FaultyBlockDevice: injected fault after {limit} writes (block 0x{block_addr:x})"
                    ),
                )));
            }
        }

        let result = self.inner.write_at(offset, buf);
        if result.is_ok() {
            self.write_count += 1;
        }
        result
    }

    fn invalidate_read_cache(&mut self) {
        self.inner.invalidate_read_cache();
    }
}
