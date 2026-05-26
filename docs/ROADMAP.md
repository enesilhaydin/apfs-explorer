# apfs-explorer — Yol Haritası & Devam Durumu (compact-sonrası resume)

> Bu dosya **kalıcı durum kaydı**. Oturum compact'lansa bile buradan devam et.
> Tarih: 2026-05-26. Tasarım: `DESIGN.md` · Onaylı spec: `docs/superpowers/specs/`
> · Süreç kuralları: `CLAUDE.md` (md.7 LOCKED cross-check workflow, md.6
> sor-NotebookLM, yağsız review, dil, squash).
>
> **En son durum (2026-05-26 Wave 16 — FINAL / v1.0.0-rc1)**: 17 production bug fix (Waves 1–16).
> Waves 11–16: M8 #2 cleanup batching, M8 #3 omap corruption fix + CIB-sorted drain,
> M11 incremental catalog, M12 incremental omap. Code-side perf saturated (W16 empirical).
> Cross-check matrix FROZEN Wave 10 final (✅60 / 🐛11 / ⚠️24 / ⏳1 / 🚫8 = 106 rows) +
> M11/M12 incremental notes appended.
> **M7–M12 CLOSED. Remaining = upstream-blocked features (Faz Q) + M10 feature backlog**.
> **Sıradaki: Faz Q** — upstream patches (WinFsp hard-link FILE_LINK_INFORMATION + FSCTL
> handle→path API), Windows installer polish, user-side deployment.
> M10 open: W4-GAP-2 (decmpfs ZLIB/LZFSE), W6-GAP-3 (UUID resolver) — not blocking v1.0.0-rc1.
> Tam detay: `docs/refs/RESUME-2026-05-26.md` · Audit overview: `docs/refs/audit-status-overview.md`.
> Cross-check matrix: `docs/refs/cross-check-matrix.md`.
> Tag: `v1.0.0-rc1` + `checkpoint/wave-16-final-2026-05-26` (main @ 280b4ee).

## Durum (main'de, merge'li)

| Milestone | Durum | main commit |
|---|---|---|
| M0 iskelet (workspace, BlockDevice, CI) | ✅ | `30e2429` |
| M1a object layer (obj_phys, Fletcher-64, nx_superblock, Container) | ✅ | `65e650c` |
| M1b checkpoint walk (en güncel superblock) | ✅ | `edfef04` |
| M1c-a omap + B-tree node (locate/validate root) | ✅ | `3efaf7a` |
| M2 omap resolve + open volumes (APSB, list_user_volumes) | ✅ | `f5dfd20` |
| M3a volume catalog (volume omap + catalog tree descent + list_dir) | ✅ | `8867a35` |
| M3b file content read (inode + j_dstream + file extents) | ✅ | `3b89810` |
| M3c xattr read (list_xattrs + get_xattr embedded/stream) | ✅ | `b1f8d75` |
| M3d decmpfs framework + LZVN (methods 1/7/8/9) | ✅ | `0e2611d` |
| M3-RT real-target differential + decmpfs method 9 | ✅ | `c697efd` |
| M4a OS-agnostic FsView (apfs crate: open/lookup/read_dir/getattr/read + cache) | ✅ | `577d843` |
| M4b Windows winfsp-host (raw PhysicalDrive + GPT + winfsp→FsView, real-USB byte-exact) | ✅ | `138ea5c` |
| M5 user-session auto-mount agent + tray (USB→drive-letter, out-of-proc, real-USB proven) | ✅ | `adba54b` |
| M6 packaging one-installer (WiX Burn: WinFsp+MSI+autostart) + robustness S1–S5 PASS | ✅ | `3f229b0` |
| M7 apfs-write engine (COW txn, create/write/unlink/rename/mkdir, WinFsp wiring, real-USB proven) | ✅ | various |
| M7b-RT snapshot reclaim subsystem (sm_fq drain, two-xid model, chaos S8) | ✅ | `6e32928` |
| M8 #1 perf opt #1 (BtreeNode clone elim + lookup alloc elim) | ✅ | `8ecf7ad` `a3bcbbb` |
| M8 #2 cleanup batching (BatchQueue, rename/truncate/setattr deferred; W3 4x) | ✅ | `a1b23fb` |
| M8 #3 batch omap corruption fix + CIB-sorted drain (S9/S10 green) | ✅ | `4bc77af` `d9ebc4d` |
| M9 WinFsp feature parity (symlinks, BSD flags, replace-existing, EA bridge, case-fold) | ✅ | various |
| M10 partial (COW reflink, snapshot revert, CLI subcommands, xattr stream write) | ✅ | various |
| M11 incremental catalog B-tree leaf primitives in rename fast path | ✅ | `3c0f7f7` |
| M12 incremental omap patch in rename fast path; perf floor confirmed | ✅ | `625f46f` |

**Okuma yolu uçtan uca çalışıyor:** gerçek macOS APFS imajı → blok 0 → checkpoint
→ container omap → resolve sanal `fs_oid` → volume superblock (APSB) → volume
omap → katalog inişi → dizin → dosya içeriği → xattr → decmpfs/LZVN transparan
açma → **`apfs::FsView` yüksek-seviye API** (`open/lookup/read_dir/getattr/read`
+ yol→inode & dizin cache; `Catalog::stat` ucuz getattr) — winfsp-host/M4b'nin
delege edeceği OS-bağımsız katman. 47 apfs-core unit testi + `apfs` FsView
gerçek-imaj testi (sentetik **ve** gerçek macOS USB imajı `/tmp/apfs-rt.img`'de
kanıtlı) + gerçek-hedef diferansiyel entegrasyon testi, saf Rust
`#![forbid(unsafe_code)]`, clippy `-D --all-features`. **M3-RT: GERÇEK bir
APFS USB'de 2931 `/usr/share/man` dosyasının TAMAMI macOS kernel ile
byte-exact, failures=0** (LZVN method-8 + method-9 + sıkıştırılmamış + dizin
+ xattr + extent). M3a–M3d ayrıca `apfs-large.img` + gerçek /usr/bin LZVN
örnekleri üzerinde Rust ⟷ bağımsız Python ⟷ macOS kernel üçlü mutabakat.
**M3a hashed-key / M3c Reading B / M3d method-8 rsrc / M3d method-9 gerçek-imaj
baytıyla RE-AUDIT-onaylı**; gerçek macOS diski nihai hakem (`apfs-spec:m3*-
confirmed` RE-AUDIT bölümleri).

## Kalan yol haritası

> **Wave 16 state**: M7–M12 CLOSED. Code-side perf saturated. v1.0.0-rc1 tagged.
> Open work: Faz Q (upstream patches + deployment) + M10 feature backlog (non-blocking).

### Faz Q — Upstream patches + deployment (sıradaki)

- **WinFsp hard-link (M9 #2)** — upstream winfsp-rs `FILE_LINK_INFORMATION` callback missing.
  File upstream PR to winfsp-rs; track issue. Blocked until merged.
- **FSCTL_DUPLICATE_EXTENTS_TO_FILE body** — WinFsp 0.13 lacks handle→path API.
  File upstream issue/PR to WinFsp. Body already dispatched (returns STATUS_NOT_SUPPORTED).
- **Windows installer polish** — bundle version from `installer_version`, S3 drive-letter assert.
- **User-side deployment** — deploy v1.0.0-rc1 to real Windows host; collect user feedback.

### M10 feature backlog (non-blocking, post-Faz-Q)

- **M3e decmpfs ZLIB (methods 3/4)** — W4-GAP-2. Pure-Rust DEFLATE. Rare on modern macOS.
- **M3f decmpfs LZFSE (methods 11/12)** — Pure-Rust LZFSE (FSE + LZ). Rare on modern macOS.
- **UUID/role resolver (W6-GAP-3)** — multi-volume open by UUID + volume role.

---

- **M3a — Volume catalog (dizin listeleme).** ✅ TAMAM (`8867a35`). volume
  omap + sanal `apfs_root_tree_oid` çözümü + değişken-KV (`kvloc_t`) katalog
  B-tree inişi (ara-düğüm çocukları **sanal**, volume omap'tan volume xid ile)
  + `j_key_t`/`j_obj_types` + DREC dizin listeleme (hashed/non-hashed key).
  Spec `apfs-spec:m3-confirmed`'e kilitli; hashed-key tetiği ampirik düzeltildi
  (CASE_INSENSITIVE 0x01 | NORMALIZATION_INSENSITIVE 0x08).
- **M3b — Dosya içeriği okuma.** ✅ TAMAM (`3b89810`). `inode.rs` (packed
  `j_inode_val` prefix, xfields@92, `round_up(x_size,8)`, `INO_EXT_TYPE_DSTREAM`
  → `j_dstream`) + `Catalog::read_file` (INODE bul → dstream.size → FILE_EXTENT
  `private_id` ile → logical_addr sırala → birleştir + truncate). Spec
  `apfs-spec:m3b-confirmed`; xfields hizalama NotebookLM+linux-apfs+apfs-fuse
  üç-kaynak çapraz. `apfs-large.img` 4096×'A' Rust + bağımsız Python + F-64.
- **M3c — xattr okuma.** ✅ TAMAM (`b1f8d75`). `xattr.rs` (`j_xattr_flags`
  STREAM 0x1/EMBEDDED 0x2/FS_OWNED 0x4, `XattrEntry`, `xattr_key_name`,
  `xattr_val`, `XattrDstream` 48B inline) + `Catalog::read_stream` (DRY
  primitive; `read_file` ince sarmalayıcı) + `list_xattrs`/`get_xattr`
  (embedded→ham xdata; stream→`XattrDstream`→`read_stream` `xattr_obj_id` ile).
  Reading B (48B inline `j_xattr_dstream_t`) **gerçek macOS baytıyla RE-AUDIT-
  kanıtlı** (`xdata_len=48`); spec `apfs-spec:m3c-confirmed`.
- **M3d — decmpfs çerçeve + LZVN.** ✅ TAMAM (`0e2611d`). `lzvn.rs` (Apple
  otoriter `apfs-spec:lzvn-ref` `lzvn_decode_base.c`'nin sadık güvenli-Rust
  portu; opcode tablosu birebir, 21-slot udef) + `decmpfs.rs` (16B `fpmc`
  başlık; method 1 raw / 7 LZVN-xattr / 8 LZVN-64k-rsrc-fork / 9 raw-inline-
  1bayt-önek — method-8 offset-array & method-9 ikisi de **gerçek macOS'tan
  reverse** (libfsapfs "TODO"/"Unknown"); 3/4/11/12 → `UnsupportedMethod`) + `Catalog::read_file_
  decompressed` (decmpfs varsa aç, yoksa `read_file`). Fixture: gerçek
  /usr/bin method-8 örnekleri kopyalanır (kernel-oracle). `apfs-spec:m3d-
  confirmed` + `apfs-spec:lzvn-ref`. **`ditto --hfsCompression` bu macOS'ta
  decmpfs üretmiyor** — fixture gerçek sistem örneği kopyalayarak çözüldü.
- **M3-RT — Gerçek-hedef diferansiyel.** ✅ TAMAM (`c697efd`). `Catalog::
  resolve_path` (yol→inode) + `tests/real_target_differential.rs` (env-var
  `APFS_RT_IMAGE`+`APFS_RT_MOUNT`; bizim okuyucu hızlı yerel imaj ⟷ macOS
  kernel `/Volumes/...` oracle byte-byte; veri-bütçeli; eksik imajda nazik
  skip; `assert fails==0 && files_ok>0` → false-green imkânsız) + `xtask
  rt-snapshot` (cihazı çözer, tek-seferlik `sudo dd`+env reçetesini basar).
  **Gerçek APFS USB'de 2931 `/usr/share/man` dosyası, failures=0, byte-exact
  vs macOS kernel.** M3-RT, hiçbir spec'te olmayan **decmpfs method 9**'u
  yakalattı → gerçek-veriden reverse edilip M3d'ye eklendi (`apfs-spec:m3d-
  confirmed` METHOD 9). Operatör reçetesi: `xtask rt-snapshot /Volumes/<v>`
  → `sudo dd ... bs=4m count=512` (bitene kadar; yavaş USB ~dk) → env ile koş.
- **M3e — decmpfs ZLIB (method 3/4) — SIRADAKİ.** Saf-Rust DEFLATE/RFC1951;
  method 3 xattr-içi, method 4 64k-chunked resource fork (libfsapfs ZLIB-rsrc
  footer `cmpf`@32). `apfs-spec:libfsapfs`+`apfs-spec:pdf-verbatim` referans;
  gerçek ZLIB örneği craft + (varsa) gerçek; md.6/gerçek-disk hakem. (Not:
  method 1 raw + method 9 marker-prefix raw M3d'de zaten çözüldü.)
- **M3f — decmpfs LZFSE (method 11/12).** Saf-Rust LZFSE (FSE + LZ); Apple
  açık-kaynak `lzfse` referans (yeni KB kaynağı eklenecek). Gerçek macOS LZFSE
  örnekleri + kernel oracle.
- **M4a — OS-bağımsız `FsView`.** ✅ TAMAM (`577d843`). `apfs` crate'inde
  `FsView<D:BlockDevice>` (open/lookup/read_dir/getattr/read + yol→inode &
  dizin cache, read-only ⇒ invalidasyon yok) + apfs-core `Inode.mode`@80 &
  `Catalog::stat`→`InodeStat{size,is_dir}` (ucuz getattr, içerik okumaz).
  winfsp-host/M4b'nin delege edeceği katman. Sentetik + gerçek macOS USB
  imajında kanıtlı. `apfs` sınırı: yalnız apfs-core+thiserror.
- **M4b — Windows `winfsp-host`.** ✅ TAMAM (`138ea5c`). Ham
  `\\.\PhysicalDriveN` `BlockDevice` (sektör-hizalı; `align`) + GPT parse
  (APFS container offset, tip GUID `7C3457EF-…`) + `OffsetBlockDevice` +
  oto `CachingBlockDevice` → `apfs::FsView` → `winfsp` callback'leri
  (GetSecurityByName/Open/Read/ReadDirectory/GetFileInfo/Close), her biri
  `catch_unwind`; `#[cfg(windows)]`/target-dep gate (macOS workspace temiz).
  **Gerçek APFS USB Windows 11'de 336 ms'de mount (NTFS-hızı), 25 dosya
  SHA-256 + 2 dizin FsView-oracle ile byte-exact, failures=0** (SSH ile
  operatör Windows kutusunda kanıtlı). Gerçek-donanım proof'u 4 gerçek bug
  yakalattı (hepsi düzeltildi) — durable notlar aşağıda.
  - **Durable M4b notları (KB kaybolursa buradan):** `winfsp` crate **0.13**
    (`0.13.0+winfsp-2.1`; plan 0.11 tahmin etti, derleyici hakem) +
    `windows 0.58`. `winfsp-rs` zorunlu **delay-load**: `build.rs` →
    `winfsp::build::winfsp_link_delayload()` + `[target.'cfg(windows)'.
    build-dependencies] winfsp`. `winfsp_init` delay-load resolver WinFsp
    DLL'ini bulamaz (SetDllDirectory yok sayılır) → `ensure_winfsp_dll()`
    `winfsp-x64.dll`'i exe yanına oto-kopyalar (registry/env'den; exe-dizini
    her LoadLibrary modunda aranır). `read_directory` **DirMarker** zorunlu
    onurlandırılır (`DirMarker::inner()->Option<&[u16]>`,
    `from_utf16_lossy`; yoksa büyük dizinde sonsuz enum → hang). `read`
    per-handle açılmış-içerik cache (read-only ⇒ değişmez; aksi halde her
    WinFsp chunk'ında tüm dosya yeniden açılır). `CachingBlockDevice`
    **64 KiB** blok / 1024 max (1 MiB soğuk-metadata read-amplification'ı
    mount'u yavaşlatıyordu). `mount_apfs` hataları `io_err` ile gerçek
    nedeni stderr'e (hata yutma yok).
  - **Sürücü-harfi görünürlüğü = M5 işi:** WinFsp sürücü-harfi oturum-bağlı;
    SSH/non-interactive oturumda `Z:` görünmez (NTFS'te de böyle). Proof
    dizin-bağlama (`C:\apfsmnt`, oturum-bağımsız reparse) ile yapıldı. M5
    servis kullanıcı oturumunda/launcher ile sürücü-harfini global açar.
- **Ürün vizyonu (kalıcı):** nihai hedef NTFS/FAT32 gibi sorunsuz — hem oku
  hem (M7) yaz/sil tüm yetkiler, hem aşırı hızlı hem her şeye karşı korumalı
  (crash/donma/güç-kaybı). Aşamalı: M1–M4 kurşungeçirmez okuma+mount (✅),
  M7 güvenli yazma (snapshot→yaz→tekrar-oku→checksum→rollback), M8
  sertleştirme. Hız: oto-cache; güvenlik: `#![forbid(unsafe_code)]` çekirdek
  + her callback `catch_unwind` + yazımda doğrula-önce.
- **M4b+ takip (açık):** (#44) oto read-ahead/LRU cache — TEMEL ATILDI
  (`CachingBlockDevice` 64 KiB, mount.rs+dump.rs); hız-ayarı tamamen
  otomatik, ileride aygıt-hızına adaptif yapılabilir. (#45) `apfs-mount
  --image <yol>` tam-disk imaj modu — `FileBlockDevice` + aygıttan-bağımsız
  GPT zaten var; CLI bayrağı + autodetect eklenecek (adli/imaj akışı).
  - **Oto boş sürücü-harfi** ✅ (`64e0a78`): `--mount` verilmezse
    `drive::first_free_drive_letter(GetLogicalDrives)` ilk boş harfi
    seçer (Z→D, A/B/C atlanır); Z: çakışma riski yok, gerçek-USB'de
    kanıtlı. Saf mantık 6 macOS unit testi.
- **M5 — kullanıcı-oturumu auto-mount ajanı + tray.** ✅ TAMAM (`adba54b`).
  Karar (kullanıcı onaylı): "boot servisi" yerine **kullanıcı-oturumu ajanı**
  — WinFsp sürücü-harfi oturum-bağlı görünür (M4b bulgusu); ajan kullanıcı
  oturumunda çalışınca sürücü doğrudan görünür (NTFS gibi), session-0
  köprüsü/UAC yok. Tek binary `apfs-tray` (gizli pencere + WM_DEVICECHANGE
  → re-scan; saf `Supervisor` state-machine + watchdog; `tray-icon` menü:
  durum/Unmount All/Quit; HKCU autostart). APFS algılama winfsp-host
  (`PhysicalDrive`/`gpt`) yeniden kullanılır. Her APFS sürücü ayrı
  **out-of-process** `apfs-mount.exe --drive N --wait-for-kill` çocuğu
  (fault-izolasyon: bir mount çökmesi ajanı/tray'i etkilemez); **Windows
  Job Object KILL_ON_JOB_CLOSE** ⇒ ajan herhangi bir yolla ölse (quit/
  panic/force-kill) çocuk da ölür, sürücü orphan kalmaz (proof'ta yakalanan
  gerçek bug → düzeltildi). `apfs-service` crate skeleton kalır (boot-time
  Windows Servisi M5 kapsamı değil; gerekirse M8). **Gerçek APFS USB
  Windows 11'de oto-algılanıp Z:'ye mount edildi** (root `man`+macOS
  metadata = gerçek APFS); ajan quit → çocuk reaped + sürücü gitti; saf
  Supervisor bağımsız Python state-model + 7 test, gerçek-kutu kanıtlı.
  - **Durable M5 notları:** `windows 0.58` API: `RegisterClassExW`/
    `WNDCLASSEXW` (RegisterClassW `Win32_Graphics_Gdi` ister), `CreateWindowExW`
    parent ham `HWND_MESSAGE`, `HINSTANCE(hmodule.0)`, `RegCreateKeyExW`/
    `RegSetValueExW` reserved `u32`. `tray-icon 0.19` plana uydu (değişiklik
    yok). `apfs-tray.exe` yanına `apfs-mount.exe` paketlenmeli (M6).
  - **M5+ nitler (bloke değil, #49 / M8):** spawn→job-assign mikro-yarış
    (CREATE_SUSPENDED+assign+ResumeThread ile kapat); Job HANDLE için
    Drop/CloseHandle RAII; `kill_all`'da sınırlı wait.
- **M6 — Paketleme + robustness pass.** ✅ TAMAM (`3f229b0`). WiX v3
  Burn **tek-installer** `apfs-explorer-setup.exe`: RegistrySearch ile
  WinFsp varsa atla yoksa kur (`winfsp-2.1.25156.msi`, MsiPackage,
  Permanent) → ürün MSI (`apfs-tray.exe`+`apfs-mount.exe` Program Files +
  HKCU Run autostart). `xtask package` sürücüsü. Kutuda sessiz
  install/uninstall temiz kanıtlı (WinFsp korunur). **Robustness S1–S5
  gerçek-USB'de HEPSİ PASS:** ani-USB-çekme / ajan-zorla-kill /
  hızlı-replug×5 / yavaş-cihaz / APFS-değil → panik/BSOD/hang/orphan YOK,
  temiz hata propagasyonu (S5 `mount_apfs: find_apfs_container failed:
  NoApfsPartition`, exit 1). Saf `installer_version` Python-cross-check'li
  5 macOS testi.
  - **Durable M6 notları:** WiX **v3.14.1.8722** kutuda (`candle/light`
    PATH'te: `C:\Program Files (x86)\WiX Toolset v3.14\bin`); **cargo-wix
    KULLANILMADI** (wix/ dizinini crate-relative arar, repo-root layout
    uyumsuz) → `candle -arch x64 -ext WixBalExtension/WixUtilExtension
    -dVersion=/-dTrayExe=/-dMountExe= … + light` doğrudan. WinFsp
    2.1.25156 MSI olarak chained. Session-0 SSH: `apfs-tray` (tray GUI)
    interaktif session-1 ister (OS gerçeği, bug değil — gerçek logon
    zaten session-1, M5 ajanı interaktif yolda kanıtlı); robustness
    WinFsp-host crash-yüzeyi `apfs-mount.exe`'de koşuldu (doğru fault
    sınırı).
  - **M6+ nitler (bloke değil, M7 cilası):** bundle version
    `-dVersion=0.0.0.0` sabit (installer_version'dan bağla); S3 sürücü-harf
    serbest-bırakma assert'i (sadece orphan-proc kontrol ediliyor).
- **M7a/b/c/d + Faz A1 — apfs-write (scratch + real-USB).** ✅ TAMAM.
  COW txn engine + create_file + write_file + unlink + rename + mkdir +
  catalog B-tree split (3+ level) + nested dirs + multi-file. Scratch-
  decisive + real-USB write GREEN (32GB USB).
- **M7b-RT — Snapshot reclaim subsystem.** ✅ TAMAM (`6e32928`, tag
  `checkpoint/m7b-rt-canonical`). Subtasks A (single-omap), B (omap
  retention), D1-D3 (sm_fq subsystem), E (canonical Apple model:
  two-xid + empty extref + frozen-zero), D4 partial (drain wiring +
  no-op gate; full gating accessor next). C deferred (volume-level
  fsroot deletion, not file unlink). Reference repos integrated:
  linux-apfs-rw (write authority), apfs-fuse, libfsapfs, apfsprogs,
  go-apfs. CLAUDE.md md.6 refined to "≥ 2 local-repo cross-checks
  before NotebookLM-ask". Three new locked specs:
  `apfs-spec:two-xid-snapshot`, `apfs-spec:snapshot-extref-empty-tree`,
  `apfs-spec:frozen-vsb-zeros`, `apfs-spec:fs-alloc-count-semantics`.
  Closeout: `docs/superpowers/specs/2026-05-25-m7b-rt-CLOSEOUT.md`.
- **M7-WinFsp — Windows write wiring (NTFS/FAT32 parite blokeri).**
  YÜRÜRLÜKTE. Plan: Steps A→E, branch `feat/m7-winfsp-write`.
  - **Step A** PhysicalDrive write support: open_rw + WritableBlockDevice
    impl + is_writable() guard. ✅ DONE (`b637454`).
  - **Step B** CLI --writable flag + read_only_volume gating. ✅ DONE
    (`b637454`). Windows release build PASS verified end-to-end:
    OSFMount read-only loopback correctly rejects open_rw with
    E_INVALIDARG — proves --writable wires through to CreateFileW.
  - **Step C** ApfsFs&lt;D&gt; generic + writable FsView. ✅ DONE (`48ea740`).
    CachingBlockDevice + OffsetBlockDevice forward WritableBlockDevice.
    Cache invalidates overlapping read-ahead blocks AFTER inner write
    succeeds. ApfsFs is now generic over D: WritableBlockDevice + Send
    + 'static. Windows release build PASS (2s) verified end-to-end.
    --writable mount succeeds; write callbacks default to STATUS_INVALID
    _DEVICE_REQUEST until Step D.
  - **Step D** WinFsp write callbacks bound to apfs-write API.
    UNLINK PROVEN end-to-end against macOS oracle ✓ (commit 2439722).
    CREATE wired but dispatch debug WIP.
    
    **UNLINK proof**: Windows `[System.IO.File]::Delete` →
    `open(DELETE 0x10080)` → `set_delete name="\\BIG.DAT"` →
    `cleanup flags=0x21` → `apfs_write::unlink` → commit + FsView
    reopen → Windows listing: only `.fseventsd` → macOS hdiutil
    attach + ls: `big.dat present? False`. THREE LOCKS unlocked it:
    (1) VolumeParams memfs-mirror flags (post_cleanup_when_modified
    _only + pass_query_directory_filename + flush_and_purge_on_cleanup);
    (2) FILE_ATTRIBUTE_NORMAL (not READONLY) on regular files;
    (3) cleanup canonical-case lookup via read_dir + eq_ignore_ascii
    _case before apfs_write::unlink.
    
    **CREATE PROVEN end-to-end** ✓ (commit 863e0dc). Unlock keys:
    persistent_acls(true) VolumeParams flag, u32 args (winfsp_sys
    types not re-exported), FILE_DIRECTORY_FILE bit routing.
    Windows cmd echo redirect / New-Item dispatch reach our
    apfs_write::create_file. macOS oracle: hello.txt visible.

    **WRITE wired + corruption WIP** (commit dc5f59f). Handle.write
    _buf accumulates chunks (seeded with existing on-disk content
    via FsView::read). Cleanup-non-delete flushes via TWO separate
    Transactions: unlink + write_file (apfs_write::write_file
    rejects existing names in vsb_raw; staged unlink within one
    tx is invisible until commit). End-to-end dispatch fires
    correctly (write callback hit, cleanup flush triggered). But
    the SECOND commit's FsView reopen fails with Container BadMagic
    (expected=11 NXSB, found=2 BTREE) — multiple writable commits
    against an OSFMount-backed image corrupt the container in our
    with_writable pattern. Root cause is suspected: device read
    position desync after commit OR an unflushed buffer in the
    OSFMount virtual disk layer between commits.
    
    Reference: studied SnowflakePowered/winfsp-rs/filesystems/memfs
    -winfsp-rs and ntptfs-winfsp-rs Rust binding examples. Memfs
    writes are INSTANT to in-memory state (no flush deferral); for
    a disk-COW filesystem the natural equivalent is an
    `apfs_write::overwrite_existing_file(ino, content)` API that
    rewrites an existing inode in a single transaction (truncate
    + new file_extent + extentref COW), rather than the unlink +
    write_file dance. Tracked as next-session apfs-write extension.
    
    **CREATE dispatch DEPRECATED debug note**: create callback added
    with apfs_write::create_file / mkdir fork (FILE_DIRECTORY_FILE
    bit). Compiles + binary mounts. Windows New-Item /
    cmd echo redirect / Win32 CreateFile(CREATE_ALWAYS) all return
    ERROR_PATH_NOT_FOUND (3) BEFORE reaching our create callback
    (STDERR shows no create log). Open callback still hit for
    listing/getattr. WinFsp seems to require additional protocol
    handshake for the create path. Hypotheses next session:
    OpenFileInfo.normalized_name needs to be set in
    get_security_by_name response (memfs sets it for missing files
    so Create yolu açılır); or the "post_disposition_when_necessary
    _only" flag (Rust binding'de görünmüyor — raw access gerekebilir);
    or directory mount vs drive-letter mount fark. Mevcut state:
    - apfs::FsView::read_vsb_omap_raw, Transaction::abort, ApfsFs::
      with_writable helper, set_delete override, cleanup with
      FspCleanupDelete flag, unlink delegation — hepsi kod düzeyinde
      land oldu (commits b637454, 48ea740, fa12ed6, 1d9d187).
    - Windows SSH-Box yeni binary build PASS. Writable mount running.
    - **Açık problem**: Windows `del` komutu CreateFile dispatch'i bizim
      open callback'imize ulaşmıyor (cleanup logları sadece 0x20
      SetLastAccessTime flag'i ile geliyor, hiç delete-bit yok;
      set_delete hiç tetiklenmiyor). `del exit=0 + "Could Not Find"`
      görünür — Windows OS layer'da bir cache/dispatch farkı.
    - `flush_and_purge_on_cleanup(true)` VolumeParams flag'i denendi,
      etkilemedi.
    Sıradaki tur hipotezleri (sırayla denenecek):
    - `post_cleanup_when_modified_only(false)` + `pass_query_directory
      _filename(true)` kombinasyonu (notification + query flow).
    - Open callback `OpenFileInfo.set_granted_access`/`set_disposition`
      veya `set_attributes` — Windows DELETE_ON_CLOSE flag'i için
      explicit acknowledge gerekebilir.
    - Mount path olarak directory yerine drive letter — reparse point
      üzerinde dispatch farklı dağılıyor olabilir.
    - WinFsp documentation `WinFsp-NTFS-Compatibility.md` review:
      DELETE protocol prerequisites.
    - `FsView::into_dev` ekledim (consume → device döner, cache drop).
    - ApfsFs içi `Mutex&lt;Option&lt;FsView&lt;D&gt;&gt;&gt;`'a sarıldı; read
      callbacks `as_mut().expect()` ile Option-aware. Windows release
      build PASS (2s) hala başarılı, mount + read regressionsız.
    Kalan iş (gerçek write implementation):
    - `apfs::FsView::read_vsb_omap_raw(&mut self)` — device-generic
      helper (scratch testindeki path-bağlı versiyonu portla).
    - Write callbacks tek tek: Create → Write (per-handle buffered) →
      Cleanup-with-FspCleanupDelete → Rename → Overwrite → SetBasicInfo
      → SetFileSize. Her biri: lock → take_dev → Transaction::begin →
      apfs_write API → commit → FsView::open(dev_back). catch_unwind
      ile sarmalı.
    - Per-handle content buffer pattern (WinFsp write callback'i
      offset+buf alıyor; apfs-write `write_file` parent_ino+name+
      tam_content alıyor — Cleanup'ta tek atomic flush).
  - **Step E** SSH-Box proof: OSFMount writable loopback + apfs-mount
    --writable + write file via Windows + macOS hdiutil oracle SHA-256
    compare. PENDING.
  - Bundle: #45 (--image mode, apfs-core FileBlockDevice native via
    apfs-mount.exe), #49 (supervisor nits) folded into Steps C/D.
- **M7b-RT v2 polish (deferred).** Two remaining
  `repro_overalloc_snapshot_pinned` warnings (both tracking, not
  corruption): (1) `apfs_fs_alloc_count` drift in write/unlink
  manual deltas — fix by making `txn.alloc_block`/`txn.free_block`
  auto-bump VSB ±1 (mechanical refactor); (2) "Insert missing
  physical extent" — investigate after (1) (likely shared root cause).
  Volume mounts and reads/writes correctly today; warnings are housekeeping.
- **M8 — Sertleştirme.** Robustness scenario matrix (#50: fuzz on
  FsView paths via cargo-fuzz, power-loss simulation, panic injection,
  kill-9 during mount/unmount, USB yank during read, sustained fuzz);
  FileVault/LZFSE write (v3+, NTFS-parite isteğe bağlı); decmpfs
  write. M6 task #52 robustness pass üzerine sistematik genişleme.

## Metodoloji (her milestone, yağsız — CLAUDE.md "Süreç (konsolide)")

1. Spec'i kaynaklardan topla+çapraz: `ctx_search(source:"apfs-spec:pdf-verbatim")`
   (pymupdf sayfa-bazlı — birincil verbatim) + `apfs-spec:libfsapfs` (libyal RE)
   + Context7 `/pradorocchi/apfs-filesystem-full-reference` + NotebookLM. Bunlar
   HİPOTEZ; **nihai hakem gerçek macOS diski**. Genuine belirsizlik/çelişki →
   **md.6**: NotebookLM sor-BEKLE-kilitle + ZORUNLU çift kanıt (kontrollü
   bilinen değer fixture'a fix + bağımsız reverse-decode). Apple'ı "yanlış"
   ilan etme; "doküman eksik/tutarsız, disk şunu gösteriyor" diye çerçevele.
2. Placeholder'sız plan yaz → `docs/superpowers/plans/YYYY-MM-DD-<ad>.md`,
   `main`'e commit, `feat/<ad>` branch.
3. Yürüt: görev başına implementer (TDD kırmızı→yeşil, self-test 1 kez) +
   **TEK** konsolide bağımsız review (spec+kalite+gate 1 kez; rapora güvenme).
4. Milestone sonu: 1 final review + 1 decisive Python çapraz-kontrol
   (gerçek imaj, Fletcher-64 doğrula).
5. squash-merge `feat/...`→main, branch sil, merge sonrası gate doğrula.
- Kurallar: `apfs-core` `#![forbid(unsafe_code)]`; üretimde unwrap/expect/panic/
  ham-index YOK; kod/yorum/commit İngilizce, açıklama/sohbet Türkçe; asla
  `git add -A`/`.`; gerçek-imaj TDD.

## Bilgi tabanı kaynakları (ctx_search source:"...")

- `apfs-spec:pdf-verbatim` — **BİRİNCİL** Apple PDF, pymupdf ile sayfa-bazlı
  (181 sayfa, `## PAGE N` başlıklı → tam verbatim lookup; FTS5 chunk satır
  kaçırma sorunu çözüldü). **KB ephemeral** — compact sonrası yeniden indeksle:
  `pip3 install --user pymupdf` → `ctx_execute(python)`: `import fitz; doc=
  fitz.open("docs/Apple-File-System-Reference.pdf")`; her sayfa `doc[i].
  get_text("text")`, footer'daki basılı sayfa no ile `## PAGE {n}` başlığı,
  `/tmp/apfs-spec-pages.md`'e yaz → `ctx_index(path, source:"apfs-spec:pdf-verbatim")`.
- `apfs-spec:libfsapfs` — libyal/libfsapfs bağımsız RE spec (asciidoc). Yeniden:
  `ctx_fetch_and_index("https://raw.githubusercontent.com/libyal/libfsapfs/main/documentation/Apple%20File%20System%20(APFS).asciidoc", "apfs-spec:libfsapfs")`.
- `apfs-spec:pdf` — eski pypdf çıkarımı (yedek; `apfs-spec:pdf-verbatim` yoksa).
- `apfs-spec:lzvn-ref` — Apple otoriter `lzfse/src/lzvn_decode_base.c` (LZVN
  decode algoritması). Yeniden: `ctx_fetch_and_index("https://raw.githubusercontent.com/lzfse/lzfse/master/src/lzvn_decode_base.c", "apfs-spec:lzvn-ref")`.
- `apfs-fuse:repo` — sgan81/apfs-fuse: bağımsız C++ APFS read-only FUSE
  sürücüsü (5. çapraz-kaynak; M4b FUSE-op→reader eşleme + GPT-container
  keşfi + decmpfs + FileVault/snapshot referansı). README yeniden:
  `ctx_fetch_and_index("https://raw.githubusercontent.com/sgan81/apfs-fuse/master/README.md", "apfs-fuse:repo")`.
  İlgili milestone'da on-demand ham kaynak (raw.githubusercontent.com/
  sgan81/apfs-fuse/master/<dosya>): M3e/M3f → `ApfsFile.cpp`/`Decmpfs*`,
  M4b → FUSE main/`Apfs*`, ileride FileVault → `Crypto*`.
- `winfsp:apiref` — WinFsp C API ref (M4b winfsp-host callback'leri). Yeniden:
  `ctx_fetch_and_index("https://winfsp.dev/doc/WinFsp-API-winfsp.h/", "winfsp:apiref")`.
- `apfs-spec:m1-confirmed` (obj_phys/nx_superblock offsetleri),
  `apfs-spec:fletcher64` (algoritma, 3-impl korobore),
  `apfs-spec:m1b-confirmed` (checkpoint ring + high-bit kuralı),
  `apfs-spec:m1c-confirmed` (omap_phys @48, btree node DATA_BASE=56/btree_info=40),
  `apfs-spec:m2-confirmed` (apfs_superblock tam offset tablosu, OBJECT_TYPE_FS
  0x0d, APFS_MAGIC 0x42535041, volume rolleri),
  `apfs-spec:m3-confirmed` (katalog tree linkage/JKey/drec; hashed-key
  RE-AUDIT: gerçek-imaj baytı + 4-kaynak),
  `apfs-spec:m3b-confirmed` (inode packed @92, xfields round_up(8), j_dstream),
  `apfs-spec:m3c-confirmed` (j_xattr; Reading B = 48B inline j_xattr_dstream_t,
  gerçek-imaj baytıyla RE-AUDIT-onaylı),
  `apfs-spec:m3d-confirmed` (decmpfs `fpmc` başlık/method'lar; method-8 rsrc
  offset-array layout gerçek-macOS reverse; LZVN port notları + fixture).
- Kaybolursa: bu offsetler ilgili commit'li planlarda (`docs/superpowers/plans/`)
  ve kod yorumlarında da yazılı — plan dosyaları durable spec'tir.

## Test verisi / komutlar

- `cargo run -p xtask -- gen-fixture` → `tests/fixtures/apfs-tiny.img` (16MB, gerçek).
- `cargo run -p xtask -- gen-fixture --large` → `apfs-large.img` (~256MB,
  16 dir×64 file dolu — M3 multi-node catalog testi için). İkisi de gitignored.
- Gate: `cargo test --workspace` · `cargo clippy --workspace --all-targets
  --all-features -- -D warnings` · `cargo fmt --all --check` (hepsi
  `--manifest-path /Users/eiaydin/Documents/projects/apfs-explorer/Cargo.toml`).
- Git kökü izole: `/Users/eiaydin/Documents/projects/apfs-explorer/.git` (sistem
  kökü ev dizini — dikkat). Apple PDF `docs/Apple-File-System-Reference.pdf`
  (gitignored, lokal).

## Compact sonrası DEVAM resmi adımları

1. `git -C /Users/eiaydin/Documents/projects/apfs-explorer log --oneline -16` ile
   son durumu gör (main'de M0–M5 + **M6** merge'li; M4b winfsp-host
   `138ea5c`, M5 auto-mount ajanı+tray `adba54b`, M6 tek-installer +
   robustness `3f229b0`; md.6+RE-AUDIT `c14a9f7`; apfs-fuse/winfsp:apiref
   `eb3715f`).
2. Bu dosya + `DESIGN.md` + `docs/superpowers/specs/` + `CLAUDE.md`'i oku.
3. KB boşsa `apfs-spec:pdf-verbatim` (pymupdf reçetesi yukarıda) + `apfs-spec:
   libfsapfs`'i yeniden indeksle. `apfs-spec:m3*-confirmed` kaybolursa ilgili
   `docs/superpowers/plans/2026-05-18-m3*.md` + kod yorumlarından türetilebilir;
   RE-AUDIT bulguları (hashed-key, xattr Reading B) o KB notlarında.
4. Sıradaki = **M7** (apfs-write v2). Güvenli yazma: snapshot → yaz →
   bağımsız tekrar-oku → checksum → tutmazsa rollback (yarıda kesilme/güç
   kaybında bölüm bozulmaz). `write` feature; `linux-apfs-rw` referans;
   gerçek-disk hakem + md.6 (on-disk yazma yapıları belirsizse). M0–M6
   (oku+mount+tek-installer+robustness) hazır temel. **Açık takipler:**
   #50 robustness — yazma-crash S-serisi M7'de, fuzz/power-loss/soak M8;
   #49 M5+ nitler + M6+ nitler (bundle version, S3 harf-assert) M7/M8
   cilası; #44 oto-cache adaptif tuning; #45 `apfs-mount --image` imaj modu.
   - **SSH Windows kutusu** (operatör): alias `apfswin`, anahtar
     `~/.ssh/apfs_win` (parola YOK; rotasyon önerildi). Repo bundle ile
     `C:\apfs\apfs-explorer`; reçete: `git bundle` → `scp` → box `git
     fetch/reset` → `cargo build/run -p winfsp-host`. Mount'u `Start-Process`
     ile (Win32_Process.Create farklı oturum → mount görünmez); script
     SONUNDA mount'u durdur (yoksa SSH kanalı asılır). Box: WinFsp 2.1 +
     Rust 1.91 MSVC + git kurulu.
   - **M3e/M3f ERTELENDİ (kullanıcı kararı, gerçek-veri gerekçeli):** modern
     macOS decmpfs = yalnız LZVN(8)+uncompressed(1/9); ZLIB(3/4) & LZFSE(11/12)
     5427-dosya sistem taramasında **hiç yok** → legacy/nadir, ürün-kritik
     değil. M4/M5 sonrası tamlık veya M8'e. (M3e=ZLIB DEFLATE/RFC1951;
     M3f=LZFSE Apple `lzfse` ref.)
   - **M3-RT gerçek-USB koşusu** hâlâ operatör-tetiklemeli: `xtask rt-snapshot
     /Volumes/<v>` → `sudo dd ... bs=4m count=512` (bitene kadar) →
     `APFS_RT_IMAGE=.. APFS_RT_MOUNT=.. cargo test ... --test
     real_target_differential`.
   - **Crate-hijyen notu (M8):** `apfs_core::container::ContainerError`'a
     `thiserror::Error` ekle → `apfs` `FsError` `#[from]` + düzgün `Display`.
