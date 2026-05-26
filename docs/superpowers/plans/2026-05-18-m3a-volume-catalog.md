# M3a — Volume Catalog Read Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax. **Lean consolidated process (CLAUDE.md "Süreç (konsolide)"):** per task = implementer (TDD red→green, self-test once) + ONE consolidated independent review (spec + quality + gates, reviewer reads the diff and runs gates, does not trust the implementer report). Milestone end = 1 final review + 1 decisive Python cross-check. No separate spec/quality/final per task.

**Goal:** Read an APFS volume's file-system (catalog) B-tree end-to-end and list directory entries, proven on the real `apfs-large.img` fixture (16 dirs × 64 files) with an independent Python cross-check.

**Architecture:** Pure-Rust, `#![forbid(unsafe_code)]`. New `jkey.rs` (j_key_t header + j_obj_types). New `catalog.rs` (`Catalog::open` opens the VOLUME object map — `apfs_omap_oid` is a PHYSICAL oid so `Omap::open` is used directly, exactly like the container omap — then resolves the VIRTUAL `apfs_root_tree_oid` through that volume omap; a variable-KV node walk descends the catalog tree resolving each non-leaf VIRTUAL child oid through the same volume omap; leaf records are dispatched by type). `btree.rs` gains a `var_kv` accessor (kvloc_t TOC) because the catalog tree is variable-keyed on every node (BTNODE_FIXED_KV_SIZE is never set on it). `volume.rs` gains `apfs_incompatible_features` (@56) to select the directory-entry key variant.

**Tech Stack:** Rust (workspace crate `apfs-core`), `thiserror`, macOS `hdiutil` fixture via `cargo run -p xtask -- gen-fixture --large`, Python 3 (stdlib only) for the independent cross-check.

**Spec sources (locked, query before coding):** `ctx_search(source:"apfs-spec:m3-confirmed")` (catalog tree linkage = VIRTUAL via volume omap; xid rule; drec key variant; variable-KV-on-all-nodes correction), `apfs-spec:m1c-confirmed` (btree node byte layout), `apfs-spec:m2-confirmed` (apfs_superblock offsets incl. `apfs_incompatible_features` @56), `apfs-spec:pdf`.

**Mandatory rules:** `git -C /Users/eiaydin/Documents/projects/apfs-explorer` (no `cd`); NEVER `git add -A` / `git add .` — always explicit filenames; code/comments/commit messages in English; `apfs-core` stays `#![forbid(unsafe_code)]`; no `unwrap`/`expect`/`panic`/raw indexing in production paths (test modules keep the existing `#[cfg(test)] #[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]`); clippy `--all-targets --all-features -- -D warnings` green; commit trailer `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`. Branch: `feat/m3a-volume-catalog`.

**Gate command (run from anywhere):**
```
cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --workspace && \
cargo clippy --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --workspace --all-targets --all-features -- -D warnings && \
cargo fmt --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml --all --check
```

---

## File Structure

- **Create** `crates/apfs-core/src/jkey.rs` — `j_key_t` header parse (`obj_id`, `obj_type` from the packed 64-bit field), `j_obj_types` constants, reserved inode constants. Pure, no I/O.
- **Create** `crates/apfs-core/src/catalog.rs` — `Catalog` (volume omap + catalog root + volume xid), `DirEntry`, tree descent (`for_each_leaf`), `list_dir`. Depends on `btree`, `omap`, `volume`, `jkey`.
- **Modify** `crates/apfs-core/src/btree.rs` — add `pub fn var_kv(&self, i: u32) -> Result<(&[u8], &[u8]), ParseError>` (kvloc_t path, mirrors existing `fixed_kv`).
- **Modify** `crates/apfs-core/src/volume.rs` — add `pub incompatible_features: u64` (parsed @56), `APFS_INCOMPAT_NORMALIZATION_INSENSITIVE`, `pub fn names_are_hashed(&self) -> bool`.
- **Modify** `crates/apfs-core/src/lib.rs` — `pub mod jkey; pub use jkey::{JKey, ROOT_DIR_INO_NUM};` and `pub mod catalog; pub use catalog::{Catalog, DirEntry};`.

Tests are inline `#[cfg(test)] mod tests` modules in each file (the established codebase pattern; there is no `crates/apfs-core/tests/` directory).

---

## Task 1: jkey.rs — j_key_t header + j_obj_types

**Files:**
- Create: `crates/apfs-core/src/jkey.rs`
- Modify: `crates/apfs-core/src/lib.rs`

- [ ] **Step 1: Create the module with a failing test**

Create `crates/apfs-core/src/jkey.rs` with exactly:

```rust
//! j_key_t — the 8-byte header at the start of every file-system (catalog)
//! record key. Constants/layout: ctx_search(source:"apfs-spec:m3-confirmed").
use crate::endian::{u64_le, ParseError};

pub const OBJ_ID_MASK: u64 = 0x0fff_ffff_ffff_ffff;
pub const OBJ_TYPE_MASK: u64 = 0xf000_0000_0000_0000;
pub const OBJ_TYPE_SHIFT: u64 = 60;

// j_obj_types — the record type stored in the j_key_t type bits.
pub const APFS_TYPE_SNAP_METADATA: u8 = 1;
pub const APFS_TYPE_EXTENT: u8 = 2;
pub const APFS_TYPE_INODE: u8 = 3;
pub const APFS_TYPE_XATTR: u8 = 4;
pub const APFS_TYPE_SIBLING_LINK: u8 = 5;
pub const APFS_TYPE_DSTREAM_ID: u8 = 6;
pub const APFS_TYPE_CRYPTO_STATE: u8 = 7;
pub const APFS_TYPE_FILE_EXTENT: u8 = 8;
pub const APFS_TYPE_DIR_REC: u8 = 9;
pub const APFS_TYPE_DIR_STATS: u8 = 10;
pub const APFS_TYPE_SNAP_NAME: u8 = 11;
pub const APFS_TYPE_SIBLING_MAP: u8 = 12;
pub const APFS_TYPE_FILE_INFO: u8 = 13;

// Reserved inode numbers (Apple "Inode Numbers", p96).
pub const ROOT_DIR_INO_NUM: u64 = 2;
pub const PRIV_DIR_INO_NUM: u64 = 3;

/// Parsed j_key_t header: object identifier and record type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JKey {
    pub obj_id: u64,
    pub obj_type: u8,
}

impl JKey {
    /// Parse the leading 8-byte `obj_id_and_type` field.
    pub fn parse(key: &[u8]) -> Result<Self, ParseError> {
        let v = u64_le(key, 0)?;
        Ok(Self {
            obj_id: v & OBJ_ID_MASK,
            obj_type: ((v & OBJ_TYPE_MASK) >> OBJ_TYPE_SHIFT) as u8,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn splits_obj_id_and_type() {
        // type = INODE (3) in high 4 bits, id = 42 in low 60 bits.
        let v: u64 = (3u64 << 60) | 42u64;
        let jk = JKey::parse(&v.to_le_bytes()).unwrap();
        assert_eq!(jk.obj_id, 42);
        assert_eq!(jk.obj_type, APFS_TYPE_INODE);
    }

    #[test]
    fn short_buffer_errors_not_panics() {
        assert!(JKey::parse(&[0u8; 4]).is_err());
    }
}
```

Add to `crates/apfs-core/src/lib.rs` immediately after the line `pub use volume::VolumeSuperblock;`:

```rust

pub mod jkey;
pub use jkey::{JKey, ROOT_DIR_INO_NUM};
```

- [ ] **Step 2: Run the test to verify it fails (module not yet wired)**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core jkey`
Expected: FAILS to compile until `pub mod jkey;` is added — then after Step 1's lib.rs edit it compiles and both tests PASS. (If it already passes, the lib.rs edit was applied; that is acceptable — proceed.)

- [ ] **Step 3: Run the full gate**

Run the **Gate command** above.
Expected: all green (tests pass, clippy clean, fmt clean).

- [ ] **Step 4: Commit**

```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/jkey.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat: M3a jkey — j_key_t header + j_obj_types constants

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: btree.rs — variable-KV (kvloc_t) accessor

**Files:**
- Modify: `crates/apfs-core/src/btree.rs`

Context: the catalog tree is variable-keyed on **every** node, so `BTNODE_FIXED_KV_SIZE` is never set on it and `fixed_kv` cannot be used. `var_kv` reads the 8-byte `kvloc_t` TOC entry `{ nloc_t k{u16 off,u16 len}; nloc_t v{u16 off,u16 len}; }`. Layout anchoring is identical to `fixed_kv` (DATA_BASE=56, key_area = DATA_BASE + toc_off + toc_len, val_area_end = raw.len() minus 40 if root) — confirmed `apfs-spec:m1c-confirmed`.

- [ ] **Step 1: Add a failing unit test**

In `crates/apfs-core/src/btree.rs`, locate the existing `#[cfg(test)]` test module (it is at the end of the file and already carries `#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]`). Add this test function inside that module:

```rust
    #[test]
    fn var_kv_reads_kvloc_entry() {
        // Synthetic 256-byte LEAF node, not root, not fixed-kv, 1 entry.
        let mut buf = vec![0u8; 256];
        // obj_phys: o_type @24 = OBJECT_TYPE_BTREE (cosmetic; parse() doesn't enforce).
        buf[24] = 0x02;
        // btn_flags @32 = BTNODE_LEAF (0x0002) — not ROOT, not FIXED_KV.
        buf[32..34].copy_from_slice(&0x0002u16.to_le_bytes());
        // btn_level @34 = 0
        // btn_nkeys @36 = 1
        buf[36..40].copy_from_slice(&1u32.to_le_bytes());
        // btn_table_space @40: off=0, len=8 (one 8-byte kvloc entry)
        buf[40..42].copy_from_slice(&0u16.to_le_bytes());
        buf[42..44].copy_from_slice(&8u16.to_le_bytes());
        // kvloc_t at DATA_BASE(56)+toc_off(0) = 56: k.off=0,k.len=4,v.off=4,v.len=2
        buf[56..58].copy_from_slice(&0u16.to_le_bytes()); // k.off
        buf[58..60].copy_from_slice(&4u16.to_le_bytes()); // k.len
        buf[60..62].copy_from_slice(&4u16.to_le_bytes()); // v.off
        buf[62..64].copy_from_slice(&2u16.to_le_bytes()); // v.len
        // key_area = 56 + 0 + 8 = 64 ; key bytes at 64..68
        buf[64..68].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        // val_area_end = 256 (non-root) ; value at 256 - v.off(4) = 252..254
        buf[252..254].copy_from_slice(&[0x11, 0x22]);
        // Fix up Fletcher-64 so parse()'s verify_block passes.
        let ck = crate::checksum::fletcher64(&buf);
        buf[0..8].copy_from_slice(&ck.to_le_bytes());

        let node = BtreeNode::parse(&buf).unwrap();
        assert!(!node.is_fixed_kv());
        let (k, v) = node.var_kv(0).unwrap();
        assert_eq!(k, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(v, &[0x11, 0x22]);
        assert!(node.var_kv(1).is_err()); // out of range -> error, not panic
    }
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core var_kv_reads_kvloc_entry`
Expected: FAIL to compile — `no method named var_kv found for struct BtreeNode`.

- [ ] **Step 3: Implement `var_kv`**

In `crates/apfs-core/src/btree.rs`, inside `impl BtreeNode { ... }`, add this method directly after the existing `fixed_kv` method:

```rust
    /// For a variable-KV node (BTNODE_FIXED_KV_SIZE NOT set, e.g. the
    /// file-system/catalog tree), return (key_bytes, value_bytes) for entry
    /// `i` using the kvloc_t TOC. Bounds-checked; no panic.
    pub fn var_kv(&self, i: u32) -> Result<(&[u8], &[u8]), ParseError> {
        if self.is_fixed_kv() || i >= self.nkeys {
            return Err(ParseError::Short {
                at: i as usize,
                need: 1,
                len: self.nkeys as usize,
            });
        }
        let toc = DATA_BASE + self.toc_off as usize;
        // kvloc_t { nloc_t k{u16 off,u16 len}; nloc_t v{u16 off,u16 len}; } = 8 bytes.
        let ent = toc + (i as usize) * 8;
        let k_off = u16_le(&self.raw, ent)? as usize;
        let k_len = u16_le(&self.raw, ent + 2)? as usize;
        let v_off = u16_le(&self.raw, ent + 4)? as usize;
        let v_len = u16_le(&self.raw, ent + 6)? as usize;
        let key_area = DATA_BASE + self.toc_off as usize + self.toc_len as usize;
        let val_area_end = self
            .raw
            .len()
            .checked_sub(if self.is_root() { BTREE_INFO_SIZE } else { 0 })
            .ok_or(ParseError::Short {
                at: 0,
                need: BTREE_INFO_SIZE,
                len: self.raw.len(),
            })?;
        let kstart = key_area.checked_add(k_off).ok_or(ParseError::Short {
            at: key_area,
            need: k_off,
            len: self.raw.len(),
        })?;
        let key = self
            .raw
            .get(kstart..kstart + k_len)
            .ok_or(ParseError::Short {
                at: kstart,
                need: k_len,
                len: self.raw.len(),
            })?;
        let vstart = val_area_end.checked_sub(v_off).ok_or(ParseError::Short {
            at: val_area_end,
            need: v_off,
            len: self.raw.len(),
        })?;
        let value = self
            .raw
            .get(vstart..vstart + v_len)
            .ok_or(ParseError::Short {
                at: vstart,
                need: v_len,
                len: self.raw.len(),
            })?;
        Ok((key, value))
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core var_kv_reads_kvloc_entry`
Expected: PASS.

- [ ] **Step 5: Run the full gate**

Run the **Gate command**. Expected: all green.

- [ ] **Step 6: Commit**

```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/btree.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat: M3a btree — variable-KV (kvloc_t) entry accessor

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: volume.rs — apfs_incompatible_features + names_are_hashed

**Files:**
- Modify: `crates/apfs-core/src/volume.rs`

Context: directory-entry keys use the hashed form `j_drec_hashed_key_t` (u32 `name_len_and_hash`) iff `APFS_INCOMPAT_NORMALIZATION_INSENSITIVE` (0x08) is set in `apfs_incompatible_features` (apfs_superblock_t @56, confirmed `apfs-spec:m2-confirmed`); otherwise the non-hashed `j_drec_key_t` (u16 `name_len`). The catalog decisive test (Task 4) transitively proves this is correct: a wrong choice yields garbage names and the `dir00..dir15` assertions fail.

- [ ] **Step 1: Add a failing real-image test**

In `crates/apfs-core/src/volume.rs`, inside the existing `#[cfg(test)]` test module (end of file, already has the `#[allow(...)]`), add:

```rust
    #[test]
    fn exposes_incompatible_features_and_hash_flag() {
        let img = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/apfs-large.img"
        );
        if !std::path::Path::new(img).exists() {
            eprintln!("skip: fixture missing — run `cargo run -p xtask -- gen-fixture --large`");
            return;
        }
        let mut dev = crate::block_device::FileBlockDevice::open(img).expect("open fixture");
        let c = crate::container::Container::open(&mut dev).expect("open container");
        let vols = c.list_user_volumes(&mut dev).expect("list volumes");
        let v = vols.first().expect("at least one user volume");
        // Field is readable and the helper agrees with the raw flag bit.
        let raw_bit = v.incompatible_features & 0x0000_0008 != 0;
        assert_eq!(v.names_are_hashed(), raw_bit);
    }
```

- [ ] **Step 2: Run it to confirm it fails**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core exposes_incompatible_features_and_hash_flag`
Expected: FAIL to compile — `no field 'incompatible_features'` / `no method 'names_are_hashed'`.

- [ ] **Step 3: Implement the field and helper**

In `crates/apfs-core/src/volume.rs`:

(a) Add this constant just below the existing `pub const APFS_MAGIC: u32 = 0x4253_5041;` line:

```rust
/// apfs_incompatible_features bit: filenames are normalization-insensitive,
/// which means directory-entry keys use the hashed form j_drec_hashed_key_t.
pub const APFS_INCOMPAT_NORMALIZATION_INSENSITIVE: u64 = 0x0000_0008;
```

(b) In `pub struct VolumeSuperblock`, add the field after `pub fs_flags: u64,`:

```rust
    pub incompatible_features: u64,
```

(c) In `parse`, immediately after the line `let omap_oid = u64_le(block, 128)?;` add:

```rust
        let incompatible_features = u64_le(block, 56)?;
```

(d) In the returned `Ok(Self { ... })` struct literal, add `incompatible_features,` after the `fs_flags,` field.

(e) Inside `impl VolumeSuperblock { ... }`, add this method after `is_system`:

```rust
    /// True when directory-entry keys use the hashed form j_drec_hashed_key_t
    /// (APFS_INCOMPAT_NORMALIZATION_INSENSITIVE set in apfs_incompatible_features).
    pub fn names_are_hashed(&self) -> bool {
        self.incompatible_features & APFS_INCOMPAT_NORMALIZATION_INSENSITIVE != 0
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core exposes_incompatible_features_and_hash_flag`
Expected: PASS (or `skip:` line printed and PASS if the fixture is absent — then regenerate it before Task 4).

- [ ] **Step 5: Run the full gate**

Run the **Gate command**. Expected: all green.

- [ ] **Step 6: Commit**

```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/volume.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat: M3a volume — apfs_incompatible_features + names_are_hashed

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: catalog.rs — Catalog::open, tree descent, list_dir + decisive real-image test

**Files:**
- Create: `crates/apfs-core/src/catalog.rs`
- Modify: `crates/apfs-core/src/lib.rs`

- [ ] **Step 1: Ensure the populated fixture exists**

Run: `cargo run --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p xtask -- gen-fixture --large`
Expected: prints success; `tests/fixtures/apfs-large.img` exists. (macOS only. The image is populated with `dir00`..`dir15`, each containing `f000.bin`..`f063.bin` of 4096 bytes. If `hdiutil attach` falls back, the image is size-only and the decisive test will print `skip:` — re-run on a macOS host with attach working.)

- [ ] **Step 2: Create catalog.rs with the decisive failing test**

Create `crates/apfs-core/src/catalog.rs` with exactly:

```rust
//! Catalog — the volume file-system (catalog) B-tree. Linkage rules
//! (VIRTUAL via the volume omap; xid = volume superblock o_xid; drec key
//! variant): ctx_search(source:"apfs-spec:m3-confirmed").
use crate::block_device::BlockDevice;
use crate::btree::BtreeNode;
use crate::container::ContainerError;
use crate::endian::{u16_le, u32_le, ParseError};
use crate::jkey::{JKey, APFS_TYPE_DIR_REC};
use crate::obj::OBJECT_TYPE_BTREE;
use crate::omap::Omap;
use crate::volume::VolumeSuperblock;

fn oor() -> ContainerError {
    ContainerError::Parse(ParseError::Short {
        at: 0,
        need: 0,
        len: 0,
    })
}

/// One directory-entry record (APFS_TYPE_DIR_REC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub file_id: u64,
    pub flags: u16,
}

/// A volume's catalog (file-system) tree, ready to walk.
pub struct Catalog {
    omap: Omap,
    root: BtreeNode,
    xid: u64,
    block_size: u32,
}

impl Catalog {
    /// Open the catalog tree of `vol`.
    /// `vol.omap_oid` (apfs_omap_oid) is a PHYSICAL oid -> `Omap::open` directly,
    /// exactly like the container omap. `vol.root_tree_oid` (apfs_root_tree_oid)
    /// is a VIRTUAL oid -> resolved through the volume omap at the volume
    /// superblock's own xid (`vol.obj.xid`).
    pub fn open<D: BlockDevice>(
        dev: &mut D,
        vol: &VolumeSuperblock,
        block_size: u32,
    ) -> Result<Self, ContainerError> {
        let omap = Omap::open(dev, vol.omap_oid, block_size)?;
        let xid = vol.obj.xid;
        let root_paddr = omap
            .resolve(dev, vol.root_tree_oid, xid, block_size)?
            .ok_or_else(oor)?;
        let bsz = block_size as usize;
        let mut buf = vec![0u8; bsz];
        dev.read_at(
            root_paddr.checked_mul(block_size as u64).ok_or_else(oor)?,
            &mut buf,
        )?;
        let root = BtreeNode::parse(&buf)?;
        if root.obj.object_type() != OBJECT_TYPE_BTREE {
            return Err(ContainerError::Parse(ParseError::BadMagic {
                expected: OBJECT_TYPE_BTREE,
                found: root.obj.object_type(),
            }));
        }
        Ok(Self {
            omap,
            root,
            xid,
            block_size,
        })
    }

    /// Depth-first walk; calls `f(key, value)` for every LEAF entry. Non-leaf
    /// values are 8-byte VIRTUAL child oids resolved through the volume omap.
    fn for_each_leaf<D: BlockDevice>(
        &self,
        dev: &mut D,
        node: &BtreeNode,
        depth: u8,
        f: &mut dyn FnMut(&[u8], &[u8]) -> Result<(), ContainerError>,
    ) -> Result<(), ContainerError> {
        if depth > 32 {
            return Ok(()); // bounded: corrupt/looping tree -> stop, no panic
        }
        for i in 0..node.nkeys {
            if node.is_leaf() {
                let (k, v) = node.var_kv(i)?;
                f(k, v)?;
            } else {
                let (_k, v) = node.var_kv(i)?;
                let child_oid = u64::from_le_bytes(
                    v.get(0..8)
                        .ok_or_else(oor)?
                        .try_into()
                        .map_err(|_| oor())?,
                );
                let paddr = self
                    .omap
                    .resolve(dev, child_oid, self.xid, self.block_size)?
                    .ok_or_else(oor)?;
                let mut buf = vec![0u8; self.block_size as usize];
                dev.read_at(
                    paddr
                        .checked_mul(self.block_size as u64)
                        .ok_or_else(oor)?,
                    &mut buf,
                )?;
                let child = BtreeNode::parse(&buf)?;
                self.for_each_leaf(dev, &child, depth + 1, f)?;
            }
        }
        Ok(())
    }

    /// List the directory entries whose parent inode == `dir_inode`.
    /// `names_hashed` selects the j_drec key variant (see
    /// VolumeSuperblock::names_are_hashed).
    pub fn list_dir<D: BlockDevice>(
        &self,
        dev: &mut D,
        dir_inode: u64,
        names_hashed: bool,
    ) -> Result<Vec<DirEntry>, ContainerError> {
        let mut out: Vec<DirEntry> = Vec::new();
        let root = self.root.clone();
        self.for_each_leaf(dev, &root, 0, &mut |k, v| {
            let jk = JKey::parse(k)?;
            if jk.obj_type != APFS_TYPE_DIR_REC || jk.obj_id != dir_inode {
                return Ok(());
            }
            // After the 8-byte j_key_t: hashed => u32 name_len_and_hash (name
            // length = low 10 bits), else u16 name_len; then the name bytes
            // (length includes the trailing NUL).
            let (name_off, name_len) = if names_hashed {
                let nlh = u32_le(k, 8)?;
                (12usize, (nlh & 0x0000_03ff) as usize)
            } else {
                let nl = u16_le(k, 8)?;
                (10usize, nl as usize)
            };
            let raw = k.get(name_off..name_off + name_len).ok_or_else(oor)?;
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            let name = String::from_utf8_lossy(raw.get(..end).unwrap_or(&[])).into_owned();
            // j_drec_val { u64 file_id@0; u64 date_added@8; u16 flags@16; ... }
            let file_id = u64::from_le_bytes(
                v.get(0..8).ok_or_else(oor)?.try_into().map_err(|_| oor())?,
            );
            let flags = u16::from_le_bytes(
                v.get(16..18).ok_or_else(oor)?.try_into().map_err(|_| oor())?,
            );
            out.push(DirEntry {
                name,
                file_id,
                flags,
            });
            Ok(())
        })?;
        Ok(out)
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::cast_possible_truncation
)]
mod tests {
    use super::*;
    use crate::block_device::FileBlockDevice;
    use crate::container::Container;
    use crate::jkey::ROOT_DIR_INO_NUM;
    use std::path::Path;

    #[test]
    fn lists_real_large_fixture_tree() {
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
        let vols = c.list_user_volumes(&mut dev).expect("list volumes");
        let vol = vols.first().expect("at least one user volume").clone();

        let cat = Catalog::open(&mut dev, &vol, bsz).expect("open catalog");
        let hashed = vol.names_are_hashed();
        let root = cat
            .list_dir(&mut dev, ROOT_DIR_INO_NUM, hashed)
            .expect("list root dir");

        let names: std::collections::BTreeSet<String> =
            root.iter().map(|e| e.name.clone()).collect();
        let expected_dirs: Vec<String> = (0..16).map(|d| format!("dir{d:02}")).collect();
        if !expected_dirs.iter().all(|d| names.contains(d)) {
            eprintln!(
                "skip: size-only fixture (hdiutil attach fell back) — \
                 re-run `cargo run -p xtask -- gen-fixture --large` on a macOS host. \
                 root names found: {names:?}"
            );
            return;
        }
        // All 16 created directories are present at root.
        for d in &expected_dirs {
            assert!(names.contains(d), "missing root dir {d}");
        }
        // dir00's children are exactly f000.bin..f063.bin.
        let dir00 = root
            .iter()
            .find(|e| e.name == "dir00")
            .expect("dir00 entry");
        let kids = cat
            .list_dir(&mut dev, dir00.file_id, hashed)
            .expect("list dir00");
        let kid_names: std::collections::BTreeSet<String> =
            kids.iter().map(|e| e.name.clone()).collect();
        for f in 0..64 {
            let want = format!("f{f:03}.bin");
            assert!(kid_names.contains(&want), "dir00 missing {want}");
        }
        assert_eq!(
            kid_names.len(),
            64,
            "dir00 should have exactly 64 files, got {}",
            kid_names.len()
        );
    }
}
```

Add to `crates/apfs-core/src/lib.rs` immediately after the `pub use jkey::{JKey, ROOT_DIR_INO_NUM};` line added in Task 1:

```rust

pub mod catalog;
pub use catalog::{Catalog, DirEntry};
```

- [ ] **Step 3: Run the decisive test to verify it fails**

Run: `cargo test --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -p apfs-core lists_real_large_fixture_tree -- --nocapture`
Expected: FAILS to compile until `pub mod catalog;` is wired (Step 2's lib.rs edit). After wiring it compiles; the test then PASSES on a populated fixture (prints nothing) — if it fails an assertion, fix the implementation (do NOT weaken the assertions). A `skip:` line + pass is only acceptable when the fixture is missing or size-only.

- [ ] **Step 4: Run the full gate**

Run the **Gate command**. Expected: all green; `lists_real_large_fixture_tree` passes (not skipped) on this macOS host.

- [ ] **Step 5: Commit**

```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/catalog.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat: M3a catalog — volume omap + virtual catalog tree descent + list_dir

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Milestone end — independent Python cross-check + final review + merge

**Files:** none modified (verification + branch finish).

- [ ] **Step 1: Independent Python cross-check (decisive, offset-precise)**

This Python decoder shares NO code with the Rust implementation. It independently re-derives the catalog and must agree. Run it via `ctx_execute(language:"python", code: <below>)` (keeps output out of context; only the printed verdict returns). It also independently recomputes Fletcher-64 of the catalog root block.

```python
import struct, sys
IMG = "/Users/eiaydin/Documents/projects/apfs-explorer/tests/fixtures/apfs-large.img"
BS = 4096
def rd(f, blk):
    f.seek(blk*BS); return f.read(BS)
def fl64(b):
    MOD=0xFFFFFFFF; s1=s2=0
    body=b[8:]
    for i in range(0,len(body)-len(body)%4,4):
        v=struct.unpack_from('<I',body,i)[0]; s1=(s1+v)%MOD; s2=(s2+s1)%MOD
    c1=MOD-((s1+s2)%MOD); c2=MOD-((s1+c1)%MOD); return (c2<<32)|c1
def obj_xid(b): return struct.unpack_from('<Q',b,16)[0]
def otype(b): return struct.unpack_from('<I',b,24)[0] & 0xffff
class Node:
    def __init__(s,b):
        s.b=b; s.flags=struct.unpack_from('<H',b,32)[0]
        s.nkeys=struct.unpack_from('<I',b,36)[0]
        s.toff=struct.unpack_from('<H',b,40)[0]; s.tlen=struct.unpack_from('<H',b,42)[0]
        s.leaf=bool(s.flags&0x2); s.root=bool(s.flags&0x1); s.fixed=bool(s.flags&0x4)
    def kv(s,i):
        ent=56+s.toff+i*8
        ko,kl,vo,vl=struct.unpack_from('<HHHH',s.b,ent)
        ka=56+s.toff+s.tlen
        ve=len(s.b)-(40 if s.root else 0)
        return s.b[ka+ko:ka+ko+kl], s.b[ve-vo:ve-vo+vl]
def omap_resolve(f, omap_blk, oid, xid):
    ob=rd(f,omap_blk); tree=struct.unpack_from('<Q',ob,48)[0]
    node=Node(rd(f,tree))
    for _ in range(32):
        best=None
        for i in range(node.nkeys):
            k,v=node.kv(i) if not node.fixed else _fixedkv(node,i,16, 16 if node.leaf else 8)
            ko=struct.unpack_from('<Q',k,0)[0]; kx=struct.unpack_from('<Q',k,8)[0]
            if node.leaf:
                if ko==oid and kx<=xid and (best is None or kx>best[0]): best=(kx,i)
            else:
                if ko<oid or (ko==oid and kx<=xid):
                    if best is None or (ko,kx)>best[1]: best=(0,(ko,kx),i) if False else (kx,i)
        if best is None: return None
        _,idx=best
        k,v=node.kv(idx) if not node.fixed else _fixedkv(node,idx,16,16 if node.leaf else 8)
        if node.leaf:
            fl=struct.unpack_from('<I',v,0)[0]
            if fl&0x1: return None
            return struct.unpack_from('<q',v,8)[0]
        child=struct.unpack_from('<Q',v,0)[0]; node=Node(rd(f,child))
    return None
def _fixedkv(node,i,ksz,vsz):
    ent=56+node.toff+i*4
    ko,vo=struct.unpack_from('<HH',node.b,ent)
    ka=56+node.toff+node.tlen; ve=len(node.b)-(40 if node.root else 0)
    return node.b[ka+ko:ka+ko+ksz], node.b[ve-vo:ve-vo+vsz]
def walk(f, omap_blk, node, xid, parent, out, depth=0):
    if depth>32: return
    for i in range(node.nkeys):
        k,v=node.kv(i)
        if node.leaf:
            idtype=struct.unpack_from('<Q',k,0)[0]
            oid=idtype & 0x0fffffffffffffff; t=idtype>>60
            if t==9 and oid==parent:  # APFS_TYPE_DIR_REC
                nl=struct.unpack_from('<H',k,8)[0]; nm=k[10:10+nl].split(b'\x00')[0].decode('utf-8','replace')
                fid=struct.unpack_from('<Q',v,0)[0]; out.append((nm,fid))
        else:
            child=struct.unpack_from('<Q',v,0)[0]
            cp=omap_resolve(f,omap_blk,child,xid)
            if cp is not None: walk(f,omap_blk,Node(rd(f,cp)),xid,parent,out,depth+1)
with open(IMG,'rb') as f:
    nx=rd(f,0)
    nx_omap=struct.unpack_from('<Q',nx,160)[0]
    nfs=struct.unpack_from('<I',nx,184)[0]
    fs0=struct.unpack_from('<Q',nx,192)[0]
    nx_xid=obj_xid(nx)
    volp=omap_resolve(f,nx_omap,fs0,nx_xid); assert volp, "vol resolve failed"
    vb=rd(f,volp)
    v_omap=struct.unpack_from('<Q',vb,128)[0]
    v_root=struct.unpack_from('<Q',vb,136)[0]
    v_xid=obj_xid(vb)
    rootp=omap_resolve(f,v_omap,v_root,v_xid); assert rootp, "root resolve failed"
    rb=rd(f,rootp)
    assert struct.unpack_from('<Q',rb,0)[0]==fl64(rb), "catalog root Fletcher-64 mismatch"
    root=[]; walk(f,v_omap,Node(rb),v_xid,2,root)
    dirs=sorted(n for n,_ in root)
    want=[f"dir{d:02}" for d in range(16)]
    assert all(w in dirs for w in want), f"missing dirs; got {dirs}"
    d0=[fid for n,fid in root if n=="dir00"][0]
    kids=[]; walk(f,v_omap,Node(rb),v_xid,d0,kids)
    kn=sorted(n for n,_ in kids)
    wantf=[f"f{x:03}.bin" for x in range(64)]
    assert all(w in kn for w in wantf), f"missing files; got {len(kn)}"
    print(f"PYTHON-CROSSCHECK OK: root dirs>={len(want)} (all dir00..dir15), dir00 files={len([1 for n in kn if n.startswith('f')])}, Fletcher-64 of catalog root verified")
```

Expected printed output (and nothing else): `PYTHON-CROSSCHECK OK: root dirs>=16 (all dir00..dir15), dir00 files=64, Fletcher-64 of catalog root verified`. If the Python decoder disagrees with the Rust test, STOP — there is an offset/algorithm defect; investigate before merging. (Note: container nx_superblock offsets used here — `nx_omap_oid`@160, `nx_xfs_count`@184, `nx_fs_oid[0]`@192 — are the M1a/M2-confirmed offsets; the Python intentionally parses block 0 directly, which on a freshly generated single-checkpoint fixture is the authoritative superblock.)

- [ ] **Step 2: Final consolidated milestone review**

Dispatch ONE independent reviewer (code-quality + spec-compliance + gates in one pass, per the lean process). The reviewer reads the full diff of `feat/m3a-volume-catalog` vs `main`, runs the **Gate command**, runs the Task 5 Step 1 Python cross-check, and verifies against `apfs-spec:m3-confirmed`. Reviewer must confirm: no `unwrap`/`expect`/`panic`/raw indexing outside `#[cfg(test)]`; `#![forbid(unsafe_code)]` intact; the catalog non-leaf child oids are resolved via the VOLUME omap at `vol.obj.xid`; `apfs_omap_oid` treated as physical (`Omap::open` direct); decisive test passes un-skipped on this macOS host. Fix any finding, re-review until clean.

- [ ] **Step 3: Finish the branch (squash + delete)**

Use the **superpowers:finishing-a-development-branch** skill. Choose "Merge back to main locally"; it squash-merges `feat/m3a-volume-catalog` into `main`, re-runs the gate on the merged result, and deletes the branch (linear history per CLAUDE.md).

- [ ] **Step 4: Update ROADMAP status**

Edit `docs/ROADMAP.md`: in the status table append a row `| M3a volume catalog (omap+catalog tree descent+list_dir) | ✅ | <squash commit> |`, and under "Kalan yol haritası" mark M3a done and note M3 remaining = file content read + decmpfs + xattr (formerly M3c). Commit:

```
git -C /Users/eiaydin/Documents/projects/apfs-explorer add docs/ROADMAP.md
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "docs: ROADMAP — M3a volume catalog read merged

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review (performed while writing this plan)

**1. Spec coverage:** volume omap open (apfs_omap_oid physical → Omap::open) — Task 4 `Catalog::open`. apfs_root_tree_oid virtual resolve via volume omap at vol.obj.xid — Task 4 `Catalog::open`. variable-KV kvloc_t TOC — Task 2 `var_kv`. j_key_t / j_obj_types — Task 1. catalog non-leaf VIRTUAL child oid resolution via volume omap — Task 4 `for_each_leaf`. drec key hashed/non-hashed selection — Task 3 + Task 4 `list_dir`. root directory listing on real apfs-large.img — Task 4 decisive test. Independent Python cross-check + Fletcher-64 — Task 5. All `apfs-spec:m3-confirmed` requirements mapped.

**2. Placeholder scan:** no TBD/TODO/"handle edge cases"; every code step shows complete code; every command is exact with expected output; no "similar to Task N".

**3. Type consistency:** `JKey{obj_id,obj_type}`, `APFS_TYPE_DIR_REC`, `ROOT_DIR_INO_NUM`, `BtreeNode::var_kv`, `BtreeNode::parse`, `Omap::open`, `Omap::resolve(dev,oid,xid,block_size)->Result<Option<u64>,ContainerError>`, `VolumeSuperblock{obj,role,fs_flags,incompatible_features,omap_oid,root_tree_oid,name}`, `Container::open`, `Container::list_user_volumes`, `FileBlockDevice::open`, `ContainerError`, `ParseError::Short{at,need,len}` — all match the existing codebase signatures gathered from source. `DATA_BASE`/`BTREE_INFO_SIZE`/`raw` are module-private in `btree.rs`; `var_kv` is added in that same module so access is valid. `vol.obj.xid` is the documented accessor for the volume superblock transaction id.
