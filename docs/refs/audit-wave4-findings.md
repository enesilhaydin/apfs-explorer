# Wave 4 Audit Findings — Rare / Edge Paths

**Date:** 2026-05-26
**Auditor:** md.7 LOCKED cross-check workflow
**Reference:** APFS specification + linux-apfs-rw cross-reference analysis

---

## Scope

10 surfaces audited against reference implementation counterparts.

| # | Surface | Apple symbol(s) |
|---|---------|-----------------|
| W4-S1 | xattr large stream write (>3804 B) | `_apfs_setxattr_internal`, `_alloc_xattr_tmp_ino` |
| W4-S2 | decmpfs codec coverage | `_decmpfs_read_compressed`, `_decmpfs_decompress_file` |
| W4-S3 | resource fork write | `_apfs_vnop_makenamedstream`, `_apfs_setxattr_as_namedstream` |
| W4-S4 | sparse file hole paddr=0 | `_apfs_vnop_allocate`, `_handle_is_sparse` |
| W4-S5 | dir hash collision | already confirmed wave-2 |
| W4-S6 | FITRIM / Windows TRIM | `_apfs_vnop_ioctl` (TRIM absent) |
| W4-S7 | large file >4 GiB extents | n/a (u64 clean) |
| W4-S8 | private_id reuse | `_alloc_xattr_tmp_ino::get_next_apfs_obj_ids` |
| W4-S9 | xfield blob grow | `_apfs_setxattr_internal` → `update_jobj` |
| W4-S10 | apfs_create_inode counter policy | `get_next_apfs_obj_ids` from VSB |

---

## Findings

### W4-BUG-1: sparse file hole — reads from physical block 0 instead of returning zeros

**File:** `crates/apfs-core/src/catalog.rs`, function `read_stream`
**Severity:** Medium — data corruption on read of sparse files (paddr=0 extents)

**Apple behavior:** A `j_file_extent_val` with `phys_block_num == 0` encodes a hole (unallocated
range). Apple's read path returns zeros for such extents without issuing a disk read.

**Our behavior:** `read_stream` pre-fills `out = vec![0u8; size]` (all zeros) then iterates
extents. For an extent with `paddr == 0`, the code calls:
```
dev.read_at(0 * bsz as u64, &mut tmp)
```
This reads `len` bytes from physical offset 0 of the image — i.e., the NX container superblock
bytes — and writes them into the file's output buffer. This is wrong.

**Fix needed (Wave 5):** In `read_stream`, skip the `dev.read_at` call when `paddr == 0`:
```rust
if paddr == 0 {
    // hole — already zero-filled by pre-alloc; no disk read
    continue;
}
```

---

### W4-GAP-1: write path — no large xattr stream support (>XATTR_MAX_EMBEDDED_SIZE)

**File:** `crates/apfs-write/src/file.rs`
**Severity:** Missing feature (not a bug in existing code)
**Status:** FIXED — commit `429a02a` (feat(apfs-write): large xattr stream write)

`set_xattr()` dispatches on `XATTR_EMBEDDED_THRESHOLD` (2048 B). Values above the threshold
use the stream path: allocates a private dstream id via `alloc_oid()`, writes data blocks via
`alloc_blocks_run()` loop, builds `DSTREAM_ID` + `FILE_EXTENT` + `phys_ext` records, and stores
a 52-byte `j_xattr_val` with inline `j_xattr_dstream_t`. Replace frees old extents. `delete_xattr()`
is idempotent. Tested: 100 KiB, 1 MiB, replace, delete, embedded round-trip.
Limitation: multi-GB xattrs deferred to M10 (single-transaction only; alloc_blocks_run loop
handles multi-chunk allocation within one transaction).

**Tracked as:** W4-GAP-1 — resolved

---

### W4-GAP-2: decmpfs — methods 3/4 (ZLIB) and 5/6 (LZFSE) unsupported

**File:** `crates/apfs-core/src/decmpfs.rs`
**Severity:** Missing feature — `UnsupportedMethod` error returned, no panic/data corruption

The reference implementation uses an external plugin mechanism for methods 3/4/5/6. These methods appear on:
- HFS+-converted APFS volumes (methods 3/4 ZLIB)
- macOS system volume files (methods 5/6 LZFSE)

Our code correctly returns `Err(DecmpfsError::UnsupportedMethod(m))` which propagates cleanly.
No wrong data returned. For a pure read-only APFS explorer of user data volumes (macOS 12+),
methods 7/8/9 cover the common cases. Methods 5/6 would be needed to read system volume files.

**Tracked as:** W4-GAP-2 (deferred — system volume support future scope)

---

### W4-GAP-3: resource fork — write path absent

**File:** `crates/apfs-write/src/file.rs`
**Severity:** Missing feature (same root cause as W4-GAP-1)
**Status:** FIXED — resolved automatically by W4-GAP-1 fix (commit `429a02a`)

`com.apple.ResourceFork` is just an xattr name. Now that `set_xattr()` handles large stream
xattrs, ResourceFork write works: `set_xattr(inode_id, "com.apple.ResourceFork", data)`.
No separate implementation needed.

**Tracked as:** W4-GAP-3 — resolved (dependent fix via W4-GAP-1)

---

### W4-FRAG-1: xfield blob — extra xfields silently dropped on inode update

**File:** `crates/apfs-write/src/file.rs`, `build_inode_val`
**Severity:** Low fragility — no current bug, potential future issue

`build_inode_val` always serializes exactly NAME + optional DSTREAM xfields. If an inode from
an externally-created APFS volume has additional xfields (e.g., DOCUMENT_ID type 9, FINDER_INFO
type 6), our next `insert_jobj` call for that inode will write a value that drops those extra
xfields. Apple's `update_jobj` preserves unrecognized xfields by copying the full old blob.

**Status: FIXED** — added `parse_unknown_xfields` (apfs-core::inode) and
`build_inode_val_with_xfields` + `rebuild_inode_preserving_unknown` (apfs-write::file).
All inode rewrite paths (rename via `rebuild_inode_with_name`, content overwrite via
`overwrite_existing_file`, size change via `truncate_file_fast`) now parse and round-trip
unknown xfields verbatim. `set_inode_attrs` was already safe (in-place clone).
Regression test: `crates/apfs-write/tests/xfield_preserve.rs` (4 tests, all green).

**Tracked as:** W4-FRAG-1 (fixed this session)

---

## Confirmed non-issues (✅)

| Surface | Result |
|---------|--------|
| sparse hole zero-fill pre-alloc | Pre-alloc of zeros is correct baseline; bug is only for paddr=0 extents |
| dir hash collision | Already confirmed ✅ wave-2 |
| large file >4 GiB | No boundary — all u64, clean |
| private_id reuse | Monotonic counter, no reuse — matches Apple exactly |
| FITRIM/discard | Non-goal — image-file model, no IOMedia TRIM passthrough needed |
| inode counter policy | Per-volume VSB `next_obj_id` — matches Apple exactly |

---

## Wave 5 fix targets

| ID | Location | Description |
|----|----------|-------------|
| W4-BUG-1 | `apfs-core/src/catalog.rs::read_stream` | Skip disk read when `paddr == 0` (sparse hole) — 🐛 fixed in `63c209e` |

---

## Non-goals confirmed

- `_apfs_vnop_getnamedstream` / `_apfs_vnop_makenamedstream` full named-stream vnode model: macOS-specific vnode API, no Windows equivalent.
- FITRIM / IOMedia unmap: host-layer concern, not filesystem layer.
- decmpfs method 3/4/5/6 plugins: out of scope until system volume support planned.
