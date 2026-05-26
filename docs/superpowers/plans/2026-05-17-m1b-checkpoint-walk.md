# M1b — Checkpoint Descriptor Walk Uygulama Planı

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development. Adımlar `- [ ]` kutucuklu.

**Amaç:** `Container::open`, blok 0'daki bootstrap kopyaya güvenmek yerine, checkpoint descriptor halka tamponunu gezip **en yüksek geçerli `xid`**'li `nx_superblock`'u seçer (temiz kapatılmamış disklerde doğru superblock). Gerçek APFS imajına karşı TDD + sentetik halka testleri.

**Mimari:** Saf Rust, unsafe yok. `NxSuperblock`'a `xp_desc_base`(i64@112) + `xp_desc_blocks`(u32@104) eklenir. Yeni `apfs-core::checkpoint` modülü halkayı gezer; her bloğu mevcut `NxSuperblock::parse` (zaten Fletcher-64 + NX_MAGIC + tip doğruluyor) ile dener, en yüksek `obj.xid`'liyi seçer, hiç geçerli yoksa bootstrap (blok 0) kopyaya düşer. `Container::open` bunu kullanır.

**Spec kaynağı (subagent'lar koddan önce sorgular):** `ctx_search(source:"apfs-spec:m1b-confirmed")`, `ctx_search(source:"apfs-spec:m1-confirmed")`, tam PDF `apfs-spec:pdf`, Context7 `apfs-spec:m1-structs`.

**Zorunlu kurallar:** `git -C /Users/eiaydin/Documents/projects/apfs-explorer` (`cd` yok); asla `git add -A`/`.`; **kod/yorum/commit İngilizce**; `apfs-core` `#![forbid(unsafe_code)]`; üretim yolunda `unwrap/expect/panic`/ham slice indexleme yok (`#[cfg(test)]` içinde scoped allow serbest); clippy `-D warnings --all-features` yeşil; commit trailer `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`. Branch: `feat/m1-checkpoint-walk`. Gerçek fixture: `cargo run -p xtask -- gen-fixture` (macOS, mevcut).

---

## Dosya Yapısı (M1b sonu)

```
crates/apfs-core/src/
  nx.rs          # +xp_desc_base, +xp_desc_blocks alanları + parse + test
  checkpoint.rs  # NEW — latest_superblock(dev, bootstrap)
  container.rs   # open() artık checkpoint::latest_superblock kullanır
  lib.rs         # + pub mod checkpoint; re-export
```

---

## Task 1: `nx.rs` — checkpoint-ring alanlarını ekle (TDD genişletme)

**Files:** Modify `crates/apfs-core/src/nx.rs`.

- [ ] **Step 1:** `NxSuperblock` struct'ına şu iki alanı ekle (mevcut alanların yanına, `block_count`'tan sonra mantıklı bir yere):
```rust
    pub xp_desc_blocks: u32,
    pub xp_desc_base: i64,
```
`parse()` içinde, mevcut okumaların arasına ekle (offsetler `apfs-spec:m1b-confirmed`):
```rust
        let xp_desc_blocks = u32_le(block, 104)?;
        let xp_desc_base = crate::endian::i64_le(block, 112)?;
```
ve `Ok(Self { ... })` literaline `xp_desc_blocks,` ve `xp_desc_base,` alanlarını ekle.

- [ ] **Step 2:** `nx::tests::parses_real_container_superblock` testinin sonuna şu assert'leri ekle (gerçek imajda halka bilgisi anlamlı olmalı):
```rust
        assert!(sb.xp_desc_blocks > 0, "xp_desc_blocks {}", sb.xp_desc_blocks);
        assert!(sb.xp_desc_base > 0, "xp_desc_base {}", sb.xp_desc_base);
```

- [ ] **Step 3:** Doğrula:
`cargo run -p xtask --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- gen-fixture`
`cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` → tüm testler geçer (`parses_real_container_superblock` yeni assert'lerle de geçer; gerçek imajda xp_desc_blocks>0 ve xp_desc_base>0). `cargo clippy --workspace --all-targets --all-features --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- -D warnings` → exit 0.

- [ ] **Step 4: Commit**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/nx.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): expose nx_xp_desc_base/blocks on NxSuperblock

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `checkpoint.rs` — halkayı gez, en yüksek geçerli xid'li superblock'u seç (TDD)

**Files:** Create `crates/apfs-core/src/checkpoint.rs`; modify `lib.rs`.

- [ ] **Step 1 (TDD red):** `crates/apfs-core/src/checkpoint.rs` dosyasını şu içerikle oluştur; `latest_superblock` gövdesini ÖNCE stub yap (`{ let _=(dev,bootstrap); Ok(bootstrap.clone()) }`) ki sentetik test KIRMIZI olsun:
```rust
//! Checkpoint descriptor area walk: select the newest valid container
//! superblock (highest xid) instead of trusting the block-0 bootstrap copy.
//! Spec: ctx_search(source:"apfs-spec:m1b-confirmed").
use crate::block_device::BlockDevice;
use crate::container::ContainerError;
use crate::nx::NxSuperblock;

/// Walk the checkpoint descriptor ring described by `bootstrap`
/// (`xp_desc_base` .. `+ xp_desc_blocks`) and return the `nx_superblock`
/// with the largest `xid` that is well-formed (valid Fletcher-64, NX_MAGIC
/// and object type — all enforced by `NxSuperblock::parse`). If no ring
/// entry is a valid superblock, fall back to `bootstrap` (the block-0 copy).
pub fn latest_superblock<D: BlockDevice>(
    dev: &mut D,
    bootstrap: &NxSuperblock,
) -> Result<NxSuperblock, ContainerError> {
    // The high bit of nx_xp_desc_blocks is a flag: when set, the checkpoint
    // descriptor area is non-contiguous and nx_xp_desc_base is the oid of a
    // B-tree, NOT a paddr (Apple APFS Reference). Resolving that tree is out of
    // M1b scope, so fall back to the block-0 bootstrap copy in that case.
    const XP_DESC_TREE_FLAG: u32 = 0x8000_0000;
    let bsize = bootstrap.block_size as usize;
    if bsize < 1024
        || bootstrap.xp_desc_base <= 0
        || bootstrap.xp_desc_blocks & XP_DESC_TREE_FLAG != 0
    {
        return Ok(bootstrap.clone());
    }
    let count = bootstrap.xp_desc_blocks & 0x7FFF_FFFF;
    if count == 0 {
        return Ok(bootstrap.clone());
    }
    let base = bootstrap.xp_desc_base as u64;
    let mut best = bootstrap.clone();
    let mut buf = vec![0u8; bsize];
    for i in 0..u64::from(count) {
        let off = base
            .checked_add(i)
            .and_then(|b| b.checked_mul(bsize as u64));
        let Some(off) = off else { continue };
        if dev.read_at(off, &mut buf).is_err() {
            continue;
        }
        // Non-superblock ring blocks (checkpoint maps) fail this parse and
        // are skipped; only valid superblocks with a larger xid win.
        if let Ok(sb) = NxSuperblock::parse(&buf) {
            if sb.obj.xid > best.obj.xid {
                best = sb;
            }
        }
    }
    Ok(best)
}

#[cfg(test)]
#[allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::block_device::{BlockDevice, BlockError};
    use crate::checksum::fletcher64;
    use crate::nx::NX_MAGIC;
    use crate::obj::OBJECT_TYPE_NX_SUPERBLOCK;

    /// In-memory block device over a flat byte vector.
    struct MemDev(Vec<u8>);
    impl BlockDevice for MemDev {
        fn size(&self) -> u64 {
            self.0.len() as u64
        }
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BlockError> {
            let o = offset as usize;
            let end = o + buf.len();
            if end > self.0.len() {
                return Err(BlockError::OutOfRange {
                    offset,
                    len: buf.len() as u64,
                    size: self.0.len() as u64,
                });
            }
            buf.copy_from_slice(&self.0[o..end]);
            Ok(())
        }
    }

    /// Build a minimal valid 4096-byte nx_superblock block with the given
    /// xid and ring descriptor (so parse() accepts it), with a correct
    /// Fletcher-64 written into o_cksum.
    fn make_sb(xid: u64, xp_base: i64, xp_blocks: u32) -> [u8; 4096] {
        let mut b = [0u8; 4096];
        b[8..16].copy_from_slice(&1u64.to_le_bytes()); // o_oid
        b[16..24].copy_from_slice(&xid.to_le_bytes()); // o_xid
        b[24..28].copy_from_slice(&OBJECT_TYPE_NX_SUPERBLOCK.to_le_bytes()); // o_type
        b[32..36].copy_from_slice(&NX_MAGIC.to_le_bytes()); // nx_magic
        b[36..40].copy_from_slice(&4096u32.to_le_bytes()); // nx_block_size
        b[40..48].copy_from_slice(&64u64.to_le_bytes()); // nx_block_count
        b[104..108].copy_from_slice(&xp_blocks.to_le_bytes()); // nx_xp_desc_blocks
        b[112..120].copy_from_slice(&xp_base.to_le_bytes()); // nx_xp_desc_base
        b[180..184].copy_from_slice(&1u32.to_le_bytes()); // nx_max_file_systems
        b[184..192].copy_from_slice(&1026u64.to_le_bytes()); // nx_fs_oid[0]
        let c = fletcher64(&b);
        b[0..8].copy_from_slice(&c.to_le_bytes());
        b
    }

    #[test]
    fn picks_highest_valid_xid_from_ring() {
        // bootstrap describes a 2-block ring starting at block 1.
        let boot = make_sb(5, 1, 2);
        let mut img = vec![0u8; 4096 * 3];
        img[0..4096].copy_from_slice(&boot); // block 0 (bootstrap)
        img[4096..8192].copy_from_slice(&make_sb(5, 1, 2)); // ring[0] xid 5
        img[8192..12288].copy_from_slice(&make_sb(9, 1, 2)); // ring[1] xid 9
        let mut dev = MemDev(img);
        let bootstrap = NxSuperblock::parse(&boot).unwrap();
        let sel = latest_superblock(&mut dev, &bootstrap).unwrap();
        assert_eq!(sel.obj.xid, 9);
    }

    #[test]
    fn skips_corrupt_higher_xid_keeps_valid() {
        let boot = make_sb(7, 1, 2);
        let mut img = vec![0u8; 4096 * 3];
        img[0..4096].copy_from_slice(&boot);
        img[4096..8192].copy_from_slice(&make_sb(7, 1, 2)); // valid xid 7
        let mut corrupt = make_sb(99, 1, 2); // would-be newest...
        corrupt[200] ^= 0xFF; // ...but checksum now invalid
        img[8192..12288].copy_from_slice(&corrupt);
        let mut dev = MemDev(img);
        let bootstrap = NxSuperblock::parse(&boot).unwrap();
        let sel = latest_superblock(&mut dev, &bootstrap).unwrap();
        assert_eq!(sel.obj.xid, 7, "corrupt xid 99 must be rejected");
    }
}
```

- [ ] **Step 2:** `lib.rs`'e ekle (container satırından sonra): `pub mod checkpoint;`

- [ ] **Step 3 (red doğrula):** `cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml checkpoint` → `picks_highest_valid_xid_from_ring` KIRMIZI (stub `Ok(bootstrap.clone())` döndüğü için xid 5 != 9). Sebebin bu olduğunu doğrula.

- [ ] **Step 4 (green):** Stub `latest_superblock` gövdesini yukarıdaki GERÇEK implementasyonla değiştir. `cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` → her iki checkpoint testi de geçer (`picks_highest_valid_xid_from_ring`, `skips_corrupt_higher_xid_keeps_valid`).

- [ ] **Step 5:** `cargo clippy --workspace --all-targets --all-features --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- -D warnings` → exit 0 (üretim kodunda ham indexleme/unwrap yok; `let-else`/`checked_*`/`vec!` kullanıldı).

- [ ] **Step 6: Commit**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/checkpoint.rs crates/apfs-core/src/lib.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): checkpoint ring walk selects newest valid superblock

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `container.rs` — open() en güncel superblock'u kullanır (TDD, gerçek imaj)

**Files:** Modify `crates/apfs-core/src/container.rs`.

- [ ] **Step 1:** `Container::open` içinde, blok 0 bootstrap superblock parse edildikten SONRA, checkpoint walk ile en güncelini seç. Mevcut `open` gövdesindeki `Ok(Self { superblock: sb })` ve block_size yeniden-okuma mantığını şununla değiştir (bootstrap'i seçtikten sonra ring'i gez):
```rust
        let bootstrap = NxSuperblock::parse(&buf)?;
        // block 0 may be stale; the authoritative superblock is the newest
        // valid one in the checkpoint descriptor ring (M1b).
        let superblock = crate::checkpoint::latest_superblock(dev, &bootstrap)?;
        Ok(Self { superblock })
```
(Önceki >=1024 farklı block_size yeniden-okuma dalını kaldır; M1a fixture'ında block_size 4096 ve `latest_superblock` zaten `block_size` ile çalışır. Non-4096 desteği gerekirse ayrı milestone.)

- [ ] **Step 2:** `container::tests::opens_real_container_via_blockdevice` testine ekle:
```rust
        // selected superblock must be at least as new as the block-0 copy
        let mut dev0 = FileBlockDevice::open(img).expect("reopen");
        let mut b0 = vec![0u8; 4096];
        dev0.read_at(0, &mut b0).expect("read blk0");
        let boot = NxSuperblock::parse(&b0).expect("blk0 sb");
        assert!(c.superblock.obj.xid >= boot.obj.xid);
        assert_eq!(c.superblock.block_size, 4096);
        assert!(!c.superblock.fs_oids.is_empty());
```
(Testin başındaki `use` ihtiyaçları: `crate::block_device::BlockDevice`, `crate::nx::NxSuperblock` — gerekiyorsa ekle.)

- [ ] **Step 3:** `cargo run -p xtask -- gen-fixture` (manifest-path ile) sonra `cargo test -p apfs-core --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` → tüm testler geçer; gerçek imajda seçilen superblock geçerli ve `xid >= blok0.xid`. `cargo clippy --workspace --all-targets --all-features --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- -D warnings` → exit 0.

- [ ] **Step 4: Commit**
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src/container.rs
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "feat(apfs-core): Container::open uses newest checkpoint superblock

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Gates + final doğrulama

**Files:** none (doğrulama; fmt commit olabilir).

- [ ] **Step 1:** `cargo fmt --all --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` sonra `cargo fmt --all --check ...` exit 0.
- [ ] **Step 2:** `cargo clippy --workspace --all-targets --all-features --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml -- -D warnings` exit 0.
- [ ] **Step 3:** `cargo run -p xtask -- gen-fixture` sonra `cargo test --workspace --manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml` → apfs-core artık 12+ test (önceki 10 + 2 checkpoint), hepsi yeşil. `cargo build --workspace ...` exit 0.
- [ ] **Step 4:** fmt bir şey değiştirdiyse commit (yoksa atla — `git status --porcelain` boşsa):
```bash
git -C /Users/eiaydin/Documents/projects/apfs-explorer add crates/apfs-core/src
git -C /Users/eiaydin/Documents/projects/apfs-explorer commit -m "style: fmt + clippy gate green for M1b

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```
- [ ] **Step 5:** `git -C /Users/eiaydin/Documents/projects/apfs-explorer log --oneline -8` raporla. Push YOK.

---

## Self-Review (plan yazarı)

- **Spec coverage:** M1b = mount procedure adım 2-4 (PDF p26-27): blok 0'dan ring'i bul, gez, en yüksek geçerli xid'li superblock'u seç. checkpoint_map_phys_t'nin `cpm_map` çözümü (ephemeral oid→paddr) BİLİNÇLİ ertelendi (spaceman/omap milestone'u) — non-superblock bloklar `NxSuperblock::parse` başarısız olunca atlanır.
- **Placeholder yok:** tüm kod tam; sentetik test gerçek Fletcher-64 (`checksum::fletcher64`) ile geçerli bloklar üretir → ring seçim mantığı deterministik kanıtlanır; ayrıca gerçek imaj testi.
- **Tip tutarlılığı:** `latest_superblock(&mut D, &NxSuperblock)->Result<NxSuperblock,ContainerError>`; `NxSuperblock` alanları M1a ile uyumlu + yeni `xp_desc_base/xp_desc_blocks`; `ContainerError` mevcut.
- **Risk:** non-4096 block_size yeniden-okuma kaldırıldı (M1a fixture 4096; genel destek gerekirse ayrı iş). checked_add/checked_mul ile offset taşması güvenli; ring'de bozuk/yabancı blok parse hatasıyla atlanır (panik yok).
