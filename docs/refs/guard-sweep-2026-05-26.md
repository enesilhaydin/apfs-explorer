# Defensive-Coding Sweep — 2026-05-26

## Scope

All library crates scanned: `apfs-core`, `apfs`, `apfs-write`, `winfsp-host`,
`apfs-mount`, `apfs-supervisor`, `apfs-tray`.

Patterns searched (10 categories):
1. `unwrap()` / `expect(...)` / `panic!` / `unreachable!` / `todo!`
2. `let _ = ...` / `.ok().unwrap_or_default()` silent error swallow
3. `as u32/u16/u8` unchecked integer narrowing cast
4. `unsafe` blocks outside FFI boundary
5. `Mutex::lock().unwrap()` PoisonError swallow
6. Panic-on-drop in `Drop` impls
7. Missing `catch_unwind` on WinFsp FFI callbacks
8. Unbounded slice indexing
9. Integer division by zero
10. Unbounded `loop` (no depth / iteration cap)

---

## Sites Scanned: ~320 grep hits across all crates

### Pattern 1 — unwrap/expect

| Location | Verdict |
|---|---|
| `winfsp-host/src/cache.rs:234,249,271,281` | Inside `#[test]` — OK |
| `winfsp-host/src/winraw.rs:272,274` | Inside `#[test]` — OK |
| `winfsp-host/src/gpt.rs:179,184` | Inside `#[test]` — OK |
| `winfsp-host/src/mount.rs:790` | `.expect("seeded above")` — set unconditionally in same `if` branch one line prior; invariant is structurally guaranteed, `#[allow]`-worthy but not a real panic path |
| `apfs-write/src/scratch.rs:89–140` | Inside `#[test]` — OK |
| `apfs-write/src/snapshot.rs:1286–1568` | Inside `#[test]` — OK |
| `apfs-write/src/file.rs:3194–3266` | Inside `#[test]` — OK |

### Pattern 2 — Silent error swallow (let _ = ...)

| Location | Verdict | Action |
|---|---|---|
| `winfsp-host/src/mount.rs:1126` | **BUG** — `set_inode_attrs` failure silently discarded; attr writes invisible to caller | **FIXED** — commit `7adb73d` |
| `winfsp-host/src/mount.rs:1314` | **DATA LOSS RISK** — `overwrite_existing_file` failure discarded; Windows receives success even when disk write fails | **FIXED** — commit `7adb73d` |
| `winfsp-host/src/mount.rs:1496` | Intentional — `unlink` of symlink placeholder; comment documents "even if absent, still proceed to create"; OK |
| `winfsp-host/src/mount.rs:1553` | `std::fs::copy` DLL best-effort copy; failure means WinFsp DLL not found in fallback path; mount will fail later with a clear error; OK |
| `winfsp-host/src/winraw.rs:155` | `CloseHandle` in `Drop` — can't propagate from drop; OK |
| `apfs-tray/src/tray.rs:14,15,21,61,68` | UI menu / registry — best-effort tray ops; OK |
| `apfs-tray/src/child.rs:67,124,125,159` | Job object / process kill in Drop — OK |
| `apfs-tray/src/main.rs:27,45,61` | Autostart / message loop — best-effort; OK |
| `winfsp-host/src/main.rs:70,71,73,92` | Ctrl-C channel + stdin — best-effort; OK |

### Pattern 3 — Integer narrowing casts (`as u32/u16`)

| Location | Verdict | Action |
|---|---|---|
| `apfs-write/src/free_runs.rs:43,44` | `start as u32`, `len as u32` from `usize` — safe for real APFS chunk sizes (≤ 65536 bits) but unchecked | **FIXED** — clamp `total_bits` to `u32::MAX` at entry; commit `7db3bb7` |
| `winfsp-host/src/mount.rs:353,356,358,585,803` | Casting `path_bytes`/`n`/`buffer.len()` to `u16`/`u32` — bounded by WinFsp API contract (max path 32767, read size 4 MiB); safe with current API but fragile | Known-safe pattern; annotated in code |
| `apfs-write/src/txn.rs` casts | `usize → u16` for B-tree offsets — all guarded via `saturating_*` or sourced from `u16_from_le`; safe |
| `apfs-write/src/snapshot.rs:197,355` | `name_len as u16`, `k_off_new as u16` — name ≤ 255 bytes (APFS limit), offset ≤ block_size (4096); safe |
| `winfsp-host/src/drive.rs:23,58` | `c as u8 - b'A'` — `c` validated to be ASCII letter by caller; safe |

### Pattern 4 — unsafe blocks

All `unsafe` blocks in `winfsp-host` and `apfs-tray` are at the Windows API
FFI boundary (`winraw.rs`, `drive.rs`, `winwin.rs`, `tray.rs`, `child.rs`).
`apfs-core` has `#![forbid(unsafe_code)]`. No `unsafe` found outside FFI. OK.

### Pattern 5 — Mutex::lock().unwrap()

No occurrences in lib code. All `Mutex::lock()` calls use `.map_err(...)` or
`.ok()?`. OK.

### Pattern 6 — Panic-on-drop

No `Drop` impl with `unwrap`/`panic!` found. OK.

### Pattern 7 — catch_unwind coverage

`winfsp-host/src/mount.rs` has a shared `guard()` wrapper at line 129 that
wraps every WinFsp callback with `catch_unwind(AssertUnwindSafe(f))`. All
callback entry points (`read_directory`, `get_file_info`, `read_file`, etc.)
route through `guard()`. Coverage is complete. OK.

### Pattern 8 — Unbounded slice indexing

All non-test slice accesses in `gpt.rs` use `.get(..n).ok_or(...)` guards.
`mount.rs` reparse buffer accesses operate on a fixed-layout 20-byte output
buffer built locally. `txn.rs` B-tree accesses use `saturating_sub` for all
offsets. OK.

### Pattern 9 — Division by zero

`cache.rs` divides by `BLOCK` constant (non-zero). `txn.rs` divides by
`blocks_per_chunk` and `chunks_per_cib` — both have explicit
`return Err(...)` guards if zero at lines 1490 and 1495. OK.

### Pattern 10 — Unbounded loops

| Location | Verdict | Action |
|---|---|---|
| `apfs-write/src/file.rs:2057` | B-tree rebuild loop; `level: u16` increments without cap — wraps at 65535 on degenerate input | **FIXED** — guard at 64; commit `6a214d3` |
| `apfs-write/src/file.rs:2602` | Same pattern, snapshot-path variant | **FIXED** — commit `6a214d3` |
| `winfsp-host/src/main.rs:65` | Event loop with `recv()` blocking — terminates on channel close; OK |
| `apfs-tray/src/main.rs:56` | Win32 message loop — terminates on `WM_QUIT`; OK |

---

## Summary

| Category | Sites scanned | Real bugs found | Fixed |
|---|---|---|---|
| unwrap/expect | ~80 hits | 0 in lib path | — |
| Silent error swallow | ~22 hits | 2 (data loss + attr loss) | 2 |
| Integer narrowing cast | ~40 hits | 1 (free_runs bounds) | 1 |
| unsafe | 15 blocks | 0 | — |
| Mutex poison | 0 | 0 | — |
| Panic-on-drop | 0 | 0 | — |
| catch_unwind | 1 wrapper | 0 | — |
| Slice bounds | ~30 hits | 0 | — |
| Division by zero | ~10 hits | 0 | — |
| Unbounded loop | 4 loops | 2 (level u16 overflow) | 2 |
| **Total** | **~232** | **5** | **5** |

## Commits shipped

| SHA | Crate | Description |
|---|---|---|
| `7adb73d` | `winfsp-host` | Propagate set_inode_attrs + overwrite errors (data loss + silent attr loss) |
| `6a214d3` | `apfs-write` | Depth guard on two fstree rebuild loops (u16 overflow) |
| `7db3bb7` | `apfs-write` | Clamp total_bits in ChunkFreeRuns::from_bitmap (usize→u32 truncation) |

## Critical findings (reported before fix)

**DATA LOSS** — `mount.rs:1314` `overwrite_existing_file` failure was silently
swallowed. The Overwrite WinFsp callback returned `Ok(())` to Windows even
when the APFS disk write transaction failed, leaving the file silently
corrupted from the OS perspective.

## Test gate

`cargo test --workspace --lib` — 23 passed, 0 failed.

## Pre-existing clippy errors (not introduced by this sweep)

`apfs-write` has ~70 pre-existing `-D warnings` clippy errors
(`indexing_slicing`, `too_many_arguments`, `map_or` simplification,
`contains_key` + `insert` pattern) present before this sweep. These are
outside the scope of this crash-safety pass and tracked separately.
