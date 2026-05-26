# Stress / Property Test Suite — 2026-05-26

Crate: `apfs-write`. Gate: `APFS_WRITE_SCRATCH=1` (macOS + hdiutil).

## CI invocation

```sh
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --release \
  --test property_random_workload \
  --test property_mount_cycle \
  --test stress_many_snapshots \
  --test stress_large_file \
  -- --test-threads=1
```

## Suite inventory

| File | Tests | Ops/cycles | Invariants |
|---|---|---|---|
| `tests/property_random_workload.rs` | 4 (seed1–seed4) | 1000 ops × 4 seeds | I1 parseable every 50 ops, I4 final readback |
| `tests/property_mount_cycle.rs` | 1 | 50 mount/unmount cycles | XID monotonic, re-mount succeeds |
| `tests/stress_many_snapshots.rs` | 1 | 30 create + 30 delete (mixed order) | XID monotonic, num_snapshots tracks, final parseable |
| `tests/stress_large_file.rs` | 1 | 4 writes (10 MiB, overwrite, shrink, 18 MiB) | XID advances per write, parseable after each |

Total: **7 test functions**, **~4000+ ops** coverage per full run.

## Seeds (property_random_workload)

| Label | Seed |
|---|---|
| seed1 | `0xDEAD_BEEF_0000_0001` |
| seed2 | `0xCAFE_BABE_0000_0002` |
| seed3 | `0x1234_5678_9ABC_DEF0` |
| seed4 | `0x0FF1_CE00_DEAD_C0DE` |

Same seed always produces the same op sequence (xorshift64 PRNG, no wall-clock seeding).

## Invariants checked

- **I1** — `Transaction::begin` succeeds after every nth committed op (device parseable = NXSB valid).
- **I2** — Live file cap (64) prevents bitmap exhaustion in 64 MiB image.
- **I3** — Op-level errors tolerated (log + continue); panics are test failures.
- **I4** — Final shadow-map readback: every surviving filename → correct byte content.
- **I5** (mount_cycle) — XID advances monotonically across 50 write+drop+reopen cycles.
- **I6** (many_snaps) — `num_snapshots` VSB field stays in `(0, N]` range during create phase, reaches 0 after all deletes succeed.
- **I7** (large_file) — Multi-block extent writes (up to 18 MiB = 4608 x 4 KiB blocks) commit cleanly.

## Last run result (2026-05-26 — first real runtime execution)

Environment: macOS darwin 24.6.0, `--release`, `--test-threads=1`, `APFS_WRITE_SCRATCH=1`.

| Suite | Tests | Pass | Fail | Time |
|-------|-------|------|------|------|
| `chaos_write` (S1–S8) | 8 | 8 | 0 | 9.93s |
| `property_random_workload` (seed1–seed4) | 4 | 4 | 0 | 7.84s |
| `property_mount_cycle` (50 cycles) | 1 | 1 | 0 | 1.37s |
| `stress_many_snapshots` (30 create + 30 delete) | 1 | 1 | 0 | 1.34s |
| `stress_large_file` | 1 | 1 | 0 | 1.40s |
| **Total** | **15** | **15** | **0** | **~22s** |

property_random_workload details: 789–811 successful ops per seed, 59–63 surviving files, all verified by final readback.

**All 15 tests PASS.**

## Bug findings

4 test bugs found and fixed during first runtime execution (no production bugs):

### B1 — Wrong APFS_GPT_TYPE UUID (4 files)

Files: `property_random_workload.rs`, `property_mount_cycle.rs`, `stress_many_snapshots.rs`, `stress_large_file.rs`.

Wave 4 copy contained `[0x53, 0xEF, 0x10, 0x7C, 0xD7, 0xD4, 0xEF, 0x46, ...]` instead of the correct `[0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC]`. All 4 files failed immediately with "no APFS partition found in GPT". Fixed to match `chaos_write.rs` + real disk (empirically confirmed from `/tmp/apfs-chaos-s1.img`).

### B2 — Wrong `names_hashed` bit mask in `readback_verify`

File: `property_random_workload.rs`, `readback_verify()`.

Used `incompatible_features & 0x2` (DATALESS_SNAPS bit — not a hash flag) instead of `& (0x1 | 0x8)` (APFS_INCOMPAT_CASE_INSENSITIVE | APFS_INCOMPAT_NORMALIZATION_INSENSITIVE). Mismatch: `build_drec_key` always writes a 4-byte `name_len_and_hash` header, but `list_dir` with `names_hashed=false` reads only a 2-byte `name_len` — key layout mismatch caused all renamed files (with `r`-prefix names) to be reported missing. Fixed to match `VolumeSuperblock::names_are_hashed()`.

### B3 — Multi-write single-txn stale vsb/omap

File: `property_mount_cycle.rs`.

Cycles with N > 1 (starting cycle 2, N=3) wrote N files in one transaction using the same `vsb_raw`/`omap_raw` snapshot for all N calls. Each `write_file` COWs the omap/vsb to new blocks; subsequent calls with stale metadata staged against freed blocks, corrupting the omap (BadChecksum on re-open). Fixed: one transaction per file with fresh `read_vsb_omap_raw()` call per write.

### B4 — Wrong `VSBI_NUM_SNAPSHOTS` offset

File: `stress_many_snapshots.rs`.

Hardcoded `0xC0` (= `VSBI_NUM_DIRECTORIES`) instead of `0xD8` (= `apfs_num_snapshots`). `read_num_snapshots()` always returned 0, causing the post-create assertion `num_snapshots > 0` to fail. Fixed to `0xD8`, matching `snapshot.rs` and `file.rs`.
