# Test Coverage — 2026-05-26

Tool: `cargo-llvm-cov` 0.8.7 (LLVM source-based, more accurate than tarpaulin).
Command: `cargo llvm-cov --workspace --all-features --json`

## Crate-by-crate summary

| Crate | Lines% (before) | Lines% (after Wave 6) | Lines% (after Wave 7) | Fns% (Wave 7) |
|---|---|---|---|---|
| apfs-core | 59.3% | 63.0% | **~65%** | ~70% |
| apfs-write | 32.9% | 35.5% | **~38%** | ~48% |
| apfs | 0.0% | 0.0% | **~19%** | ~42% |
| winfsp-host | 82.6% | 82.6% | 82.6% | 77.8% |
| apfs-tray | 97.2% | 97.2% | 97.2% | 93.3% |
| installer | 100% | 100% | 100% | 100% |
| apfs-service | 0.0% | 0.0% | 0.0% | 0.0% |
| **TOTAL** | **42.2%** | **44.9%** | **~48.1%** | **~57%** |

> Wave 7 measured: `cargo llvm-cov --workspace --all-features --summary-only`
> Lines: 44.9% → **48.05%** (+3.15 pp). 31 new tests in 5 atomic commits.

## apfs-core per-file (Wave 7)

| File | Lines% |
|---|---|
| block_device.rs | 77.2% |
| btree.rs | 100% |
| catalog.rs | **35.80%** (was 31.0%, +6 tests) |
| checkpoint.rs | 89.6% |
| checksum.rs | 81.6% |
| container.rs | **52.78%** (was 18.5%, +3 tests) |
| decmpfs.rs | 66.7% |
| endian.rs | 100% |
| inode.rs | 100% |
| jkey.rs | 100% |
| lzvn.rs | 58.4% |
| nx.rs | 77.5% |
| obj.rs | 100% |
| omap.rs | 70.1% |
| volume.rs | 78.6% |
| xattr.rs | 100% |

## apfs-write per-file (Wave 7)

| File | Lines% |
|---|---|
| file.rs | **26.24%** (was 23.4%, +11 serialization helper tests) |
| free_runs.rs | 95.1% |
| scratch.rs | 100% |
| sm_fq.rs | 97.0% |
| snapshot.rs | 47.0% |
| txn.rs | **38.97%** (was 24.4%, +3 multi-block alloc tests) |

## apfs per-file (Wave 7)

| File | Lines% |
|---|---|
| lib.rs | **18.65%** (was 0.0%, +8 components path parser tests) |

## Gaps filled (3 atomic commits — Wave 6)

### 1. `crates/apfs-write/src/txn.rs` — NxView::parse error paths
Commit: `test(txn): add NxView::parse error-path unit tests`
- Added `make_valid_nx_block()` helper that builds a valid 4096-byte NX superblock.
- Tests: short block, bad checksum, bad magic, invalid block_size (512), valid round-trip, fs_oid zero-termination.
- What it proves: the parser rejects every class of corrupt input before touching any field, and the fs_oid loop stops at the first zero entry.

### 2. `crates/apfs-core/src/lzvn.rs` — copy_match overlap + lrg_l opcode
Commit: `test(lzvn): add copy_match overlap-splat, d_val=0 error, and lrg_l tests`
- `copy_match_overlap_splat_semantic`: D=1 with M=4 on literal "a" → "aaaaa" — verifies the byte-by-byte splat (not memcpy).
- `copy_match_distance_exceeds_output_errors`: D > out.len() returns ParseError, not panic.
- `large_literal_only_no_match`: exercises the lrg_l (0xE0) opcode with L_byte=0 → l_lit=16 (implicit bias of 16).

### 3. `crates/apfs-core/src/block_device.rs` — WritableOffsetBlockDevice
Commit: `test(block_device): add WritableOffsetBlockDevice unit tests`
- Local `VecDev` in-memory helper (no file I/O needed).
- Tests: read offset translation, write offset translation, read OOB, write OOB, size() reports window not inner size.
- block_device.rs: 40.7% → 77.2% lines.

## Gaps filled (5 atomic commits — Wave 7)

### 1. `crates/apfs/src/lib.rs` — components() path parser
Commit: `test(apfs): components() path parser — 0% → covered (8 tests)`
- Tests: empty string, root slash, single component, multi component, trailing slash, double slash collapse, deep path, extension dots.
- lib.rs 0.0% → ~19% lines.

### 2. `crates/apfs-core/src/catalog.rs` — read_stream + for_each_leaf error branches
Commit: `test(apfs-core): catalog read_stream + for_each_leaf error branches (6 tests)`
- BoundedMemDev helper + catalog_from_leaf factory.
- Tests: size=0 returns empty, no-matching-OID returns zeros, two extents assembled in order, truncated request clamped, empty leaf calls closure zero times.
- catalog.rs 31.0% → 35.80% lines.

### 3. `crates/apfs-core/src/container.rs` — Container::open error branches
Commit: `test(apfs-core): container open error branches — all-zeros, bad magic, ring scan (3 tests)`
- Inline MemDev + make_nx_block() helper.
- Tests: all-zeros block0 rejected, wrong magic (valid checksum) rejected as Parse, newer xid selected from descriptor ring.
- container.rs 18.5% → 52.78% lines.

### 4. `crates/apfs-write/src/file.rs` — serialization helpers pure-unit
Commit: `test(apfs-write): file.rs serialization helpers — pure-unit coverage (11 tests)`
- Tests: encode_jkey, build_inode_key, build_file_extent_{key,val}, build_dstream_id_{key,val}, build_phys_ext_{key,val}, build_drec_val, build_xattr_{key,val_embedded}.
- file.rs 23.4% → 26.24% lines.

### 5. `crates/apfs-write/src/txn.rs` — alloc_blocks_run multi-block
Commit: `test(apfs-write): txn alloc_blocks_run multi-block + no-free error (3 tests)`
- make_bitmap_spaceman() helper (bitmap-based CIB, no bitmap-less shortcut).
- Tests: 4-block contiguous run from all-free chunk, partial run when fewer available, NoFreeBlocks when fully used.
- txn.rs 24.4% → 38.97% lines.

## Gaps filled (4 test files — Wave 8)

Wave 8 strategy: integration tests on real scratch images (64 MiB APFS-in-GPT).
Gate: `APFS_WRITE_SCRATCH=1`. All 21 new tests pass. Runtime: ~28 s total (4 test binaries × ~7 s each).

Coverage measured per-binary with:
`APFS_WRITE_SCRATCH=1 cargo llvm-cov --workspace --all-features --summary-only --test <name> -- --test-threads=1`

| File | Lines% before | Lines% (Wave 8 integration only) | Delta |
|---|---|---|---|
| `apfs-write/src/file.rs` | 26.24% | **79.76%** | +53 pp |
| `apfs-write/src/txn.rs` | 38.97% | **85.32%** | +46 pp |
| `apfs-write/src/snapshot.rs` | 43.81% | **91.72%** | +48 pp |
| `apfs/src/lib.rs` | 18.65% | **89.58%** | +71 pp |

> Note: TOTAL line in per-binary run (77%) reflects only the 4 test binaries, not all
> workspace tests. Full workspace TOTAL (all non-failing tests) stays at ~47% because
> integration tests with `APFS_WRITE_SCRATCH=1` are excluded from the default gate-less run.

### 1. `crates/apfs-write/tests/integration_file_ops.rs` — file operation lifecycle (6 tests)
Commit: `test(apfs-write): integration_file_ops — create/write/read/delete/rename/mkdir/truncate/overwrite`
- T1: write 1 KiB → read via FsView → unlink → confirm gone
- T2: write 64 KiB → truncate_file_fast 16 KiB → size + content correct (FastPathDeclined gracefully skipped)
- T3: write → rename → old path gone, new path readable
- T4: mkdir nested (2 levels) → write inside → read via multi-component path
- T5: write → overwrite_existing_file → new content confirmed
- T6: create_file (empty) → zero-size inode persists
- file.rs: 26.24% → 79.76% in integration-only run

### 2. `crates/apfs-write/tests/integration_txn_lifecycle.rs` — transaction lifecycle (5 tests)
Commit: `test(apfs-write): integration_txn_lifecycle — begin/commit/abort/XID monotonicity`
- L1: commit persists state; XID advances after each commit
- L2: abort (drop without commit) leaves state and XID unchanged
- L3: 5 sequential commits → XID strictly monotonic each time
- L4: begin on fresh image → xid >= 2
- L5: alloc_block + free_block in same txn → image remains parseable
- txn.rs: 38.97% → 85.32% in integration-only run

### 3. `crates/apfs-write/tests/integration_snapshot.rs` — snapshot lifecycle (5 tests)
Commit: `test(apfs-write): integration_snapshot — create/delete/multi-snap/survives-live-delete`
- S1: create_snapshot increments om_snap_count; delete_snapshot decrements back to 0
- S2: 3 snapshots created → deleted in order (oldest first)
- S3: 3 snapshots created → deleted in reverse order (newest first)
- S4: snapshot survives live-tree file delete; delete_snapshot succeeds after
- S5: create_snapshot XIDs strictly increasing
- snapshot.rs: 43.81% → 91.72% in integration-only run

### 4. `crates/apfs-write/tests/integration_fsview.rs` — FsView read-side (5 tests)
Commit: `test(apfs-write): integration_fsview — FsView open/lookup/read_dir/getattr/read`
- F1: mount → read_dir root → getattr → read full + partial + past-EOF
- F2: lookup non-existent → None; lookup "" → root inode 2; volume_size total/free
- F3: read_dir with 50 files → all entries returned (pagination)
- F4: multi-component path (3 levels deep) → lookup/getattr/read/read_dir all correct
- F5: getattr directory → is_dir=true
- lib.rs: 18.65% → 89.58% in integration-only run

### Key fix discovered during authoring
`vsb.omap_oid` (volume superblock's omap_oid field) is a **physical paddr**, not a
virtual OID. Reading it via `omap.resolve()` through the container omap gives wrong
results. Correct pattern (matching chaos_write.rs and snapshot_scratch.rs):
`dev.read_at(vsb.omap_oid * bsz, &mut omap_raw)` — direct physical read.

### Pre-existing failure (not introduced by Wave 8)
`snapshot_scratch::decisive_create_delete_snapshot_on_scratch` — fails with
`Fix overallocation (0xc0+1) bitmap address (0xbc)? NO` from fsck_apfs. This failure
pre-dates Wave 8 (confirmed via `git stash` + rerun). Not a regression.

## Top remaining gaps (Wave 9 targets)

| File | Lines% (gate-less baseline) | Why hard / what to do |
|---|---|---|
| `apfs-core/src/catalog.rs` | 35.80% | `stat()` internals, `list_xattrs`, `get_xattr` dstream path — needs multi-level B-tree in-memory or fixture |
| `apfs-core/src/decmpfs.rs` | 66.7% | lzfse decompressor path, header-only decode — needs compressed fixture blocks |
| `apfs-core/src/lzvn.rs` | 58.4% | remaining opcodes (D=0 match, 3-byte end-of-stream) |
| `apfs-write/src/file.rs` | ~26% (gate-less) | remaining branches: move_entry cross-dir, set_inode_attrs, set_volume_label |
| `winfsp-host/src/offset_dev.rs` | 70.8% | OffsetBlockDevice read/write boundary cases |

---

## Wave 9 — 2026-05-26

Command: `cargo llvm-cov -p apfs-core --all-features --summary-only`

### Summary

16 new pure-unit tests across 3 files, 3 atomic commits. Zero fixture dependency.

| File | Lines% before | Lines% after | Delta |
|---|---|---|---|
| `catalog.rs` | 35.80% | **46.27%** | +10.5 pp |
| `decmpfs.rs` | 66.7% | **95.56%** | +28.9 pp |
| `lzvn.rs` | 58.4% | **82.75%** | +24.4 pp |
| apfs-core TOTAL | ~65% | **71.01%** | +6 pp |

### Commits (3 atomic)

#### 1. `crates/apfs-core/src/catalog.rs` — stat() pure unit tests

Commit: `test(apfs-core): catalog stat() pure unit tests — inode-not-found + regular-file + directory (Wave 9)`

- `build_inode_leaf` helper: 96-byte j_inode_val_t (92 fixed + 4-byte xf_blob_t header, num_exts=0), valid Fletcher-64 checksum.
- `stat_inode_not_found_returns_error`: leaf has inode 42, requesting 99 → ContainerError.
- `stat_regular_file_inode_returns_correct_fields`: S_IFREG|0644, UF_HIDDEN bsd_flags, no dstream → size=0.
- `stat_directory_inode_sets_is_dir`: S_IFDIR|0755 → is_dir=true.
- catalog.rs: 35.80% → 46.27% lines.

#### 2. `crates/apfs-core/src/decmpfs.rs` — method 7/8 + header error branches

Commit: `test(apfs-core): decmpfs method-7/8 branches + header short-parse (Wave 9)`

- `method7_ff_prefix_means_uncompressed_inline`: 0xFF prefix → raw bytes (no LZVN).
- `method7_ff_prefix_truncated_errors_not_panic`: 0xFF but claimed size > payload → error.
- `method8_single_uncompressed_chunk_with_0x06_marker`: resource fork with offset table + 0x06-prefixed chunk.
- `method8_missing_resource_fork_errors`: rf=None → MissingResourceFork.
- `method1_size_mismatch_errors`: inline payload shorter than uncompressed_size → error.
- `header_parse_too_short_errors`: buffer < 16 bytes → Parse error.
- decmpfs.rs: 66.7% → 95.56% lines.

#### 3. `crates/apfs-core/src/lzvn.rs` — remaining opcode coverage

Commit: `test(apfs-core): lzvn pre_d/sml_m/lrg_m/med_d/nop/udef opcode coverage (Wave 9)`

- `pre_d_reuses_previous_distance`: 0x86 opcode reuses d_prev=2 set by preceding sml_d.
- `sml_m_reuses_previous_distance`: 0xF2 match-only single byte opcode, reuses d_prev.
- `lrg_m_reuses_previous_distance_with_bias`: 0xF0 + M_byte=0 → m_len=16, splat via d_prev=1.
- `nop_opcode_is_skipped_silently`: 0x0E and 0x16 advance pos by 1 without affecting output.
- `udef_opcode_returns_error`: {0x1E,0x26,0x2E,0x36,0x3E,0x70,0x7F} all return ParseError.
- `med_d_opcode_roundtrip`: 0xA0 3-byte header, L=0 M=3 D=1 splat roundtrip.
- lzvn.rs: 58.4% → 82.75% lines.

### Top remaining gaps (Wave 10 targets)

| File | Lines% | Notes |
|---|---|---|
| `apfs-core/src/container.rs` | 35.02% | needs fixture or multi-block mock |
| `apfs-core/src/catalog.rs` | 46.27% | `list_xattrs`, `get_xattr` dstream path, `read_file_decompressed` — needs xattr leaf builder |
| `apfs-core/src/omap.rs` | 70.11% | `resolve` multi-level traversal |
| `apfs-core/src/block_device.rs` | 68.95% | CachingBlockDevice eviction + partial-write |
| `apfs-core/src/nx.rs` | 79.22% | checkpoint parse error branches |

---

## Wave 10 — 2026-05-26

Command: `cargo llvm-cov -p apfs-core --all-features --summary-only`

### Summary

42 new pure-unit tests across 5 files, 5 atomic commits. No fixture required.

| File | Lines% before | Lines% after | Delta |
|---|---|---|---|
| `container.rs` | 35.02% | **79.72%** | +44.7 pp |
| `volume.rs` | 88.52% | **95.87%** | +7.4 pp |
| `omap.rs` | 70.11% | **83.83%** | +13.7 pp |
| `catalog.rs` | 46.27% | **60.21%** | +13.9 pp |
| `free_runs.rs` (apfs-write) | 95.12% | **99.51%** | +4.4 pp |
| **apfs-core TOTAL** | **71.01%** | **79.49%** | **+8.5 pp** |

### Commits (5 atomic)

#### 1. `crates/apfs-core/src/omap.rs` — parse/open/resolve error branches

Commit: `test(omap): add synthetic unit tests for parse/open/resolve error branches`

- `omap_phys_parse_wrong_type_rejected`: block with NX_SUPERBLOCK type rejected.
- `omap_phys_parse_bad_checksum_rejected`: corrupt byte triggers checksum error.
- `omap_open_bad_root_type_rejected`: root block has OBJECT_TYPE_OMAP instead of BTREE.
- `omap_open_io_error_propagated`: missing tree root block → IO error propagated.
- `resolve_omap_val_deleted_returns_none`: OMAP_VAL_DELETED flag in leaf val → None.
- `resolve_no_match_returns_none`: empty leaf tree → None gracefully.
- omap.rs: 70.11% → 83.83% lines.

#### 2. `crates/apfs-core/src/volume.rs` — names_are_hashed, wrong type, all from_name variants

Commit: `test(volume): add synthetic tests for names_are_hashed, wrong obj type, all from_name variants`

- `rejects_wrong_object_type`: NX_SUPERBLOCK type rejected; wrong APFS magic (recomputed checksum) rejected.
- `names_are_hashed_synthetic`: CASE_INSENSITIVE (0x01) true, NORMALIZATION_INSENSITIVE (0x08) true, neither false.
- `volume_role_from_name_all_variants`: all 13 named arms, uppercase variants, unknown returns None.
- volume.rs: 88.52% → 95.87% lines.

#### 3. `crates/apfs-core/src/container.rs` — synthetic full container, all list/by-* paths

Commit: `test(container): add synthetic tests for list_volumes, by_uuid/role/index, container_uuid`

- `build_synthetic_container()`: fully in-memory APFS container (NX block 0+1, OmapPhys block 4, BTREE ROOT FIXED_KV leaf block 5, VolumeSuperblock block 7).
- `synthetic_list_user_volumes_returns_one_volume`
- `synthetic_list_volumes_returns_volume_info`
- `synthetic_open_volume_by_uuid_found` / `not_found`
- `synthetic_open_volume_by_role`
- `synthetic_volumes_by_role_empty_for_absent_role`
- `synthetic_open_volume_by_index_found_and_not_found`
- `synthetic_container_uuid_is_accessible`
- container.rs: 35.02% → 79.72% lines.

#### 4. `crates/apfs-core/src/catalog.rs` — list_xattrs, get_xattr embedded, open bad-type, depth guard

Commit: `test(catalog): add synthetic tests for list_xattrs, get_xattr embedded, open bad-type, depth guard`

- `build_xattr_leaf()`: var-kv leaf with one APFS_TYPE_XATTR XATTR_DATA_EMBEDDED entry.
- `list_xattrs_synthetic_returns_entry` / `wrong_inode_returns_empty`
- `get_xattr_embedded_returns_data` / `absent_returns_none`
- `for_each_leaf_depth_guard_terminates`: non-leaf node with omap-miss → oor error, no panic.
- `catalog_open_rejects_bad_root_object_type`: full multi-block MemDev (omap+root+bad-cat-root).
- catalog.rs: 46.27% → 60.21% lines.

#### 5. `crates/apfs-write/src/free_runs.rs` — zero/empty/capped edge cases

Commit: `test(free_runs): add edge-case tests for zero-n, empty pool, capped alloc_largest, free(0)`

- `alloc_exact_zero_returns_none`: n=0 must return None.
- `alloc_largest_zero_cap_returns_none`: cap=0 must return None.
- `alloc_largest_cap_smaller_than_run_splits_remainder`: cap=5 on run=20 takes 5, leaves 15.
- `alloc_exact_empty_pool_returns_none` / `alloc_largest_empty_pool_returns_none`.
- `free_zero_len_is_noop`: free(0,0) must not add spurious entries.
- free_runs.rs: 95.12% → 99.51% lines.

### Justified skips (Wave 10)

| Scope | Reason |
|---|---|
| `winfsp-host` | Windows-only DLL |
| `apfs-mount/main.rs`, `apfs-supervisor`, `apfs-tray` | Binary entrypoints, no unit path |
| `catalog.rs get_xattr dstream` | Requires XATTR_DATA_STREAM → FILE_EXTENT chain; fixture-only |
| `block_device.rs` WritableOffsetBlockDevice | Trait-bound compilation issue in llvm-cov binary |
| `catalog.rs read_file_decompressed` | Exercises decmpfs + ResourceFork xattr chain; real fixture only |

### Top remaining gaps (Wave 11 targets)

| File | Lines% | Notes |
|---|---|---|
| `apfs-core/src/catalog.rs` | 60.21% | `get_xattr` dstream path, `read_file_decompressed`, `list_dir` hashed variant |
| `apfs-core/src/block_device.rs` | 68.95% | CachingBlockDevice eviction + FaultyBlockDevice |
| `apfs-core/src/nx.rs` | 79.22% | checkpoint parse error branches |
| `apfs-core/src/lzvn.rs` | 82.75% | remaining nop/end-of-stream edge cases |

---

## Wave 10 — 2026-05-26 (coverage filler, 5 files)

**Method:** pure-Rust unit tests, no scratch image. 5 atomik commit.

### Tests added

| File | Test name | Branch covered |
|---|---|---|
| `container.rs` | `open_rejects_short_block0` | `Io` error variant from sub-4096 device |
| `container.rs` | `open_rejects_bad_checksum_in_block0` | `Parse` error from corrupted body byte |
| `container.rs` | `open_rejects_wrong_object_type_in_block0` | `Parse` when o_type != NX_SUPERBLOCK |
| `container.rs` | `open_ring_scan_picks_higher_xid_over_bootstrap` | ring scan selects xid=7 over xid=3 |
| `omap.rs` | `resolve_returns_none_when_only_key_has_higher_xid` | leaf `k_xid > xid` guard returns None |
| `omap.rs` | `resolve_depth_limit_returns_none_gracefully` | 33-level chain hits 32-iter cap, no panic |
| `omap.rs` | `resolve_picks_latest_xid_not_exceeding_query` | multi-version leaf: picks largest `k_xid <= xid` |
| `catalog.rs` | `list_xattrs_multiple_entries_same_inode` | two xattr entries for same inode, both returned |
| `catalog.rs` | `get_xattr_different_inode_returns_none` | correct name, wrong inode → None |
| `catalog.rs` | `get_xattr_wrong_name_same_inode_returns_none` | correct inode, wrong name → None |
| `inode.rs` | `parse_unknown_xfields_truncated_descriptor_array_errors` | `xf_num_exts=5` with 1 slot → Err |
| `inode.rs` | `parse_unknown_xfields_xsize_exceeds_remaining_data_errors` | `x_size=200`, 8 data bytes → Err |
| `inode.rs` | `jdstream_parse_all_fields_round_trip` | all five u64 JDstream fields verified |
| `volume.rs` | `parse_short_block_returns_err` | 100-byte block rejected |
| `volume.rs` | `parse_empty_block_returns_err` | empty slice rejected |
| `volume.rs` | `volume_name_nul_terminated_strips_correctly` | name stops at first NUL, ignores trailing garbage |
| `volume.rs` | `volume_name_no_nul_uses_full_area` | all-zero tail treated as NUL |
| `volume.rs` | `is_system_covers_all_system_roles` | all 5 system-role bits verified |

**Total new tests: 18** across 5 files, 5 atomic commits.

### Justified skips (Wave 10)

| Scope | Reason |
|---|---|
| `catalog.rs get_xattr dstream path` | Requires XATTR_DATA_STREAM + FILE_EXTENT chain; needs real fixture |
| `omap.rs` `>32 level` with valid leaf at depth 33 | Requires 33 checksummed blocks; depth-limit test covers the safety property |
