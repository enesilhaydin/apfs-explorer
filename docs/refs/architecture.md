# Mimari Referansı

## Katmanlar ve tek sorumluluk

```
apfs-tray        ince UI istemci (ayrı süreç)
   │  named pipe IPC (durum oku / komut gönder)
apfs-service     Windows Servisi (LocalSystem)
   │
winfsp-host      WinFsp callback implementasyonu
   │
apfs             güvenli yüksek API
   │
apfs-core        on-disk parse + BlockDevice trait
   │
\\.\PhysicalDriveN   raw blok cihaz
```

Bağımlılık tek yönlü (yukarı bilmez): `apfs-core → apfs → {apfs-write,
winfsp-host} → apfs-service → apfs-tray`. `xtask` bağımsız (fixture/harness).

### `apfs-core`
- NX superblock, checkpoint alanı, object map (omap), B-tree (catalog/extent/
  snapshot), Fletcher-64 checksum, oid/xid çözümleme.
- `BlockDevice` trait: `read_block`/`block_size`/`size`. Prod = raw `\\.\`
  handle; test = dosya-tabanlı image. **Parser cihaz tipini bilmez** (test
  edilebilirlik).
- Hedef `#![forbid(unsafe_code)]`. `unwrap/expect/panic` YASAK. Bozuk girdi →
  `Result`. APFS yapıları için kaynak: `apfs-spec-sources.md`.

### `apfs`
- Container aç, volume listele + **sistem volume filtresi** (Preboot/Recovery/
  VM/Update gizli; rol bayraklarından), dizin gez, dosya extent oku, xattr +
  macOS/Finder metadata (etiket, renkli etiket, resource fork).
- Sıkıştırma codec'leri: ZLIB (DEFLATE), LZVN, LZFSE — `decmpfs` başlığından
  yönlendirme.
- FileVault (yazılım) çözme: keybag → wrapped VEK → AES-XTS.
- Snapshot okuma (xid bazlı). Tüm public fn `Result<_, ApfsError>`.

### `apfs-write` (`write` feature, v2)
- Copy-on-time-COW mutasyon. **Doğrulama-önce mimarisi zorunlu:**
  1. Yazmadan önce otomatik snapshot
  2. Yazma
  3. Bağımsız tekrar-oku (yazan koddan ayrı yol) + checksum differential
  4. Tutmazsa snapshot'a rollback + `Result::Err` + kullanıcı bildirimi
- linux-apfs-rw doğruluk *referansı* (kod kopyası değil).
- v1 gerçek diskte stabil olmadan derlenmez bile (feature kapalı).

### `winfsp-host`
- v1 read-only callback: `GetVolumeInfo`, `GetSecurityByName` (statik "everyone
  read" DACL), `Open`, `Close`, `Read`, `ReadDirectory`, `GetFileInfo`.
- `Create/Write/Rename/SetBasicInfo/Flush` v1'de **kod düzeyinde yok** →
  salt-okunur garantisi derleyiciyle zorlanır.
- Her callback `std::panic::catch_unwind` ile sarılır → bir dosya bozuksa
  volume değil sadece o işlem hata döner.
- v2'de write callback'leri ayrı feature; her biri `apfs-write` doğrulama
  hattından geçer.

### `apfs-service`
- Windows Servisi (LocalSystem). Boot'ta başlar, UAC yok, oturum kapalıyken
  çalışır.
- `RegisterDeviceNotification` + `WM_DEVICECHANGE`
  (`DBT_DEVICEARRIVAL`/`DBT_DEVICEREMOVECOMPLETE`).
- Disk gelince: `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` → GPT tip GUID
  `7C3457EF-0000-11AA-AA11-00306543ECAC` → `NXSB` (`0x4253584E`) magic doğrula
  → kullanıcı volume'larını boş sürücü harflerine read-only mount.
- Watchdog: panik/exit → otomatik restart; mount durumu kalıcı (config/state),
  yeniden kurulur.
- Config (TOML): sürücü harfleri, auto-mount aç/kapa, log seviyesi.

### `apfs-tray`
- Ayrı süreç. Servise named pipe ile bağlanır: disk/volume listesi, kapasite,
  mount durumu, manuel mount/unmount, salt-okunur uyarı, log görüntü.
- Çökerse mount **etkilenmez** (servis bağımsız).

## Veri akışı (mount)

disk takılır → servis olayı yakalar → partition/GPT/NXSB doğrula → volume filtre
→ `winfsp-host` mount → File Explorer çağrıları → `winfsp-host` callback →
`apfs` API → `apfs-core` parse → raw `BlockDevice` read. Çıkarma → temiz unmount.

## WinFsp / installer

- `winfsp` crate (SnowflakePowered/winfsp-rs ≥0.13), native API.
- WinFsp Microsoft-imzalı MSI **kendi installer'ımıza gömülü** (WiX). Tek
  seferlik, reboot yok. Kullanıcı elle bir şey kurmaz.
- WinFsp sürümü stable 2.x'e pin; yeni stable çıkınca güncelle.
