# APFS Explorer for Windows — Tasarım Dokümanı

> Durum: **v3 — onaylandı** · Tarih: 2026-05-17 · Dil: **Pür Rust** · Lisans: **GPLv3**
> Proje: `apfs-explorer`
> Onaylanmış tam spec: `docs/superpowers/specs/2026-05-17-apfs-explorer-design.md`
> Derin referanslar: `docs/refs/` · Bu dosya üst seviye özettir.

Windows 10/11'de bir Mac diski (USB / SSD) takıldığında **otomatik** olarak APFS
volume'larını algılayıp File Explorer'da normal bir sürücü harfi (örn. `Z:`) olarak
sunan, **tamamen kendi yazdığımız pür-Rust** uygulama. Kullanıcı hiçbir şeyi elle
mount etmez; cihaz takılınca görünür, çıkarılınca kaybolur.

**Hedef: tam macOS paritesi.** NTFS/FAT32'de File Explorer'da yapılabilen her şey
APFS'te de: oku + yaz, ZLIB/LZVN/LZFSE sıkıştırma, FileVault (yazılım) çözme,
snapshot okuma, xattr + Finder metadata. "Önce okuma → sonra yazma" özellik kısma
değil **inşa sırasıdır** (yazıcı, okuyucunun üstüne kurulur); bitiş %100 paritedir.

## Temel kararlar

- **Pür Rust, parser sıfırdan bizim.** Dış C kütüphanesi (libfsapfs) YOK. Bellek
  güvenliği güvenilmeyen disk verisinde en kritik burada.
- **Spec kaynağı = Context7 + NotebookLM.** Apple "Apple File System Reference"ın
  tamamı Context7 (`pradorocchi/apfs-filesystem-full-reference`) ve NotebookLM
  (soru-cevap) üzerinden bize açık. Bu sayede libfsapfs'in atladığı LZFSE / snapshot
  / FileVault gibi alanları doğrudan spec'ten uygulayabiliriz — kapsama avantajımız.
  Kullanım: `docs/refs/apfs-spec-sources.md`.
- **WinFsp** ile gerçek sürücü harfi. Microsoft-imzalı kernel sürücüsü; son
  kullanıcı test-signing/Secure Boot ile uğraşmaz. NSE/shell extension YOK.
- **Servis-önce mount motoru.** Windows Servisi boot'ta başlar, UAC sormaz, oturum
  kapalıyken bile mount eder ("kesinlikle sorunsuz" şartının cevabı). Tray = ince
  UI istemci. WinFsp installer'a gömülü.
- **Yazma** ayrı `write` feature + doğrulama-önce mimarisi (snapshot → yaz →
  bağımsız tekrar-oku → checksum → tutmazsa rollback). linux-apfs-rw yalnız
  doğruluk *referansı* (kod kopyası değil).
- **Crash koruması çekirdek şart:** kütüphane yollarında `unwrap/panic` yasak, her
  şey `Result`, WinFsp/servis sınırında `catch_unwind`, kapsamlı `tracing` logging,
  servis watchdog. Detay: `docs/refs/error-handling-crash-safety.md`.

## Mimari (özet)

```
apfs-tray  →(IPC)→ apfs-service → winfsp-host → apfs → apfs-core → \\.\PhysicalDriveN
```

Crate sorumlulukları, veri akışı, yazma güvenliği: `docs/refs/architecture.md`.
Test/CI: `docs/refs/testing.md`. Git akışı: `docs/refs/git-workflow.md`.

## Kapsam dışı (kalıcı)

| Özellik | Neden |
|---|---|
| T2 / Apple Silicon dahili disk donanım şifresi | Anahtar Secure Enclave'de — teknik olarak imkânsız |
| APFS v1 (eski format) | Nadir; gerekirse sonra |

## Yol haritası

M0 iskelet · M1 `apfs-core` · M2 `apfs` okuma (ZLIB/LZVN) · M3 LZFSE+FileVault+
snapshot · M4 `winfsp-host` read-only + gerçek USB differential · M5 servis+tray
auto-mount · M6 MSI paketleme · M7 `apfs-write` (doğrulama-önce) · M8 sertleştirme.

Tam tablo, riskler ve açık iş: onaylanmış spec dosyasında.
