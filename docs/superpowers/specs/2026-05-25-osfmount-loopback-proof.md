# OSFMount loopback test workflow — USB-less APFS proof (2026-05-25)

## Result

Three-way byte-exact match on a 9000 byte file (`big.dat`) read through
the full Windows stack with **no physical USB** required:

| Source | SHA-256 |
|---|---|
| Rust deterministic formula `byte[i] = i % 251` (test source of truth) | `4b81efbd205e7fb4e42bc0d72d9d7413642298735289d35a74c1755883bcc45c` |
| Windows `apfs-mount.exe` reading via OSFMount loopback | matches |
| macOS kernel APFS reader (oracle, hdiutil-mounted same image) | matches |

This unlocks Windows read+(future)write verification on any developer
machine without an APFS-formatted USB drive — just a scratch image
produced by `cargo test -p apfs-write` and an OSFMount loopback.

## The setup (reproducible recipe)

### 1. On macOS — produce a scratch image
```
APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_scratch \
  decisive_write_multiblock_on_scratch -- --test-threads=1
# Leaves /tmp/apfs-scratch-m7a.img (64 MiB, APFS-in-GPT) with big.dat (9000 B)
```

### 2. Copy to Windows box
```
scp /tmp/apfs-scratch-m7a.img user@win-box:C:/Users/user/Documents/
```

### 3. On Windows — loopback via OSFMount
- Tool: PassMark OSFMount (free): https://www.osforensics.com/tools/mount-disk-images.html
- Step 1: Disk image file (.img, ...) → select the .img
- Step 2: Mount entire image as virtual disk
- Step 3: defaults
- Step 4: Read-only drive ✓, Drive emulation = **Physical Disk Emulation**
- Result: virtual disk appears as `\\.\PhysicalDriveN` (the next available N)

### 4. Mount with apfs-mount.exe
```
apfs-mount.exe --drive <N> --mount C:\apfsmnt --wait-for-kill
```

Notes:
- Use a **directory** mount point (e.g. `C:\apfsmnt`), not a drive letter,
  for session-independent reparse-point binding (drive letters are
  per-session under WinFsp; invisible from SSH/service contexts).
- The directory must NOT exist at mount time — WinFsp creates it.
- Run from an elevated shell (raw `\\.\PhysicalDriveN` needs Administrator).

### 5. Verify
```powershell
Get-ChildItem C:\apfsmnt
$bytes = [System.IO.File]::ReadAllBytes("C:\apfsmnt\big.dat")
$sha = [BitConverter]::ToString(
  [System.Security.Cryptography.SHA256]::Create().ComputeHash($bytes)
).Replace("-","").ToLower()
Write-Host "len=$($bytes.Length) sha256=$sha"
```

## Why this matters

The proof demonstrates the full read path works end-to-end on a real
Windows host without a physical APFS USB:
- OSFMount provides a `\\.\PhysicalDriveN` virtual disk
- apfs-mount.exe (M4b stack): winraw PhysicalDrive → GPT parse →
  OffsetBlockDevice → CachingBlockDevice → apfs::FsView → WinFsp
  callbacks (read-only)
- Pure-Rust APFS reader returns the exact bytes the kernel APFS writer
  produced

Future work — **winfsp write wiring** — can be validated identically: a
fresh image, OSFMount loopback, apfs-mount.exe with write callbacks
enabled, write a file via Windows, then independently re-read via macOS
kernel oracle.

## Caveats

- Release build (`target/release/apfs-mount.exe`) was unreliable in the
  immediate post-attach window (process exited silently after one or
  two reads). The debug build (`target/debug/apfs-mount.exe`) was
  stable through full read + SHA-256. Suspect: release-build optimizer
  + delay-load DLL race in the first WinFsp callbacks. Worth a dedicated
  triage when winfsp write wiring lands.
- The same `\\.\PhysicalDriveN` cannot be opened by two processes at
  once. Stop any previous `apfs-mount.exe` before remounting.
- OSFMount's "Read-only drive" checkbox protects the image file from
  accidental writes — leave it on for read-mode tests; turn it off for
  write tests once write callbacks are wired.

## Status

This is the user-test path that replaces "you need an APFS USB to
verify Windows mount". `apfs-mount.exe --image <path>` native support
(task #45) lands with winfsp write wiring; until then, OSFMount is the
zero-code-change workaround.
