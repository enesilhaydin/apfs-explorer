# M3-RT — Real-Target Differential Harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`). **Lean consolidated process (CLAUDE.md "Süreç (konsolide)"):** per task = implementer (TDD red→green, self-test once) + ONE consolidated independent review (spec + quality + gates; reviewer reads the diff and runs gates). Milestone end = 1 final review + the decisive real-target run when an APFS target is provided.

**Goal:** Prove the pure-Rust reader against a real APFS volume (USB or disk image) by walking every file/dir and byte-comparing our output to the macOS kernel's own read (the ground-truth oracle), with a data budget for slow media and clean skip when no target is provided.

**Architecture:** A new env-gated integration test `crates/apfs-core/tests/real_target_differential.rs` opens a read-only APFS container image with our reader and walks the same volume as macOS-mounted (the oracle); a new `Catalog::resolve_path` maps a filesystem path to an inode by descending `list_dir` from the root. No new APFS parsing — this reuses M3a–M3d. An optional `xtask rt-snapshot` resolves the APFS container device from a mount point and prints the exact one-time `sudo dd` command + env vars (no sudo inside cargo; the slow USB is read sequentially once into a fast local image).

**Tech Stack:** Rust (`apfs-core` integration test, `xtask`), std only (no new deps; recursive `std::fs` walker), macOS `diskutil`/`dd` for the one-time snapshot (run by the user).

**This is a TEST-HARNESS milestone:** no APFS on-disk spec work; no md.6 / NotebookLM. The oracle is the macOS kernel itself. Spec/methodology rules: `CLAUDE.md`, `docs/ROADMAP.md`.

**Mandatory rules:** `git -C /Users/eiaydin/Documents/projects/apfs-explorer` (no `cd`); NEVER `git add -A`/`git add .` — explicit filenames; English code/comments/commits; `apfs-core` stays `#![forbid(unsafe_code)]`; no `unwrap`/`expect`/`panic!`/raw indexing in **production** code (`crates/apfs-core/src/**`) — integration tests under `crates/apfs-core/tests/` and `xtask` may use `expect`/`?` freely (they are test/tool code, not the `#![forbid(unsafe_code)]` library surface); the harness MUST never panic on a bad file — it records a failure and continues, then asserts zero failures at the end (crash-safety: a real volume must not crash the reader). clippy `--all-targets --all-features -- -D warnings` green; commit trailer (exact last line) `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`. Branch: `feat/m3-rt-differential`.

**Gate command:**
```
cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --workspace && \
cargo clippy --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --workspace --all-targets --all-features -- -D warnings && \
cargo fmt --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --all --check
```

---

## File Structure

- **Modify** `crates/apfs-core/src/catalog.rs` — add `pub fn resolve_path` to `impl Catalog` + an inline unit test using the existing `apfs-large.img` fixture.
- **Create** `crates/apfs-core/tests/real_target_differential.rs` — env-gated integration test: open image, walk macOS-mounted oracle tree, byte-diff per the budget, compare dir listings, summary, assert zero mismatches; skip cleanly when env unset/missing.
- **Modify** `xtask/src/main.rs` — add an `rt-snapshot <mount-point>` subcommand: resolve the APFS container device via `diskutil info`, print the exact one-time `sudo dd` command + the `APFS_RT_*` env exports. Does NOT run sudo.

Existing API reused (do NOT change behavior): `Catalog::open`, `list_dir(dir_inode,names_hashed)→Vec<DirEntry>`, `read_file_decompressed(inode)→Result<Vec<u8>,ContainerError>`, `DirEntry{name,file_id,flags}`, `crate::jkey::ROOT_DIR_INO_NUM` (=2), `vol.names_are_hashed()`, `Container::open`, `c.list_user_volumes`, `c.superblock.block_size`, `FileBlockDevice::open`.

---

## Task 1: Catalog::resolve_path + unit test (on the existing fixture)

**Files:**
- Modify: `crates/apfs-core/src/catalog.rs`

- [ ] **Step 1: Add a failing unit test**

In the existing `#[cfg(test)] mod tests` block in `crates/apfs-core/src/catalog.rs`, add:

```rust
    #[test]
    fn resolve_path_navigates_real_fixture() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-large.img"
        );
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing — run `cargo run -p xtask -- gen-fixture --large`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("open container");
        let bsz = c.superblock.block_size;
        let vol = c
            .list_user_volumes(&mut dev)
            .expect("list volumes")
            .first()
            .expect("user volume")
            .clone();
        let cat = Catalog::open(&mut dev, &vol, bsz).expect("open catalog");
        let hashed = vol.names_are_hashed();

        // dir00 may be absent on a size-only fixture; then this is a no-op skip.
        if cat
            .resolve_path(&mut dev, &["dir00"], hashed)
            .expect("resolve dir00")
            .is_none()
        {
            eprintln!("skip: size-only fixture — regenerate gen-fixture --large");
            return;
        }
        let ino = cat
            .resolve_path(&mut dev, &["dir00", "f000.bin"], hashed)
            .expect("resolve dir00/f000.bin")
            .expect("dir00/f000.bin must exist");
        let content = cat
            .read_file_decompressed(&mut dev, ino)
            .expect("read resolved file");
        assert_eq!(content.len(), 4096);
        assert!(content.iter().all(|&b| b == b'A'));

        // Missing components resolve to None (no panic).
        assert!(cat
            .resolve_path(&mut dev, &["dir00", "does-not-exist"], hashed)
            .expect("resolve missing")
            .is_none());
        assert!(cat
            .resolve_path(&mut dev, &["nope", "x"], hashed)
            .expect("resolve missing parent")
            .is_none());
        // Empty path resolves to the root directory inode.
        assert_eq!(
            cat.resolve_path(&mut dev, &[], hashed).expect("resolve root"),
            Some(crate::jkey::ROOT_DIR_INO_NUM)
        );
    }
```

- [ ] **Step 2: Run it — confirm it fails**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core resolve_path_navigates_real_fixture`
Expected: compile error — `no method named resolve_path`.

- [ ] **Step 3: Implement `resolve_path`**

In `crates/apfs-core/src/catalog.rs`, inside `impl Catalog`, directly after `list_dir`, add:

```rust
    /// Resolve a slash-free path given as components (e.g. ["a","b","c"])
    /// to its inode number by descending `list_dir` from the root directory.
    /// Empty `components` → the root inode. Returns `Ok(None)` if any
    /// component is absent. Does not follow symlinks.
    pub fn resolve_path<D: BlockDevice>(
        &self,
        dev: &mut D,
        components: &[&str],
        names_hashed: bool,
    ) -> Result<Option<u64>, ContainerError> {
        let mut ino = crate::jkey::ROOT_DIR_INO_NUM;
        for comp in components {
            let entries = self.list_dir(dev, ino, names_hashed)?;
            match entries.iter().find(|e| e.name == *comp) {
                Some(e) => ino = e.file_id,
                None => return Ok(None),
            }
        }
        Ok(Some(ino))
    }
```

- [ ] **Step 4: Run the test → PASS**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core resolve_path_navigates_real_fixture -- --nocapture`
Expected: PASS un-skipped on the populated fixture.

- [ ] **Step 5: Full gate**

Run the **Gate command**. Expected: all green; apfs-core test count = previous 43 + 1 = 44.

- [ ] **Step 6: Commit**

```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/catalog.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat: M3-RT catalog — Catalog::resolve_path (path -> inode descent)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Real-target differential integration test + xtask rt-snapshot

**Files:**
- Create: `crates/apfs-core/tests/real_target_differential.rs`
- Modify: `xtask/src/main.rs`

- [ ] **Step 1: Create the integration test**

Create `crates/apfs-core/tests/real_target_differential.rs` with EXACTLY:

```rust
//! Real-target differential: open a real APFS container image with our reader
//! and byte-compare every file against the macOS kernel's own read of the
//! same volume (the ground-truth oracle). Opt-in, env-gated, data-budgeted.
//!
//! Run (after `xtask rt-snapshot` produced the image):
//!   APFS_RT_IMAGE=/tmp/apfs-rt.img APFS_RT_MOUNT=/Volumes/TEST \
//!     cargo test -p apfs-core --test real_target_differential -- --nocapture
//!
//! Env knobs (bytes): APFS_RT_FULL_MAX (default 4194304), APFS_RT_PREFIX
//! (1048576), APFS_RT_TAIL (262144), APFS_RT_BUDGET (536870912).
use apfs_core::block_device::FileBlockDevice;
use apfs_core::catalog::Catalog;
use apfs_core::container::Container;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

// Directory names that are volume/OS bookkeeping, not user data; macOS
// exposes them on the mount but they are not meaningful to differential-check.
const NOISE: &[&str] = &[
    ".fseventsd",
    ".Spotlight-V100",
    ".Trashes",
    ".DocumentRevisions-V100",
    ".TemporaryItems",
    ".PKInstallSandboxManager-SystemSoftware",
    ".apdisk",
];

struct Budget {
    full_max: usize,
    prefix: usize,
    tail: usize,
    remaining: i64,
}

#[test]
fn real_target_differential() {
    let img = match std::env::var("APFS_RT_IMAGE") {
        Ok(v) => v,
        Err(_) => {
            eprintln!(
                "skip: APFS_RT_IMAGE unset — `cargo run -p xtask -- rt-snapshot \
                 /Volumes/<vol>` prints how to make the image + env vars"
            );
            return;
        }
    };
    let mount = match std::env::var("APFS_RT_MOUNT") {
        Ok(v) => v,
        Err(_) => {
            eprintln!("skip: APFS_RT_MOUNT unset (macOS mount path = the oracle)");
            return;
        }
    };
    if !Path::new(&img).exists() || !Path::new(&mount).is_dir() {
        eprintln!("skip: APFS_RT_IMAGE or APFS_RT_MOUNT path missing");
        return;
    }

    let mut dev = FileBlockDevice::open(&img).expect("open APFS_RT_IMAGE");
    let c = Container::open(&mut dev).expect("open container from image");
    let bsz = c.superblock.block_size;
    let vols = c.list_user_volumes(&mut dev).expect("list user volumes");
    let vol = vols.first().expect("at least one user volume").clone();
    let cat = Catalog::open(&mut dev, &vol, bsz).expect("open catalog");
    let hashed = vol.names_are_hashed();

    let mut b = Budget {
        full_max: env_usize("APFS_RT_FULL_MAX", 4 * 1024 * 1024),
        prefix: env_usize("APFS_RT_PREFIX", 1024 * 1024),
        tail: env_usize("APFS_RT_TAIL", 256 * 1024),
        remaining: env_usize("APFS_RT_BUDGET", 512 * 1024 * 1024) as i64,
    };

    let mut files_ok = 0u64;
    let mut bytes_cmp = 0u64;
    let mut dirs_ok = 0u64;
    let mut skipped_symlink = 0u64;
    let mut skipped_special = 0u64;
    let mut fails: Vec<String> = Vec::new();

    // Iterative DFS over the oracle mount. `rel` = components from the volume root.
    let mut stack: Vec<(PathBuf, Vec<String>)> = vec![(PathBuf::from(&mount), Vec::new())];
    while let Some((dir_abs, rel)) = stack.pop() {
        // Compare directory child-name sets (excluding NOISE).
        let oracle_names: BTreeSet<String> = match fs::read_dir(&dir_abs) {
            Ok(rd) => rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| !NOISE.contains(&n.as_str()))
                .collect(),
            Err(e) => {
                fails.push(format!("oracle read_dir {dir_abs:?}: {e}"));
                continue;
            }
        };
        let comps: Vec<&str> = rel.iter().map(|s| s.as_str()).collect();
        let dir_ino = match cat.resolve_path(&mut dev, &comps, hashed) {
            Ok(Some(i)) => i,
            Ok(None) => {
                fails.push(format!("reader: dir path not found: /{}", rel.join("/")));
                continue;
            }
            Err(e) => {
                fails.push(format!("reader resolve_path /{}: {e:?}", rel.join("/")));
                continue;
            }
        };
        let our_names: BTreeSet<String> = match cat.list_dir(&mut dev, dir_ino, hashed) {
            Ok(v) => v
                .into_iter()
                .map(|d| d.name)
                .filter(|n| !NOISE.contains(&n.as_str()))
                .collect(),
            Err(e) => {
                fails.push(format!("reader list_dir /{}: {e:?}", rel.join("/")));
                continue;
            }
        };
        if our_names != oracle_names {
            let miss: Vec<_> = oracle_names.difference(&our_names).collect();
            let extra: Vec<_> = our_names.difference(&oracle_names).collect();
            fails.push(format!(
                "dir mismatch /{}: missing-in-reader={miss:?} extra-in-reader={extra:?}",
                rel.join("/")
            ));
        } else {
            dirs_ok += 1;
        }

        for name in &oracle_names {
            let child_abs = dir_abs.join(name);
            let md = match fs::symlink_metadata(&child_abs) {
                Ok(m) => m,
                Err(e) => {
                    fails.push(format!("oracle stat {child_abs:?}: {e}"));
                    continue;
                }
            };
            let ft = md.file_type();
            if ft.is_symlink() {
                skipped_symlink += 1;
                continue;
            }
            if ft.is_dir() {
                let mut nrel = rel.clone();
                nrel.push(name.clone());
                stack.push((child_abs, nrel));
                continue;
            }
            if !ft.is_file() {
                skipped_special += 1;
                continue;
            }

            // Regular file: resolve in our reader and byte-compare per budget.
            let mut crel: Vec<&str> = comps.clone();
            crel.push(name.as_str());
            let ino = match cat.resolve_path(&mut dev, &crel, hashed) {
                Ok(Some(i)) => i,
                Ok(None) => {
                    fails.push(format!("reader: file not found /{}/{name}", rel.join("/")));
                    continue;
                }
                Err(e) => {
                    fails.push(format!("reader resolve /{}/{name}: {e:?}", rel.join("/")));
                    continue;
                }
            };
            let ours = match cat.read_file_decompressed(&mut dev, ino) {
                Ok(v) => v,
                Err(e) => {
                    fails.push(format!("reader read /{}/{name}: {e:?}", rel.join("/")));
                    continue;
                }
            };
            let want = match fs::read(&child_abs) {
                Ok(v) => v,
                Err(e) => {
                    fails.push(format!("oracle read {child_abs:?}: {e}"));
                    continue;
                }
            };
            if ours.len() != want.len() {
                fails.push(format!(
                    "size /{}/{name}: reader={} oracle={}",
                    rel.join("/"),
                    ours.len(),
                    want.len()
                ));
                continue;
            }
            let full = want.len() <= b.full_max || b.remaining > 0;
            let mismatch_at = if full {
                if b.remaining > 0 && want.len() > b.full_max {
                    b.remaining -= want.len() as i64;
                }
                bytes_cmp += want.len() as u64;
                ours.iter().zip(&want).position(|(a, c)| a != c)
            } else {
                // Budget spent: sample prefix + tail only.
                let p = b.prefix.min(want.len());
                let t = b.tail.min(want.len().saturating_sub(p));
                bytes_cmp += (p + t) as u64;
                ours[..p]
                    .iter()
                    .zip(&want[..p])
                    .position(|(a, c)| a != c)
                    .or_else(|| {
                        let s = want.len() - t;
                        ours[s..]
                            .iter()
                            .zip(&want[s..])
                            .position(|(a, c)| a != c)
                            .map(|x| s + x)
                    })
            };
            match mismatch_at {
                Some(off) => fails.push(format!(
                    "BYTE MISMATCH /{}/{name} at offset {off} (len {})",
                    rel.join("/"),
                    want.len()
                )),
                None => files_ok += 1,
            }
        }
    }

    eprintln!(
        "M3-RT differential: files_ok={files_ok} bytes_compared={bytes_cmp} \
         dirs_ok={dirs_ok} symlinks_skipped={skipped_symlink} \
         special_skipped={skipped_special} failures={}",
        fails.len()
    );
    for f in fails.iter().take(50) {
        eprintln!("  FAIL: {f}");
    }
    assert!(
        fails.is_empty(),
        "{} differential failure(s) vs macOS kernel oracle (see above)",
        fails.len()
    );
    assert!(
        files_ok > 0,
        "no regular files compared — is APFS_RT_MOUNT the right populated volume?"
    );
}
```

- [ ] **Step 2: Add `rt-snapshot` to xtask**

In `xtask/src/main.rs`, add a new subcommand arm `rt-snapshot` (match the existing arg-dispatch style; the existing `gen-fixture` arm shows the pattern). It takes one arg: the mount point. Implementation:

```rust
fn rt_snapshot(mount: &str) -> Result<String, String> {
    let info = std::process::Command::new("diskutil")
        .args(["info", mount])
        .output()
        .map_err(|e| format!("diskutil: {e}"))?;
    if !info.status.success() {
        return Err(format!("diskutil info {mount} failed"));
    }
    let text = String::from_utf8_lossy(&info.stdout);
    let mut store = String::new();
    for line in text.lines() {
        let l = line.trim();
        if let Some(v) = l.strip_prefix("APFS Physical Store:") {
            store = v.trim().to_string();
        } else if store.is_empty() {
            if let Some(v) = l.strip_prefix("Device Node:") {
                store = v.trim().trim_start_matches("/dev/").to_string();
            }
        }
    }
    if store.is_empty() {
        return Err(format!(
            "could not find an APFS Physical Store / Device Node for {mount} \
             (is it an APFS volume? `diskutil info {mount}`)"
        ));
    }
    let raw = format!("/dev/r{store}");
    let out = "/tmp/apfs-rt.img";
    Ok(format!(
        "APFS container device for {mount}: /dev/{store}\n\
         One-time read-only snapshot (slow USB read once into a fast local image):\n\
         \n  sudo dd if={raw} of={out} bs=1m\n\n\
         Then run the differential (no sudo):\n\
         \n  APFS_RT_IMAGE={out} APFS_RT_MOUNT={mount} \\\n\
         \    cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml \\\n\
         \    -p apfs-core --test real_target_differential -- --nocapture\n"
    ))
}
```

Wire it into the subcommand dispatch so `cargo run -p xtask -- rt-snapshot <mount>` prints the returned string (mirror how `gen-fixture` prints its result). On `Err`, print to stderr and exit non-zero (match the existing error style).

- [ ] **Step 3: Self-test (no real target → clean skip; xtask compiles)**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core --test real_target_differential -- --nocapture`
Expected: prints `skip: APFS_RT_IMAGE unset ...` and the test PASSES (returns early). Run `cargo run --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p xtask -- rt-snapshot /Volumes` and confirm it prints a clear error or guidance (no panic).

- [ ] **Step 4: Full gate**

Run the **Gate command**. Expected: all green (the new integration test skips → passes; apfs-core total 44 + the new test binary 1 = the integration test counts separately and passes as skip).

- [ ] **Step 5: Commit**

```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/tests/real_target_differential.rs xtask/src/main.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat: M3-RT — real-target differential harness + xtask rt-snapshot

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: Milestone end — decisive real run (if target ready) + final review + merge

**Files:** none modified (verification + branch finish + ROADMAP).

- [ ] **Step 1: Decisive real-target run (if an APFS target is available)**

If the user has provided a populated real APFS volume + the snapshot image (env vars set), run, via `ctx_execute(language:"shell", code: ...)`:
```
APFS_RT_IMAGE=<image> APFS_RT_MOUNT=<mount> cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core --test real_target_differential -- --nocapture 2>&1 | tail -40
```
Expected: `failures=0`, `files_ok > 0`, dirs_ok > 0. ANY `BYTE MISMATCH`/`size`/`dir mismatch` is a real reader defect on real-world data — STOP, capture the path+offset, fix the responsible module (catalog/inode/decmpfs/xattr), do not weaken the harness. If no target is available yet, record that the decisive run is deferred (the harness is proven structurally by Task 1 `resolve_path` on the fixture + Task 2 skip-path); the user triggers the real run later and it is verified then.

- [ ] **Step 2: Final consolidated milestone review**

Dispatch ONE independent reviewer over the whole `feat/m3-rt-differential` branch vs `main`. Reviewer reads the full diff, runs the **Gate command**, and confirms: `resolve_path` correct (root for empty, None for missing, descends list_dir) + its fixture unit test passes un-skipped; the integration test is correctly env-gated (skips clean when unset, no false green that hides a real failure — i.e. `assert!(files_ok > 0)` guards against "walked nothing"); budget logic sound (small files full-compare, large files sampled, global budget); NOISE exclusion symmetric on both sides; symlinks/special counted not byte-failed; the harness never panics on reader errors (records + asserts) ; `#![forbid(unsafe_code)]` intact in the library; no production-code `unwrap`/`panic` added (test/xtask code exempt); xtask rt-snapshot prints correct device + commands and errors cleanly. If a real target was run in Step 1, confirm `failures=0`. Fix findings, re-review until clean.

- [ ] **Step 3: Finish the branch (squash + delete)**

Use **superpowers:finishing-a-development-branch** → "Merge back to main locally": squash-merge `feat/m3-rt-differential` into `main`, re-run the gate on the merged result (apfs-core 44 unit tests + the skipping integration test green, clippy/fmt clean), delete the branch.

- [ ] **Step 4: Update ROADMAP + re-index KB**

Edit `docs/ROADMAP.md`: add status-table row `| M3-RT real-target differential harness | ✅ | <squash commit> |` (note: decisive real-USB run is operator-triggered, harness skip-safe); update the "Okuma yolu" paragraph (add: real-target differential vs macOS kernel oracle, env-gated); in "Kalan yol haritası" mark M3-RT done and confirm next = **M3e** (ZLIB methods 1/3/4) then **M3f** (LZFSE 11/12); update "Compact sonrası DEVAM" step 1 (M0–M3d + M3-RT merged) and step 4 (next = M3e; note the `rt-snapshot`→`real_target_differential` recipe for the operator real run). Commit:
```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add docs/ROADMAP.md
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "docs: ROADMAP — M3-RT real-target differential harness merged; next = M3e ZLIB

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```
Then re-index: `ctx_index(source:"project:roadmap-state", path:"/Users/eiaydin/Documents/projects/apfs-explorer/docs/ROADMAP.md")`.

---

## Self-Review (performed while writing this plan)

**1. Spec coverage:** env-gated open of real APFS image + macOS-mount oracle → Task 2 test. path→inode via `Catalog::resolve_path` (root inode 2, list_dir per component, None on miss) → Task 1. byte-exact per-file diff with slow-USB data budget (full ≤ full_max, else prefix+tail, global budget) → Task 2 Budget logic. dir child-name set compare excluding system noise → Task 2 NOISE + BTreeSet diff. symlink/special counted not failed; reader errors recorded not panicked; assert zero failures + files_ok>0 → Task 2. one-time snapshot guidance (resolve device, sudo dd, env) → xtask `rt-snapshot`. clean skip when no target → Task 2 early returns. decisive real run when target provided → Task 3 Step 1. All covered.

**2. Placeholder scan:** complete code for `resolve_path`, the full integration test, and `rt_snapshot`; exact commands + expected output; no TBD/"handle edge cases"/uncited symbols. (`Catalog`/`Container`/`FileBlockDevice`/`DirEntry`/`ROOT_DIR_INO_NUM`/`list_dir`/`read_file_decompressed`/`names_are_hashed`/`list_user_volumes`/`superblock.block_size` are all existing M3a–d API.)

**3. Type consistency:** `Catalog::resolve_path<D:BlockDevice>(&self,&mut D,&[&str],bool)->Result<Option<u64>,ContainerError>` used identically in Task 1 test and Task 2 harness; `DirEntry.name`/`.file_id`; budget env keys (`APFS_RT_IMAGE/MOUNT/FULL_MAX/PREFIX/TAIL/BUDGET`) consistent between the test and the xtask printed recipe; integration test depends only on `apfs_core` public items (`block_device::FileBlockDevice`, `catalog::Catalog`, `container::Container`) which are `pub` per lib.rs.
