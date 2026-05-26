# Hata Yönetimi, Crash Koruması, Logging

Bu proje **güvenilmeyen disk verisi** parse eder ve File Explorer'ın arkasında
çalışır. Crash koruması opsiyonel değil, **çekirdek gereksinim** (kullanıcı şartı).

## Kesin kurallar

1. **Kütüphane yollarında `unwrap()`, `expect()`, `panic!()`, `unreachable!()`,
   `todo!()`, dizi index panic'i YASAK.** Her hata `Result<_, ApfsError>` ile
   propagate edilir. (`apfs-core`, `apfs`, `apfs-write`)
2. Hata tipi: crate başına `thiserror` enum (`ApfsError`, `WriteError`, ...);
   üst katman `#[from]` ile sarar. Anlamlı varyant (offset, beklenen/bulunan).
3. **Error yutma YASAK:** boş `catch`, `let _ = result`, `if let Ok(_)` ile
   sessiz düşürme yok. Ya `?` ile propagate ya `tracing::warn!/error!` ile logla.
4. **Panik bariyerleri:** dış sınırlarda `std::panic::catch_unwind`:
   - Her WinFsp callback (bir dosya bozuksa volume düşmez)
   - Servis görev döngüsü / her device-event handler
   - FFI/Win32 çağrı sarmalayıcıları
   `catch_unwind` yakaladığını `error!` ile loglar + güvenli hata kodu döner.
5. **Bozuk/kesik/kötü niyetli image → panik değil, nazik `Err`.** Bu fuzzing ile
   kanıtlanır (`cargo-fuzz` hedefleri: superblock, btree, extent, decmpfs,
   keybag parse). CI'da fuzz smoke.
6. **Clippy gate (CI'da `-D`):** `clippy::unwrap_used`, `expect_used`, `panic`,
   `indexing_slicing` (lib crate'lerde), `unwrap_in_result`. `cargo deny` ile
   lisans/RUSTSEC. `#![forbid(unsafe_code)]` `apfs-core`'da; diğerlerinde
   `unsafe` sadece gerekçeli FFI/raw-IO bloğunda (`// SAFETY:` yorumu zorunlu).

## Logging

- `tracing` + `tracing-subscriber`. Dosya: `tracing-appender` günlük rotasyon.
- Seviye config'ten (TOML), default `info`; `debug/trace` sorun gidermede.
- Servis ayrıca **Windows Event Log**'a kritik olayları yazar (mount başarısız,
  watchdog restart, doğrulama hatası).
- Yapısal alanlar: `volume_id`, `oid`, `xid`, `path` (PII sızdırma — dosya
  içeriği loglanmaz, sadece metadata).
- Yazma (v2): her yazma işlemi audit log'a (öncesi snapshot id, sonrası
  differential sonuç).

## Servis dayanıklılığı

- Watchdog: servis ana döngüsü panik/exit → otomatik restart (SCM recovery +
  iç supervisor). Restart'ta son mount durumu kalıcı state'ten yeniden kurulur.
- Mount kaybı kullanıcıya tray bildirimi + Event Log.
- Graceful shutdown: SCM stop → tüm volume'ları temiz unmount, sonra çık.

## Salt-okunur garantisi (v1)

Write/Create/Rename callback'leri **kod düzeyinde implement edilmez** (sadece
`STATUS_MEDIA_WRITE_PROTECTED` döner). Derleyici read-only'yi zorlar; kullanıcı
yazmaya çalışınca net "salt-okunur" uyarısı (tray + Explorer hata).

## Test eşiği

Hiçbir milestone, ilgili crate'in (a) clippy gate yeşil, (b) bozuk-image
testleri + fuzz smoke geçer, (c) gerçek USB differential (M4+) tutar olmadan
"tamam" sayılmaz.
