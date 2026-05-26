# M1a — Object Layer + Container Superblock Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Steps use `- [ ]` checkboxes.

**Goal:** `apfs-core` can open a real APFS container image, validate physical block 0 (NX_MAGIC + Fletcher-64), and parse the `nx_superblock` (block size/count, xid, omap/spaceman/reaper oids, volume fs_oids). TDD against a **real macOS-generated APFS image**.

**Architecture:** Pure-Rust, no unsafe. Bytes are read through the existing `BlockDevice` trait. New modules in `apfs-core`: `endian` (LE readers), `checksum` (Fletcher-64), `obj` (obj_phys header + type constants), `nx` (nx_superblock), `container` (open + validate). Fletcher-64 is the community-canonical APFS algorithm (spec omits it — confirmed via PDF/Context7/NotebookLM); correctness is proven empirically against a genuine Apple-written checksum in a real image fixture.

**Tech Stack:** Rust stable, thiserror, the existing `BlockDevice`/`BlockError`, macOS `hdiutil` for the real fixture, `xtask` for fixture generation.

**Spec source of truth (subagents: query before coding):** `ctx_search(source:"apfs-spec:m1-confirmed")` (exact offsets/constants/algorithm), full Apple PDF `ctx_search(source:"apfs-spec:pdf")`, Context7 structs `ctx_search(source:"apfs-spec:m1-structs")`. Also `docs/refs/apfs-spec-sources.md`.

**Mandatory rules:** `git -C /Users/eiaydin/Documents/projects/apfs-explorer` (no `cd`); never `git add -A`/`.`; English only (code/comments/commits); no `unwrap/expect/panic` outside `#[cfg(test)]` (`apfs-core` is `#![forbid(unsafe_code)]`, clippy gate denies them); commit trailer `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`. Branch: `feat/m1-object-layer`.

---

## File Structure (end state)

```
crates/apfs-core/src/
  lib.rs           # add: pub mod endian/checksum/obj/nx/container + re-exports
  block_device.rs  # (unchanged, from M0)
  endian.rs        # NEW — bounds-checked little-endian readers
  checksum.rs      # NEW — Fletcher-64 + verify
  obj.rs           # NEW — ObjPhys + ObjectType/flags constants
  nx.rs            # NEW — NxSuperblock parse + validate
  container.rs     # NEW — Container::open(BlockDevice)
xtask/src/main.rs  # add: `gen-fixture` subcommand (hdiutil → tests/fixtures)
tests/fixtures/    # apfs-tiny.img (real APFS, generated; gitignored — see Task 1)
```

---

## Task 1: `xtask gen-fixture` — generate a real APFS image fixture

**Files:** Modify `xtask/src/main.rs`; modify root `.gitignore`.

- [ ] **Step 1: Gitignore the generated fixture (raw image, regenerable, macOS-only).**
Append to `/Users/eiaydin/Documents/projects/apfs-explorer/.gitignore`:
```
# Generated real APFS fixture (macOS hdiutil; regenerate via `cargo run -p xtask -- gen-fixture`)
/tests/fixtures/apfs-tiny.img
/tests/fixtures/apfs-tiny.img.manifest.json
```

- [ ] **Step 2: Replace `xtask/src/main.rs` with:**
```rust
//! xtask — fixture generation + differential harness (M2+). M1a: gen-fixture.
use std::path::Path;
use std::process::Command;

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "gen-fixture" => match gen_fixture() {
            Ok(p) => println!("fixture ready: {p}"),
            Err(e) => {
                eprintln!("gen-fixture failed: {e}");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("usage: xtask gen-fixture");
            std::process::exit(2);
        }
    }
}

/// Create a small real APFS container image at tests/fixtures/apfs-tiny.img.
/// macOS only (uses hdiutil). `-layout NONE` => no partition map, so the APFS
/// container superblock is at file offset 0 (physical block 0).
fn gen_fixture() -> Result<String, String> {
    let root = env!("CARGO_MANIFEST_DIR");
    let proj = Path::new(root).parent().ok_or("no parent")?;
    let fixtures = proj.join("tests/fixtures");
    std::fs::create_dir_all(&fixtures).map_err(|e| e.to_string())?;
    let img = fixtures.join("apfs-tiny.img");
    if img.exists() {
        std::fs::remove_file(&img).map_err(|e| e.to_string())?;
    }
    // hdiutil writes <stem>.dmg; we request the stem then rename to .img.
    let stem = fixtures.join("apfs-tiny");
    let out = Command::new("hdiutil")
        .args([
            "create",
            "-megabytes",
            "16",
            "-layout",
            "NONE",
            "-fs",
            "APFS",
            "-volname",
            "APFSTEST",
            "-ov",
            stem.to_str().ok_or("bad path")?,
        ])
        .output()
        .map_err(|e| format!("hdiutil spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "hdiutil: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let dmg = fixtures.join("apfs-tiny.dmg");
    std::fs::rename(&dmg, &img).map_err(|e| e.to_string())?;
    // Sanity: NX_MAGIC 'NXSB' is at file offset 32 (after the 32-byte obj_phys).
    let bytes = std::fs::read(&img).map_err(|e| e.to_string())?;
    let magic = bytes.get(32..36).ok_or("image too small")?;
    if magic != b"NXSB" {
        return Err(format!("offset 32 not NXSB: {magic:02x?}"));
    }
    Ok(img.display().to_string())
}
```

- [ ] **Step 3: Generate and verify.**
Run: `cargo run -p xtask --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- gen-fixture`
Expected: `fixture ready: .../tests/fixtures/apfs-tiny.img`. Then
`xxd -s 32 -l 4 /Users/eiaydin/Documents/projects/apfs-explorer/tests/fixtures/apfs-tiny.img` → `4e58 5342  NXSB`.
If `hdiutil` reports `-layout NONE` invalid on this macOS, retry with `-layout SPUD` (single-partition); re-check that offset-32 == `NXSB` (with SPUD a partition map may shift the container — if magic not at 32, scan: the container is at the partition start; for M1a require `NONE` works — macOS 15 supports it; if neither yields NXSB at 0 report BLOCKED with `xxd -l 64` output).

- [ ] **Step 4: Commit.**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add xtask/src/main.rs .gitignore
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(xtask): gen-fixture creates a real APFS image (hdiutil)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `endian.rs` — bounds-checked little-endian readers (TDD)

**Files:** Create `crates/apfs-core/src/endian.rs`; modify `crates/apfs-core/src/lib.rs`.

- [ ] **Step 1: Failing test — create `crates/apfs-core/src/endian.rs`:**
```rust
//! Bounds-checked little-endian readers over byte slices. No panics.
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("buffer too short: need {need} bytes at offset {at}, have {len}")]
    Short { at: usize, need: usize, len: usize },
    #[error("bad magic: expected {expected:#010x}, found {found:#010x}")]
    BadMagic { expected: u32, found: u32 },
    #[error("checksum mismatch: stored {stored:#018x}, computed {computed:#018x}")]
    BadChecksum { stored: u64, computed: u64 },
}

fn slice(buf: &[u8], at: usize, need: usize) -> Result<&[u8], ParseError> {
    buf.get(at..at + need)
        .ok_or(ParseError::Short { at, need, len: buf.len() })
}

pub fn u32_le(buf: &[u8], at: usize) -> Result<u32, ParseError> {
    let s = slice(buf, at, 4)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

pub fn u64_le(buf: &[u8], at: usize) -> Result<u64, ParseError> {
    let s = slice(buf, at, 8)?;
    let mut a = [0u8; 8];
    a.copy_from_slice(s);
    Ok(u64::from_le_bytes(a))
}

pub fn i64_le(buf: &[u8], at: usize) -> Result<i64, ParseError> {
    Ok(u64_le(buf, at)? as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_le_values() {
        let b = [0x4e, 0x58, 0x53, 0x42, 1, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(u32_le(&b, 0).unwrap(), 0x4253_584e);
        assert_eq!(u64_le(&b, 4).unwrap(), 1);
        assert_eq!(i64_le(&b, 4).unwrap(), 1);
    }

    #[test]
    fn short_buffer_errors_not_panics() {
        let b = [0u8; 3];
        assert_eq!(
            u32_le(&b, 0),
            Err(ParseError::Short { at: 0, need: 4, len: 3 })
        );
        assert!(u64_le(&b, 0).is_err());
    }
}
```

- [ ] **Step 2: Add to `crates/apfs-core/src/lib.rs`** (after the existing `pub use block_device::...` line):
```rust
pub mod endian;
pub use endian::ParseError;
```

- [ ] **Step 3: Test fails then passes.** Run `cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml`. (It will actually pass on first run since impl is included — acceptable: this module is pure helpers; the failing/passing TDD discipline is exercised in Tasks 3/5/6 which depend on it.) Expected: all apfs-core tests pass (4: 2 block_device + 2 endian).

- [ ] **Step 4: Commit.**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/endian.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): bounds-checked little-endian readers + ParseError

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `checksum.rs` — APFS Fletcher-64 (TDD, real-image proof)

**Files:** Create `crates/apfs-core/src/checksum.rs`; modify `lib.rs`.

- [ ] **Step 1: Failing test first.** Create `crates/apfs-core/src/checksum.rs` with ONLY the tests + function signature stub `pub fn fletcher64(_block: &[u8]) -> u64 { 0 }` and `pub fn verify_block(_block: &[u8]) -> Result<(), crate::endian::ParseError> { Ok(()) }`, plus:
```rust
//! APFS Fletcher-64. The Apple spec leaves the algorithm unspecified
//! (confirmed: PDF stub, Context7, NotebookLM); this is the community-canonical
//! algorithm used by libfsapfs/apfs-fuse/linux-apfs, proven here against a real
//! macOS-written checksum (tests/fixtures/apfs-tiny.img block 0).
use crate::endian::{u64_le, ParseError};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn verifies_real_apfs_block_zero() {
        let img = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/apfs-tiny.img");
        if !Path::new(img).exists() {
            // macOS-only fixture; skip cleanly elsewhere (CI macOS job covers it).
            eprintln!("skip: fixture missing ({img}) — run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let data = std::fs::read(img).unwrap();
        let block = &data[0..4096];
        // Genuine Apple checksum must verify.
        verify_block(block).expect("real APFS block 0 must pass Fletcher-64");
        // Tamper one payload byte (outside the 8-byte o_cksum) -> must fail.
        let mut bad = block.to_vec();
        bad[100] ^= 0xFF;
        assert!(verify_block(&bad).is_err());
    }
}
```

- [ ] **Step 2: Run, expect FAIL.** `cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml fletcher` (or run gen-fixture first if needed). Expected: `verifies_real_apfs_block_zero` FAILS at `verify_block(...).expect(...)` (stub returns Ok but the tamper assertion / or replace stub so it fails — ensure the stub `verify_block` returns `Ok(())` so the FIRST assertion passes but the tamper `assert!(... is_err())` FAILS → red). Confirm it is red for the right reason.

- [ ] **Step 3: Implement** (replace the stubs, keep tests):
```rust
const MOD: u64 = 0xFFFF_FFFF;

/// Fletcher-64 over `data` interpreted as little-endian u32 words.
/// `data` MUST be the object bytes starting AFTER the 8-byte o_cksum field
/// (i.e. block[8..block_size]); its length must be a multiple of 4.
fn fletcher64_words(data: &[u8]) -> u64 {
    let mut s1: u64 = 0;
    let mut s2: u64 = 0;
    for w in data.chunks_exact(4) {
        let v = u32::from_le_bytes([w[0], w[1], w[2], w[3]]) as u64;
        s1 = (s1 + v) % MOD;
        s2 = (s2 + s1) % MOD;
    }
    let c1 = MOD - ((s1 + s2) % MOD);
    let c2 = MOD - ((s1 + c1) % MOD);
    (c2 << 32) | c1
}

/// Compute the APFS Fletcher-64 for a full object block. The stored o_cksum
/// occupies block[0..8] and is EXCLUDED from the input.
pub fn fletcher64(block: &[u8]) -> u64 {
    let body = block.get(8..).unwrap_or(&[]);
    let end = body.len() - (body.len() % 4);
    fletcher64_words(&body[..end])
}

/// Verify a block's stored Fletcher-64 (block[0..8], little-endian u64).
pub fn verify_block(block: &[u8]) -> Result<(), ParseError> {
    let stored = u64_le(block, 0)?;
    let computed = fletcher64(block);
    if stored == computed {
        Ok(())
    } else {
        Err(ParseError::BadChecksum { stored, computed })
    }
}
```

- [ ] **Step 4: Add to `lib.rs`:** `pub mod checksum;`

- [ ] **Step 5: gen-fixture then test → PASS.**
`cargo run -p xtask --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- gen-fixture` then `cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml`. Expected: `verifies_real_apfs_block_zero ... ok` — **a genuine Apple-written checksum validates**, proving the algorithm. All apfs-core tests pass.

- [ ] **Step 6: Commit.**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/checksum.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): APFS Fletcher-64 + verify, proven vs real image

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `obj.rs` — obj_phys header + object-type constants (TDD)

**Files:** Create `crates/apfs-core/src/obj.rs`; modify `lib.rs`.

- [ ] **Step 1: Failing test — create `crates/apfs-core/src/obj.rs`:**
```rust
//! obj_phys_t — the 32-byte header prefixing paged APFS objects.
//! Offsets/constants: ctx_search(source:"apfs-spec:m1-confirmed").
use crate::endian::{u32_le, u64_le, ParseError};

pub const OBJ_PHYS_SIZE: usize = 32;
pub const OBJECT_TYPE_MASK: u32 = 0x0000_ffff;
pub const OBJECT_TYPE_FLAGS_MASK: u32 = 0xffff_0000;
pub const OBJ_STORAGETYPE_MASK: u32 = 0xc000_0000;
pub const OBJECT_TYPE_NX_SUPERBLOCK: u32 = 0x0000_0001;
pub const OBJECT_TYPE_BTREE: u32 = 0x0000_0002;
pub const OBJECT_TYPE_BTREE_NODE: u32 = 0x0000_0003;
pub const OBJECT_TYPE_OMAP: u32 = 0x0000_000b;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjPhys {
    pub cksum: u64,
    pub oid: u64,
    pub xid: u64,
    pub o_type: u32,
    pub subtype: u32,
}

impl ObjPhys {
    /// Parse the 32-byte header from the start of `block`.
    pub fn parse(block: &[u8]) -> Result<Self, ParseError> {
        Ok(Self {
            cksum: u64_le(block, 0)?,
            oid: u64_le(block, 8)?,
            xid: u64_le(block, 16)?,
            o_type: u32_le(block, 24)?,
            subtype: u32_le(block, 28)?,
        })
    }
    /// Object type without storage-class/flag bits.
    pub fn object_type(&self) -> u32 {
        self.o_type & OBJECT_TYPE_MASK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_header_and_masks_type() {
        let mut b = [0u8; 32];
        b[0..8].copy_from_slice(&0xAABB_u64.to_le_bytes());
        b[8..16].copy_from_slice(&1u64.to_le_bytes());
        b[16..24].copy_from_slice(&7u64.to_le_bytes());
        // type 0x4000_0001 = NX_SUPERBLOCK | PHYSICAL flag
        b[24..28].copy_from_slice(&0x4000_0001u32.to_le_bytes());
        b[28..32].copy_from_slice(&0u32.to_le_bytes());
        let o = ObjPhys::parse(&b).unwrap();
        assert_eq!(o.oid, 1);
        assert_eq!(o.xid, 7);
        assert_eq!(o.o_type, 0x4000_0001);
        assert_eq!(o.object_type(), OBJECT_TYPE_NX_SUPERBLOCK);
    }

    #[test]
    fn short_block_errors() {
        assert!(ObjPhys::parse(&[0u8; 16]).is_err());
    }
}
```

- [ ] **Step 2: Run, expect PASS** (helpers exist). `cargo test -p apfs-core ... obj`. (TDD red/green discipline is enforced in Tasks 5/6 which compose this; this task is a thin typed wrapper — acceptable per plan.)

- [ ] **Step 3: Add to `lib.rs`:** `pub mod obj; pub use obj::ObjPhys;`

- [ ] **Step 4: Commit.**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/obj.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): obj_phys header + object-type constants

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: `nx.rs` — nx_superblock parse + validate (TDD vs real image)

**Files:** Create `crates/apfs-core/src/nx.rs`; modify `lib.rs`.

- [ ] **Step 1: Failing test first.** Create `crates/apfs-core/src/nx.rs` with the test below + a stub `impl NxSuperblock { pub fn parse(_b:&[u8])->Result<Self,ParseError>{ Err(ParseError::Short{at:0,need:0,len:0}) } }` and the struct fields, so the test compiles and FAILS:
```rust
//! nx_superblock_t — APFS container superblock (physical block 0).
//! Field offsets: ctx_search(source:"apfs-spec:m1-confirmed").
use crate::checksum::verify_block;
use crate::endian::{u32_le, u64_le, ParseError};
use crate::obj::{ObjPhys, OBJECT_TYPE_NX_SUPERBLOCK};

pub const NX_MAGIC: u32 = 0x4253_584e; // 'NXSB' little-endian
pub const NX_MAX_FILE_SYSTEMS: usize = 100;

#[derive(Debug, Clone)]
pub struct NxSuperblock {
    pub obj: ObjPhys,
    pub block_size: u32,
    pub block_count: u64,
    pub next_xid: u64,
    pub spaceman_oid: u64,
    pub omap_oid: u64,
    pub reaper_oid: u64,
    pub max_file_systems: u32,
    pub fs_oids: Vec<u64>,
}

impl NxSuperblock {
    /// Parse + validate the container superblock from a full block-0 buffer.
    /// Validates: Fletcher-64, NX_MAGIC, object type == NX_SUPERBLOCK.
    pub fn parse(block: &[u8]) -> Result<Self, ParseError> {
        verify_block(block)?;
        let obj = ObjPhys::parse(block)?;
        let magic = u32_le(block, 32)?;
        if magic != NX_MAGIC {
            return Err(ParseError::BadMagic { expected: NX_MAGIC, found: magic });
        }
        if obj.object_type() != OBJECT_TYPE_NX_SUPERBLOCK {
            return Err(ParseError::BadMagic {
                expected: OBJECT_TYPE_NX_SUPERBLOCK,
                found: obj.object_type(),
            });
        }
        let block_size = u32_le(block, 36)?;
        let block_count = u64_le(block, 40)?;
        let next_xid = u64_le(block, 96)?;
        let spaceman_oid = u64_le(block, 152)?;
        let omap_oid = u64_le(block, 160)?;
        let reaper_oid = u64_le(block, 168)?;
        let max_file_systems = u32_le(block, 180)?;
        let n = (max_file_systems as usize).min(NX_MAX_FILE_SYSTEMS);
        let mut fs_oids = Vec::with_capacity(n);
        for i in 0..n {
            let oid = u64_le(block, 184 + i * 8)?;
            if oid != 0 {
                fs_oids.push(oid);
            }
        }
        Ok(Self {
            obj,
            block_size,
            block_count,
            next_xid,
            spaceman_oid,
            omap_oid,
            reaper_oid,
            max_file_systems,
            fs_oids,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn parses_real_container_superblock() {
        let img = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/apfs-tiny.img");
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing — run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let data = std::fs::read(img).unwrap();
        let sb = NxSuperblock::parse(&data[0..4096]).expect("real nx_superblock");
        assert_eq!(sb.obj.object_type(), OBJECT_TYPE_NX_SUPERBLOCK);
        assert!(sb.block_size == 4096, "block_size {}", sb.block_size);
        assert!(sb.block_count > 0);
        assert!(sb.omap_oid != 0, "container omap oid must be set");
        assert!(!sb.fs_oids.is_empty(), "at least one volume fs_oid");
    }

    #[test]
    fn rejects_bad_magic() {
        let mut b = vec![0u8; 4096];
        // valid checksum over zeroed body but wrong magic -> BadMagic
        let c = crate::checksum::fletcher64(&b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
        assert!(matches!(
            NxSuperblock::parse(&b),
            Err(ParseError::BadMagic { .. })
        ));
    }
}
```

- [ ] **Step 2: Run, expect FAIL** (stub `parse` returns Err): `cargo test -p apfs-core ... nx`. Confirm red.
- [ ] **Step 3: Implement** = replace the stub with the full `parse` shown above (it is the real impl; remove the stub).
- [ ] **Step 4: Add to `lib.rs`:** `pub mod nx; pub use nx::NxSuperblock;`
- [ ] **Step 5: gen-fixture + test → PASS.** Run gen-fixture then `cargo test -p apfs-core ...`. Expected both nx tests pass (real superblock parsed: block_size 4096, omap_oid set, ≥1 fs_oid).
- [ ] **Step 6: Commit.**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/nx.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): nx_superblock parse + validate (real image TDD)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: `container.rs` — Container::open(BlockDevice) (TDD vs real image)

**Files:** Create `crates/apfs-core/src/container.rs`; modify `lib.rs`.

- [ ] **Step 1: Failing test first.** Create `crates/apfs-core/src/container.rs` with the test + a stub `Container::open` returning `Err`:
```rust
//! Container — opens an APFS container via a BlockDevice and validates block 0.
use crate::block_device::{BlockDevice, BlockError};
use crate::endian::ParseError;
use crate::nx::NxSuperblock;

#[derive(Debug)]
pub enum ContainerError {
    Io(BlockError),
    Parse(ParseError),
}
impl From<BlockError> for ContainerError {
    fn from(e: BlockError) -> Self {
        ContainerError::Io(e)
    }
}
impl From<ParseError> for ContainerError {
    fn from(e: ParseError) -> Self {
        ContainerError::Parse(e)
    }
}

pub struct Container {
    pub superblock: NxSuperblock,
}

impl Container {
    /// Read physical block 0 (bootstrap copy) and validate the container
    /// superblock. NOTE: block 0 is the bootstrap copy; the latest valid
    /// superblock via the checkpoint descriptor area is M1b.
    pub fn open<D: BlockDevice>(dev: &mut D) -> Result<Self, ContainerError> {
        // Read block 0 with a provisional 4096 buffer, then re-read with the
        // superblock's declared block_size if it differs.
        let mut buf = vec![0u8; 4096];
        dev.read_at(0, &mut buf)?;
        let sb = NxSuperblock::parse(&buf)?;
        if sb.block_size as usize != 4096 && (sb.block_size as usize) >= 1024 {
            let mut b2 = vec![0u8; sb.block_size as usize];
            dev.read_at(0, &mut b2)?;
            let sb2 = NxSuperblock::parse(&b2)?;
            return Ok(Self { superblock: sb2 });
        }
        Ok(Self { superblock: sb })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_device::FileBlockDevice;
    use std::path::Path;

    #[test]
    fn opens_real_container_via_blockdevice() {
        let img = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/apfs-tiny.img");
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing — run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).unwrap();
        let c = Container::open(&mut dev).expect("open real APFS container");
        assert_eq!(c.superblock.block_size, 4096);
        assert!(!c.superblock.fs_oids.is_empty());
    }
}
```

- [ ] **Step 2: Run, expect FAIL** (stub). Confirm red.
- [ ] **Step 3: Implement** = the full `open` above (replace stub).
- [ ] **Step 4: `lib.rs`:** `pub mod container; pub use container::{Container, ContainerError};`
- [ ] **Step 5: gen-fixture + test → PASS.** `cargo test -p apfs-core ...` — real container opens via the `BlockDevice` abstraction.
- [ ] **Step 6: Commit.**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/container.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): Container::open via BlockDevice (real image TDD)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: Gates green + final verification

**Files:** none (verification + possible fmt).

- [ ] **Step 1:** `cargo fmt --all --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` then `cargo fmt --all --check ...` exit 0.
- [ ] **Step 2:** `cargo clippy --workspace --all-targets --all-features --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- -D warnings` → exit 0. If a lib path trips `unwrap_used`/`indexing_slicing`, fix with `Result`/`get(..)` (do NOT add allow to non-test code).
- [ ] **Step 3:** `cargo run -p xtask -- gen-fixture` then `cargo test --workspace --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` → all green; apfs-core now has block_device (2) + endian (2) + obj (2) + checksum (1) + nx (2) + container (1) tests passing (fixture-dependent ones run on macOS; skip-clean elsewhere).
- [ ] **Step 4:** Commit only if fmt changed files:
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src xtask/src
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "style: fmt + clippy gate green for M1a

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```
(Skip if `git status --porcelain` empty.)

- [ ] **Step 5:** Report `git -C /Users/eiaydin/Documents/projects/apfs-explorer log --oneline` (M1a chain). Do NOT push.

---

## Self-Review (plan author)

- **Spec coverage:** M1a covers obj_phys, object-type constants, Fletcher-64, nx_superblock parse/validate, Container::open — the object layer + container entry point per design §3 / roadmap M1. Checkpoint-descriptor walk (latest-xid superblock) is explicitly deferred to **M1b**; object map (omap) B-tree to **M1c** — bounded scope, noted.
- **Placeholders:** none — every file has complete code; the only spec-unspecified item (Fletcher-64) uses the community-canonical algorithm and is *proven* against a real Apple-written checksum.
- **Type consistency:** `ParseError` (endian.rs) is the shared error; `ObjPhys`/`NxSuperblock`/`Container` names consistent across tasks; `verify_block`/`fletcher64` signatures stable; `BlockDevice::read_at` matches M0.
- **Test data:** real APFS image generated locally via `hdiutil` (this Mac); fixture-dependent tests skip cleanly where absent so non-macOS `cargo test` stays green; macOS CI job (later) runs them.
- **Risk:** `hdiutil -layout NONE` placing the container at offset 0 is verified in Task 1 Step 3 before any parsing depends on it (fail-fast with diagnostic).
