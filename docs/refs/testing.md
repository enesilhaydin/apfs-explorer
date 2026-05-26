# Test ve CI Stratejisi

İlke: testler bir şeyin *çalıştığını* kanıtlar, *hiçbir koşulda bozmadığını*
kanıtlamaz. Bu yüzden katmanlı test + differential doğrulama + fuzzing.

## Katman 1 — Parser unit (her platformda)

- `apfs-core` / `apfs`, `BlockDevice` trait üzerinden **dosya-tabanlı fixture
  image**'larla.
- Kapsam: NX superblock, checkpoint, object map, B-tree gezinme, dizin listele,
  dosya extent, xattr/Finder metadata, sıkıştırma (ZLIB/LZVN/LZFSE), FileVault
  çözme, snapshot.
- **Bozuk/kesik image testleri:** her parser girişi için truncate/bitflip
  fixture → `Err` döner, panik yok.
- `cargo test` her PR'da; coverage takibi (`cargo-llvm-cov`).

## Katman 2 — Windows entegrasyon

- **(a) CI:** GitHub Actions `windows-latest` → `msiexec /i winfsp.msi /qn`
  (Microsoft-imzalı, reboot yok) → fixture'ı mount → listele → dosya hash'lerini
  manifest'le karşılaştır → unmount. Cargo feature ile gate.
- **(b) Gerçek donanım:** kullanıcının **gerçek APFS USB'si**, SSH ile
  erişilebilen Windows makinesinde. M4+ her sürümde: gerçek Mac diskini mount
  et, içeriği bilinen referansla karşılaştır. Bu **release gate**.

## Differential oracle (sessiz bozulmaya karşı)

- Aynı fixture/diski **bağımsız ikinci yolla** oku, byte/checksum karşılaştır.
- Okuma oracle'ı: apfs-fuse / linux-apfs araçları (sadece doğrulama; kod
  kopyalanmaz). Referans manifest fixture üretiminde kaydedilir.
- **Yazma (v2) için ZORUNLU:** yaz → bağımsız oku → checksum eşleşmezse fail +
  rollback. Bu olmadan write feature derlenmez.

## Kontrollü fixture üretimi (`xtask`, macOS runner)

Kullanıcının tek gerçek diski var; çeşitlilik için **ücretsiz macOS GitHub
Actions runner**'da fixture üret (kişisel Mac gerekmez):

- `hdiutil create -size 64m -fs APFS -volname Test test.dmg`
- `newfs_apfs` / `diskutil` ile snapshot'lı, sıkıştırılmış (`afsctool`/
  kopyalanan sistem dosyaları), FileVault'lu küçük image'lar
- Her fixture yanında **manifest** (dosya yolu → boyut → sha256 → xattr) →
  Git LFS'te `tests/fixtures/`
- `xtask gen-fixtures` lokal+CI'da tekrar üretilebilir; image'lar küçük (<64MB)

## Fuzzing

- `cargo-fuzz` hedefleri: superblock, omap, btree node, file extent, decmpfs
  header, keybag. CI'da kısa smoke; nightly uzun.
- Bulunan her panik/crash → önce kırılan test (corpus'a ekle), sonra fix
  (TDD bugfix kuralı).

## CI özet (`.github/workflows/ci.yml`)

| Job | Platform | İçerik |
|---|---|---|
| lint | ubuntu | `cargo fmt --check`, `clippy -D`, `cargo deny` |
| test | ubuntu | Katman 1 + coverage |
| gen-fixtures | macos | `xtask gen-fixtures` (cache/LFS) |
| win-integration | windows | Katman 2(a): WinFsp + mount fixture |
| fuzz-smoke | ubuntu | `cargo-fuzz` kısa |

Gerçek donanım (Katman 2b) manuel/scheduled — SSH hedefi gizli.
