# M7a — Safe Write Foundation: COW Transaction Engine + Snapshot Create/Delete

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. This plan touches APFS ON-DISK WRITE structures: CLAUDE.md md.6 is IN FORCE — at any genuine ambiguity (struct/offset/order/algorithm where sources conflict or you are unsure) STOP, surface a copyable NotebookLM question to the user, WAIT for the answer, lock it into KB `apfs-spec:m7a-confirmed`, then continue. The final arbiter is a real macOS disk (here: a scratch APFS image + the macOS kernel + `fsck_apfs`), not any document.

**Goal:** Build the minimal Apple-correct copy-on-write write engine and prove it by creating and deleting an APFS snapshot on a disposable scratch image, verified byte-correct and consistent by the macOS kernel itself — establishing the safe-write foundation (no in-place mutation; atomic transaction commit; verify-before-trust + rollback) that later file-data write (M7b) builds on.

**Why snapshot first:** Apple APFS is copy-on-write — nothing is ever overwritten in place; every change allocates new blocks, writes new object versions (each with a Fletcher-64 header), updates the object map at a new transaction id (xid), appends a new checkpoint, and atomically installs a new container superblock. A snapshot is the smallest *Apple-native, non-destructive* operation that exercises this ENTIRE machinery (spaceman allocation → new objects → omap → checkpoint → atomic nx_superblock flip), and it is trivially reversible (delete the snapshot / discard the image). It is the correct, lowest-blast-radius first write.

**Safety model (non-negotiable, applies to every write):**
1. Writes ONLY to a freshly generated **disposable scratch APFS image** (`xtask gen-scratch`). NEVER a real USB / the user's data until M7+ explicitly authorizes it after extensive proof.
2. **Snapshot-before**: capture the pre-write state (the scratch image is copied; the APFS pre-state is also a natural rollback point).
3. **Verify-before-trust** after every committed transaction: (a) macOS `fsck_apfs -n` reports the volume CLEAN; (b) macOS kernel can mount it and the snapshot is listed by `diskutil apfs listSnapshots`; (c) our own reader re-reads it; (d) an independent **zero-shared-code Python** re-decode of the changed structures. Any disagreement ⇒ FAIL the test, do NOT proceed, treat as a real bug.
4. **Rollback**: on any verify failure the scratch image is discarded (and the code path must also support APFS-level revert); the user's data is never at risk because only scratch images are ever opened writable.

**Architecture:** `apfs-write` crate. A `Transaction` accumulates new/modified objects in memory, allocates free blocks via a read-only-then-write space-manager view, writes each object with a correct `obj_phys_t` (Fletcher-64, new `o_xid`), updates the object map B-tree for virtual objects, appends a checkpoint (descriptor + data), and **atomically** writes the new `nx_superblock` last (the single commit point). Snapshot create = add the snapshot-metadata records + bump the volume's snapshot structures within one `Transaction`. All on-disk write structures go through md.6 if ambiguous. `apfs-write` depends on `apfs-core` (parsing/Fletcher reused, never duplicated) and is `#![forbid(unsafe_code)]`-aligned with the workspace lints (no `unwrap/expect/panic` in production).

**Tech Stack:** Rust 2021; reuse `apfs-core` (obj_phys, Fletcher-64, B-tree, omap, spaceman parse); macOS `hdiutil`/`diskutil apfs`/`fsck_apfs` for scratch-image gen + the kernel oracle; Python3 for the independent re-decode. Cross-platform (the writer + scratch-image flow run on the macOS dev host — this milestone needs NO Windows box).

**Spec sources (HYPOTHESIS; real scratch disk + macOS kernel = arbiter):** `apfs-spec:pdf-verbatim` (checkpoint, spaceman, omap, snapshot metadata, object lifecycle), `apfs-spec:libfsapfs`, Context7 `/pradorocchi/apfs-filesystem-full-reference`, and **`apfs-spec:linux-apfs-rw`** — the linux-apfs-rw kernel driver, the only open-source APFS *writer*; index on-demand: `ctx_fetch_and_index("https://raw.githubusercontent.com/linux-apfs/linux-apfs-rw/master/<file>", "apfs-spec:linux-apfs-rw")` for `super.c` (commit/checkpoint), `spaceman.c` (allocation), `snapshot.c` (snap metadata), `btree.c`/`object.c` (COW). NotebookLM `apfs-reference-apple-file-syst` for md.6.

---

## File Structure

- `crates/apfs-write/Cargo.toml` + `src/lib.rs` — crate wiring.
- `crates/apfs-write/src/scratch.rs` — pure helpers for scratch-image geometry; the actual image gen is an xtask shell-out.
- `crates/apfs-write/src/txn.rs` — `Transaction`: object staging, block allocation view, Fletcher-64 object write, omap update, checkpoint append, atomic superblock commit.
- `crates/apfs-write/src/snapshot.rs` — `create_snapshot` / `delete_snapshot` building the snapshot-metadata mutations on a `Transaction`.
- `crates/apfs-write/tests/snapshot_scratch.rs` — env-gated decisive test: gen scratch → create snapshot → fsck clean + kernel lists it + our reader + Python re-decode → delete → fsck clean.
- `xtask/src/main.rs` — `gen-scratch` (make a disposable APFS image via hdiutil) + `verify-scratch` (fsck_apfs -n + diskutil listSnapshots) subcommands.
- `tools/redecode_snapshot.py` — standalone zero-shared-code APFS snapshot re-decoder (independent evidence).
- `docs/ROADMAP.md` — M7a status (final task).
- KB: `apfs-spec:m7a-confirmed` (md.6 locked write-structure facts).

Pure logic (txn block-allocation math, checkpoint-ring index math, Fletcher via apfs-core) is unit-tested in isolation; the end-to-end correctness is the macOS-kernel-verified scratch test.

---

### Task 0: Spec gather + md.6 gate (DO THIS FIRST — no code until ambiguities are locked)

**Files:** none (research → KB `apfs-spec:m7a-confirmed`)

- [ ] **Step 1: Gather the write-path spec from all sources.**
`ctx_search(source:"apfs-spec:pdf-verbatim", queries:[...])` + `apfs-spec:libfsapfs` + Context7 `/pradorocchi/apfs-filesystem-full-reference` + `ctx_fetch_and_index` linux-apfs-rw `super.c`,`spaceman.c`,`snapshot.c`,`btree.c` → `apfs-spec:linux-apfs-rw`. Extract, with page/line citations, the EXACT mechanics of: (a) checkpoint commit — descriptor area ring (`nx_xp_desc_base/_blocks/_next/_index/_len`), data area, what objects go where, the **order** of writes and that `nx_superblock` is written LAST with `o_xid == nx_next_xid`, then `nx_next_xid`++; (b) ephemeral vs physical vs virtual object handling on write; (c) spaceman free-block allocation + how the allocation/bitmap/free-queue is updated in a transaction; (d) omap B-tree insert of a new `{oid,xid}→paddr` for each modified virtual object; (e) **snapshot create**: `j_snap_metadata`/`j_snap_metadata_key`, the volume superblock snapshot fields (`apfs_snap_meta_tree_oid`, `apfs_revert_to_xid`, `apfs_num_snapshots`), the snapshot metadata tree, and the extent-reference tree implications; `snap_meta_ext`; (f) **snapshot delete** mechanics.

- [ ] **Step 2: Identify genuine ambiguities and run md.6.**
For EVERY write structure/offset/order where the Apple PDF is silent/ambiguous or sources conflict (expected: exact checkpoint write-order, snapshot-metadata tree linkage, extent-ref-tree obligations, spaceman free-queue update on alloc), prepare a precise, copyable NotebookLM question, present it to the USER, and WAIT for the answer. Do not guess. Lock each confirmed answer into KB `apfs-spec:m7a-confirmed` with the source + the dual-evidence plan (controlled-value + independent reverse-decode) noted.

- [ ] **Step 3: Write the locked spec summary** to `docs/superpowers/specs/2026-05-19-m7a-write-spec-notes.md` (committed) — the durable, citation-backed write-structure reference the tasks below depend on. Include the exact field offsets/order, the checkpoint commit sequence, and the snapshot create/delete record set.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-05-19-m7a-write-spec-notes.md
git commit -m "docs(m7a): locked APFS write/checkpoint/snapshot spec notes (md.6, real-disk-arbiter pending)"
```

> Tasks 1+ MUST cite this notes file / `apfs-spec:m7a-confirmed`. If a later task hits a NEW ambiguity, return to md.6 (ask-wait-lock) before coding it.

---

### Task 1: `apfs-write` crate skeleton + scratch-image geometry (pure, tested)

**Files:**
- Modify: `crates/apfs-write/Cargo.toml`, `crates/apfs-write/src/lib.rs`
- Create: `crates/apfs-write/src/scratch.rs`

- [ ] **Step 1: Write the failing test** — `crates/apfs-write/src/scratch.rs` with a pure `ScratchSpec` (image size, block size 4096, min container size that APFS accepts) + validation, and unit tests asserting: 4096 block size, size rounds up to block multiple, rejects sub-minimum sizes. (Exact minimum-APFS-container size is confirmed empirically in Task 2 by whether `hdiutil`/`diskutil apfs` accepts it; the pure helper just enforces block alignment + a conservative floor cited from Task 0 notes.) Replace `lib.rs` with module wiring + crate-root `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]`; `Cargo.toml` deps `apfs-core = { path = "../apfs-core" }`.

- [ ] **Step 2: Run** `cargo test -p apfs-write scratch` — FAIL (module absent), then implement minimal `ScratchSpec`.

- [ ] **Step 3: Implement** the pure `ScratchSpec` per Task 0 notes (block-align, floor).

- [ ] **Step 4: Verify** `cargo test -p apfs-write` PASS; `cargo build --workspace` + `cargo clippy -p apfs-write --all-targets -- -D warnings` clean.

- [ ] **Step 5: Commit**

```bash
git add crates/apfs-write/Cargo.toml crates/apfs-write/src/lib.rs crates/apfs-write/src/scratch.rs
git commit -m "feat(apfs-write): crate skeleton + pure scratch-image geometry"
```

---

### Task 2: `xtask gen-scratch` / `verify-scratch` (the macOS kernel oracle)

**Files:**
- Modify: `xtask/src/main.rs`

- [ ] **Step 1:** Add `gen-scratch <path> [size_mb]`: shell out to `hdiutil create -size <N>m -fs APFS -volname APFSWRITE -layout GPTSPUD <path>` (or `diskutil` equivalent confirmed working on this macOS), producing a real disposable APFS-in-GPT image; print the device-relative APFS container byte offset (reuse the M4b GPT logic conceptually) and verify our reader can open it read-only first. Add `verify-scratch <path>`: attach read-only, run `fsck_apfs -n` on the APFS volume device, run `diskutil apfs listSnapshots`, print `CLEAN`/`DIRTY` + snapshot list, detach. (Exact macOS commands resolved empirically — the macOS tooling is the arbiter; record the working invocation in code comments.)

- [ ] **Step 2:** Run `cargo run -p xtask -- gen-scratch /tmp/apfs-scratch.img 64` then `cargo run -p xtask -- verify-scratch /tmp/apfs-scratch.img` — Expected: image created, `fsck_apfs -n` CLEAN, zero snapshots, detached cleanly. Confirm our reader opens it (lists root).

- [ ] **Step 3: Commit**

```bash
git add xtask/src/main.rs
git commit -m "feat(xtask): gen-scratch + verify-scratch (disposable APFS image + fsck/kernel oracle)"
```

---

### Task 3: `Transaction` — COW object staging, allocation, Fletcher write, checkpoint commit

**Files:**
- Create: `crates/apfs-write/src/txn.rs`

This is the core engine. Implement strictly per `apfs-spec:m7a-confirmed` (Task 0). If ANY sub-step is ambiguous → md.6 before coding it.

- [ ] **Step 1: Write the failing test** — unit tests (no disk) for the pure pieces: checkpoint-ring index advance math (wrap within `nx_xp_desc_blocks`), block-allocation bump from a free range, `obj_phys` serialization with a recomputed Fletcher-64 (cross-checked against `apfs-core`'s verified Fletcher on a known buffer), and that `commit()` orders writes as: all new data/metadata objects → omap updates → checkpoint data → checkpoint descriptor → **`nx_superblock` LAST** with `o_xid = next_xid`. Tests assert ordering via a recording fake `BlockDevice` (writes captured in sequence).

- [ ] **Step 2: Run** `cargo test -p apfs-write txn` — FAIL, then implement.

- [ ] **Step 3: Implement** `Transaction` over a writable `BlockDevice` (extend the trait with `write_at`, or a `WritableBlockDevice` supertrait, in `apfs-core`; keep `apfs-core` `#![forbid(unsafe_code)]`, no panics). Allocation uses a spaceman free view (read via apfs-core) + an in-txn bump allocator over confirmed-free blocks (full free-queue update per Task 0 notes). Every written object gets `obj_phys_t` with correct `o_oid/o_xid/o_type/o_subtype` and Fletcher-64 via `apfs_core`. Commit writes superblock last (the atomic point) and increments `nx_next_xid/nx_next_oid`.

- [ ] **Step 4: Verify** `cargo test -p apfs-write` PASS; clippy `-D` clean; `cargo build --workspace` clean.

- [ ] **Step 5: Commit**

```bash
git add crates/apfs-write/src/txn.rs crates/apfs-core/src/block_device.rs
git commit -m "feat(apfs-write): COW Transaction (alloc + Fletcher object write + atomic checkpoint commit)"
```

---

### Task 4: `create_snapshot` / `delete_snapshot`

**Files:**
- Create: `crates/apfs-write/src/snapshot.rs`

- [ ] **Step 1: Write the failing test** — unit test that `create_snapshot` stages exactly the record set from `apfs-spec:m7a-confirmed` (snapshot-metadata key/val, volume superblock snapshot field deltas, snapshot-meta tree linkage) onto a `Transaction` (assert against a captured staging list), and `delete_snapshot` stages the inverse. No disk yet.

- [ ] **Step 2: Run** `cargo test -p apfs-write snapshot` — FAIL, then implement per the locked spec (md.6 if any new ambiguity).

- [ ] **Step 3: Implement** `create_snapshot(&mut Transaction, name)` / `delete_snapshot(&mut Transaction, name)`.

- [ ] **Step 4: Verify** unit tests PASS; clippy `-D`; workspace build.

- [ ] **Step 5: Commit**

```bash
git add crates/apfs-write/src/snapshot.rs
git commit -m "feat(apfs-write): snapshot create/delete record staging on a Transaction"
```

---

### Task 5: DECISIVE end-to-end scratch test (macOS kernel + Python independent re-decode)

**Files:**
- Create: `crates/apfs-write/tests/snapshot_scratch.rs`, `tools/redecode_snapshot.py`

- [ ] **Step 1: Write the test** (env-gated `APFS_WRITE_SCRATCH=1`, skips gracefully otherwise): (1) `xtask gen-scratch /tmp/apfs-wr.img 64`; copy it (rollback baseline). (2) Open writable, `create_snapshot("t1")`, `commit()`. (3) **Verify-before-trust:** assert `fsck_apfs -n` = CLEAN; assert `diskutil apfs listSnapshots` lists `t1`; assert our reader still mounts + lists root unchanged; run `tools/redecode_snapshot.py` (zero shared code — its own struct parsing) and assert it independently finds snapshot `t1` with the expected xid/metadata. (4) `delete_snapshot("t1")`, `commit()`; assert fsck CLEAN + snapshot gone + reader still good. (5) Any mismatch → restore the copied baseline (rollback) and FAIL loudly.

- [ ] **Step 2: Run** `APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_scratch -- --nocapture`. Iterate against the macOS kernel as the arbiter. If `fsck_apfs` reports DIRTY or the kernel/Python disagree → it is a real write bug; root-cause against `apfs-spec:m7a-confirmed` (md.6 re-ask if a structure assumption was wrong); NEVER weaken the assertions. Expected end state: fsck CLEAN before+after, kernel + our reader + Python all agree, rollback path exercised.

- [ ] **Step 3: Commit**

```bash
git add crates/apfs-write/tests/snapshot_scratch.rs tools/redecode_snapshot.py
git commit -m "test(apfs-write): decisive scratch snapshot proof (fsck + kernel + zero-shared-code Python)"
```

---

### Task 6: Milestone-end review + decisive cross-check + ROADMAP + finish

- [ ] **Step 1:** One independent consolidated review of the whole `feat/m7a-write-snapshot` branch: spec-compliance (matches `apfs-spec:m7a-confirmed`; md.6 was actually used for ambiguities, answers locked), quality (no `unwrap/expect/panic` in prod; `apfs-core` still `#![forbid(unsafe_code)]`; writer never opens a non-scratch device in tests), crash-safety (commit is atomic — superblock written last; partial-write before the superblock leaves the prior valid filesystem, since old superblock still points at old objects — reason this through explicitly), gates (`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace`). Plus the milestone decisive cross-check: re-run the scratch proof and confirm `fsck_apfs` CLEAN + Python independent agreement (the real-disk arbiter).

- [ ] **Step 2:** Update `docs/ROADMAP.md`: M7a status row + squash commit; durable notes (locked write/checkpoint/snapshot facts pointer to the spec-notes file + `apfs-spec:m7a-confirmed`; scratch-only safety model; that real-USB write stays FORBIDDEN until a future milestone explicitly authorizes it); next = **M7b** (COW write of existing file data, same safety model) then M8. Re-index `project:roadmap-state`.

- [ ] **Step 3:**

```bash
git add docs/ROADMAP.md
git commit -m "docs(roadmap): M7a safe-write foundation + snapshot complete; next M7b"
```

Then superpowers:finishing-a-development-branch: verify gates → squash-merge `feat/m7a-write-snapshot` → main → delete branch → fill `<squash>` SHA → re-index KB.

---

## Self-Review

**Spec coverage:** M7 ROADMAP = `write` feature, verify-before-commit (snapshot→write→re-read→checksum→rollback), linux-apfs-rw reference, real-disk arbiter + md.6. This plan delivers the Apple-correct COW transaction engine (Task 3) + the smallest non-destructive Apple-native write, snapshot (Task 4), with the full safety harness (Task 2 scratch+fsck oracle, Task 5 quadruple verify + rollback) and md.6 front-loaded (Task 0). File-data write is the explicit follow-on M7b (same harness). Real-USB write is explicitly forbidden until later — matches the user's "scratch only" decision.

**Placeholder scan:** no TODO/placeholder. The two deliberate deferrals are (a) exact macOS `hdiutil`/`fsck_apfs` invocations (resolved empirically — macOS tooling is the arbiter, with a concrete verify step) and (b) on-disk write-structure exactness, which is explicitly routed through md.6 (Task 0) and the macOS-kernel decisive proof (Task 5) — the project's mandated methodology, not hand-waving. Tasks 3/4 are gated on Task 0's locked notes.

**Type consistency:** `Transaction` is created over a writable block device, accumulates objects, `commit()` writes superblock last; `create_snapshot/delete_snapshot(&mut Transaction, name)` stage records consumed by `commit()`; the scratch test drives gen-scratch → Transaction → verify, consistently. The `apfs-core` writable-device extension is introduced in Task 3 and used by `txn.rs` only.

**Scope:** one milestone = the safe-write foundation + snapshot, deliberately the smallest Apple-native non-destructive write; md.6 + scratch-only + quadruple verify make the inherent risk safe; M7b/M8 explicitly carry the rest. Splitting snapshot (M7a) from file-data write (M7b) is risk-driven, not artificial — M7a de-risks the engine before any user-file mutation.
