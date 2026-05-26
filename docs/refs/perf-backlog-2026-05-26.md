# Perf Backlog — 2026-05-26

Wave 2 sweep completed 2026-05-26. Items 1, 2, 7 applied (commits
`05d4d3f`, `9166a9c`). Items 3, 4, 5, 6 remain deferred — reasons below.

---

## 1. `listing()` → entry-API to avoid double HashMap lookup (apfs/src/lib.rs:171)

**Pattern:** `contains_key` + `insert` — two hash lookups on cache miss.
**Fix applied (commit `05d4d3f`):** `Entry::Vacant` pattern — single lookup
on cache miss. `or_try_insert_with` (nightly) not needed; stable `match`
on `entry()` is sufficient and cleaner.
**Status:** ✅ DONE

---

## 2. `txn.rs` dirty_bitmaps `contains_key` + `insert` (apfs-write/src/txn.rs:1577, 1721)

**Pattern:** Double-lookup in both alloc_blocks and free_block hot paths.
**Fix applied (commit `9166a9c`):** `Entry::Vacant` in both sites. Error
paths (`continue` / `return Err`) handled inside the `if let` arm — no
helper extraction needed; 4 lines each, cleaner than original.
**Status:** ✅ DONE

---

## 3. `read_dir()` clones full `&[DirEntry]` on every call (apfs/src/lib.rs:228)

**Pattern:** `self.listing(ino)?.to_vec()` — clones entire cached Vec.
**Fix options:** (A) change signature to `-> &[DirEntry]`; (B) `Arc<[DirEntry]>` in cache.
**Why skipped:** Both require propagating the API change to winfsp-host
`mount.rs` callers (and potentially other consumers). Not a single-file
change; risk of behavioral regression in WinFsp callback wiring.
**Apply when:** M9 WinFsp API cleanup milestone, with full mount.rs audit.

---

## 4. `catalog.rs` — full B-tree scan for every lookup (apfs-core/src/catalog.rs)

`for_each_leaf` does a full linear scan of the B-tree for every stat/
read_file/get_xattr call.
**Why skipped:** ~60 LoC new traversal (`search_key` variant), requires
full understanding of `cat_key_cmp` comparison semantics and B-tree
node structure. Architectural — correctness risk if key comparison is wrong.
**Apply when:** Dedicated M8/M9 B-tree optimization milestone with benchmark
baseline and differential fixture testing.

---

## 5. `pivot` clone on every b-tree node split (apfs-write/src/file.rs)

Lines 2053, 2060, 2086, 2593, 2600, 2618: `chunk.first().map(|(k, _)| k.clone())`.
**Why skipped:** Cold path (catalog rebuilds on write only). Eliminating
the clone requires changing the split return type — architectural. ROI
negligible vs. risk.
**Apply when:** Never — cold path, type change not worth it.

---

## 6. `Vec::new()` + push loops without `with_capacity` (various)

**Why skipped:** No hot-path Vec::new() + push without sizing hint found.
Write path sizes are runtime-only. Rule of Three not met — speculative
abstraction.
**Status:** No action required.

---

## 7. `HashMap` default capacity in `FsView` (apfs/src/lib.rs)

**Fix applied (commit `05d4d3f`):** `path_cache: HashMap::with_capacity(64)`,
`dir_cache: HashMap::with_capacity(16)`. Avoids 4–5 rehashes on a typical
mount. Safe — no behavior change, capacity is a hint only.
**Status:** ✅ DONE
