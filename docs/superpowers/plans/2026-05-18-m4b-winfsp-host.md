# M4b — Windows winfsp-host Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Mount a real APFS USB/SSD as a Windows drive letter by delegating every read-only filesystem operation to `apfs::FsView`, with raw `\\.\PhysicalDriveN` access and GPT→APFS-container offset discovery.

**Architecture:** Three pure cross-platform layers (`align` sector-window math, `gpt` partition parser, `OffsetBlockDevice` container-offset wrapper) feed `apfs::FsView`. Windows-only layers (`PhysicalDrive` raw device, `ApfsFs` winfsp callbacks, CLI binary) sit behind `#[cfg(windows)]` and `[target.'cfg(windows)'.dependencies]` so the macOS workspace still builds and tests. Every winfsp callback wraps its body in `std::panic::catch_unwind` (crash-safety: a parser bug must not BSOD the mount).

**Tech Stack:** Rust 2021, `winfsp` crate (Windows-only, pinned), `windows` crate (CreateFileW / ReadFile / DeviceIoControl), `widestring`, existing `apfs` + `apfs-core` crates.

**Empirically locked facts (validated on the real Windows box + real APFS USB, 2026-05-18):**
- Raw `\\.\PhysicalDriveN` reads MUST be sector-aligned: byte offset a multiple of the device sector size (512) AND length a multiple of the sector size. Unaligned/odd-size reads fail. This is THE core constraint of the Windows raw device and the reason `align` is its own tested module.
- Protective MBR at LBA0 (sig `0x55 0xAA` at byte 510); GPT header at LBA1 (offset 512), ASCII `"EFI PART"` at its byte 0.
- GPT header: `PartitionEntryLBA` u64 LE @ +72, `NumberOfPartitionEntries` u32 LE @ +80, `SizeOfPartitionEntry` u32 LE @ +84 (relative to the GPT-header LBA, i.e. absolute byte `partLBA*512`).
- GPT partition entry: PartitionTypeGUID 16 bytes @ +0 (mixed-endian "GUID" layout: first 3 fields LE, last 8 bytes BE), FirstLBA u64 LE @ +32, LastLBA u64 LE @ +40.
- APFS container partition type GUID = `7C3457EF-0000-11AA-AA11-00306543ECAC`.
- APFS container superblock magic: ASCII `"NXSB"` (`4E 58 53 42`) at container byte offset `firstLBA*512 + 32`; `block_size` u32 LE @ +36 (observed 4096).
- Reference USB: `\\.\PhysicalDrive3`, APFS partition firstLBA = 409640 (byte offset 209,735,680).

**Source-of-truth discipline (CLAUDE.md md.6 analog for our own integration):** the `winfsp` crate's exact module paths/struct fields in Context7 are HYPOTHESIS. The arbiter is the actual pinned crate built on the Windows box (`cargo doc -p winfsp --open` / crate source). When the documented shape and the compiler disagree, the compiler wins; record the resolved API shape in a code comment. No APFS on-disk spec is touched (the reader is already proven byte-exact in M3-RT) so no NotebookLM md.6 gate applies here.

**Windows box (operator-provided, SSH alias `apfswin`, key `~/.ssh/apfs_win`):** Windows 11 Pro x64, admin, WinFsp installed, Rust 1.91 MSVC + git, repo synced to `C:\apfs\apfs-explorer`. Build/test recipe in Task 8.

---

## File Structure

- `crates/winfsp-host/Cargo.toml` — add `apfs` (cross-platform) + Windows-only target deps.
- `crates/winfsp-host/src/lib.rs` — module wiring + cross-platform re-exports.
- `crates/winfsp-host/src/align.rs` — pure sector-alignment window math (Task 2).
- `crates/winfsp-host/src/gpt.rs` — pure GPT parser → APFS container byte range (Task 3).
- `crates/winfsp-host/src/offset_dev.rs` — `OffsetBlockDevice<D>` (Task 4).
- `crates/winfsp-host/src/winraw.rs` — `#[cfg(windows)]` raw `\\.\PhysicalDriveN` + auto-detect (Task 5).
- `crates/winfsp-host/src/mount.rs` — `#[cfg(windows)]` `ApfsFs` winfsp callbacks + `mount_apfs` (Task 6).
- `crates/winfsp-host/src/main.rs` — `#[cfg(windows)]` CLI binary (Task 7).
- `crates/winfsp-host/tests/gpt_offset_fsview.rs` — cross-platform integration test (Task 4).
- `docs/ROADMAP.md` — status update (Task 8).

Each pure module is independently unit-tested on macOS in the normal `cargo test`. Windows modules compile only under `#[cfg(windows)]`; their proof is the SSH real-USB run (Task 8), consistent with the project's "real disk is the final arbiter" methodology.

---

### Task 1: Crate skeleton — Windows-target deps, cfg gating, macOS build stays green

**Files:**
- Modify: `crates/winfsp-host/Cargo.toml`
- Modify: `crates/winfsp-host/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add to `crates/winfsp-host/src/lib.rs` (replacing the M0 skeleton body, keeping the top `cfg_attr`):

```rust
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! winfsp-host — mount an APFS container as a Windows drive letter.
//!
//! Cross-platform (unit-tested on macOS): [`align`], [`gpt`], [`offset_dev`].
//! Windows-only (`#[cfg(windows)]`): raw physical device + winfsp callbacks.

pub mod align;
pub mod gpt;
pub mod offset_dev;

#[cfg(windows)]
pub mod winraw;
#[cfg(windows)]
pub mod mount;

#[cfg(test)]
mod skeleton_tests {
    #[test]
    fn modules_are_wired() {
        // Compiles only if align/gpt/offset_dev are reachable cross-platform.
        let _ = crate::align::aligned_window;
        let _ = crate::gpt::find_apfs_container::<crate::offset_dev::tests::MemDevice>;
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p winfsp-host`
Expected: FAIL — `align`, `gpt`, `offset_dev` modules do not exist yet (unresolved module / path).

- [ ] **Step 3: Write minimal implementation**

Create empty stub files so the crate tree resolves (full impls land in later tasks):

`crates/winfsp-host/src/align.rs`:
```rust
//! Sector-alignment window math (Task 2).
```
`crates/winfsp-host/src/gpt.rs`:
```rust
//! GPT partition parser (Task 3).
```
`crates/winfsp-host/src/offset_dev.rs`:
```rust
//! Container-offset BlockDevice wrapper (Task 4).
```

Replace `crates/winfsp-host/Cargo.toml` with:

```toml
[package]
name = "winfsp-host"
version = "0.0.0"
edition.workspace = true
license.workspace = true

[lints]
workspace = true

[[bin]]
name = "apfs-mount"
path = "src/main.rs"

[dependencies]
apfs = { path = "../apfs" }
apfs-core = { path = "../apfs-core" }

[target.'cfg(windows)'.dependencies]
winfsp = "0.11"
widestring = "1"

[target.'cfg(windows)'.dependencies.windows]
version = "0.58"
features = [
  "Win32_Foundation",
  "Win32_Storage_FileSystem",
  "Win32_System_IO",
  "Win32_System_Ioctl",
  "Win32_Security",
]

[dev-dependencies]
tempfile = { workspace = true }
```

Create `crates/winfsp-host/src/main.rs` (stub; real CLI in Task 7) so the `[[bin]]` resolves on every platform:

```rust
//! apfs-mount CLI (Task 7). On non-Windows this binary is a no-op.
#[cfg(not(windows))]
fn main() {
    eprintln!("apfs-mount is Windows-only.");
}

#[cfg(windows)]
fn main() {
    eprintln!("apfs-mount: not implemented yet (M4b Task 7).");
}
```

Note: the `skeleton_tests` reference to `gpt::find_apfs_container` / `offset_dev::tests::MemDevice` will not resolve until Tasks 3-4. For Task 1, simplify the test body to only assert the crate compiles:

```rust
#[cfg(test)]
mod skeleton_tests {
    #[test]
    fn crate_compiles_cross_platform() {
        assert_eq!(2 + 2, 4);
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p winfsp-host` (on macOS)
Expected: PASS — crate compiles cross-platform, Windows deps not pulled in on macOS.
Also run: `cargo build --workspace` — Expected: PASS (macOS workspace unaffected).

- [ ] **Step 5: Commit**

```bash
git add crates/winfsp-host/Cargo.toml crates/winfsp-host/src/lib.rs crates/winfsp-host/src/align.rs crates/winfsp-host/src/gpt.rs crates/winfsp-host/src/offset_dev.rs crates/winfsp-host/src/main.rs
git commit -m "feat(winfsp-host): crate skeleton with windows-target gated deps"
```

---

### Task 2: `align` — sector-alignment window math (empirically critical)

**Files:**
- Modify: `crates/winfsp-host/src/align.rs`

The Windows raw device can only read at sector-aligned offsets with sector-multiple lengths. Given an arbitrary requested `(offset, len)` and `sector` size, compute the enclosing aligned window `(aligned_offset, aligned_len)` plus the sub-slice `(skip, take)` to copy out of the window buffer. This is pure and the single most safety-critical helper (it bit us twice in raw PowerShell reads), so it gets exhaustive unit tests on macOS.

- [ ] **Step 1: Write the failing test**

Append to `crates/winfsp-host/src/align.rs`:

```rust
//! Sector-alignment window math for raw physical-device reads.
//!
//! A raw `\\.\PhysicalDriveN` handle only accepts reads whose byte offset
//! and length are multiples of the device sector size. [`aligned_window`]
//! widens an arbitrary `(offset, len)` request to the enclosing aligned
//! window and reports where the requested bytes sit inside that window.

/// The aligned read window enclosing a requested byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Sector-aligned start offset to read from.
    pub aligned_offset: u64,
    /// Sector-multiple number of bytes to read.
    pub aligned_len: usize,
    /// Bytes to skip at the front of the window buffer.
    pub skip: usize,
    /// Bytes to copy out after `skip` (== requested len).
    pub take: usize,
}

/// Compute the aligned window for reading `len` bytes at `offset` from a
/// device with the given `sector` size. `sector` must be a power of two.
/// `len == 0` yields an empty zero-length window at the aligned offset.
pub fn aligned_window(offset: u64, len: usize, sector: u64) -> Window {
    let mask = sector - 1;
    let aligned_offset = offset & !mask;
    let skip = (offset - aligned_offset) as usize;
    let end = offset + len as u64;
    let aligned_end = (end + mask) & !mask;
    let aligned_len = (aligned_end - aligned_offset) as usize;
    Window {
        aligned_offset,
        aligned_len,
        skip,
        take: len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_aligned_full_sector() {
        let w = aligned_window(0, 512, 512);
        assert_eq!(
            w,
            Window { aligned_offset: 0, aligned_len: 512, skip: 0, take: 512 }
        );
    }

    #[test]
    fn unaligned_offset_and_len_widens_both_ends() {
        // Request 100 bytes at offset 600 with 512-byte sectors:
        // window must cover [512, 1024) -> 512 bytes, skip 88, take 100.
        let w = aligned_window(600, 100, 512);
        assert_eq!(
            w,
            Window { aligned_offset: 512, aligned_len: 512, skip: 88, take: 100 }
        );
    }

    #[test]
    fn spans_multiple_sectors() {
        // 5000 bytes at offset 600 -> end 5600 -> aligned_end 6144,
        // aligned_offset 512 -> aligned_len 5632.
        let w = aligned_window(600, 5000, 512);
        assert_eq!(w.aligned_offset, 512);
        assert_eq!(w.aligned_len, 5632);
        assert_eq!(w.skip, 88);
        assert_eq!(w.take, 5000);
        assert_eq!(w.aligned_len % 512, 0);
        assert!(w.aligned_offset + w.aligned_len as u64 >= 600 + 5000);
    }

    #[test]
    fn zero_len_is_empty_window() {
        let w = aligned_window(700, 0, 512);
        assert_eq!(w.aligned_offset, 512);
        assert_eq!(w.aligned_len, 0);
        assert_eq!(w.skip, 188);
        assert_eq!(w.take, 0);
    }

    #[test]
    fn large_real_world_offset_4k_sector() {
        // Container at 209_735_680, read 4096 at container+1000.
        let w = aligned_window(209_735_680 + 1000, 4096, 4096);
        assert_eq!(w.aligned_offset % 4096, 0);
        assert_eq!(w.aligned_len % 4096, 0);
        assert!(w.aligned_offset <= 209_735_680 + 1000);
        assert!(
            w.aligned_offset + w.aligned_len as u64 >= 209_735_680 + 1000 + 4096
        );
        assert_eq!(w.take, 4096);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p winfsp-host align`
Expected: FAIL — the file currently only has the doc comment from Task 1; the appended tests reference `aligned_window`/`Window` which must be compiled together. (If Step 1 already includes the impl, instead temporarily comment out the function body to observe a red test, then restore. Goal: see red before green.)

- [ ] **Step 3: Write minimal implementation**

The impl in Step 1 is the minimal correct implementation. Ensure the `sector - 1` mask, `& !mask` rounding are present exactly as shown.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p winfsp-host align`
Expected: PASS — all 5 alignment tests green.
Run: `cargo clippy -p winfsp-host --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/winfsp-host/src/align.rs
git commit -m "feat(winfsp-host): sector-alignment window math with exhaustive tests"
```

---

### Task 3: `gpt` — parse GPT, locate the APFS container byte range

**Files:**
- Modify: `crates/winfsp-host/src/gpt.rs`

Parse the protective MBR + GPT header + partition entries off any `BlockDevice` and return the APFS container's byte offset and length. Pure and `BlockDevice`-generic so it is unit-tested on macOS against a crafted in-memory device.

- [ ] **Step 1: Write the failing test**

Write `crates/winfsp-host/src/gpt.rs`:

```rust
//! Minimal GPT parser: find the APFS container partition's byte range.
//!
//! Only the fields needed to locate the APFS container are read. Offsets
//! are validated against the project plan's empirically-locked GPT layout.

use apfs_core::block_device::{BlockDevice, BlockError};

/// APFS container GPT partition type GUID, canonical string form.
pub const APFS_TYPE_GUID: &str = "7C3457EF-0000-11AA-AA11-00306543ECAC";

const SECTOR: u64 = 512;

#[derive(Debug)]
pub enum GptError {
    Block(BlockError),
    NoProtectiveMbr,
    NoGptHeader,
    NoApfsPartition,
    Malformed(&'static str),
}

impl From<BlockError> for GptError {
    fn from(e: BlockError) -> Self {
        GptError::Block(e)
    }
}

/// Byte range of the APFS container within the whole disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContainerRange {
    pub offset: u64,
    pub length: u64,
}

fn u32_le(b: &[u8], at: usize) -> Result<u32, GptError> {
    let s = b.get(at..at + 4).ok_or(GptError::Malformed("u32"))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_le(b: &[u8], at: usize) -> Result<u64, GptError> {
    let s = b.get(at..at + 8).ok_or(GptError::Malformed("u64"))?;
    Ok(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

/// Format the 16-byte GPT partition type field as the canonical GUID
/// string. GPT stores Data1/2/3 little-endian and Data4 big-endian.
fn guid_string(g: &[u8]) -> Result<String, GptError> {
    let g: &[u8; 16] = g
        .get(..16)
        .ok_or(GptError::Malformed("guid"))?
        .try_into()
        .map_err(|_| GptError::Malformed("guid"))?;
    let d1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
    let d2 = u16::from_le_bytes([g[4], g[5]]);
    let d3 = u16::from_le_bytes([g[6], g[7]]);
    Ok(format!(
        "{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}",
        d1, d2, d3, g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    ))
}

/// Locate the first APFS-type partition and return its byte range.
pub fn find_apfs_container<D: BlockDevice>(
    dev: &mut D,
) -> Result<ContainerRange, GptError> {
    // Protective MBR signature.
    let mut mbr = [0u8; 512];
    dev.read_at(0, &mut mbr)?;
    if mbr.get(510..512) != Some(&[0x55, 0xAA]) {
        return Err(GptError::NoProtectiveMbr);
    }

    // GPT header at LBA1.
    let mut hdr = [0u8; 512];
    dev.read_at(SECTOR, &mut hdr)?;
    if hdr.get(0..8) != Some(b"EFI PART") {
        return Err(GptError::NoGptHeader);
    }
    let part_lba = u64_le(&hdr, 72)?;
    let num_parts = u32_le(&hdr, 80)?;
    let part_size = u32_le(&hdr, 84)? as u64;
    if part_size < 128 || num_parts == 0 || num_parts > 65_536 {
        return Err(GptError::Malformed("gpt header fields"));
    }

    let table_base = part_lba
        .checked_mul(SECTOR)
        .ok_or(GptError::Malformed("table base overflow"))?;
    for i in 0..num_parts as u64 {
        let mut ent = vec![0u8; part_size as usize];
        let ent_off = table_base
            .checked_add(i * part_size)
            .ok_or(GptError::Malformed("entry offset overflow"))?;
        dev.read_at(ent_off, &mut ent)?;
        if guid_string(&ent)?.eq_ignore_ascii_case(APFS_TYPE_GUID) {
            let first_lba = u64_le(&ent, 32)?;
            let last_lba = u64_le(&ent, 40)?;
            if last_lba < first_lba {
                return Err(GptError::Malformed("last_lba < first_lba"));
            }
            let offset = first_lba
                .checked_mul(SECTOR)
                .ok_or(GptError::Malformed("offset overflow"))?;
            let length = (last_lba - first_lba + 1)
                .checked_mul(SECTOR)
                .ok_or(GptError::Malformed("length overflow"))?;
            return Ok(ContainerRange { offset, length });
        }
    }
    Err(GptError::NoApfsPartition)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In-memory disk: protective MBR + GPT header + one APFS entry whose
    /// container region starts with the NXSB magic.
    struct MemDisk {
        data: Vec<u8>,
    }
    impl BlockDevice for MemDisk {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let o = offset as usize;
            let end = o + buf.len();
            let src = self
                .data
                .get(o..end)
                .ok_or(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.data.len() as u64,
                })?;
            buf.copy_from_slice(src);
            Ok(())
        }
    }

    fn apfs_type_bytes() -> [u8; 16] {
        // 7C3457EF-0000-11AA-AA11-00306543ECAC in GPT mixed-endian form.
        [
            0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00,
            0x30, 0x65, 0x43, 0xEC, 0xAC,
        ]
    }

    fn build_disk(first_lba: u64, last_lba: u64) -> MemDisk {
        let total = (last_lba as usize + 16) * 512;
        let mut d = vec![0u8; total];
        d[510] = 0x55;
        d[511] = 0xAA;
        d[512..520].copy_from_slice(b"EFI PART");
        d[512 + 72..512 + 80].copy_from_slice(&2u64.to_le_bytes()); // partLBA=2
        d[512 + 80..512 + 84].copy_from_slice(&128u32.to_le_bytes()); // numParts
        d[512 + 84..512 + 88].copy_from_slice(&128u32.to_le_bytes()); // partSize
        let ent = 2 * 512;
        d[ent..ent + 16].copy_from_slice(&apfs_type_bytes());
        d[ent + 32..ent + 40].copy_from_slice(&first_lba.to_le_bytes());
        d[ent + 40..ent + 48].copy_from_slice(&last_lba.to_le_bytes());
        let cont = first_lba as usize * 512;
        d[cont + 32..cont + 36].copy_from_slice(b"NXSB");
        MemDisk { data: d }
    }

    #[test]
    fn finds_apfs_partition_range() {
        let mut disk = build_disk(40, 200);
        let r = find_apfs_container(&mut disk).unwrap();
        assert_eq!(r.offset, 40 * 512);
        assert_eq!(r.length, (200 - 40 + 1) * 512);
        // Sanity: NXSB magic sits at container offset + 32.
        let mut m = [0u8; 4];
        disk.read_at(r.offset + 32, &mut m).unwrap();
        assert_eq!(&m, b"NXSB");
    }

    #[test]
    fn rejects_missing_protective_mbr() {
        let mut disk = build_disk(40, 200);
        disk.data[510] = 0;
        assert!(matches!(
            find_apfs_container(&mut disk),
            Err(GptError::NoProtectiveMbr)
        ));
    }

    #[test]
    fn errors_when_no_apfs_partition() {
        let mut disk = build_disk(40, 200);
        let ent = 2 * 512;
        disk.data[ent] ^= 0xFF; // corrupt the type GUID
        assert!(matches!(
            find_apfs_container(&mut disk),
            Err(GptError::NoApfsPartition)
        ));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p winfsp-host gpt`
Expected: FAIL first run only if impl incomplete. With the full impl above it should compile; to honor red→green, first paste only the `tests` module + signatures with `todo!()`-free `unimplemented` replaced by `return Err(GptError::NoApfsPartition)` in `find_apfs_container`, observe `finds_apfs_partition_range` FAIL, then add the real body.

- [ ] **Step 3: Write minimal implementation**

The `find_apfs_container` body above is the implementation. Note `vec!`/`format!` are fine here (winfsp-host is not `#![forbid(unsafe_code)]`-pure like apfs-core, but still must obey workspace clippy: no `unwrap`/`expect`/`panic` outside `#[cfg(test)]`).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p winfsp-host gpt`
Expected: PASS — 3 tests green.
Run: `cargo clippy -p winfsp-host --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/winfsp-host/src/gpt.rs
git commit -m "feat(winfsp-host): GPT parser locating APFS container byte range"
```

---

### Task 4: `OffsetBlockDevice` + end-to-end FsView over a GPT-wrapped fixture

**Files:**
- Modify: `crates/winfsp-host/src/offset_dev.rs`
- Create: `crates/winfsp-host/tests/gpt_offset_fsview.rs`

`OffsetBlockDevice<D>` rebases a child device so the APFS container starts at byte 0 (what `Container::open` expects). Then prove the full stack on macOS: GPT-wrapped image → `gpt` → `OffsetBlockDevice` → `apfs::FsView`.

- [ ] **Step 1: Write the failing test**

Write `crates/winfsp-host/src/offset_dev.rs`:

```rust
//! `OffsetBlockDevice` — present a sub-range of a device as if it began
//! at byte 0, so `apfs_core::Container::open` sees the APFS container
//! directly regardless of the GPT partition offset.

use apfs_core::block_device::{BlockDevice, BlockError};

pub struct OffsetBlockDevice<D: BlockDevice> {
    inner: D,
    base: u64,
    len: u64,
}

impl<D: BlockDevice> OffsetBlockDevice<D> {
    /// Wrap `inner` exposing `[base, base+len)` as `[0, len)`.
    pub fn new(inner: D, base: u64, len: u64) -> Self {
        Self { inner, base, len }
    }
}

impl<D: BlockDevice> BlockDevice for OffsetBlockDevice<D> {
    fn size(&self) -> u64 {
        self.len
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let blen = buf.len() as u64;
        let end = offset
            .checked_add(blen)
            .ok_or(BlockError::OutOfRange {
                offset,
                len: blen,
                size: self.len,
            })?;
        if end > self.len {
            return Err(BlockError::OutOfRange {
                offset,
                len: blen,
                size: self.len,
            });
        }
        let abs = self
            .base
            .checked_add(offset)
            .ok_or(BlockError::OutOfRange {
                offset,
                len: blen,
                size: self.len,
            })?;
        self.inner.read_at(abs, buf)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub struct MemDevice {
        pub data: Vec<u8>,
    }
    impl BlockDevice for MemDevice {
        fn size(&self) -> u64 {
            self.data.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let o = offset as usize;
            let src = self.data.get(o..o + buf.len()).ok_or(
                BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.data.len() as u64,
                },
            )?;
            buf.copy_from_slice(src);
            Ok(())
        }
    }

    #[test]
    fn rebases_reads_to_base() {
        let mut data = vec![0u8; 4096];
        data[1000..1004].copy_from_slice(b"APFS");
        let mut dev = OffsetBlockDevice::new(MemDevice { data }, 1000, 100);
        assert_eq!(dev.size(), 100);
        let mut b = [0u8; 4];
        dev.read_at(0, &mut b).unwrap();
        assert_eq!(&b, b"APFS");
    }

    #[test]
    fn read_past_logical_end_errors() {
        let dev_data = MemDevice { data: vec![0u8; 4096] };
        let mut dev = OffsetBlockDevice::new(dev_data, 1000, 16);
        let mut b = [0u8; 32];
        assert!(matches!(
            dev.read_at(0, &mut b),
            Err(BlockError::OutOfRange { .. })
        ));
    }
}
```

Create `crates/winfsp-host/tests/gpt_offset_fsview.rs`:

```rust
//! End-to-end (macOS-runnable): a GPT-prefixed copy of the APFS fixture is
//! parsed by `gpt`, rebased by `OffsetBlockDevice`, and read by
//! `apfs::FsView` — proving the M4b cross-platform stack without Windows.

use std::io::{Read, Write};
use std::path::PathBuf;

use apfs::FsView;
use apfs_core::block_device::FileBlockDevice;
use winfsp_host::gpt::find_apfs_container;
use winfsp_host::offset_dev::OffsetBlockDevice;

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/apfs-large.img")
}

/// Build a temp disk image: [protective MBR][GPT hdr][GPT table][... pad to
/// container LBA ...][raw apfs-large.img bytes]. Container firstLBA chosen
/// so the container is 1 MiB into the disk (well past the GPT table).
#[test]
fn fsview_over_gpt_wrapped_fixture() {
    let fx = fixture_path();
    if !fx.exists() {
        eprintln!("SKIP: fixture {} absent", fx.display());
        return;
    }
    let mut apfs_bytes = Vec::new();
    std::fs::File::open(&fx)
        .unwrap()
        .read_to_end(&mut apfs_bytes)
        .unwrap();

    let first_lba: u64 = 2048; // 1 MiB
    let sector = 512u64;
    let cont_off = (first_lba * sector) as usize;
    let last_lba = first_lba + (apfs_bytes.len() as u64 / sector) - 1;

    let mut disk = vec![0u8; cont_off + apfs_bytes.len()];
    disk[510] = 0x55;
    disk[511] = 0xAA;
    disk[512..520].copy_from_slice(b"EFI PART");
    disk[512 + 72..512 + 80].copy_from_slice(&2u64.to_le_bytes());
    disk[512 + 80..512 + 84].copy_from_slice(&128u32.to_le_bytes());
    disk[512 + 84..512 + 88].copy_from_slice(&128u32.to_le_bytes());
    let ent = 2 * 512;
    let apfs_guid: [u8; 16] = [
        0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11, 0xAA, 0x11, 0x00,
        0x30, 0x65, 0x43, 0xEC, 0xAC,
    ];
    disk[ent..ent + 16].copy_from_slice(&apfs_guid);
    disk[ent + 32..ent + 40].copy_from_slice(&first_lba.to_le_bytes());
    disk[ent + 40..ent + 48].copy_from_slice(&last_lba.to_le_bytes());
    disk[cont_off..cont_off + apfs_bytes.len()].copy_from_slice(&apfs_bytes);

    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    tmp.write_all(&disk).unwrap();
    tmp.flush().unwrap();

    let mut dev = FileBlockDevice::open(tmp.path()).unwrap();
    let range = find_apfs_container(&mut dev).unwrap();
    assert_eq!(range.offset, first_lba * sector);

    let dev = FileBlockDevice::open(tmp.path()).unwrap();
    let rebased = OffsetBlockDevice::new(dev, range.offset, range.length);
    let mut fs = FsView::open(rebased).unwrap();

    // Same assertions as the M4a FsView fixture test: root listing exists
    // and a known file reads back its 4096 'A' bytes.
    let root = fs.read_dir("/").unwrap();
    assert!(!root.is_empty(), "root dir must list entries");
    let attr = fs.getattr("/dir00/f000.bin").unwrap().unwrap();
    assert_eq!(attr.size, 4096);
    assert!(!attr.is_dir);
    let data = fs.read("/dir00/f000.bin", 0, 4096).unwrap();
    assert_eq!(data.len(), 4096);
    assert!(data.iter().all(|&b| b == b'A'));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p winfsp-host --test gpt_offset_fsview`
Expected: FAIL — `winfsp_host::offset_dev::OffsetBlockDevice` not yet public / not implemented (and `lib.rs` must `pub use`/expose `gpt` & `offset_dev`, already done in Task 1).

- [ ] **Step 3: Write minimal implementation**

`offset_dev.rs` content above is the impl. Confirm `crates/winfsp-host/src/lib.rs` has `pub mod gpt;` and `pub mod offset_dev;` (from Task 1). Adjust the M4a fixture path/expected file (`/dir00/f000.bin`, 4096 'A') to whatever `crates/apfs/tests/fsview_real_image.rs` actually asserts for `apfs-large.img`; reuse the exact known values from that test so this stays in lockstep.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p winfsp-host` (unit + the integration test)
Expected: PASS — `offset_dev` units + `fsview_over_gpt_wrapped_fixture` green (or SKIP printed if the fixture is absent in this checkout; regenerate with `cargo run -p xtask -- gen-fixture --large` first).
Run: `cargo test --workspace` — Expected: PASS (no macOS regression).

- [ ] **Step 5: Commit**

```bash
git add crates/winfsp-host/src/offset_dev.rs crates/winfsp-host/tests/gpt_offset_fsview.rs
git commit -m "feat(winfsp-host): OffsetBlockDevice + end-to-end FsView over GPT-wrapped fixture"
```

---

### Task 5: `#[cfg(windows)]` raw `\\.\PhysicalDriveN` device + APFS auto-detect

**Files:**
- Modify: `crates/winfsp-host/src/winraw.rs`

Windows-only. Opens a raw physical drive, queries its sector size, implements `BlockDevice` using the `align` helper, and enumerates drives to auto-find the APFS USB. Compiles only under `#[cfg(windows)]`; verified on the SSH box (Task 8) — the compiler + real device are the arbiter for exact `windows` crate signatures (treat any doc as hypothesis; pin `windows = 0.58` and adjust call sites to the pinned API, recording the resolved shape in comments).

- [ ] **Step 1: Write the failing test**

Append to `crates/winfsp-host/src/winraw.rs`:

```rust
//! Raw `\\.\PhysicalDriveN` block device (Windows-only).

use std::ffi::c_void;

use apfs_core::block_device::{BlockDevice, BlockError};
use widestring::U16CString;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, SetFilePointerEx, FILE_BEGIN, FILE_FLAG_NO_BUFFERING,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, FILE_GENERIC_READ,
};
use windows::Win32::System::Ioctl::{
    IOCTL_DISK_GET_DRIVE_GEOMETRY, DISK_GEOMETRY,
};
use windows::Win32::System::IO::DeviceIoControl;

use crate::align::aligned_window;
use crate::gpt::find_apfs_container;

pub struct PhysicalDrive {
    handle: HANDLE,
    sector: u64,
    size: u64,
}

// SAFETY note: HANDLE is a raw OS handle; PhysicalDrive owns it exclusively
// and closes it on Drop. Sent across winfsp worker threads only behind the
// mount's Mutex (see mount.rs), so no concurrent handle use.
unsafe impl Send for PhysicalDrive {}

impl PhysicalDrive {
    /// Open `\\.\PhysicalDrive{n}` for raw read.
    pub fn open(n: u32) -> Result<Self, BlockError> {
        let path = U16CString::from_str(format!("\\\\.\\PhysicalDrive{n}"))
            .map_err(|_| io_other("bad device path"))?;
        // Exact CreateFileW signature is resolved against the pinned
        // `windows` crate on the build box; this is the 0.58 shape.
        let handle = unsafe {
            CreateFileW(
                windows::core::PCWSTR(path.as_ptr()),
                FILE_GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_NO_BUFFERING,
                HANDLE::default(),
            )
        }
        .map_err(|e| io_other_string(format!("CreateFileW: {e}")))?;

        let mut geo = DISK_GEOMETRY::default();
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_DISK_GET_DRIVE_GEOMETRY,
                None,
                0,
                Some(&mut geo as *mut _ as *mut c_void),
                std::mem::size_of::<DISK_GEOMETRY>() as u32,
                Some(&mut returned),
                None,
            )
        };
        let sector = if ok.is_ok() && geo.BytesPerSector >= 512 {
            geo.BytesPerSector as u64
        } else {
            512
        };
        let cylinders = geo.Cylinders;
        let size = unsafe { *cylinders.QuadPart() } as u64
            * geo.TracksPerCylinder as u64
            * geo.SectorsPerTrack as u64
            * sector;

        Ok(Self { handle, sector, size })
    }

    /// Open the first physical drive whose GPT has an APFS partition.
    /// Returns the opened drive plus the APFS container byte range.
    pub fn autodetect_apfs(
    ) -> Result<(Self, crate::gpt::ContainerRange), BlockError> {
        for n in 0..16u32 {
            if let Ok(mut d) = PhysicalDrive::open(n) {
                if let Ok(range) = find_apfs_container(&mut d) {
                    return Ok((d, range));
                }
            }
        }
        Err(io_other("no APFS physical drive found"))
    }

    pub fn sector(&self) -> u64 {
        self.sector
    }
}

fn io_other(msg: &'static str) -> BlockError {
    BlockError::Io(std::io::Error::new(std::io::ErrorKind::Other, msg))
}
fn io_other_string(msg: String) -> BlockError {
    BlockError::Io(std::io::Error::new(std::io::ErrorKind::Other, msg))
}

impl Drop for PhysicalDrive {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

impl BlockDevice for PhysicalDrive {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.is_empty() {
            return Ok(());
        }
        let w = aligned_window(offset, buf.len(), self.sector);
        let mut scratch = vec![0u8; w.aligned_len];
        let mut newpos = 0i64;
        let moved = unsafe {
            SetFilePointerEx(
                self.handle,
                w.aligned_offset as i64,
                Some(&mut newpos),
                FILE_BEGIN,
            )
        };
        moved.map_err(|e| io_other_string(format!("seek: {e}")))?;

        let mut done = 0usize;
        while done < scratch.len() {
            let mut read = 0u32;
            let chunk = &mut scratch[done..];
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    Some(chunk),
                    Some(&mut read),
                    None,
                )
            };
            ok.map_err(|e| io_other_string(format!("ReadFile: {e}")))?;
            if read == 0 {
                return Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.size,
                });
            }
            done += read as usize;
        }

        let slice = scratch
            .get(w.skip..w.skip + w.take)
            .ok_or(io_other("alignment slice out of range"))?;
        buf.copy_from_slice(slice);
        Ok(())
    }
}
```

(No macOS unit test for this module — it is `#[cfg(windows)]`. Its red→green is the Task 8 SSH build + real-USB run. Add a `#[cfg(all(test, windows))]` smoke test asserting `PhysicalDrive::autodetect_apfs()` succeeds and the rebased container starts with `NXSB`, run on the box in Task 8.)

```rust
#[cfg(all(test, windows))]
mod win_tests {
    use super::*;
    #[test]
    fn autodetect_finds_nxsb() {
        let (mut d, range) = PhysicalDrive::autodetect_apfs().unwrap();
        let mut m = [0u8; 4];
        d.read_at(range.offset + 32, &mut m).unwrap();
        assert_eq!(&m, b"NXSB");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

On macOS this module is not compiled (`#[cfg(windows)]`). Verification is deferred to Task 8 on the Windows box where `cargo build` first FAILS until the `windows`-crate call signatures match the pinned `0.58` API (compiler = arbiter), then PASSES.

- [ ] **Step 3: Write minimal implementation**

Implementation above. On the box, run `cargo doc -p windows --no-deps` / inspect errors and adjust `CreateFileW`/`ReadFile`/`SetFilePointerEx`/`DeviceIoControl` argument shapes to the pinned crate; keep the alignment logic (via `aligned_window`) unchanged.

- [ ] **Step 4: Run test to verify it passes**

Deferred to Task 8 (SSH): `cargo build -p winfsp-host --target x86_64-pc-windows-msvc` then the `win_tests::autodetect_finds_nxsb` run against the real USB.

- [ ] **Step 5: Commit**

```bash
git add crates/winfsp-host/src/winraw.rs
git commit -m "feat(winfsp-host): raw PhysicalDrive block device + APFS autodetect (windows)"
```

---

### Task 6: `#[cfg(windows)]` `ApfsFs` winfsp callbacks + `mount_apfs`

**Files:**
- Modify: `crates/winfsp-host/src/mount.rs`

Implement the read-only `winfsp` `FileSystemContext` over `Mutex<FsView<OffsetBlockDevice<PhysicalDrive>>>`. Every callback body runs inside `std::panic::catch_unwind`; a caught panic maps to `STATUS_IO_DEVICE_ERROR` so a reader bug degrades a single op instead of crashing the mount. WinFsp paths are `\`-separated UTF-16 → translate to `/` for `FsView`.

- [ ] **Step 1: Write the failing test**

Append to `crates/winfsp-host/src/mount.rs` (the exact `winfsp` API — module paths `winfsp::filesystem` vs `winfsp::host`, `FileInfo`/`DirInfo`/`OpenFileInfo` field names — is resolved against the pinned `winfsp = 0.11` crate on the build box; the shape below follows winfsp-rs docs and is corrected to compile there, recording the resolved API in comments):

```rust
//! WinFsp read-only host delegating to `apfs::FsView` (Windows-only).

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use apfs::FsView;
use widestring::U16CStr;
use winfsp::filesystem::{
    DirInfo, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp::U16CString;

use crate::offset_dev::OffsetBlockDevice;
use crate::winraw::PhysicalDrive;

type Dev = OffsetBlockDevice<PhysicalDrive>;

const STATUS_SUCCESS: i32 = 0;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC0000034u32 as i32;
const STATUS_IO_DEVICE_ERROR: i32 = 0xC0000185u32 as i32;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_READONLY: u32 = 0x01;

pub struct ApfsFs {
    fs: Mutex<FsView<Dev>>,
}

/// Per-open handle: the normalized `/`-path of the opened node.
pub struct Handle {
    path: String,
}

fn win_to_unix(name: &U16CStr) -> String {
    let s = name.to_string_lossy();
    let s = s.replace('\\', "/");
    if s.is_empty() {
        "/".to_string()
    } else {
        s
    }
}

fn guard<T>(f: impl FnOnce() -> winfsp::Result<T>) -> winfsp::Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => Err(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR)),
    }
}

impl ApfsFs {
    pub fn new(fs: FsView<Dev>) -> Self {
        Self { fs: Mutex::new(fs) }
    }

    fn fill_info(attr: &apfs::Attr, info: &mut FileInfo) {
        info.file_size = attr.size;
        info.allocation_size = (attr.size + 4095) & !4095;
        info.file_attributes = if attr.is_dir {
            FILE_ATTRIBUTE_DIRECTORY
        } else {
            FILE_ATTRIBUTE_READONLY
        };
        info.index_number = attr.inode;
    }
}

impl FileSystemContext for ApfsFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _sd: Option<&mut [std::ffi::c_void]>,
        _resolve: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        guard(|| {
            let path = win_to_unix(file_name);
            let mut fs = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            match fs
                .getattr(&path)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
            {
                None => Err(winfsp::FspError::NTSTATUS(
                    STATUS_OBJECT_NAME_NOT_FOUND,
                )),
                Some(a) => Ok(FileSecurity {
                    reparse: false,
                    sz_security_descriptor: 0,
                    attributes: if a.is_dir {
                        FILE_ATTRIBUTE_DIRECTORY
                    } else {
                        FILE_ATTRIBUTE_READONLY
                    },
                }),
            }
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: windows::Win32::Storage::FileSystem::FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        guard(|| {
            let path = win_to_unix(file_name);
            let mut fs = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let attr = fs
                .getattr(&path)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                .ok_or(winfsp::FspError::NTSTATUS(
                    STATUS_OBJECT_NAME_NOT_FOUND,
                ))?;
            Self::fill_info(&attr, file_info.as_mut());
            Ok(Handle { path })
        })
    }

    fn close(&self, _context: Self::FileContext) {}

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        guard(|| {
            let mut fs = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let attr = fs
                .getattr(&context.path)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                .ok_or(winfsp::FspError::NTSTATUS(
                    STATUS_OBJECT_NAME_NOT_FOUND,
                ))?;
            Self::fill_info(&attr, file_info);
            Ok(())
        })
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        guard(|| {
            let mut fs = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let data = fs
                .read(&context.path, offset, buffer.len())
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let n = data.len().min(buffer.len());
            buffer
                .get_mut(..n)
                .ok_or(winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?
                .copy_from_slice(&data[..n]);
            Ok(n as u32)
        })
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        _marker: winfsp::filesystem::DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        guard(|| {
            let mut fs = self
                .fs
                .lock()
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let entries = fs
                .read_dir(&context.path)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let mut cursor = 0u32;
            for e in entries {
                let mut di = DirInfo::<255>::new();
                di.set_name(e.name.as_str())
                    .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
                let child = if context.path == "/" {
                    format!("/{}", e.name)
                } else {
                    format!("{}/{}", context.path, e.name)
                };
                if let Some(a) = fs.getattr(&child).ok().flatten() {
                    Self::fill_info(&a, di.file_info_mut());
                }
                if !di.append_to_buffer(buffer, &mut cursor) {
                    break;
                }
            }
            DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
            Ok(cursor)
        })
    }
}

/// Build the device stack and mount the APFS container at `mountpoint`
/// (e.g. `"Z:"`). If `drive` is `None`, auto-detect the APFS USB.
pub fn mount_apfs(
    drive: Option<u32>,
    mountpoint: &str,
) -> winfsp::Result<FileSystemHost<'static>> {
    let _init = winfsp::winfsp_init()
        .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
    let (pd, range) = match drive {
        Some(n) => {
            let mut pd = PhysicalDrive::open(n)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            let r = crate::gpt::find_apfs_container(&mut pd)
                .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
            (pd, r)
        }
        None => PhysicalDrive::autodetect_apfs()
            .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?,
    };
    let dev = OffsetBlockDevice::new(pd, range.offset, range.length);
    let fsview = FsView::open(dev)
        .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;

    let mut vp = VolumeParams::new();
    vp.filesystem_name("APFS")
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .read_only_volume(true);

    let mut host = FileSystemHost::new(vp, ApfsFs::new(fsview))
        .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
    host.mount(mountpoint)
        .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
    host.start()
        .map_err(|_| winfsp::FspError::NTSTATUS(STATUS_IO_DEVICE_ERROR))?;
    Ok(host)
}
```

- [ ] **Step 2: Run test to verify it fails**

macOS: not compiled (`#[cfg(windows)]`). On the box (Task 8) `cargo build` FAILS until the `winfsp = 0.11` API names match (e.g. `winfsp::filesystem` vs `winfsp::host` paths, `FspError` variant, `DirInfo::set_name` arg type, `VolumeParams` builder method names). Resolve each against `cargo doc -p winfsp` on the box; the compiler is the arbiter. Note `apfs_core::catalog::DirEntry` field name for the entry name — confirm it is `name: String` (adjust `e.name` accordingly).

- [ ] **Step 3: Write minimal implementation**

The implementation above. On the box, fix only API-shape mismatches; do not change the delegation logic or the `catch_unwind` guard.

- [ ] **Step 4: Run test to verify it passes**

Deferred to Task 8 (SSH build).

- [ ] **Step 5: Commit**

```bash
git add crates/winfsp-host/src/mount.rs
git commit -m "feat(winfsp-host): read-only winfsp callbacks delegating to FsView (windows)"
```

---

### Task 7: `#[cfg(windows)]` `apfs-mount` CLI binary

**Files:**
- Modify: `crates/winfsp-host/src/main.rs`

- [ ] **Step 1: Write the failing test**

Replace `crates/winfsp-host/src/main.rs`:

```rust
//! apfs-mount — mount an APFS USB/SSD as a Windows drive letter.
//!
//! Usage: apfs-mount [--drive N] [--mount Z:]
//! No --drive => auto-detect the first APFS physical drive.

#[cfg(not(windows))]
fn main() {
    eprintln!("apfs-mount is Windows-only.");
}

#[cfg(windows)]
fn main() {
    use std::sync::mpsc::channel;

    let mut drive: Option<u32> = None;
    let mut mount = "Z:".to_string();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--drive" => {
                drive = args.next().and_then(|s| s.parse().ok());
            }
            "--mount" => {
                if let Some(m) = args.next() {
                    mount = m;
                }
            }
            other => {
                eprintln!("unknown arg: {other}");
            }
        }
    }

    match winfsp_host::mount::mount_apfs(drive, &mount) {
        Ok(mut host) => {
            println!("APFS mounted at {mount}. Press Ctrl+C to unmount.");
            let (tx, rx) = channel();
            let _ = ctrlc_set(move || {
                let _ = tx.send(());
            });
            let _ = rx.recv();
            host.stop();
            host.unmount();
            println!("Unmounted.");
        }
        Err(e) => {
            eprintln!("mount failed: {e:?}");
            std::process::exit(1);
        }
    }
}

// Minimal Ctrl+C handler without an extra crate dependency: use winfsp's
// own service stop path if available; otherwise block on console read.
#[cfg(windows)]
fn ctrlc_set<F: FnOnce() + Send + 'static>(f: F) -> std::io::Result<()> {
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
        f();
    });
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

macOS: `cargo build -p winfsp-host` builds the `#[cfg(not(windows))]` arm — Expected: PASS (no-op binary). Real verification on the box in Task 8.

- [ ] **Step 3: Write minimal implementation**

As above. The Ctrl+C substitute (Enter-to-unmount on stdin) keeps zero extra deps; acceptable for an operator-run tool. If the pinned `winfsp` exposes a service runner, prefer it on the box and note it in a comment.

- [ ] **Step 4: Run test to verify it passes**

Run (macOS): `cargo build --workspace` — Expected: PASS.
Box verification in Task 8.

- [ ] **Step 5: Commit**

```bash
git add crates/winfsp-host/src/main.rs
git commit -m "feat(winfsp-host): apfs-mount CLI binary (windows)"
```

---

### Task 8: Windows build + real-USB mount proof over SSH (decisive milestone cross-check)

**Files:**
- Modify: `docs/ROADMAP.md`

This is the milestone-end decisive real-target proof (project methodology: the real disk + real OS is the final arbiter). SSH alias `apfswin` (key auth, no password). Repo path on the box: `C:\apfs\apfs-explorer` (clone/pull from the Mac via `git` over SSH or `rsync`-equivalent; do NOT commit Windows build artifacts).

- [ ] **Step 1: Sync repo to the box**

From the Mac: push the `feat/m4b-winfsp-host` branch, then on the box `git -C C:\apfs\apfs-explorer fetch && git checkout feat/m4b-winfsp-host`. (If no shared remote, create a bare repo on the box and push over SSH, or copy the working tree excluding `/target`.)

- [ ] **Step 2: Resolve the Windows API shapes (compiler = arbiter)**

On the box: `cargo build -p winfsp-host` (host triple is `x86_64-pc-windows-msvc`). Iterate on `windows = 0.58` / `winfsp = 0.11` signature mismatches in `winraw.rs` and `mount.rs` only (paths, arg counts, `FspError` variant, builder method names). Record each resolved API shape in a `// winfsp 0.11:` / `// windows 0.58:` code comment. Do NOT alter `align`/`gpt`/`offset_dev` or the delegation/`catch_unwind` logic.
Expected end state: `cargo build -p winfsp-host` PASS on the box.

- [ ] **Step 3: Windows unit + cross-platform tests on the box**

Run on the box: `cargo test -p winfsp-host` (runs `align`/`gpt`/`offset_dev` units + `gpt_offset_fsview` + the `#[cfg(windows)]` `winraw::win_tests::autodetect_finds_nxsb` against the real USB).
Expected: all PASS, including `autodetect_finds_nxsb` reading `NXSB` at the autodetected container offset on `\\.\PhysicalDrive3`.

- [ ] **Step 4: Real drive-letter mount + differential proof**

On the box (admin PowerShell): `cargo run -p winfsp-host --bin apfs-mount -- --mount Z:` (auto-detect). Then, in a second SSH session, differentially verify the mounted `Z:\` against the known-good reader: pick a sample of files/dirs, compare `Get-ChildItem`/file bytes on `Z:\` to `apfs::FsView` output for the same paths (FsView output is already proven byte-exact vs. macOS kernel in M3-RT, so FsView is the oracle here). Assert: directory listings match, a known file's size + first/last 64 bytes match, a decmpfs (LZVN method-8) file reads back fully. Capture pass/fail counts; require failures = 0 and files_ok > 0. Unmount (send a line to the binary's stdin or Ctrl+C).

- [ ] **Step 5: Final review + ROADMAP/KB update, then finish branch**

Dispatch the milestone-end consolidated review (spec-compliance + quality + gates, reading the diff, not the report). Then update `docs/ROADMAP.md`: add the M4b row to the status table with the squash commit, mark "Compact sonrası DEVAM" step 4 next = **M5** (apfs-service + tray auto-mount), note the resolved `winfsp 0.11`/`windows 0.58` API as a durable spec breadcrumb and the SSH `apfswin` recipe. Re-index `project:roadmap-state`. Commit ROADMAP. Then use superpowers:finishing-a-development-branch (squash-merge `feat/m4b-winfsp-host` → main, delete branch, verify gates post-merge).

```bash
git add docs/ROADMAP.md
git commit -m "docs(roadmap): M4b winfsp-host complete; next M5"
```

---

## Self-Review

**Spec coverage:** ROADMAP M4b requires: raw `\\.\PhysicalDriveN` BlockDevice (Task 5) ✓, GPT→APFS-container offset (Task 3) ✓, `winfsp` callbacks → `apfs::FsView` (Task 6) ✓, each callback `catch_unwind` (Task 6 `guard`) ✓, `#[cfg(windows)]`/target-dep gate so macOS workspace builds (Task 1 Cargo.toml + cfg) ✓, plan + Windows build/test recipe (Task 8) ✓, operator/SSH real drive-letter run (Task 8) ✓, FsView delegation surface (Task 6) ✓. M3e/M3f remain deferred (untouched). No APFS on-disk spec changed → no md.6 gate.

**Placeholder scan:** every code step contains complete code. The only deliberate deferrals are the exact `winfsp 0.11`/`windows 0.58` API call shapes, explicitly framed as "compiler-on-the-box is the arbiter" with a concrete resolution procedure (Task 8 Step 2) — consistent with the project's empirical-arbiter methodology, not a placeholder.

**Type consistency:** `OffsetBlockDevice` constructed `new(inner, base, len)` everywhere; `find_apfs_container` returns `ContainerRange{offset,length}` used consistently; `FsView::open(dev)` / `getattr→Option<Attr{inode,size,is_dir}>` / `read(path,offset,len)→Vec<u8>` / `read_dir→Vec<DirEntry>` match the M4a `apfs` API verbatim. `DirEntry.name` field usage flagged in Task 6 Step 2 to confirm against `apfs_core::catalog::DirEntry`.

**Scope:** single milestone, 8 bite-sized TDD tasks, lean consolidated review per project process.
