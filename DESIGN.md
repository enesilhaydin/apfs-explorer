# APFS Explorer for Windows - Design Document

> Status: **v3 - approved** · Date: 2026-05-17 · Language: **Pure Rust** · License: **GPLv3**
> Project: `apfs-explorer`
> Full spec: `docs/refs/`  · This file is the top-level summary.

A **pure-Rust** application that automatically detects APFS volumes when a Mac disk
(USB / SSD) is connected to a Windows 10/11 machine and exposes them as a normal
drive letter (e.g. `Z:`) in File Explorer. The user mounts nothing manually - the
volume appears on plug-in and disappears on removal.

**Goal: full macOS parity.** Everything that works in File Explorer on NTFS/FAT32
must work on APFS as well: read + write, ZLIB/LZVN/LZFSE decompression, FileVault
(software) decryption, snapshot reading, xattr + Finder metadata. "Read first →
write later" is a **build order**, not a feature cut - the writer is layered on top
of the reader; the end state is 100 % parity.

## Core decisions

- **Pure Rust, parser written from scratch.** No external C library (libfsapfs).
  Memory safety is most critical here, where untrusted disk data is parsed.
- **Spec source: Apple's APFS specification**.
  Consulting the full "Apple File System Reference" directly allows implementing
  areas that reference libraries skip - LZFSE, snapshots, FileVault - giving us
  broader coverage than a port would achieve.
- **WinFsp** for a real drive letter. Microsoft-signed kernel driver; end users
  never deal with test-signing or Secure Boot. No NSE/shell extension.
- **Service-first mount engine.** A Windows Service starts at boot, requires no
  UAC prompt, and mounts even when no user session is open (the answer to the
  "absolutely seamless" requirement). The tray app is a thin UI client, bundled
  with the WinFsp installer.
- **Write path** is a separate `write` feature flag with a verify-first architecture
  (snapshot → write → independent re-read → checksum → rollback on mismatch).
  linux-apfs-rw is used as a correctness *reference* only, not as copied code.
- **Crash safety is a hard requirement:** `unwrap`/`panic` are forbidden in library
  paths; everything returns `Result`; `catch_unwind` guards the WinFsp/service
  boundary; comprehensive `tracing` logging; service watchdog. Details:
  `docs/refs/error-handling-crash-safety.md`.

## Architecture (summary)

```
apfs-tray  →(IPC)→ apfs-service → winfsp-host → apfs → apfs-core → \\.\PhysicalDriveN
```

Crate responsibilities, data flow, write safety: `docs/refs/architecture.md`.
Testing/CI: `docs/refs/testing.md`.

## Permanent non-goals

| Feature | Reason |
|---|---|
| T2 / Apple Silicon internal disk hardware encryption | Key lives in the Secure Enclave - technically impossible |
| APFS v1 (legacy format) | Rare; can be revisited later if needed |

## Roadmap

M0 skeleton · M1 `apfs-core` · M2 `apfs` read (ZLIB/LZVN) · M3 LZFSE+FileVault+
snapshot · M4 `winfsp-host` read-only + real USB differential · M5 service+tray
auto-mount · M6 MSI packaging · M7 `apfs-write` (verify-first) · M8 hardening.

Full milestone table, risks, and open work: see the approved spec document.
