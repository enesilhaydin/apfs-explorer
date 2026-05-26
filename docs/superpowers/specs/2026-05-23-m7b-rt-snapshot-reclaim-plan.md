# M7b-RT — Snapshot reclaim subsystem — implementation plan

> Date: 2026-05-23. Task: #70. Branch: `feat/m7a-write-snapshot` (will likely
> branch a sub-branch once Subtask A is green).
> Spec sources (all answers locked):
> - KB `apfs-spec:omap-snapshot-versioning` (Q1-Q6)
> - KB `apfs-spec:cow-block-reclaim` (Q1-Q6)
> - `docs/superpowers/specs/2026-05-22-m7-rt-snapshot-omap-architecture-finding.md`

## Goal

Make `decisive_create_delete_snapshot_on_scratch` and
`repro_overalloc_snapshot_pinned` fsck CLEAN on scratch (and later real USB)
without `#[ignore]`, with no overallocation, no snapshot-invalid errors, and
zero regression on the 17 already-green decisive tests.

## TDD sub-tasks (each = red test → green code → verify → review)

### Subtask A — Single-omap architecture for snapshots
- Drop the separate "frozen omap_phys" path in `create_snapshot`.
- Frozen VSB's `apfs_omap_oid` points at the LIVE omap_phys (same paddr the
  live VSB uses, which is the current single omap).
- Keep populating `om_snapshot_tree` (subtype 0x13, key=`xid_t` 8B,
  val=`omap_snapshot_t` 16B = `oms_flags u32`, `oms_pad u32`, `oms_oid u64`)
  — already partially built by `build_omap_snapshot_tree_single`.
- `om_most_recent_snap = snap_xid`, `om_snap_count = number of entries in
  om_snapshot_tree`.
- TDD red: revive `decisive_create_delete_snapshot_on_scratch` (remove
  `#[ignore]`). Expect fsck-CLEAN container + volume after `create_snapshot`
  alone (no writes-after-snapshot yet). Iterate until green.

### Subtask B — Omap retention bounded by oldest snapshot xid
- Today: `rewrite_fstree` carries forward all prior `{oid,xid}` entries when
  ANY snapshot exists. That overretains.
- New: retention rule (Q1) = keep, for each oid, the LATEST version with
  `xid <= each active snapshot.xid` + the live version; drop intermediates.
- Active snapshots come from `om_snapshot_tree` (xid set, persistent).
- TDD red: write-then-snapshot-then-write triggers retention; confirm the
  pre-snapshot version is preserved, the intermediate (post-snap, pre-current)
  versions are pruned, fsck CLEAN.

### Subtask C — OMAP_VAL_DELETED tombstones (delete-while-snapshotted)
- When a virtual object is freed in `apply_omap_entries` but a snapshot still
  pins the previous version, write an omap entry with flag `OMAP_VAL_DELETED
  = 0x1` instead of removing the row. Read path interprets the tombstone as
  "absent in live view, present in snapshots ≤ tombstone.xid".
- TDD red: snapshot → unlink → fsck CLEAN; snapshot still mounts and lists
  the deleted file.

### Subtask D — Spaceman free queue (sm_fq) + reaper
- Replace immediate-free for snapshot-pinned blocks with deferred reclaim:
  enqueue `{xid, paddr}` into `sm_fq` (b-tree keyed by xid asc). On
  `delete_snapshot`, drain entries with `xid <= deleted_snap_xid` via the
  reaper (`OBJECT_TYPE_NX_REAPER`) and call `txn.free_block` for each.
- TDD red: `repro_overalloc_snapshot_pinned` becomes CLEAN; after
  `delete_snapshot` the freed extents return to the free pool (verified by
  delta in `nx_free_count`).

### Subtask E — Extentref refcount (`j_phys_ext_val_t.refcnt`)
- Today: live-extentref-presence is used as a single-snapshot ownership
  signal. Move to canonical: each `EXTENT` record's `refcnt` increments on
  snapshot (covering the extent) and decrements on `unlink`; only when
  `refcnt → 0` AND no snapshot pins the block do we enqueue it to `sm_fq`.
- TDD red: clone scenarios (two snapshots referencing same extents) — fsck
  CLEAN through both lifetimes.

### Subtask F — Un-ignore + USB validation
- Remove `#[ignore]` on the 2 snapshot tests once Subtasks A-E pass on
  scratch.
- Smoke on real USB (Apple_APFS, /dev/disk4s2) behind the existing safety
  guards.

## Per-subtask verification gates (CLAUDE.md workflow)

```
cargo test -p apfs-write --lib                                # baseline 46
cargo clippy -p apfs-write -- -D warnings                     # 0
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write \
  --test snapshot_scratch -- --test-threads=1                 # 17 green + N new
xtask verify-scratch (strict oracle)                          # CLEAN
```

After EACH subtask, atomic commit with explicit file list (NEVER `git add -A`).

## Safety / commitments

- `apfs-core` stays `#![forbid(unsafe_code)]`.
- No `unwrap`/`expect`/`panic!` in lib paths; all errors propagated.
- Commit messages English, plain text.
- `main` merge is irreversible — NOT proposed in this plan; only after all
  subtasks green and explicit user OK.
- md.6 holds: any genuine ambiguity → NotebookLM Q + real-disk empirical;
  never guess offsets/algorithms.

## First move (awaiting user OK)

Subtask A. Reversible (commit-per-step, branch protected by tag
`checkpoint/m7a-rt-realusb-clean-2026-05-22`).
