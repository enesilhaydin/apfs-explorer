# Chaos / Fault-Injection Test Matrix — Task #50 (2026-05-26)

Infrastructure: `crates/apfs-write/tests/chaos_write.rs` +
`crates/apfs-write/tests/common/faulty_device.rs`

Gate: `APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test chaos_write -- --test-threads=1`
(single-threaded because parallel hdiutil invocations conflict on macOS)

---

## Infrastructure

### FaultyBlockDevice

File: `crates/apfs-write/tests/common/faulty_device.rs`

Test-only wrapper over any `WritableBlockDevice`. Three independent fault modes:

| Mode | Method | Behaviour |
|---|---|---|
| `fail_after_n_writes(n)` | `set_fail_after_n_writes(n)` | Returns `BlockError::Io` on write N+1 (writes 0..n succeed) |
| `fail_on_block(paddr)` | `set_fail_on_block(paddr)` | Returns error on any write to block `paddr` |
| crash-on-drop | default | Documents intent; inner device dropped without explicit flush |

Write counter (`write_count`) and fault log (`fault_log: Vec<(usize, u64)>`) are
public for test assertions.  `into_inner()` recovers the device post-crash.

---

## Scenario Matrix

| ID | Name | Fault mode | What it proves | Result |
|---|---|---|---|---|
| S1 | partial-commit | `fail_after_n_writes(3)` | Err during commit never advances XID on disk | PASS |
| S2 | pre-checkpoint crash | `fail_on_block(nxsb_block)` | NXSB write failure leaves old superblock as latest | PASS |
| S3 | post-data/pre-meta | `fail_after_n_writes(1)` | Single data block on disk + meta absent = old XID survives | PASS |
| S4 | sm_fq mid-drain | `fail_after_n_writes(4)` | delete_snapshot crash = no XID advance, device still parseable | PASS |
| S5 | fstree split mid-write | `fail_after_n_writes(8)` | B-tree split crash = old root pointer survives | PASS |
| S6 | omap idempotent/abort | `txn.abort()` after duplicate create | Aborted txn never mutates disk; AlreadyExists is non-corrupting | PASS |
| S7 | cross-flush ordering | `fail_after_n_writes(2)` | Bitmap+CIB partial write, no NXSB = old XID survives | PASS |

All 7 PASS on macOS 15.7 (2026-05-26, apfs-explorer `main`).

---

## Invariants verified per scenario

Each scenario asserts:

1. **XID invariant**: `read_latest_xid(img)` after fault == `xid_before` (or
   `xid_after_snap` for S4).  Implemented by walking the checkpoint descriptor
   ring looking for the highest-XID block with NXSB magic `0x4253_584e`.

2. **Parseability**: `Transaction::begin(dev)` succeeds after the crash,
   meaning the spaceman, checkpoint ring, and container omap are internally
   consistent.

---

## Bugs found

None. All ACID invariants held under every injected fault scenario.

**Interpretation**: The write path (COW commit in `txn.rs`) is crash-safe for
single-node workloads because:

- The NXSB is written last in `commit()`, so any earlier write failure leaves
  the previous checkpoint as the newest valid superblock.
- `abort()` discards all staged writes without touching the device.
- `AlreadyExists` from `create_file` is a pure in-memory error (before any
  `stage_*` call); it cannot corrupt on-disk state.

**Known limitation**: Tests use `--test-threads=1` because each test calls
`hdiutil create` for a fresh 64 MiB image; parallel invocations conflict on
macOS (xtask gen-scratch observed to fail when 7 simultaneous `hdiutil` calls
compete for the same loopback resources).  A future improvement could pre-build
a single template image and copy it per-test.

---

## CI gate command

```sh
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test chaos_write -- --test-threads=1
```
