# M7-Polish: NTFS/FAT32-parity + macOS APFS performance parity

Author: Claude — 2026-05-25, post-`checkpoint/m7-winfsp-full-callbacks-proven`.

Goal: end the M7 milestone with the apfs-explorer Windows mount fully
indistinguishable from NTFS from the user's point of view — every
operation Explorer / cmd / PowerShell / Notepad++ / VSCode / Word
performs on a normal drive must succeed against our APFS-backed F:,
in every language (Turkish, Chinese, Arabic, emoji), at I/O speeds
that approach the macOS native APFS driver on the same hardware.

Three execution phases. Each item has a self-contained scratch test
(macOS) and a Windows oracle step. We commit per item, one tag at the
end of each phase.

---

## Phase 1 — Missing write-path APIs (~3–4 h)

Each subtask: design + apfs-write API + winfsp-host wire + scratch
test + Windows oracle + commit.

### 1.1 set_file_size(N > 0) — truncate / extend in place
**Why**: Notepad++ / Word / VSCode atomic-save patterns SetEndOfFile(N)
intermediate sizes. Today only N=0 truncate-to-empty works.

**API** (`apfs-write`):
```rust
pub fn resize_file<D: WritableBlockDevice>(
    txn, vsb_raw, omap_raw,
    parent_ino, name,
    new_size: u64,
) -> Result<(), TxnError>
```
- Shrink: free tail extents past `new_size`, trim partial extent if
  the new EOF lands mid-block (zero the slack), update dstream.size.
- Extend: allocate sparse extents (or zero blocks) to cover the new
  range, update dstream.size + alloced_size, write file_extent records.
- Inode mtime / ctime updated.

**Scratch test**: write 100-byte file → resize to 50 → readback 50 bytes
correct. Then resize to 4500 (spans multi-block) → readback first 100
bytes correct, rest zero. fsck CLEAN.

**winfsp-host**: `set_file_size` non-zero now calls `resize_file`.

### 1.2 set_volume_label — VSB.volume_name update
**Why**: Right-click F: → Rename. Today silently fails (Stub OK).

**API** (`apfs-write::volume`):
```rust
pub fn set_volume_label<D>(txn, vsb_raw, new_label: &str) -> Result<(), TxnError>
```
- Write `new_label` UTF-8 (≤256 bytes) into VSB apfs_volname field
  (offset 0x2C0, 256-byte array).
- stage_virtual new VSB.

**winfsp-host**: `set_volume_label` callback wires through.

**Test**: rename via PowerShell `Set-Volume -DriveLetter F -NewFileSystemLabel`,
verify via WMI Win32_LogicalDisk + macOS oracle (apfs.fs sees new
APFS_VOLNAME).

### 1.3 Cross-directory move
**Why**: Drag-drop a file from F:\src → F:\dst (different parent inodes)
today returns `STATUS_NOT_IMPLEMENTED`. Critical for real usage.

**API** (`apfs-write`):
```rust
pub fn move_entry<D>(
    txn, vsb_raw, omap_raw,
    old_parent: u64, old_name: &str,
    new_parent: u64, new_name: &str,
) -> Result<(), TxnError>
```
- One transaction:
  - Look up `(old_parent, old_name)` DREC → file_id + dt_type + date_added.
  - Read INODE for file_id; rebuild with `parent_id = new_parent`,
    new NAME xfield, mtime/ctime now.
  - Catalog updates in `rewrite_fstree`:
    - remove: old DREC, old INODE
    - insert: new DREC `(new_parent, new_name)`, updated INODE
    - parent nchildren: -1 on old, +1 on new
- COW catalog + extref unchanged (file extents stay).

**Scratch test**: create `/a.txt` and dir `/dst`; move `/a.txt` → `/dst/a.txt`;
verify `/a.txt` gone, `/dst/a.txt` present with same inode_id, same content.

**winfsp-host**: `rename` callback drops the STATUS_NOT_IMPLEMENTED guard,
routes to `apfs_write::move_entry` when `old_parent_ino != new_parent_ino`.

### 1.4 set_basic_info — persist mtime + read-only attribute
**Why**: today reflects times only in session FileInfo (stub). Persistent
mtime / "Salt-okunur" toggle isteniyor.

**API** (`apfs-write`):
```rust
pub fn set_inode_attrs<D>(
    txn, vsb_raw, omap_raw,
    parent_ino, name,
    new_mode_or: Option<u16>,    // OR-mask bits (e.g. clear S_IWUSR)
    new_mode_and: Option<u16>,   // AND-mask bits
    new_mtime_ns: Option<u64>,
    new_ctime_ns: Option<u64>,
    new_atime_ns: Option<u64>,
    new_btime_ns: Option<u64>,
) -> Result<(), TxnError>
```
- Read INODE, rebuild with the requested fields touched.
- rewrite_fstree single-record update.

**winfsp-host**: map FILE_ATTRIBUTE_READONLY → clear S_IWUSR; clear of
READONLY → set S_IWUSR. Timestamps converted from Windows FILETIME
(100ns since 1601) to APFS ns since 1970.

**Test**: PowerShell `Set-ItemProperty -Path F:\a -Name IsReadOnly -Value $true`;
verify Explorer attribute checkbox is set + macOS oracle shows mode 0o444.
`(Get-Item F:\a.txt).LastWriteTime = ...`; verify mtime persisted across
remount.

---

## Phase 2 — Robustness matrix (~4–5 h)

Stress test the proven write path against realistic real-world patterns.
Each scenario gets a scripted Windows-side runner + macOS oracle check
+ fsck pass. The image backup is restored before each scenario.

### 2.1 Multi-file write stress
`for i in 1..200: echo "..." > F:\f$i.txt`. Verify all 200 readable,
fsck CLEAN, fstree multi-level (forces internal nodes).

### 2.2 Deep nested directory tree
`mkdir F:\a\b\c\d\e\f\g\h\i\j` (10 deep). Create files at each level.
Verify path lookup + traverse works at depth, mtime per level correct.

### 2.3 Large file (10 MiB)
Generate `head -c 10485760 /dev/urandom > random.bin` on macOS, scp to
Windows, `Copy-Item random.bin F:\big.bin`. macOS readback SHA-256 must
match source. Multi-block extents, single extent if contiguous.

### 2.4 Recursive copy
`Copy-Item -Path C:\Some\Tree -Destination F:\TreeCopy -Recurse`.
Verify mirror tree + all SHA-256 match + fsck CLEAN.

### 2.5 Recursive delete
`Remove-Item -Path F:\TreeCopy -Recurse -Force`. Verify empty, fsck
CLEAN (no orphan extents in extref tree). Counts blocks freed.

### 2.6 Concurrent writes
Two PowerShell jobs simultaneously writing `F:\a.txt` and `F:\b.txt`.
Today with_writable is a global Mutex (one tx at a time); verify serial
ordering produces no corruption. Document the lock granularity.

### 2.7 UTF-8 stress
Filenames in Chinese (`合同.docx`), Arabic (`مستند.txt`), Russian
(`Документ.txt`), emoji (`📊rapor.xlsx`), Turkish dotted-I edge
(`İSTANBUL.txt`, `istanbul.txt`). Each: create, list, rename, delete,
fsck CLEAN.

### 2.8 Atomic-save patterns
Notepad++, Word `.docx`, VSCode `.json` save flow: create temp +
write + rename-over-original. Verify final file content === written,
no temp leftover.

### 2.9 Mid-operation kill
`Start-Process apfs-mount`, write 1 MiB, `Stop-Process -Force` halfway.
OSFMount detach. macOS attach → fsck must report CLEAN (transaction
either committed or not; no torn state). Apple two-NX checkpoint
guarantee.

### 2.10 Edge filename chars
Space (` `), dot prefix (`.hidden`), trailing space, mixed-case,
extra-long name (>255 UTF-8 bytes — must be rejected with a specific
error, not silently truncated).

---

## Phase 3 — Benchmark + targeted performance (~3–4 h)

### 3.1 Baseline benchmark harness
PowerShell script + macOS oracle: sequential write/read MB/s for 1
MiB, 16 MiB, 64 MiB transfers; random 4 KiB IOPS; many-small-files
ops/sec.

### 3.2 Run baseline against current build
Capture numbers. Reference upper bound: macOS native APFS on same
device.

### 3.3 Profile bottlenecks
Count transactions per scenario, FsView reopen overhead, cache hit
rate, alloc_block + free_block call counts. Identify the top three
costs.

### 3.4 Targeted optimisation #1 — batching cleanup-flush
Today every file cleanup → one tx. Multi-file copy = N transactions.
Investigate batching N cleanups inside one with_writable when the
parent dir matches and the operations are independent — defer FsView
reopen.

### 3.5 Targeted optimisation #2 — cache size tuning
Read-ahead block is 64 KiB / 1024 blocks. For large sequential writes
maybe a larger block + larger LRU. Re-measure.

### 3.6 Re-run benchmark, compare delta
Plot deltas against the baseline. Goal: 50–80% of native APFS
sequential throughput on the same hardware.

---

## Done criteria

- All Phase 1 APIs implemented, scratch tests GREEN, Windows oracle
  CLEAN, mount survives the full Phase 2 matrix unchanged.
- Phase 3 produces a benchmark report (file + chart) checked into
  `docs/m7-polish-benchmark.md`.
- One tag per phase: `checkpoint/m7-polish-phase{1,2,3}-proven`.
- Final RESUME doc update with the full proof matrix + benchmark
  numbers + remaining-work pointers.
