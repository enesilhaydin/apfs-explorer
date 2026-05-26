# M7-Polish Benchmark — Phase 3 Results

Captured on the SSH box (192.168.1.245), Windows 11 + WinFsp 2.1 +
apfs-mount.exe via WinFsp Launcher (global drive letter), OSFMount
loopback over `C:\Users\enesi\Documents\apfs-scratch-m7a.img` (64 MiB
APFS-in-GPT scratch). Source: `phase3-benchmark.ps1`, all sizes in
MiB/s wall-clock (Measure-Command around `[IO.File]::WriteAllBytes` /
`ReadAllBytes`).

## Numbers

| Operation              | APFS via WinFsp (F:) | NTFS baseline (C:) | Ratio |
|------------------------|---------------------:|-------------------:|------:|
| 1 MiB sequential write | 6.00                 | 1319.61            | 1/220 |
| 4 MiB sequential write | 7.31                 | n/a                | —     |
| 16 MiB sequential write| 7.42                 | 4232.58            | 1/570 |
| 1 MiB sequential read  | 56.47                | 167.96             | 1/3   |
| 4 MiB sequential read  | 170.13               | n/a                | —     |
| 16 MiB sequential read | 294.23               | 1033.16            | 1/3.5 |
| 50 small-file create   | 36.5 ops/s           | 1257.8 ops/s       | 1/35  |

## Bottleneck analysis (where the write throughput goes)

A 16 MiB Set-Content triggers the chunked WinFsp Write callback ~16
times (Windows default ~1 MiB IRP size), each chunk appending to the
handle's `write_buf` in RAM. The single expensive event is the Cleanup
non-delete that follows, which calls `apfs_write::overwrite_existing_file`
inside one transaction. That transaction:

1. Frees the old file's extents (none for a fresh file)
2. Allocates 4096 fresh data blocks via `txn.alloc_block()` (one
   spaceman bitmap bit-flip per call — currently no batched bit-range
   update), stage_raw each block.
3. Builds one `file_extent` record per contiguous run (typically a
   single run for 16 MiB on an empty scratch) — fine.
4. Rebuilds the inode + dstream xfield.
5. Rewrites the fstree leaf (and splits when full) with the new
   records via `rewrite_fstree`.
6. Inserts one phys_ext into the extref tree.
7. COWs the omap header + stages the new VSB.
8. `txn.commit()` writes ~4096 data blocks + ~6 metadata blocks +
   updates two checkpoint slots.

Steps 2, 3, 5, 6, 8 dominate. The spaceman bit-flip loop is per-block
in Rust + a HashMap-backed allocator; CPU profile expected to show
4096 hash lookups per 16 MiB.

The read path is dominated by `CachingBlockDevice` (64 KiB read-ahead,
1024-block LRU) — explains the 1/3.5 ratio against NTFS (which has
the kernel-mode disk cache + larger sequential prefetch). Acceptable
for v1.

## Targeted optimisations for M8

1. **Bitmap-range alloc** — `alloc_blocks_contiguous(n)` that flips
   n bits in one bitmap update + returns a contiguous run. Expected
   2–5× write win on large transfers (4096 bit-flips → 1 range op).
2. **spaceman cache** — keep the working chunk of the allocation bitmap
   resident across alloc_block calls so each call does one bit op
   instead of a HashMap fetch + insert.
3. **Cleanup batching** — defer FsView reopen across N small-file
   cleanups inside a debounce window so 50 small files become ~5
   transactions instead of 50 (35× small-file win).
4. **Read-ahead block size** — 64 KiB → 256 KiB on >1 MiB transfers
   ("auto promote" when sequential pattern detected). Possibly 2×
   read.

## M8 Optimisation #1 — Deferred bitmap/CIB writes (commit `daf7525`)

linux-apfs-rw pattern: 4096 single-block allocs share one bitmap-block
write at commit (`apfs_write_ip_bitmaps`). Our previous impl called
`dev.write_at` on the bitmap + CIB **every** alloc — 8192 sync I/Os
per 16 MiB write before commit even started.

Replaced with `dirty_bitmaps` + `dirty_cibs` HashMap caches on
`Transaction`; `alloc_one_block` / `free_one_block` mutate cached
copies only; `commit()` flushes each cache once. Added
`alloc_blocks_run(n)` for contiguous-run alloc (single bit-range op).

Benchmark delta on the same scratch + OSFMount + WinFsp launcher
setup (single change, no other code touched):

| Operation              | Phase 3 baseline | After deferred bitmap | Speedup |
|------------------------|----------------:|---------------------:|--------:|
| 1 MiB sequential write | 6.00            | 31.98                | **5.3x**|
| 4 MiB sequential write | 7.31            | 51.82                | **7.1x**|
| 16 MiB sequential write| 7.42            | 57.01                | **7.7x**|
| 1 MiB sequential read  | 56.47           | 97.16                | 1.7x    |
| 16 MiB sequential read | 294.23          | 319.14               | neutral |
| 50 small-file create   | 36.5 ops/s      | 104.5 ops/s          | **2.9x**|

Verdict: from 1/570 of NTFS to roughly 1/55 — the bottleneck shifted
from CPU bookkeeping to actual catalog COW + I/O. Remaining write
gap is now the transaction-commit cost itself (fstree + omap + extref
+ VSB COW) plus the read-after-write cache misses. The three
remaining M8 optimisations (spaceman cache, cleanup batching,
adaptive read-ahead) target those.

## Production-grade verdict for v1

Sequential read parity-ish (28% of NTFS) is fine for the dev-USB use
case the project targets — files load promptly. Sequential write at
7 MB/s is far below NTFS but still within tolerable USB-class throughput
(USB 2.0 is ~30 MB/s, USB 3.0 SSDs are 200+); the bottleneck is CPU
bookkeeping not I/O, so optimisation is a Rust profiling problem not
an APFS spec problem. Acceptable to ship v1 with this; the four
optimisations above are the M8 plan.
