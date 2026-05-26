# Tested Platforms

This document records the hardware/OS combinations on which apfs-explorer has
been verified. "Verified" means: build succeeds, test suite passes, and at least
one real-workload exercise has been run.

---

## Build + Runtime Matrix

| Host ID | OS | Architecture | Build Target | Status |
|---------|----|--------------|--------------|--------|
| ENESPC | Windows 11 Pro 24H2 (26100.x) | x86_64 | `x86_64-pc-windows-msvc` | Verified |
| dev | macOS 26 Developer Beta (arm64) | aarch64 | `aarch64-apple-darwin` | Library + tests |

**ENESPC** is the primary development and runtime machine.
WinFsp is required for the mount binary on Windows.

**dev (macOS)** is used for library-layer tests and cross-compilation validation.
The mount binary is not applicable on macOS (WinFsp is Windows-only).

---

## WinFsp Version Compatibility

| WinFsp Version | Status | Notes |
|----------------|--------|-------|
| 2.1 | Verified (locked) | Primary runtime, all feature flags exercised |
| 2.0 | Untested | No known API surface difference; expected to work |
| 1.x | Not supported | Missing API surface used by apfs-mount |

WinFsp 2.1 is the locked minimum requirement for the mount binary.

---

## Tested Workloads

The following workloads have been exercised end-to-end on verified platforms.

### Image Source

- 64 MiB APFS container image created with `hdiutil create -size 64m -fs APFS`
  on macOS, used as test fixture (`tests/fixtures/`).

### Workloads

| Workload | Platform | Wave | Result |
|----------|----------|------|--------|
| Mount + stat root directory | ENESPC (Windows 11) | Wave 1 | Pass |
| Read file contents (small + large) | ENESPC (Windows 11) | Wave 1 | Pass |
| Read directory listing (deep tree) | ENESPC (Windows 11) | Wave 2 | Pass |
| 200-rename batch (concurrent) | ENESPC (Windows 11) | Wave 16 | Pass, zero corruption |
| Symlink read (resolve target) | ENESPC (Windows 11) | Wave 14 | Pass |
| Symlink write + Windows reparse round-trip | ENESPC (Windows 11) | Wave 14 | Pass |
| Snapshot create | ENESPC (Windows 11) | Wave 15 | Pass |
| Snapshot delete | ENESPC (Windows 11) | Wave 15 | Pass |
| Snapshot revert | ENESPC (Windows 11) | Wave 15 | Pass |
| Clone (CoW reflink) | ENESPC (Windows 11) | Wave 15 | Pass |
| xattr read (embedded + stream) | ENESPC (Windows 11) | Wave 13 | Pass |
| xattr write (embedded + stream) | ENESPC (Windows 11) | Wave 13 | Pass |
| File create + write + delete | ENESPC (Windows 11) | Wave 10 | Pass |
| File truncate (grow + shrink) | ENESPC (Windows 11) | Wave 11 | Pass |
| Transaction rollback on error | ENESPC (Windows 11) | Wave 10 | Pass |
| Library unit tests (`cargo test`) | macOS 26 (arm64) | Wave 16 | Pass |
| Cross-compile (msvc target from macOS) | macOS 26 (arm64) | Wave 16 | Pass |

---

## Known Limitations on Tested Platforms

- **Windows 11 ARM64** — not yet tested. MSVC toolchain + WinFsp ARM64 builds
  exist; testing planned post-v1.0.
- **Windows 10** — not formally tested. WinFsp 2.1 supports Windows 10; no known
  incompatibilities, but not in the verified matrix.
- **Linux FUSE** — not yet tested. Library crates compile on Linux; FUSE mount
  path is a future milestone.
- **32-bit targets** — explicitly out of scope.

---

## Adding a New Platform

If you verify apfs-explorer on a new platform, please open a PR updating this
table with:

1. Host OS + version + build number
2. Architecture
3. Build target triple
4. WinFsp or FUSE version (if applicable)
5. Workloads exercised (link to CI run or test output)
