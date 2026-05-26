# M7-WinFsp RESUME (2026-05-25)

## State

- Branch: `feat/m7-winfsp-write`
- HEAD: `a9a7db0`
- Tag (next): `checkpoint/m7-winfsp-create-delete-proven`
- Backups:
  - macOS: `/tmp/apfs-scratch-m7a-backup.img`
  - Windows: `C:\Users\enesi\Documents\apfs-scratch-m7a-backup.img`
- Live test image (corrupted from latest write attempt): restore from
  Windows backup before next run.

## Proven (end-to-end, Windows → macOS oracle)

| Capability | Commit | Proof |
|---|---|---|
| Mount RO | M4b era | OSFMount loopback + 25-file SHA-256 |
| Mount RW | b637454, 48ea740 | `apfs-mount.exe --writable` succeeds |
| Delete file | 2439722 | Win `[IO]::Delete` → macOS: `big.dat` gone |
| Create file | 863e0dc | Win `echo > hello.txt` → macOS: regular 0-byte file |

## Step D Write — KORUNAN durum

`dc5f59f` commit'i Write callback'i wires; dispatch fires end-to-end:
- `write: (handle.write_buf accumulate, seeded from FsView::read)`
- `cleanup: flags=0xf2 delete=false` → flush branch tetiklendi
- Cleanup-flush iki tx: `with_writable(unlink) + with_writable(write_file)`

**Açık bug**: ikinci `with_writable`'ın commit sonrası FsView reopen
`Container BadMagic { expected: 11 NXSB, found: 2 BTREE }`. Image
corrupted.

## Önemli ayrım — scratch testler iki-tx pattern KULLANIYOR + GREEN

`crates/apfs-write/tests/snapshot_scratch.rs`:
- Line 312: `Transaction::begin(dev) … create_snapshot … commit` (tx1)
- Line 347: yeni image open, `Transaction::begin(dev) … delete_snapshot
  … commit` (tx2)
- decisive_create_delete_snapshot_on_scratch: GREEN ✓

- Line 549: `Transaction::begin … write_file … commit` (tx1)
- Line 561: yeni image open, `Transaction::begin … unlink … commit` (tx2)
- decisive_write_then_unlink_on_scratch: GREEN ✓

**Yani apfs-write iki-tx sequential commit AKIYLA SAĞLAM. Sorun
winfsp-host with_writable Windows side.** Farklılıklar:

| Scratch test | winfsp-host with_writable |
|---|---|
| `FileBlockDeviceRw::open_rw(path)` her tx için fresh | `view.into_dev()` aynı `CachingBlockDevice<OffsetBlockDevice<PhysicalDrive>>` |
| Her tx arası dev TAMAMEN drop + yeni open | Device DROPPlanmadan reuse |
| Cache yok (file-backed) | CachingBlockDevice 64KiB block cache var |
| FILE_FLAG_NO_BUFFERING yok | PhysicalDrive::open_rw FILE_FLAG_WRITE_THROUGH (no buffering kapalı) |

## Hipotezler (next-session debug)

1. **Cache stale**: 1. tx commit'ten sonra CachingBlockDevice'in 64KiB
   bloklarından bazıları YAZILMAMIŞ olabilir → 2. tx Container::open
   ESKI nx_superblock okur (write_through doğru, ama cache hit eski).
   Test: with_writable içinde commit sonrası CachingBlockDevice cache
   tamamen invalidate et (blocks.clear()), sonra FsView::open.

2. **OSFMount write ordering**: PhysicalDrive::write_at FILE_FLAG_WRITE
   _THROUGH ile çağrılıyor ama OSFMount'un virtual disk emulation katmanı
   write'ları sıra dışı kalıcı yapabilir. Eğer 1. tx'in son yazısı
   (nx_superblock at xid_n) commit edilmeden 2. tx'in ilk yazısı diske
   gitse, ortada kaos. Test: scratch'te aynı pattern'i çalıştır.

3. **Sequential txn xid bumping**: 1. tx commit nx.next_xid=N+1 bumps.
   2. tx Transaction::begin nx.next_xid okur (=N+1) + kendi xid'ini
   N+1 alır + commit'te N+2 bumps. Container superblock paylaşılan
   blok — 1. tx'in commit blok'u 2. tx'in begin'i tarafından
   read_at'lanır. Cache hit ise stale değer.

## Önerilen sıra (sonraki tur)

1. **Cache invalidation testi**: with_writable içinde commit + FsView
   reopen arasına `dev.blocks.clear()` ekle (CachingBlockDevice'a public
   `invalidate_all` ekle). Eğer fix ise → hipotez 1 doğrulandı.
2. Eğer hala fail: **apfs-write::overwrite_existing_file** API ekle —
   tek tx içinde mevcut inode'u truncate + rewrite (unlink+write_file
   yerine). 200-300 satır kod + scratch test ile valide.
3. Sonra Windows test + oracle proof.

## Reference (sshfs-win + winfsp-rs memfs)

- Memfs Rust binding (`SnowflakePowered/winfsp-rs/filesystems/memfs-
  winfsp-rs`): write callback ANLIK in-memory state günceller, no
  deferred flush. Bizim disk-COW eşdeğeri = single-tx overwrite API.
- VolumeParams flags ALL three confirmed memfs-mirror:
  post_cleanup_when_modified_only, pass_query_directory_filename,
  persistent_acls.

## Global mount via WinFsp Launcher (visible across all sessions)

`host.mount(L"F:")` without administrative elevation creates a
**user-mode mount** that only the session that called it can see.
SSH session and an interactive RDP/console session are different
session IDs, so the drive letter never appears alongside C/D/E from
the user's perspective.

The production fix is the WinFsp **Launcher** service (memfs and
sshfs-win both use it). Launcher runs as Local System, so when it
spawns the FS process the mount is registered in the global
namespace and every session sees the drive letter.

One-time registry registration (M6 installer should do this):

```
HKLM\Software\WOW6432Node\WinFsp\Services\apfs-mount
  Executable   = REG_SZ : <full path to apfs-mount.exe>
  CommandLine  = REG_SZ : --drive %1 --mount %2 --writable --wait-for-kill
  Security     = REG_SZ : D:P(A;;RPWPLC;;;WD)      # everyone may start it
  JobControl   = REG_DWORD : 1
  Credentials  = REG_DWORD : 0
```

Mount via launcher (replaces direct `apfs-mount.exe` invocation):

```
"C:\Program Files (x86)\WinFsp\bin\launchctl-x64.exe" start apfs-mount inst1 <drive#> "<letter>:"
"C:\Program Files (x86)\WinFsp\bin\launchctl-x64.exe" stop  apfs-mount inst1
```

PROVEN visible from any session:

```
DeviceID FileSystem VolumeName       <-- Get-WmiObject Win32_LogicalDisk
C:       NTFS
D:       NTFS       Yerel Disk
E:       NTFS       Yeni Birim
F:       APFS       APFS             <-- our mount
```

TODO M6: have the WiX installer (`crates/installer`) emit this
registry block during install and the matching `RemoveRegistryKey`
during uninstall. apfs-tray autostart should drive launcher start
when a USB is detected.

## Quick resume commands

```bash
# macOS
git checkout feat/m7-winfsp-write
git log --oneline -10
cargo check -p winfsp-host  # macOS PASS

# Windows backup restore + remount (when needed)
scp /tmp/apfs-scratch-m7a-backup.img \
    apfswin:C:/Users/enesi/Documents/apfs-scratch-m7a.img
# Windows: OSFMount detach + remount writable + apfs-mount.exe --writable
```

## M7-Polish — Phase 1/2/3 PROVEN end-to-end

Three M7-Polish tags on top of the M7-WinFsp base:

- `checkpoint/m7-polish-phase1-proven` — set_file_size(N>0),
  set_volume_label, cross-directory move, set_inode_attrs (persistent
  mtime + READONLY bit), FILE_ATTRIBUTE_READONLY round-trip,
  case-insensitive lowercase fallback for Windows uppercase paths.
- `checkpoint/m7-polish-phase2-proven` — robustness 8/8: multi-file
  write (50), deep nested dirs (8 levels), 1 MiB byte-exact roundtrip,
  recursive copy + recursive delete (cmd rmdir /s), UTF-8 (Chinese
  合同 + Arabic مستند + Russian Документ + emoji), atomic save
  (Notepad++/VSCode pattern), edge filenames (space + .hidden). One
  bug killed during this phase:
  - FILE_DELETE_ON_CLOSE handle bit (PowerShell Remove-Item + cmd del
    take this path; we honour both that and FspCleanupDelete in flags)
  - order-based read_directory marker resume (the equality-only check
    dropped siblings when the marker entry was deleted between
    QueryDirectoryFile calls — the underlying cause of "directory
    not empty" on cmd rmdir /s)
- `checkpoint/m7-polish-phase3-benchmark` — perf measured + M8
  optimisation roadmap. Read 294 MB/s @ 16 MiB (28% of NTFS, fine).
  Write 7.4 MB/s @ 16 MiB (1/570 of NTFS, CPU-bound on per-block
  spaceman bit-flip + per-cleanup transaction commit — four targeted
  M8 optimisations identified in `docs/m7-polish-benchmark.md`).

## What works end-to-end on Windows today

The mount appears in Bilgisayar / Explorer alongside C/D/E with the
correct size and label. From any session (including SSH and RDP) you
can:

- create / write / read / delete regular files (Set-Content,
  Get-Content, Remove-Item, type, echo > F:\\... > )
- rename in place (Rename-Item, cmd ren)
- move across directories (Move-Item)
- mkdir at any depth (New-Item -ItemType Directory; rmdir /s)
- recursive copy (Copy-Item -Recurse) and recursive delete
  (cmd rmdir /s /q, Remove-Item -Recurse -Force)
- truncate / extend files (Notepad++ save, [IO.File].SetLength,
  any editor atomic save pattern)
- toggle read-only attribute (Set-ItemProperty IsReadOnly $true/$false)
- volume label (cmd `label F: MyAPFS`)
- UTF-8 filenames (Chinese, Arabic, Russian, Turkish, emoji)
- deep nested directories (8+ levels)
- 1 MiB+ files byte-exact (16 MiB tested)

macOS hdiutil attach + fsck_apfs reports CLEAN container + volume
across all of the above.

## Open subtasks

1. ~~Cache invalidation hypothesis test~~ ✅ landed `0c4162e`
   (defense-in-depth; not the root cause)
2. ~~apfs-write::overwrite_existing_file API~~ ✅ landed `03702ef`
   (cargo check + clippy + test --no-run PASS; macOS scratch test skipped
   pending sudo chown on `/tmp/apfs-scratch-m7a.img`)
3. **Step D remaining**: Rename, Overwrite, SetBasicInfo, SetFileSize
4. **Step E**: WRITE end-to-end — **PROVEN** (tag `checkpoint/m7-winfsp-step-e-proven`)
   - Windows `apfs-mount.exe --drive 3 --mount F: --writable` mounts OK
   - Windows `dir F:\` lists big.dat + .fseventsd; `echo > F:\winwrite.txt`
     succeeds; `type F:\winwrite.txt` reads back exact bytes
   - Image SHA propagates Windows → macOS bit-identical
   - macOS hdiutil attach succeeds; volume mounts as `/Volumes/APFSWRITE …`
   - macOS `ls -lia` → `21 -rw-r--r--  1 eiaydin  staff  27 … winwrite.txt`
   - macOS `cat winwrite.txt` → `hello-from-windows-winfsp` (byte-exact)
   - macOS `stat` → nlink=1 mode=-rw-r--r-- uid=501 gid=20 size=27
   - `fsck_apfs -n` → **container CLEAN + volume CLEAN**
   - Three bugs fixed along the way (each a separate commit):
     1. `feat(block_device): invalidate_read_cache trait hook` —
        defense-in-depth invalidation across the FS reopen boundary
     2. `feat: single-tx overwrite_existing_file replaces two-tx flush` —
        canonical linux-apfs-rw style in-place rewrite (no unlink/relink),
        plus winfsp-host cleanup-flush wired to it
     3. `fix: single alloc+stage for extref COW in overwrite_existing_file`
        — the free-path alloc + insert-path alloc leaked one block, fsck
        reported "overallocation"; consolidated to one alloc + one stage
     4. `feat: get_volume_info override in mount.rs` — without it,
        WinFsp's default returned STATUS_INVALID_DEVICE_REQUEST and `dir F:\`
        failed with "yanlış işlev / Incorrect function" before any
        ReadDirectory dispatch
     5. `fix(apfs-write): create_file inode nlink=1 (was 0, orphaned files)`
        — create_file delegated nchildren=0 to the union field which the
        APFS kernel driver reads as nlink for non-dir inodes; the file
        looked orphaned (open/stat/cat all ENOENT) even though fsck was happy
   - Earlier blocker (WinFsp drive-letter visible to SSH session only)
     resolved by adding `get_volume_info` override — WinFsp 0.13's default
     impl returns `STATUS_INVALID_DEVICE_REQUEST`, surfaces as "yanlış
     işlev"/"Incorrect function" on `dir F:\` before `ReadDirectory`
     dispatch. Override puts a 1 TiB nominal total / 512 GiB free + label
     "APFS" so the kernel completes the volume probe. Landed in commit
     after `03702ef`.
   - Live PROOF cycle (`feat/m7-winfsp-write` HEAD post-pivot):
     - Windows `dir F:\` → big.dat (9000) + .fseventsd OK
     - Windows `echo > winwrite.txt` → create dispatch + cleanup dispatch
       + single-tx `overwrite_existing_file` fires (NO BadMagic)
     - Windows `type F:\winwrite.txt` → exact bytes readback ✓
     - Image SHA: pre `C258B465...1D5F` → post `06C85ED0...576FD`
     - macOS hdiutil attach succeeds; `fsck_apfs -n` reports
       "the volume APFSWRITE … appears to be OK" (CLEAN)
     - macOS `ls -lia` shows `21 -rw-r--r--  0 eiaydin staff  27 …
       winwrite.txt` — file IS present, owner/group inherited, size
       exact, but **nlink column = 0** so `cat`/`stat` fail with
       "No such file or directory".
   - **Remaining defect**: macOS APFS driver reads winwrite.txt inode
     with `nlink == 0` even though `apfs_write::file::build_inode_val`
     writes `nlink_or_nchildren = old_nlink` (which `overwrite_existing_file`
     read from the old 0-byte file as 1 via `rd_u32(56)`). fsck does not
     flag it. Either (a) a different inode field shifts the kernel into
     "deleted" interpretation, (b) the DSTREAM xfield layout in the
     rebuilt inode is incompatible with the Apple driver's parse,
     (c) something else in the immediate post-cleanup write_generation
     / extra xfield path is missing.
   - **Next debug steps** (yarin / next session):
     1. Build a small read-only Rust binary using `apfs` (lib) crate to
        open `/tmp/apfs-postwrite.img` FsView and `read_raw_inode_value(21)`
        — dump the inode value bytes hex-grouped and decode every field.
        Compare side-by-side with the big.dat (inode=16) value bytes.
     2. The Python decoder `/tmp/decode_winwrite.py` (uncommitted, scratch)
        needs the nx_omap ephemeral OID lookup via the checkpoint data
        ring to resolve oid=217 to a real paddr. Apply that fix or use
        the Rust binary in (1) instead.
     3. Once the offending field is identified, patch
        `apfs_write::file::overwrite_existing_file` (and possibly
        `build_inode_val`) and re-run the same test loop.
   - **Workflow proven**: macOS commit/scp/Windows build/OSFMount restore
     + remount/apfs-mount.exe writable/Windows write/Ctrl-C/scp back/
     macOS attach + oracle works end-to-end and is reproducible in one
     SSH command. Image backup `C:\Users\enesi\Documents\
     apfs-scratch-m7a-backup.img` is the source of truth for each cycle.
5. **Squash-merge `feat/m7a-write-snapshot` → main** (IRREVERSIBLE,
   user OK required)
