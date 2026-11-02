# Testing and CI Strategy

Principle: tests prove that something *works*, not that it *cannot break under any
circumstance*. That is why the strategy relies on layered testing, differential
validation, and fuzzing.

## Layer 1 - Parser unit tests (all platforms)

- `apfs-core` / `apfs`, against **file-based fixture images** via the `BlockDevice` trait.
- Coverage: NX superblock, checkpoint, object map, B-tree traversal, directory listing,
  file extents, xattr/Finder metadata, compression (ZLIB/LZVN/LZFSE), FileVault
  decryption, snapshots.
- **Corrupt/truncated image tests:** for every parser entry point, a truncate/bitflip
  fixture must return `Err` - no panics.
- `cargo test` runs on every PR; coverage tracked with `cargo-llvm-cov`.

## Layer 2 - Windows integration

- **(a) CI:** GitHub Actions `windows-latest` → `msiexec /i winfsp.msi /qn`
  (Microsoft-signed, no reboot required) → mount fixture → list → compare file hashes
  against manifest → unmount. Gated behind a Cargo feature.
- **(b) Real hardware:** the user's **real APFS USB drive**, on a Windows machine
  accessible via SSH. On every M4+ release: mount a real Mac disk, compare contents
  against a known reference. This is the **release gate**.
- **(c) Real hardware under macOS:** `crates/apfs-write/tests/real_usb.rs` writes to
  the raw node of a removable USB drive's APFS partition (`APFS_USB_DEV=/dev/rdiskNsM`,
  volume named `APFSUSB`; everything on it is overwritten, and the node must be
  writable by the user running the test). macOS writes a tree, the writer adds and
  edits files, macOS reads every file back and writes more, the reader reads that,
  with fsck (through `diskutil verifyVolume`) between every step. The commit barrier
  uses `DKIOCSYNCHRONIZE`: `F_FULLFSYNC` fails on device nodes.
- **(d) macOS after the writer:** `crates/apfs-write/tests/macos_after_us.rs` lets
  macOS remove what the writer wrote and runs fsck again. A volume can pass fsck
  right after our commit and still be left in a state macOS mishandles (counters it
  adjusts, node layouts it edits).

## Differential oracle (guards against silent corruption)

- Read the same fixture/disk via an **independent second path**, then compare
  bytes/checksums.
- Read oracle: apfs-fuse / linux-apfs tools (validation only; no code is copied).
  Reference manifests are recorded during fixture generation.
- **Mandatory for write (v2):** write → independently read → if checksums do not match,
  fail and roll back. Write features do not compile without this check in place.

## Controlled fixture generation (`xtask`, macOS runner)

Only one real disk is available; for variety, fixtures are generated on **free macOS
GitHub Actions runners** (no personal Mac required):

- `hdiutil create -size 64m -fs APFS -volname Test test.dmg`
- Small images with snapshots, compression (`afsctool` / copied system files), and
  FileVault, created via `newfs_apfs` / `diskutil`
- Each fixture ships with a **manifest** (file path → size → sha256 → xattr) → stored
  in `tests/fixtures/` via Git LFS
- `xtask gen-fixtures` is reproducible locally and in CI; images are kept small (<64 MB)

## Fuzzing

- `cargo-fuzz` targets: superblock, omap, btree node, file extent, decmpfs header,
  keybag. Short smoke run in CI; longer run nightly.
- Every panic/crash found → first write a failing test (add to corpus), then fix
  (TDD bugfix rule).

## CI summary (`.github/workflows/ci.yml`)

| Job | Platform | Content |
|---|---|---|
| lint | ubuntu | `cargo fmt --check`, `clippy -D`, `cargo deny` |
| test | ubuntu | Layer 1 + coverage |
| gen-fixtures | macos | `xtask gen-fixtures` (cache/LFS) |
| win-integration | windows | Layer 2(a): WinFsp + mount fixture |
| fuzz-smoke | ubuntu | `cargo-fuzz` short run |

Real hardware (Layer 2b) runs manually or on a schedule - SSH target is kept private.
