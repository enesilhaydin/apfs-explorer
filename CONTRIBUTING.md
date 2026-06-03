# Contributing to APFS Explorer

Thank you for your interest in contributing. This document describes the workflow,
code standards, and testing requirements for all contributions.

## Table of Contents

- [Development workflow](#development-workflow)
- [Cross-check workflow for spec ambiguity](#cross-check-workflow-for-spec-ambiguity)
- [Git rules](#git-rules)
- [Code style](#code-style)
- [Test requirements](#test-requirements)
- [Submitting a pull request](#submitting-a-pull-request)

---

## Development workflow

All contributions follow a strict **PLAN → TEST (red) → IMPLEMENT (green) →
VERIFY → COMMIT** cycle. No step may be skipped, including for small fixes.

1. **Plan** - describe the approach in the PR description or a comment before
   writing any code. For non-trivial changes open a draft PR or issue first.
2. **Test first (TDD)** - write a failing test that demonstrates the bug or
   specifies the new behavior. Commit the red test separately if it helps
   reviewers.
3. **Implement** - make the test pass with the minimal correct change.
4. **Verify** - run the full gate (see [Test requirements](#test-requirements))
   locally. CI must also be green.
5. **Commit** - atomic commits, file names explicit (see [Git rules](#git-rules)).

---

## Cross-check workflow for spec ambiguity

APFS on-disk format details are sometimes ambiguous or contradictory across
reference sources. When you encounter an ambiguous struct layout, byte offset,
or algorithm, follow this process (do not guess):

1. **Check local reference repositories first** (in order of precedence):
   - `linux-apfs-rw/` - write-path reference (GPL-2.0+, Ernesto Fernández).
   - `apfsprogs/` - fsck/mkfs userspace tools (same author).
   - `apfs-fuse/` - independent C++ FUSE reader.
   - `libfsapfs/libfsapfs/` - independent RE-based C reader (struct layout gold
     standard).
   - `go-apfs/` - Go reader for usage examples.
2. **Require at least two independent cross-checks** that agree before
   implementing. Prefer `linux-apfs-rw` + `apfsprogs` as the first pair.
3. **If sources disagree**: produce a controlled known-value fixture on a real
   macOS disk, then write an independent Python redecode (zero shared code with
   Rust implementation) to verify the raw bytes.
4. **Do not proceed on a single source.** Open an issue and describe the
   conflict; a maintainer will adjudicate.
5. **Lock the decision** - document the resolution as a comment in the code and,
   for offset-sensitive paths, add an entry to `docs/refs/` referencing the
   evidence.

---

## Git rules

- **Never use `git add -A` or `git add .`** - always stage files by explicit
  name: `git add path/to/file.rs`.
- Branch naming: `feat/`, `fix/`, `refactor/`, `docs/`, `chore/`, `test/` +
  kebab-case description. Example: `feat/snapshot-revert-cli`.
- Branch lifetime: 3 days maximum. Keep PRs small and focused.
- Commit messages: imperative mood, present tense, 72-char subject line.
  Body explains *why*, not *what*.
- Merge strategy: **squash + delete branch** (linear history on `main`).
- `main` must always be in a deployable state; do not commit broken builds.
- Force-push to `main` is forbidden.

---

## Code style

- Format with `rustfmt` (default settings): `cargo fmt --all`.
- Zero Clippy warnings: `cargo clippy --workspace --all-features -- -D warnings`.
- No `unwrap()`, `expect()`, or `panic!()` in library code paths (`apfs-core`,
  `apfs-write`). Every fallible operation returns `Result`.
- No silent error drops: do not use empty `catch` blocks or `let _ = err`.
  Propagate errors or log them explicitly.
- No magic numbers: use named `const` or configuration values.
- No speculative abstractions: apply the Rule of Three (refactor only after the
  third repetition).
- `unsafe` is permitted only at mandatory FFI or raw-I/O boundaries and must be
  accompanied by a `// SAFETY:` comment explaining the invariant.
- `apfs-core` targets `#![forbid(unsafe_code)]`.
- Minimum supported Rust version: **1.91**.

---

## Test requirements

All contributions must pass the full test gate before requesting review:

```sh
# Format check
cargo fmt --all -- --check

# Lint
cargo clippy --workspace --all-features -- -D warnings

# Unit + integration tests
cargo test --workspace --all-features --lib

# Documentation build (no broken intra-doc links)
cargo doc --workspace --no-deps
```

For write-path contributions, also run the chaos suite:

```sh
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test chaos_write
```

Fixtures that require a real disk image must be placed under `tests/fixtures/`
and tracked with Git LFS.

---

## Submitting a pull request

1. Ensure the full test gate passes locally.
2. Fill in the pull request template completely.
3. Link the related issue if one exists.
4. For on-disk format changes, include a cross-check section describing the two
   independent sources consulted (see
   [Cross-check workflow](#cross-check-workflow-for-spec-ambiguity)).
5. Breaking changes must be documented in `CHANGELOG.md` under `[Unreleased]`.
6. A maintainer will review within 5 business days. Large changes may require
   a second reviewer.

---

## Reporting bugs

Use the GitHub issue tracker. Fill in the bug report template - include OS
version, WinFsp version, and reproduction steps. Data-loss bugs are treated
as critical and triaged within 24 hours.

## Questions

Open a GitHub Discussion or comment on a relevant issue. Please do not use the
issue tracker for general usage questions.
