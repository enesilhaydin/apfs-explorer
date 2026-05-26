# M1c-a — Object Map + B-tree Node (locate & validate) Uygulama Planı

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Adımlar `- [ ]` kutucuklu.

**Amaç:** `apfs-core`, container object map'ini (`nx_omap_oid` → fiziksel blok → `omap_phys`) parse edip doğrular ve onun B-tree kök düğümünü (`om_tree_oid` → fiziksel blok → `btree_node_phys_t`) bulup geçerli bir BTREE düğümü olduğunu kanıtlar. (oid→paddr **çözümü** M1c-b'de.) Ayrıca `xtask gen-fixture`'a büyük/doldurulmuş imaj seçeneği eklenir (kullanıcı isteği).

**Mimari:** Saf Rust, unsafe yok. Yeni modüller: `btree.rs` (btree_node_phys_t header + FIXED_KV TOC erişimi), `omap.rs` (OmapPhys parse + tree-root bul/doğrula). Offset-hassas B-tree düzeni gerçek imaja karşı **ampirik** doğrulanır (Python çapraz-decode). `om_tree_oid @ 48` üç kaynakla KESİNLEŞTİ (`apfs-spec:m1c-confirmed` RESOLVED bölümü).

**Spec (subagent koddan önce sorgular):** `ctx_search(source:"apfs-spec:m1c-confirmed")` (RESOLVED omap_phys + btree layout kuralları), `apfs-spec:pdf`, `apfs-spec:m1-confirmed`.

**Zorunlu kurallar:** `git -C /Users/eiaydin/Documents/projects/apfs-explorer` (`cd` yok); asla `git add -A`/`.`; **kod/yorum/commit İngilizce**; `apfs-core` `#![forbid(unsafe_code)]`; üretimde `unwrap/expect/panic`/ham-index yok (`#[cfg(test)]` scoped allow serbest); clippy `-D warnings --all-features` yeşil; trailer `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`. Branch: `feat/m1c-omap-btree`.

---

## Dosya Yapısı (M1c-a sonu)
```
crates/apfs-core/src/
  btree.rs   # NEW — BtreeNode header parse + FIXED_KV TOC + key/val area
  omap.rs    # NEW — OmapPhys parse + locate/validate tree root
  obj.rs     # +OBJECT_TYPE_OMAP already (0x0b) ; ensure present
  lib.rs     # + pub mod btree; pub mod omap; re-exports
xtask/src/main.rs  # gen-fixture: size arg + --large populated fixture
```

---

## Task 1: `xtask gen-fixture` — büyük/doldurulmuş imaj seçeneği

**Files:** Modify `xtask/src/main.rs`, `.gitignore`.

- [ ] **Step 1:** `.gitignore`'a ekle (mevcut apfs-tiny satırlarının yanına):
```
/tests/fixtures/apfs-large.img
/tests/fixtures/apfs-large.img.manifest.json
```

- [ ] **Step 2:** `xtask/src/main.rs`'i güncelle: `gen-fixture` artık opsiyonel `--large` argümanı alır. Mevcut `gen_fixture()` 16 MB davranışını korur (arg yok). `--large` için ayrı fn:
```rust
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("gen-fixture") => {
            let large = args.iter().any(|a| a == "--large");
            let r = if large { gen_large_fixture() } else { gen_fixture() };
            match r {
                Ok(p) => println!("fixture ready: {p}"),
                Err(e) => {
                    eprintln!("gen-fixture failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("usage: xtask gen-fixture [--large]");
            std::process::exit(2);
        }
    }
}
```
Keep the existing `gen_fixture()` (16 MB, `apfs-tiny.img`) unchanged. Add:
```rust
/// Create a larger (~256 MB) APFS image populated with a nested directory
/// tree so the object map / catalog B-trees gain multi-node depth. macOS only.
fn gen_large_fixture() -> Result<String, String> {
    let root = env!("CARGO_MANIFEST_DIR");
    let proj = std::path::Path::new(root).parent().ok_or("no parent")?;
    let fixtures = proj.join("tests/fixtures");
    std::fs::create_dir_all(&fixtures).map_err(|e| e.to_string())?;
    let img = fixtures.join("apfs-large.img");
    if img.exists() {
        std::fs::remove_file(&img).map_err(|e| e.to_string())?;
    }
    let stem = fixtures.join("apfs-large");
    let out = std::process::Command::new("hdiutil")
        .args([
            "create", "-megabytes", "256", "-layout", "NONE", "-fs", "APFS",
            "-volname", "APFSLARGE", "-ov", stem.to_str().ok_or("bad path")?,
        ])
        .output()
        .map_err(|e| format!("hdiutil create: {e}"))?;
    if !out.status.success() {
        return Err(format!("hdiutil create: {}", String::from_utf8_lossy(&out.stderr)));
    }
    std::fs::rename(fixtures.join("apfs-large.dmg"), &img).map_err(|e| e.to_string())?;
    // Attach the raw container image and populate it with a nested tree.
    let mnt = fixtures.join(".apfs-large-mnt");
    std::fs::create_dir_all(&mnt).map_err(|e| e.to_string())?;
    let att = std::process::Command::new("hdiutil")
        .args([
            "attach", "-nobrowse", "-imagekey", "diskimage-class=CRawDiskImage",
            "-mountpoint", mnt.to_str().ok_or("bad mnt")?,
            img.to_str().ok_or("bad img")?,
        ])
        .output()
        .map_err(|e| format!("hdiutil attach: {e}"))?;
    if !att.status.success() {
        // Populate is best-effort; a size-only large image is still useful.
        let _ = std::fs::remove_dir_all(&mnt);
        return verify_nxsb(&img);
    }
    for d in 0..16 {
        let dir = mnt.join(format!("dir{d:02}"));
        let _ = std::fs::create_dir_all(&dir);
        for f in 0..64 {
            let _ = std::fs::write(dir.join(format!("f{f:03}.bin")), vec![b'A'; 4096]);
        }
    }
    let _ = std::process::Command::new("hdiutil")
        .args(["detach", mnt.to_str().ok_or("bad mnt")?])
        .output();
    let _ = std::fs::remove_dir_all(&mnt);
    verify_nxsb(&img)
}

/// Confirm the APFS container superblock magic 'NXSB' is at file offset 32.
fn verify_nxsb(img: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(img).map_err(|e| e.to_string())?;
    let magic = bytes.get(32..36).ok_or("image too small")?;
    if magic != b"NXSB" {
        return Err(format!("offset 32 not NXSB: {magic:02x?}"));
    }
    Ok(img.display().to_string())
}
```
Refactor the existing `gen_fixture()` to also end with `verify_nxsb(&img)` (DRY) if trivial; otherwise leave it.

- [ ] **Step 3:** `cargo run -p xtask --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- gen-fixture` (16MB, unchanged) then `... -- gen-fixture --large`. Expected: both print `fixture ready: ...`. `xxd -s 32 -l 4` of BOTH images → `NXSB`. `ls -l` apfs-large.img ≈ 256 MB. If `hdiutil attach` of the raw image fails, the code falls back to a size-only large image (still valid NXSB) — report which path was taken. Confirm `git status --porcelain` does NOT list either .img.

- [ ] **Step 4: Commit**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add xtask/src/main.rs .gitignore
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(xtask): gen-fixture --large (populated ~256MB APFS image)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `btree.rs` — B-tree node header + FIXED_KV TOC (TDD)

**Files:** Create `crates/apfs-core/src/btree.rs`; modify `lib.rs`.

- [ ] **Step 1 (TDD red):** create `crates/apfs-core/src/btree.rs` with the struct + helpers + tests, but stub `BtreeNode::parse` to `Err(ParseError::Short{at:0,need:0,len:0})` so the synthetic test FAILS first.
```rust
//! btree_node_phys_t — APFS B-tree node (FIXED_KV path only for M1c).
//! Layout rules + offsets: ctx_search(source:"apfs-spec:m1c-confirmed").
use crate::checksum::verify_block;
use crate::endian::{u16_le, u32_le, ParseError};
use crate::obj::ObjPhys;

pub const BTNODE_ROOT: u16 = 0x0001;
pub const BTNODE_LEAF: u16 = 0x0002;
pub const BTNODE_FIXED_KV_SIZE: u16 = 0x0004;
const DATA_BASE: usize = 56; // start of btn_data (after fixed node header)
const BTREE_INFO_SIZE: usize = 40; // btree_info_t at end of a ROOT node

/// A parsed, checksum-validated B-tree node (FIXED_KV).
#[derive(Debug, Clone)]
pub struct BtreeNode {
    pub obj: ObjPhys,
    pub flags: u16,
    pub level: u16,
    pub nkeys: u32,
    pub toc_off: u16,
    pub toc_len: u16,
    raw: Vec<u8>,
}

impl BtreeNode {
    pub fn is_leaf(&self) -> bool {
        self.flags & BTNODE_LEAF != 0
    }
    pub fn is_root(&self) -> bool {
        self.flags & BTNODE_ROOT != 0
    }
    pub fn is_fixed_kv(&self) -> bool {
        self.flags & BTNODE_FIXED_KV_SIZE != 0
    }

    /// Parse + checksum-validate a B-tree node block.
    pub fn parse(block: &[u8]) -> Result<Self, ParseError> {
        verify_block(block)?;
        let obj = ObjPhys::parse(block)?;
        let flags = u16_le(block, 32)?;
        let level = u16_le(block, 34)?;
        let nkeys = u32_le(block, 36)?;
        let toc_off = u16_le(block, 40)?;
        let toc_len = u16_le(block, 42)?;
        Ok(Self {
            obj,
            flags,
            level,
            nkeys,
            toc_off,
            toc_len,
            raw: block.to_vec(),
        })
    }

    /// For a FIXED_KV node, return (key_bytes, value_bytes) for entry `i`,
    /// using fixed sizes `ksz`/`vsz`. Bounds-checked; no panic.
    pub fn fixed_kv(&self, i: u32, ksz: usize, vsz: usize) -> Result<(&[u8], &[u8]), ParseError> {
        if !self.is_fixed_kv() || i >= self.nkeys {
            return Err(ParseError::Short { at: i as usize, need: 1, len: self.nkeys as usize });
        }
        let toc = DATA_BASE + self.toc_off as usize;
        // kvoff_t { u16 k; u16 v; } per entry.
        let ent = toc + (i as usize) * 4;
        let k_off = u16_le(&self.raw, ent)? as usize;
        let v_off = u16_le(&self.raw, ent + 2)? as usize;
        let key_area = DATA_BASE + self.toc_off as usize + self.toc_len as usize;
        let val_area_end = self
            .raw
            .len()
            .checked_sub(if self.is_root() { BTREE_INFO_SIZE } else { 0 })
            .ok_or(ParseError::Short { at: 0, need: BTREE_INFO_SIZE, len: self.raw.len() })?;
        let kstart = key_area.checked_add(k_off).ok_or(ParseError::Short {
            at: key_area,
            need: k_off,
            len: self.raw.len(),
        })?;
        let key = self
            .raw
            .get(kstart..kstart + ksz)
            .ok_or(ParseError::Short { at: kstart, need: ksz, len: self.raw.len() })?;
        let vstart = val_area_end
            .checked_sub(v_off)
            .ok_or(ParseError::Short { at: val_area_end, need: v_off, len: self.raw.len() })?;
        let value = self
            .raw
            .get(vstart..vstart + vsz)
            .ok_or(ParseError::Short { at: vstart, need: vsz, len: self.raw.len() })?;
        Ok((key, value))
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::checksum::fletcher64;
    use crate::obj::OBJECT_TYPE_BTREE;

    /// Build a minimal FIXED_KV leaf node: 1 entry, key 8 bytes = 0xAA.., value
    /// 8 bytes = 0xBB.. ; toc right at DATA_BASE, key area after a 4-byte toc.
    fn make_leaf() -> [u8; 4096] {
        let mut b = [0u8; 4096];
        b[24..28].copy_from_slice(&OBJECT_TYPE_BTREE.to_le_bytes()); // o_type
        b[32..34].copy_from_slice(&(BTNODE_LEAF | BTNODE_FIXED_KV_SIZE).to_le_bytes());
        b[34..36].copy_from_slice(&0u16.to_le_bytes()); // level 0
        b[36..40].copy_from_slice(&1u32.to_le_bytes()); // nkeys 1
        b[40..42].copy_from_slice(&0u16.to_le_bytes()); // toc_off 0 (=DATA_BASE)
        b[42..44].copy_from_slice(&4u16.to_le_bytes()); // toc_len 4 (1 kvoff_t)
        // kvoff_t at DATA_BASE: k=0, v=8 (value 8 bytes back from val_area_end)
        b[56..58].copy_from_slice(&0u16.to_le_bytes()); // k_off
        b[58..60].copy_from_slice(&8u16.to_le_bytes()); // v_off
        // key area = DATA_BASE + toc_off(0) + toc_len(4) = 60 ; key 8 bytes
        b[60..68].copy_from_slice(&[0xAA; 8]);
        // value: leaf, not root -> val_area_end = 4096 ; value at 4096-8 = 4088
        b[4088..4096].copy_from_slice(&[0xBB; 8]);
        let c = fletcher64(&b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
        b
    }

    #[test]
    fn parses_and_reads_fixed_kv_entry() {
        let blk = make_leaf();
        let n = BtreeNode::parse(&blk).expect("parse btree node");
        assert!(n.is_leaf() && n.is_fixed_kv() && !n.is_root());
        assert_eq!(n.nkeys, 1);
        let (k, v) = n.fixed_kv(0, 8, 8).expect("kv 0");
        assert_eq!(k, &[0xAA; 8]);
        assert_eq!(v, &[0xBB; 8]);
        assert!(n.fixed_kv(1, 8, 8).is_err()); // out of range, no panic
    }

    #[test]
    fn rejects_bad_checksum() {
        let mut blk = make_leaf();
        blk[100] ^= 0xFF;
        assert!(BtreeNode::parse(&blk).is_err());
    }
}
```

- [ ] **Step 2:** add `pub fn u16_le(buf:&[u8],at:usize)->Result<u16,ParseError>` to `crates/apfs-core/src/endian.rs` (mirror `u32_le`, copy 2 bytes via array, no raw indexing). Add `pub mod btree;` to `lib.rs`.

- [ ] **Step 3 (red):** with the stub `parse`, `cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml btree` → `parses_and_reads_fixed_kv_entry` FAILS (stub Err). Confirm red reason.

- [ ] **Step 4 (green):** restore the real `parse` (as written). `cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` → all pass incl the 2 btree tests.

- [ ] **Step 5:** `cargo clippy --workspace --all-targets --all-features --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- -D warnings` exit 0.

- [ ] **Step 6: Commit**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/btree.rs crates/apfs-core/src/endian.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): B-tree node header + FIXED_KV TOC access

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `omap.rs` — OmapPhys parse + locate/validate tree root (TDD, gerçek imaj)

**Files:** Create `crates/apfs-core/src/omap.rs`; modify `lib.rs`; ensure `obj.rs` has `OBJECT_TYPE_OMAP=0x0b` (it does from M1a — verify).

- [ ] **Step 1 (TDD red):** create `crates/apfs-core/src/omap.rs`; stub `Omap::open` to return `Err` so the real-image test fails first.
```rust
//! omap_phys_t (object map) — parse + locate its B-tree root.
//! Field offsets CONFIRMED (3 sources): ctx_search(source:"apfs-spec:m1c-confirmed").
use crate::block_device::BlockDevice;
use crate::btree::BtreeNode;
use crate::checksum::verify_block;
use crate::container::ContainerError;
use crate::endian::{u32_le, u64_le};
use crate::obj::{ObjPhys, OBJECT_TYPE_BTREE};

pub const OBJECT_TYPE_OMAP: u32 = 0x0000_000b;

#[derive(Debug, Clone)]
pub struct OmapPhys {
    pub obj: ObjPhys,
    pub flags: u32,
    pub snap_count: u32,
    pub tree_type: u32,
    pub tree_oid: u64, // offset 48 (CONFIRMED: NotebookLM/Apple p44 + Context7 + impls)
}

impl OmapPhys {
    pub fn parse(block: &[u8]) -> Result<Self, ContainerError> {
        verify_block(block)?;
        let obj = ObjPhys::parse(block)?;
        if obj.object_type() != OBJECT_TYPE_OMAP {
            return Err(ContainerError::Parse(crate::endian::ParseError::BadMagic {
                expected: OBJECT_TYPE_OMAP,
                found: obj.object_type(),
            }));
        }
        Ok(Self {
            obj,
            flags: u32_le(block, 32)?,
            snap_count: u32_le(block, 36)?,
            tree_type: u32_le(block, 40)?,
            tree_oid: u64_le(block, 48)?,
        })
    }
}

/// The container object map plus its (validated) B-tree root node.
pub struct Omap {
    pub phys: OmapPhys,
    pub root: BtreeNode,
}

impl Omap {
    /// `omap_paddr` = the container superblock's omap_oid (a PHYSICAL oid =
    /// block address). Reads the omap, then its tree root (also physical for
    /// the container omap), validating both.
    pub fn open<D: BlockDevice>(
        dev: &mut D,
        omap_paddr: u64,
        block_size: u32,
    ) -> Result<Self, ContainerError> {
        let bsz = block_size as usize;
        let mut buf = vec![0u8; bsz];
        dev.read_at(omap_paddr.checked_mul(block_size as u64).ok_or_else(oor)?, &mut buf)?;
        let phys = OmapPhys::parse(&buf)?;
        let mut rbuf = vec![0u8; bsz];
        dev.read_at(phys.tree_oid.checked_mul(block_size as u64).ok_or_else(oor)?, &mut rbuf)?;
        let root = BtreeNode::parse(&rbuf)?;
        if root.obj.object_type() != OBJECT_TYPE_BTREE {
            return Err(ContainerError::Parse(crate::endian::ParseError::BadMagic {
                expected: OBJECT_TYPE_BTREE,
                found: root.obj.object_type(),
            }));
        }
        Ok(Self { phys, root })
    }
}

fn oor() -> ContainerError {
    ContainerError::Parse(crate::endian::ParseError::Short { at: 0, need: 0, len: 0 })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::block_device::FileBlockDevice;
    use crate::container::Container;
    use std::path::Path;

    #[test]
    fn opens_real_container_omap_and_btree_root() {
        let img = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/apfs-tiny.img");
        if !Path::new(img).exists() {
            eprintln!("skip: fixture missing — run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let mut dev = FileBlockDevice::open(img).expect("open fixture");
        let c = Container::open(&mut dev).expect("container");
        let omap = Omap::open(&mut dev, c.superblock.omap_oid, c.superblock.block_size)
            .expect("open container omap + btree root");
        assert_eq!(omap.phys.obj.object_type(), OBJECT_TYPE_OMAP);
        assert!(omap.phys.tree_oid != 0, "om_tree_oid must be set");
        assert_eq!(omap.root.obj.object_type(), OBJECT_TYPE_BTREE);
        assert!(omap.root.is_root(), "tree root node must have BTNODE_ROOT");
    }
}
```

- [ ] **Step 2:** add `pub mod omap;` + `pub use omap::{Omap, OmapPhys};` to `lib.rs`. Confirm `obj.rs` exports `OBJECT_TYPE_BTREE` (add `pub const OBJECT_TYPE_BTREE: u32 = 0x2;` if missing — M1a added it).

- [ ] **Step 3 (red):** stub `Omap::open` body = `{ let _=(dev,omap_paddr,block_size); Err(oor()) }`. `cargo run -p xtask -- gen-fixture` (manifest-path) then `cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml omap` → `opens_real_container_omap_and_btree_root` FAILS (stub). Confirm.

- [ ] **Step 4 (green):** restore the real `Omap::open`. Re-run tests → passes: real container omap parsed (object_type OMAP), om_tree_oid set, tree root is a valid BTREE root node. INDEPENDENT Python cross-check (scratch, add nothing): read apfs-tiny.img; Container block0→omap_oid(=91); read block 91*4096; assert obj_type@24 & 0xffff == 0x0b; read u64@48 = tree_oid; read block tree_oid*4096; assert obj_type@24 & 0xffff == 0x02 and flags@32 has BTNODE_ROOT. Print omap_oid, tree_oid, both object types — must match Rust. (Decisive offset-48 confirmation.)

- [ ] **Step 5:** `cargo clippy --workspace --all-targets --all-features --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- -D warnings` exit 0. `cargo test --workspace ...` all green.

- [ ] **Step 6: Commit**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/omap.rs crates/apfs-core/src/lib.rs crates/apfs-core/src/obj.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): OmapPhys parse + locate/validate B-tree root (real image)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Gates + final doğrulama

- [ ] **Step 1:** `cargo fmt --all --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` then `--check` exit 0.
- [ ] **Step 2:** `cargo clippy --workspace --all-targets --all-features --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- -D warnings` exit 0.
- [ ] **Step 3:** `cargo run -p xtask -- gen-fixture` then `cargo test --workspace --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` → all green (apfs-core ≈ 17 tests: prior 13 + u16_le + 2 btree + 1 omap). `cargo build --workspace ...` exit 0.
- [ ] **Step 4:** fmt değiştirdiyse commit (yoksa atla):
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src xtask/src
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "style: fmt + clippy gate green for M1c-a

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```
- [ ] **Step 5:** `git -C /Users/eiaydin/Documents/projects/apfs-explorer log --oneline -8`. Push YOK.

---

## Self-Review (plan yazarı)
- **Spec coverage:** OmapPhys parse (om_tree_oid@48 CONFIRMED 3-kaynak), B-tree node header + FIXED_KV TOC (omap leaf/non-leaf çözümü M1c-b), container omap → tree-root bul/doğrula. oid→paddr **descent/resolve M1c-b** (bilinçli ayrıldı). Büyük fixture kullanıcı isteği Task 1'de.
- **Placeholder yok:** tüm kod tam; sentetik B-tree düğümü gerçek `fletcher64` ile (mock değil). Offset-hassas value-area/btree_info kuralı gerçek omap + Python çapraz-decode ile ampirik doğrulanır (en riskli kısım garanti altında).
- **Tip tutarlılığı:** `ParseError`/`ContainerError` mevcut; `BtreeNode`/`OmapPhys`/`Omap` adları tutarlı; `u16_le` endian'a eklenir (u32_le ile aynı desen).
- **Risk:** hdiutil raw-image attach başarısızsa büyük fixture size-only'a düşer (rapor edilir, M1c-b/M2 deep-tree testleri gerekirse ayrı ele alınır); B-tree value-area sondan-geriye + ROOT btree_info(40B) düşümü ampirik testle korunur.
