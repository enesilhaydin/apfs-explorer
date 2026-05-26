# M2 — omap resolve + open volumes (birleşik) Uygulama Planı

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development, **yağsız mod** (CLAUDE.md "Süreç (konsolide)"): görev başına implementer + TEK konsolide bağımsız review (spec+kalite+gate 1 kez); milestone sonu 1 final review + 1 Python çapraz-kontrol. Adımlar `- [ ]`.

**Amaç:** `Omap::resolve(oid,xid)→paddr` (çok-seviyeli B-tree iniş) + volume superblock parse + container'ın **kullanıcı volume'larını listele** (sistem rolleri filtreli). Gerçek imajda `nx_fs_oid` → APSB volume superblock kanıtlanır.

**Mimari:** Saf Rust, unsafe yok. `omap.rs`'e `resolve`; yeni `volume.rs`; `container.rs`'e `list_user_volumes`. Tüm offsetler üç-kaynak+ampirik CONFIRMED: `ctx_search(source:"apfs-spec:m2-confirmed")` (RESOLVED apfs_superblock tablosu), `apfs-spec:m1c-confirmed` (btree layout), `apfs-spec:m1b/m1-confirmed`.

**Zorunlu:** `git -C /Users/eiaydin/Documents/projects/apfs-explorer` (`cd` yok); asla `git add -A`/`.`; **kod/yorum/commit İngilizce**; `apfs-core` `#![forbid(unsafe_code)]`; üretimde `unwrap/expect/panic`/ham-index YOK (`#[cfg(test)]` scoped allow ok); clippy `-D warnings --all-features`; trailer `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`. Branch: `feat/m2-resolve-volumes`. Genuine belirsizlikte CLAUDE.md md.6 (sor→NotebookLM bekle).

---

## Dosya Yapısı (M2 sonu)
```
crates/apfs-core/src/
  omap.rs     # +resolve(dev,oid,xid)->Option<u64> (multi-level descent)
  volume.rs   # NEW — VolumeSuperblock parse (APSB)
  container.rs# +list_user_volumes(dev)->Vec<VolumeSuperblock>
  lib.rs      # + pub mod volume; re-exports
```

---

## Task 1: `Omap::resolve` — çok-seviyeli B-tree iniş (TDD)

**Files:** Modify `crates/apfs-core/src/omap.rs`, `crates/apfs-core/src/btree.rs` (helper), `lib.rs` (if needed).

Confirmed (apfs-spec:m1c/m2-confirmed): omap tree FIXED_KV; key = omap_key{u64 ok_oid@0, u64 ok_xid@8} (16B); leaf value = omap_val{u32 ov_flags@0, u32 ov_size@4, i64 ov_paddr@8} (16B); non-leaf (btn_level>0) value = oid_t (8B) child block addr (BTREE_PHYSICAL → physical). Descent: at index node pick the entry with the LARGEST key (ok_oid, ok_xid) that is `<= (oid, xid)`; follow child. At leaf: entry with ok_oid==oid and LARGEST ok_xid<=xid; if found and `ov_flags & OMAP_VAL_DELETED == 0` → Some(ov_paddr) else None. Key compare: by ok_oid then ok_xid (both u64, ascending).

- [ ] **Step 1 (TDD red):** add to `btree.rs` a helper to read the i-th key bytes for a FIXED_KV node generically (already have `fixed_kv(i,ksz,vsz)`), plus `pub fn key_count(&self)->u32 { self.nkeys }` and `pub fn level(&self)->u16 { self.level }` (or reuse fields). In `omap.rs` add:
```rust
pub const OMAP_VAL_DELETED: u32 = 0x0000_0001;

impl Omap {
    /// Resolve a virtual object id at transaction `xid` to its physical block
    /// address by descending the (physical) object-map B-tree. Returns None
    /// if not found or the mapping is deleted.
    pub fn resolve<D: BlockDevice>(
        &self,
        dev: &mut D,
        oid: u64,
        xid: u64,
        block_size: u32,
    ) -> Result<Option<u64>, ContainerError> {
        let bsz = block_size as usize;
        let mut node = self.root.clone();
        // Bounded descent: APFS trees are shallow; cap to avoid loops on
        // corrupt data (no panic, just give up gracefully).
        for _ in 0..32 {
            let is_leaf = node.is_leaf();
            // FIXED_KV key = omap_key(16). Non-leaf val = oid_t(8); leaf val =
            // omap_val(16).
            let vsz = if is_leaf { 16 } else { 8 };
            let mut best: Option<(u64, u64, usize)> = None; // (k_oid,k_xid,idx)
            for i in 0..node.nkeys {
                let (k, _v) = node.fixed_kv(i, 16, vsz)?;
                let k_oid = u64::from_le_bytes(
                    k.get(0..8).ok_or_else(short)?.try_into().map_err(|_| short())?,
                );
                let k_xid = u64::from_le_bytes(
                    k.get(8..16).ok_or_else(short)?.try_into().map_err(|_| short())?,
                );
                if is_leaf {
                    if k_oid == oid && k_xid <= xid {
                        match best {
                            Some((_, bx, _)) if bx >= k_xid => {}
                            _ => best = Some((k_oid, k_xid, i as usize)),
                        }
                    }
                } else {
                    // index: largest key <= (oid,xid)
                    if (k_oid, k_xid) <= (oid, xid) {
                        match best {
                            Some((bo, bx, _)) if (bo, bx) >= (k_oid, k_xid) => {}
                            _ => best = Some((k_oid, k_xid, i as usize)),
                        }
                    }
                }
            }
            let Some((_, _, idx)) = best else { return Ok(None) };
            let (_, v) = node.fixed_kv(idx as u32, 16, vsz)?;
            if is_leaf {
                let flags = u32::from_le_bytes(
                    v.get(0..4).ok_or_else(short)?.try_into().map_err(|_| short())?,
                );
                if flags & OMAP_VAL_DELETED != 0 {
                    return Ok(None);
                }
                let paddr = i64::from_le_bytes(
                    v.get(8..16).ok_or_else(short)?.try_into().map_err(|_| short())?,
                );
                return Ok(Some(paddr as u64));
            }
            // descend: child node oid is physical (BTREE_PHYSICAL)
            let child = u64::from_le_bytes(
                v.get(0..8).ok_or_else(short)?.try_into().map_err(|_| short())?,
            );
            let mut buf = vec![0u8; bsz];
            dev.read_at(child.checked_mul(block_size as u64).ok_or_else(short)?, &mut buf)?;
            node = BtreeNode::parse(&buf)?;
        }
        Ok(None)
    }
}

fn short() -> ContainerError {
    ContainerError::Parse(crate::endian::ParseError::Short { at: 0, need: 0, len: 0 })
}
```
(If `short`/`oor` already exists in omap.rs from M1c-a, reuse it; do not duplicate — rename consistently.)

Add a `#[cfg(test)] #[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)] mod resolve_tests` with TWO tests:
(a) real image: open Container + Omap (M1c-a), `resolve(container.superblock fs_oids[0], container.superblock.next_xid or obj.xid, block_size)` returns `Some(paddr)`; read that block; assert its `ObjPhys::parse(...).object_type() == 0x0d` (OBJECT_TYPE_FS) — proving the virtual fs_oid resolved to a real volume superblock. Use the EXISTING fixture (apfs-tiny.img; its omap root is ROOT|LEAF so single-node — still exercises leaf path). Skip-clean if fixture missing.
(b) synthetic 2-level tree (real Fletcher-64): build an INDEX root node (btn_level=1, flags=ROOT|FIXED_KV, NOT leaf) with 2 entries: key(oid=10,xid=1)->child blockA, key(oid=20,xid=1)->child blockB; and two LEAF nodes (level 0, flags=LEAF|FIXED_KV) blockA has key(oid=10,xid=5) val omap_val{flags0,size4096,paddr=0xAAAA}, blockB has key(oid=20,xid=5) val paddr=0xBBBB. Lay them in a MemDev (reuse the MemDev pattern from checkpoint.rs tests or define one). `Omap{phys: <minimal>, root: <index node>}`; `resolve(oid=20,xid=9)` must descend index→blockB→ return Some(0xBBBB); `resolve(oid=10,xid=9)` → Some(0xAAAA); `resolve(oid=99,xid=9)` → None. This deterministically proves multi-level descent + key`<=` selection with genuine checksummed nodes.

- [ ] **Step 2 (red→green):** stub `resolve` body to `Ok(None)` first → synthetic test (b) FAILS (expects Some(0xBBBB)). Confirm red. Then implement the real body above → all pass.

- [ ] **Step 3:** `cargo run -p xtask --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- gen-fixture` then `cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` → all pass; `cargo clippy --workspace --all-targets --all-features --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- -D warnings` exit 0.

- [ ] **Step 4: Commit**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/omap.rs crates/apfs-core/src/btree.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): Omap::resolve — multi-level B-tree descent

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `volume.rs` — VolumeSuperblock parse (APSB) (TDD)

**Files:** Create `crates/apfs-core/src/volume.rs`; modify `lib.rs`.

Offsets CONFIRMED (apfs-spec:m2-confirmed RESOLVED): apfs_magic u32@32 == `APFS_MAGIC 0x42535041`; OBJECT_TYPE_FS=0x0d; apfs_omap_oid@128; apfs_root_tree_oid@136; apfs_fs_flags u64@264; apfs_volname[256]@704 (NUL-terminated UTF-8); apfs_role u16@964.

- [ ] **Step 1 (TDD red):** create `crates/apfs-core/src/volume.rs`; stub `VolumeSuperblock::parse` → `Err(short())` so the synthetic test fails first.
```rust
//! apfs_superblock_t — APFS volume superblock. Offsets CONFIRMED:
//! ctx_search(source:"apfs-spec:m2-confirmed") (RESOLVED).
use crate::checksum::verify_block;
use crate::container::ContainerError;
use crate::endian::{u16_le, u32_le, u64_le};
use crate::obj::ObjPhys;

pub const APFS_MAGIC: u32 = 0x4253_5041; // 'APSB' little-endian
pub const OBJECT_TYPE_FS: u32 = 0x0000_000d;

#[derive(Debug, Clone)]
pub struct VolumeSuperblock {
    pub obj: ObjPhys,
    pub role: u16,
    pub fs_flags: u64,
    pub omap_oid: u64,
    pub root_tree_oid: u64,
    pub name: String,
}

impl VolumeSuperblock {
    pub fn parse(block: &[u8]) -> Result<Self, ContainerError> {
        verify_block(block)?;
        let obj = ObjPhys::parse(block)?;
        if obj.object_type() != OBJECT_TYPE_FS {
            return Err(ContainerError::Parse(crate::endian::ParseError::BadMagic {
                expected: OBJECT_TYPE_FS,
                found: obj.object_type(),
            }));
        }
        let magic = u32_le(block, 32)?;
        if magic != APFS_MAGIC {
            return Err(ContainerError::Parse(crate::endian::ParseError::BadMagic {
                expected: APFS_MAGIC,
                found: magic,
            }));
        }
        let omap_oid = u64_le(block, 128)?;
        let root_tree_oid = u64_le(block, 136)?;
        let fs_flags = u64_le(block, 264)?;
        let role = u16_le(block, 964)?;
        let raw = block.get(704..704 + 256).ok_or_else(|| {
            ContainerError::Parse(crate::endian::ParseError::Short { at: 704, need: 256, len: block.len() })
        })?;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        let name = String::from_utf8_lossy(raw.get(..end).unwrap_or(&[])).into_owned();
        Ok(Self { obj, role, fs_flags, omap_oid, root_tree_oid, name })
    }

    /// True if this is a system-managed volume (hide from the user view).
    pub fn is_system(&self) -> bool {
        // PREBOOT 0x10 | RECOVERY 0x4 | VM 0x8 | INSTALLER 0x20 | BASEBAND 0x80
        const SYSTEM_MASK: u16 = 0x10 | 0x04 | 0x08 | 0x20 | 0x80;
        self.role & SYSTEM_MASK != 0
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::checksum::fletcher64;

    fn make_apsb(role: u16, name: &str) -> [u8; 4096] {
        let mut b = [0u8; 4096];
        b[24..28].copy_from_slice(&OBJECT_TYPE_FS.to_le_bytes()); // o_type
        b[32..36].copy_from_slice(&APFS_MAGIC.to_le_bytes());
        b[128..136].copy_from_slice(&7u64.to_le_bytes()); // omap_oid
        b[136..144].copy_from_slice(&9u64.to_le_bytes()); // root_tree_oid
        b[264..272].copy_from_slice(&0u64.to_le_bytes()); // fs_flags
        let n = name.as_bytes();
        b[704..704 + n.len()].copy_from_slice(n); // volname (NUL-padded)
        b[964..966].copy_from_slice(&role.to_le_bytes());
        let c = fletcher64(&b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
        b
    }

    #[test]
    fn parses_apsb_fields() {
        let b = make_apsb(0x40, "Macintosh HD - Data"); // DATA role
        let v = VolumeSuperblock::parse(&b).expect("parse APSB");
        assert_eq!(v.obj.object_type(), OBJECT_TYPE_FS);
        assert_eq!(v.role, 0x40);
        assert_eq!(v.omap_oid, 7);
        assert_eq!(v.root_tree_oid, 9);
        assert_eq!(v.name, "Macintosh HD - Data");
        assert!(!v.is_system());
        let p = make_apsb(0x10, "Preboot"); // PREBOOT
        assert!(VolumeSuperblock::parse(&p).unwrap().is_system());
    }

    #[test]
    fn rejects_bad_magic_and_checksum() {
        let mut b = make_apsb(0x2, "User");
        b[32] ^= 0xFF; // break magic (checksum still recomputed? no — stale)
        assert!(VolumeSuperblock::parse(&b).is_err());
        let mut c = make_apsb(0x2, "User");
        c[500] ^= 0xFF; // break checksum
        assert!(VolumeSuperblock::parse(&c).is_err());
    }
}
```
(Note: `make_apsb` writes name then computes Fletcher-64 last so the block is genuinely valid; the bad-magic test breaks a byte AFTER checksum was computed for a different block, so verify_block fails first — that's fine, still Err.)

- [ ] **Step 2 (red→green):** stub `parse`→`Err(short())` (add a local `fn short()` or reuse). Synthetic `parses_apsb_fields` FAILS. Then implement real `parse` → pass. Add `pub mod volume; pub use volume::VolumeSuperblock;` to lib.rs.

- [ ] **Step 3:** clippy `-D --all-features` exit 0; `cargo test -p apfs-core ...` all green.

- [ ] **Step 4: Commit**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/volume.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): VolumeSuperblock (APSB) parse + role filter

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `Container::list_user_volumes` + decisive real-image proof (TDD)

**Files:** Modify `crates/apfs-core/src/container.rs`.

- [ ] **Step 1:** add to `impl Container`:
```rust
    /// Resolve every container fs_oid via the container object map and return
    /// the user-visible volume superblocks (system-role volumes filtered out).
    pub fn list_user_volumes<D: crate::block_device::BlockDevice>(
        &self,
        dev: &mut D,
    ) -> Result<Vec<crate::volume::VolumeSuperblock>, ContainerError> {
        let omap = crate::omap::Omap::open(dev, self.superblock.omap_oid, self.superblock.block_size)?;
        let xid = self.superblock.obj.xid;
        let mut out = Vec::new();
        for &fs_oid in &self.superblock.fs_oids {
            if let Some(paddr) = omap.resolve(dev, fs_oid, xid, self.superblock.block_size)? {
                let mut buf = vec![0u8; self.superblock.block_size as usize];
                dev.read_at(
                    paddr
                        .checked_mul(self.superblock.block_size as u64)
                        .ok_or_else(super_short)?,
                    &mut buf,
                )?;
                if let Ok(v) = crate::volume::VolumeSuperblock::parse(&buf) {
                    if !v.is_system() {
                        out.push(v);
                    }
                }
            }
        }
        Ok(out)
    }
```
(`super_short`: reuse the existing `oor()`/`short()` helper already in container.rs from M1c-a — do not duplicate; name consistently.)

- [ ] **Step 2:** in `container.rs` tests, add real-image decisive test:
```rust
    #[test]
    fn lists_real_user_volumes() {
        let img = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/apfs-tiny.img");
        if !std::path::Path::new(img).exists() {
            eprintln!("skip: fixture missing — run `cargo run -p xtask -- gen-fixture`");
            return;
        }
        let mut dev = crate::block_device::FileBlockDevice::open(img).expect("open");
        let c = Container::open(&mut dev).expect("container");
        let vols = c.list_user_volumes(&mut dev).expect("list volumes");
        assert!(!vols.is_empty(), "expected >=1 user volume");
        let v = &vols[0];
        assert_eq!(v.obj.object_type(), crate::volume::OBJECT_TYPE_FS);
        assert!(!v.name.is_empty(), "volume name should be readable");
        assert!(!v.is_system());
    }
```

- [ ] **Step 3:** `cargo run -p xtask -- gen-fixture` then `cargo test --workspace --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` → all green incl `lists_real_user_volumes` (a genuine macOS APFS volume listed: object_type 0x0d, non-empty name, non-system). clippy `-D --all-features` exit 0.

- [ ] **Step 4: Commit**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/container.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): Container::list_user_volumes (resolve + role filter)

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Gates + fmt

- [ ] `cargo fmt --all --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` then `--check` exit 0. `cargo clippy --workspace --all-targets --all-features --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- -D warnings` exit 0. `cargo run -p xtask -- gen-fixture` then `cargo test --workspace --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` all green. `cargo build --workspace ...` exit 0. fmt değiştirdiyse `style: fmt + clippy gate green for M2` commit (yoksa atla). `git log --oneline -8`. Push YOK.

---

## Konsolide review (yağsız) — yürütme talimatı
- Görev başına: implementer (self-test 1 kez) → **TEK** konsolide bağımsız review subagent (spec-uyum + kalite + gate'leri 1 kez koşar, diff'i okur, implementer raporuna güvenmez). Ayrı spec/quality bölme YOK.
- **Milestone sonu: 1 final review + 1 decisive Python çapraz-kontrol** (gerçek apfs-tiny.img: block0→omap_oid(91)→om_tree_oid(92)→ omap descent ile fs_oid 1026 çöz → o blokta obj_type@24&0xffff==0x0d, magic@32==0x42535041, volname@704 okunur ASCII, role@964; Fletcher-64 geçerli). Python ⇄ Rust tutmalı.
- Bitince: squash-merge `feat/m2-resolve-volumes`→main, branch sil, merge sonrası gate doğrula (M0–M1c-a paterni).

## Self-Review (plan yazarı)
- **Spec coverage:** omap multi-level descent (key`<=` seçim, leaf omap_val/non-leaf oid child, BTREE_PHYSICAL), VolumeSuperblock parse (offsetler CONFIRMED apfs-spec:m2-confirmed RESOLVED), list_user_volumes rol filtresi. Catalog (dizin/dosya) M3'e bırakıldı (kapsam dışı, bilinçli).
- **Placeholder yok:** tüm kod tam; sentetik 2-seviyeli ağaç + APSB blokları gerçek `fletcher64` ile (mock değil); decisive gerçek-imaj + Python çapraz.
- **Tip tutarlılığı:** `Omap`/`BtreeNode`/`ContainerError`/`ObjPhys` M1c-a ile uyumlu; `short()`/`oor()` tekrarı yok (mevcut helper reuse); `u16_le` M1c-a'da eklendi.
- **Risk:** apfs-tiny omap tek-düğüm (ROOT|LEAF) → çok-seviyeli iniş sentetik 2-seviyeli ağaçla kanıtlanır; non-4096 block_size kapsam dışı; iniş 32 ile sınırlı (bozuk veride sonsuz döngü yok, nazik None).
