# APFS Explorer for Windows — Onaylanmış Tasarım Spec'i

> Durum: **Onaylandı** · Tarih: 2026-05-17 · Dil: Rust (pür, dış parser yok)
> Lisans: GPLv3 · Proje: `apfs-explorer`
> Bu dosya brainstorming çıktısıdır. Üst seviye özet `DESIGN.md`'de; derin
> referanslar `docs/refs/` altında.

## 1. Hedef ve Kapsam

Windows 10/11'de bir Mac diski (USB / SSD) takıldığında **otomatik** olarak APFS
volume'larını algılayıp normal bir sürücü harfi (örn. `Z:`) olarak sunan,
**tamamen kendi yazdığımız pür-Rust** uygulama. Kullanıcı elle hiçbir şey mount
etmez; cihaz takılınca görünür, çıkınca kaybolur.

**Tam macOS paritesi hedefi** (NTFS/FAT32'de File Explorer'da ne yapılıyorsa APFS'te
de): oku + yaz, ZLIB/LZVN/LZFSE transparan sıkıştırma, FileVault (yazılım) çözme,
snapshot okuma, xattr + macOS/Finder metadata (etiketler, renkli etiketler, resource
fork).

"Önce okuma → sonra yazma" bir **özellik kısma değil, inşa sırasıdır**: yazıcı katman
okuyucu katmanın üstüne kurulur (COW yazma, B-tree/object map'i önce doğru okumayı
gerektirir). Bitiş noktası %100 paritedir.

### Kapsam dışı (kalıcı)
| Özellik | Neden |
|---|---|
| T2 / Apple Silicon dahili disk donanım şifresi | Anahtar Secure Enclave'de, CPU'ya çıkmıyor — teknik olarak imkânsız |
| APFS v1 (eski format) | Nadir; gerekirse sonra |

### Avantajımız: tam spec erişimi
Apple "Apple File System Reference"ın tamamı **Context7** (`pradorocchi/
apfs-filesystem-full-reference`) ve **NotebookLM** (soru-cevap) üzerinden bize açık.
libfsapfs'in eksik bıraktığı LZFSE, snapshot, FileVault gibi alanları **biz spec'ten
doğrudan** uygulayabiliriz — yani açık kaynak parser'ların çoğundan daha geniş
kapsama ulaşmak mümkün. Spec kullanım kuralları: `docs/refs/apfs-spec-sources.md`.

## 2. Mimari Kararlar

| Karar | Seçim | Gerekçe |
|---|---|---|
| Dil | **Pür Rust** | Güvenilmeyen disk verisi parse ederken bellek güvenliği en kritik burada. Tek dil, FFI/`unsafe`/bindgen yok. C'nin tek gerekçesi olgun libfsapfs'i kullanmaktı; onu reddettik. |
| APFS parser | **Sıfırdan bizim** | Kullanıcı kararı. Spec kaynağı Context7 + NotebookLM. `apfs-types` crate'i yalnız *tip referansı* (kod kopyası değil). |
| Yazma referansı | **linux-apfs-rw** (GPL) — doğruluk *referansı* | Açık kaynakta yazan tek implementasyon. Kod kopyalanmaz; COW/checkpoint semantiği çapraz kontrol için. GPLv3 uyumlu. |
| FS sunumu | **WinFsp** (GPLv3 + FLOSS exception) | Microsoft-imzalı kernel sürücüsü; son kullanıcı test-signing/Secure Boot derdi yok. Kanıtlı (sshfs-win, rclone). NSE/shell extension YOK (explorer.exe çökertme riski). |
| WinFsp binding | **`winfsp` crate** (SnowflakePowered/winfsp-rs ≥0.13) | Bakımlı, native API sarar, conformance geçer. |
| Mount motoru | **Windows Servisi (servis-önce)** | Boot'ta başlar, UAC sormaz, oturum kapalıyken bile mount eder. "Kesinlikle sorunsuz" şartının cevabı. Tray = ince UI istemci. |
| WinFsp dağıtımı | **Installer'a gömülü** | Tek seferlik, Microsoft-imzalı MSI, reboot yok. Kullanıcı elle bir şey kurmaz. |
| Lisans | **GPLv3** | WinFsp link + linux-apfs-rw referansı bunu gerektirir. Tamamen açık kaynak. |
| Çoklu volume | Sadece kullanıcı volume'ları, ayrı harf | Sistem volume'ları (Preboot/Recovery/VM/Update) gizlenir. |

## 3. Sistem Mimarisi

Detay: `docs/refs/architecture.md`.

```
apfs-tray        ince UI: liste, manuel mount, ayar, log  (ayrı süreç)
   │ named pipe IPC
apfs-service     Windows Servisi (LocalSystem): WM_DEVICECHANGE,
   │             GPT GUID + NXSB tespit, auto-mount/unmount, config, watchdog
winfsp-host      WinFsp callback'leri (v1: 7 read-only; v2: write, feature-gated)
   │
apfs             güvenli yüksek API: volume/dizin/dosya/xattr +
   │             codec'ler (ZLIB/LZVN/LZFSE) + FileVault + snapshot
apfs-core        on-disk struct'lar, Fletcher-64, B-tree, object map,
   │             BlockDevice trait — saf parse, panik yok
\\.\PhysicalDriveN   raw blok cihaz IO
```

Crate'ler ve tek sorumlulukları:

| Crate | Sorumluluk | Kritik kural |
|---|---|---|
| `apfs-core` | On-disk yapılar, checksum, B-tree, object map, `BlockDevice` trait | `#![forbid(unsafe_code)]` hedef; `unwrap/expect/panic` yasak; bozuk girdi → `Result` |
| `apfs` | Container/volume/dizin/dosya/xattr API + sıkıştırma codec'leri + FileVault + snapshot | Tüm public fn `Result<_, ApfsError>` döner |
| `apfs-write` | Copy-on-write mutasyon | `write` cargo feature arkasında; yazma→bağımsız tekrar-oku→checksum zorunlu |
| `winfsp-host` | WinFsp callback implementasyonu | Her callback `catch_unwind`; v1'de write callback'leri kod düzeyinde YOK |
| `apfs-service` | Cihaz olayı, tespit, auto-mount, config, watchdog | Panik → otomatik restart; mount durumu kalıcı |
| `apfs-tray` | UI istemci | Çökerse mount etkilenmez (ayrı süreç) |
| `xtask` | Fixture üretimi, differential harness | macOS runner'da çalışan fixture jeneratörü |

Bağımlılık yönü tek yönlü: `apfs-core` → `apfs` → (`apfs-write`/`winfsp-host`) →
`apfs-service` → `apfs-tray`. Alt katman üst katmanı tanımaz.

## 4. Veri Akışı ve Yaşam Döngüsü

1. Servis başlar (boot'ta), zaten takılı diskleri tarar.
2. `RegisterDeviceNotification` + `WM_DEVICECHANGE`
   (`DBT_DEVICEARRIVAL`/`DBT_DEVICEREMOVECOMPLETE`).
3. Yeni disk: `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` → her partition GPT tip GUID
   `7C3457EF-0000-11AA-AA11-00306543ECAC` mı → `NXSB` (`0x4253584E`) superblock magic
   doğrula.
4. Container volume'larını listele → sistem volume'larını filtrele → kullanıcı
   volume'larını boş sürücü harflerine WinFsp ile read-only mount.
5. Disk çıkınca ilgili mount(lar) temiz unmount.
6. Tray, servisten named pipe ile durum okur; manuel mount/unmount komutu gönderir.

## 5. Hata Yönetimi, Crash Koruması, Logging (çekirdek şart)

Tam disiplin: `docs/refs/error-handling-crash-safety.md`. Özet:

- Kütüphane yollarında **`unwrap`/`expect`/`panic!`/`unreachable!` yasak**. Her şey
  `Result<_, ApfsError>` (thiserror). Clippy `unwrap_used`, `expect_used`,
  `panic` → `deny`. CI'da zorlanır.
- WinFsp callback ve servis görev sınırlarında **`std::panic::catch_unwind`** →
  tek bir bozuk dosya volume'u değil sadece o işlemi düşürür; explorer.exe asla
  etkilenmez.
- Bozuk/kesik/kötü niyetli image → nazik hata + yapısal log; **panik yok**
  (fuzzing ile kanıtlanır — `cargo-fuzz`).
- **Error yutma yasak** (boş `catch`, `let _ =` ile hata atma): ya propagate ya log.
- Logging: `tracing` + dönen dosya logu (`tracing-appender`), seviyeler config'ten.
  Servis ayrıca Windows Event Log'a yazar.
- Servis watchdog: panik/exit → otomatik restart; son mount durumu kalıcı, yeniden
  kurulur.
- v1'de yazma callback'leri **kod düzeyinde implement edilmez** → salt-okunur
  garantisi derleyiciyle zorlanır + kullanıcıya net "salt-okunur" uyarısı.

## 6. Yazma Güvenliği (`write` feature, v2)

Detay `docs/refs/architecture.md` §yazma. Doğrulama-önce mimarisi:

1. Yazmadan önce **zorunlu otomatik snapshot**.
2. Yazma.
3. **Bağımsız tekrar-oku + checksum differential** (yazan koddan ayrı okuma yolu).
4. Tutmazsa → snapshot'a rollback + hata; kullanıcıya bildir.
5. Geniş fuzzing + power-loss/kesinti simülasyonu testleri.
6. linux-apfs-rw doğruluk referansı olarak çapraz kontrol.
7. "Bu deneysel — riski kabul et" açık kullanıcı onayı.
8. v1 gerçek diskte stabil olmadan v2 başlamaz.

## 7. Test ve CI Stratejisi

Detay: `docs/refs/testing.md`. Katmanlar:

- **Katman 1 — Parser unit (her yerde):** `apfs-core`/`apfs`, dosya-tabanlı fixture
  image'larla `BlockDevice` üzerinden. Bozuk image'da nazik hata testleri.
- **Katman 2 — Windows entegrasyon:** (a) GitHub Actions `windows-latest`'te
  WinFsp kurup fixture mount/listele/hash; (b) **kullanıcının gerçek APFS USB'si**,
  SSH ile erişilebilen Windows makinesinde — gerçek Mac diskine karşı doğrulama.
- **Differential oracle:** aynı image'ı bağımsız ikinci yolla oku, byte/checksum
  karşılaştır. Yazmada bu **zorunlu gate**.
- **Kontrollü fixture üretimi:** ücretsiz **macOS GitHub Actions runner**'da
  `hdiutil`/`newfs_apfs` ile küçük sıkıştırılmış/şifreli/snapshot'lı image'lar
  (`xtask`). Kişisel Mac gerekmez.
- Sürekli fuzzing (`cargo-fuzz`), coverage takibi.

## 8. Proje Yapısı

```
apfs-explorer/
├── Cargo.toml                  # workspace
├── CLAUDE.md                   # YALIN: workflow + context-mode + docs/refs işaretçileri
├── DESIGN.md                   # üst seviye tasarım (v3)
├── LICENSE                     # GPLv3
├── .gitignore  .gitattributes  # LFS: tests/fixtures
├── rust-toolchain.toml
├── crates/
│   ├── apfs-core/   + CLAUDE.md
│   ├── apfs/        + CLAUDE.md
│   ├── apfs-write/  + CLAUDE.md
│   ├── winfsp-host/ + CLAUDE.md
│   ├── apfs-service/+ CLAUDE.md
│   └── apfs-tray/   + CLAUDE.md
├── xtask/           + CLAUDE.md
├── docs/
│   ├── refs/        # apfs-spec-sources, architecture, error-handling-crash-safety,
│   │                #   testing, git-workflow
│   └── superpowers/specs/      # bu spec
├── tests/fixtures/  # Git LFS
├── installer/       # WiX/MSI, WinFsp bundled
└── .github/workflows/ci.yml
```

Her crate'in `CLAUDE.md`'i o crate'in tek sorumluluğunu, sınırlarını, test komutunu
ve "buraya dokunmadan önce oku" referansını içerir (kök CLAUDE.md yalın kalır).

## 9. Yol Haritası

| # | Milestone | Çıktı |
|---|---|---|
| M0 | İskelet | Cargo workspace, GPLv3, CI iskeleti, docs/refs, per-crate CLAUDE.md, git+LFS |
| M1 | `apfs-core` | NX superblock, checkpoint, object map, B-tree, Fletcher-64 — fixture testleri |
| M2 | `apfs` okuma | Volume listele (sistem filtrele), dizin/dosya/xattr/Finder metadata + ZLIB/LZVN |
| M3 | `apfs` ileri | LZFSE + FileVault çözme + snapshot okuma (spec'ten, Context7/NotebookLM ile) |
| M4 | `winfsp-host` | 7 read-only callback; **gerçek USB'ne SSH ile** differential doğrulama |
| M5 | `apfs-service` + `apfs-tray` | Servis auto-mount/unmount, watchdog, tray UI, config, salt-okunur uyarı |
| M6 | Paketleme | WiX/MSI (WinFsp bundled), kullanıcı dokümanı |
| M7 (v2) | `apfs-write` | Doğrulama-önce mimarisi, snapshot+differential, fuzzing, linux-apfs-rw çapraz |
| M8 | Sertleştirme | Geniş fuzzing, coverage, power-loss simülasyonu, performans |

## 10. Riskler ve Azaltma

| Risk | Azaltma |
|---|---|
| LZFSE/snapshot spec'te eksik/az dokümante | Context7+NotebookLM tam spec; gerekirse apfs-fuse/linux-apfs-rw *referans* |
| Yazmada sessiz veri bozma | Doğrulama-önce mimarisi (snapshot+differential+fuzzing); v1 stabil olmadan başlama |
| Tek gerçek test diski | macOS runner ile kontrollü fixture çeşitliliği |
| WinFsp sürüm/CVE | Stable 2.x'e pin; yeni stable çıkınca güncelle |
| Otomatik mount yanlış disk | Sadece GPT GUID **ve** `NXSB` doğrularsa mount |
| Parser hatası → yanlış dosya | Her sürüm gerçek USB differential gate'i geçmeden "tamam" değil |

## 11. Açık İş

- NotebookLM `setup_auth` + kullanıcının APFS notebook'unun kaydı (M1 öncesi).
- Kullanıcının SSH'lenebilir Windows makinesi + APFS USB erişim detayları (M4 için).
