# M7 Real-Disk Proof — #78 Windows Mount + Write Roundtrip

**Date:** 2026-05-26  
**Task:** #78 — Windows WinFsp mount + write roundtrip on real APFS image  
**Status:** PASS

## Setup

- Image: `E:\dev\apfs-test.img` (64 MiB, copied from `C:\Users\enesi\Documents\apfs-scratch-m7a.img`)
- Binary: `E:\dev\cargo-target\release\apfs-mount.exe` (built from main, release)
- Mount point: `Z:` (WinFsp, same-session, `--wait-for-kill`)
- Windows host: `apfswin` (Rust 1.91.1, WinFsp installed)

## Build Fixes Applied

Three Windows-only compile errors fixed in `crates/winfsp-host/src/mount.rs`:

1. `io_err` — changed `fn` to `pub(crate) fn` (used by `image_mount.rs` via `crate::mount`)
2. `ensure_winfsp_dll` — changed `fn` to `pub(crate) fn` (same reason)
3. `decode_symlink_reparse` — was incorrectly placed inside `FileSystemContext for ApfsFs<D>` trait impl (not a trait member); moved to inherent `impl<D> ApfsFs<D>` block

Build result: `Finished release profile` with 0 errors, 4 warnings (unused vars, `private_interfaces`).

## Roundtrip Evidence

```
MOUNTED_OK
READ=hello apfs windows roundtrip 2026-05-26T09:58:31.8530976+03:00
ROUNDTRIP_OK
```

## Oracle Verify (macOS hdiutil)

Image pulled back to macOS via `scp`:

```
$ hdiutil attach -readonly /tmp/apfs-test-after.img
/dev/disk5s1  41504653-...  /Volumes/APFSWRITE

$ cat /Volumes/APFSWRITE/hello.txt
hello apfs windows roundtrip 2026-05-26T09:58:31.8530976+03:00
```

macOS hdiutil independently confirmed the written file. Roundtrip verified end-to-end.

## Conclusion

#78 DONE. Windows WinFsp mount + write + macOS oracle read all pass on a real 64 MiB APFS image.
