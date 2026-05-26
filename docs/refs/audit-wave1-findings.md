# Audit Wave-1 Findings

Surfaces audited: `apfs_vnop_pathconf`, `apfs_vnop_fsync`, `apfs_vnop_symlink`
(re-verify), `apfs_vnop_link`, `bt_split<K>`, `xf_set` / `xf_set_in_blob` /
`xf_get_from_blob`, `nx_find_free_blocks_callback`, `apfs_vnop_allocate`.

Raw working notes in `.cross-check-cache/audit-gap-wave1.md` (gitignored).

---

## Bugs found: NONE

No logic bugs surfaced in existing production code.

---

## Feature gaps (not bugs — existing code untouched)

### Gap 1 — Hard links (M9 #2) — risk: mid

`apfs_vnop_link` (2571 bytes) fully audited. On-disk requirements:
- `INODE_HAS_HARDLINKS` flag on the inode.
- `SIBLING_ID` xfield (type `0x0E`) on every DREC that participates in the
  hard-link set.
- `j_sibling_link_key` records mapping each sibling_id to the inode_id.
- Retroactive `create_sibling_link` wrapping the original single DREC when
  the first `link()` is issued.

We have zero of these. WinFsp surfaces hard links via `FileLinkInformation`
IRP. Implementation is gated on M9 #2 + winfsp-rs upstream support.
`xf_set` is also needed for the `SIBLING_ID` field writes — that codepath
gap is noted in the xf_set finding below.

### Gap 2 — DIRSTATS xfield not written on mutations (M9 #4) — risk: low/cosmetic

`xf_set` is called by `apfs_update_dir_stats_parent` on every create,
remove, and rename. We do not call the equivalent; directories with the
`DIRSTATS` xfield show stale `chained_size` / `total_size` / `gen_count`
after our mutations. fsck does not flag this; cosmetic for `getattrlist`
size queries. Already tracked as M9 #4.

### Gap 3 — SIBLING_ID xfield absent from DREC records — risk: low (until hard links)

`build_inode_xfields` / `build_xattr_val` do not emit `SIBLING_ID` xfields.
This is not a bug for single-link files (Apple also only writes SIBLING_ID
when `INODE_HAS_HARDLINKS` is set). Becomes mandatory once M9 #2 is
implemented.

---

## Deliberate divergences confirmed

| Surface | Our approach | Apple approach | Rationale |
|---|---|---|---|
| `apfs_vnop_fsync` | `FILE_FLAG_WRITE_THROUGH` + `FlushFileBuffers` | rw-lock + `apfs_fsync_internal` journal flush | Equivalent durability; kernel-thread model not applicable to userspace WinFsp. |
| `bt_split<K>` | No split; full-rebuild at ~75% fill | In-place 50%-fill midpoint split + parent propagation | Rebuild is simpler and correct; M8 #2 batching amortises O(catalog) cost. |
| `xf_set` grow path | Fresh blob per rebuild (no in-place grow) | `IOMallocData_external` + `memmove` grow | Rebuild eliminates the need for in-place mutation entirely. |
| `nx_find_free_blocks_callback` straddle loop | Split-across-runs (M8 opt #1) | Callback loop consuming the free-extents B-tree | Same allocation outcome; Rust `Result` propagation vs C `ctx.err` accumulation. |

---

## Non-goals confirmed

| Surface | Reason |
|---|---|
| `apfs_vnop_pathconf` | POSIX `pathconf(2)`; no WinFsp equivalent. |
| `apfs_vnop_allocate` | `fcntl(F_PREALLOCATE)` + macOS-private entitlement; no Windows IRP equivalent. |
| `nx_allocate_metadata_range` | Metadata alloc uses same code path as data alloc in our single-volume model; no separate path needed. |

---

## Matrix rows added (wave-1)

File-system operation matrix: 5 new rows (link ⏳, fsync ✅⚠️, pathconf 🚫,
allocate 🚫, symlink re-verify ✅).

Algorithm matrix: 4 new rows (bt_split ✅⚠️, xf_set ✅⚠️,
nx_find_free_blocks_callback ✅⚠️, hard-link xfield gap noted).

Pending table: 2 new rows (vnop_link mid-risk, nx_allocate_metadata_range low).

See `docs/refs/cross-check-matrix.md` for the full matrix.
