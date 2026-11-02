# APFS Explorer for Windows - Design Document

> Language: **Pure Rust** · License: **GPLv3** · Project: `apfs-explorer`
> Details: `docs/refs/` · This file is the top-level summary.

A **pure-Rust** application that exposes the APFS volumes of a Mac disk (USB /
SSD) or a disk image as a normal drive letter (e.g. `Z:`) in File Explorer on
Windows 10/11, read-only by default and read-write on request.

**Goal: macOS parity for what a file system user does.** What works in File
Explorer on NTFS should work on APFS: reading, writing, renaming, deleting,
compressed files (zlib, LZVN, LZFSE, LZBITMAP), clones, symlinks, snapshots,
Finder flags, and leaving the volume in a state macOS keeps working with.

## Core decisions

- **Pure Rust, parser written from scratch.** No external C library.
  Memory safety matters most here, where untrusted disk data is parsed.
- **Sources: Apple's APFS specification**, checked against independent
  implementations (`linux-apfs-rw`, `apfsprogs`, `apfs-fuse`, `libfsapfs`) and,
  finally, against what macOS writes and accepts. Clean room: algorithms are
  checked, no code is copied.
- **WinFsp** for a real drive letter: a signed kernel driver, so end users
  never deal with test-signing or Secure Boot. No shell extension.
- **On-demand mounts through the WinFsp launcher.** The app asks the launcher
  (a LocalSystem service WinFsp installs) to start `apfs-mount.exe`; the drive
  is then visible to the user's Explorer without a UAC prompt per mount. There
  is no service of our own.
- **Copy-on-write writes, grouped.** Every change goes to new blocks and the
  checkpoint that points to them is written last, after a durability barrier,
  so an interrupted write leaves the previous checkpoint. Operations are
  grouped into one transaction that commits within about a second.
- **Crash safety is a hard requirement:** no `unwrap`/`panic` in library
  paths; everything returns `Result`; `catch_unwind` guards each WinFsp
  callback; `tracing` logs. Details: `docs/refs/error-handling-crash-safety.md`.

## Architecture (summary)

```
apfs-explorer-gui →(WinFsp launcher)→ winfsp-host (apfs-mount.exe)
    → apfs-write → apfs → apfs-core → \\.\PhysicalDriveN or an image
```

Crate responsibilities, data flow, write safety: `docs/refs/architecture.md`.
Testing: `docs/refs/testing.md`.

## Non-goals

| Feature | Reason |
|---|---|
| FileVault / encrypted volumes | Out of scope; encrypted volumes are refused |
| T2 / Apple Silicon internal disk hardware encryption | Key lives in the Secure Enclave - technically impossible |
| Sealed system volumes, Fusion Drive | Out of scope |
| APFS v1 (legacy format) | Rare; can be revisited later if needed |
