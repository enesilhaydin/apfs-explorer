# APFS Format Compatibility Matrix

This document describes which APFS on-disk features and version flags are
supported, partially supported, or explicitly out of scope for apfs-explorer.

Reference: Apple File System Reference (public PDF), APFS specification §4 and §6.

---

## Container Incompatibility Flags (`apfs_nx_superblock_t.nx_incompatible_features`)

| Flag | Hex | Meaning | Read | Write | Notes |
|------|-----|---------|------|-------|-------|
| `NX_INCOMPAT_VERSION1` | `0x01` | Format version 1 | Yes | Yes | Baseline |
| `NX_INCOMPAT_VERSION2` | `0x02` | Format version 2 | Yes | Yes | All modern images |
| `NX_INCOMPAT_FUSION` | `0x100` | Fusion Drive (SSD+HDD) | No | No | Non-goal |

---

## Volume Incompatibility Flags (`apfs_superblock_t.apfs_incompatible_features`)

| Flag | Hex | Meaning | Read | Write | Notes |
|------|-----|---------|------|-------|-------|
| `APFS_INCOMPAT_CASE_INSENSITIVE` | `0x01` | Case-insensitive filenames | Yes | Yes | Default macOS HFS+ upgrade path |
| `APFS_INCOMPAT_DATALESS_SNAPS` | `0x02` | Snapshots with no data | Yes | No | Read display only |
| `APFS_INCOMPAT_ENC_ROLLED` | `0x04` | Encryption rolling in progress | No | No | Non-goal (encryption) |
| `APFS_INCOMPAT_NORMALIZATION_INSENSITIVE` | `0x08` | Unicode normalization-insensitive | Yes | Yes | NFD/NFC fold on lookup |
| `APFS_INCOMPAT_INCOMPLETE_RESTORE` | `0x10` | Restore in progress | No | No | Non-goal |
| `APFS_INCOMPAT_SNAPSHOTS` | — | Snapshots enabled | Yes | Yes | Required for snapshot API |
| `APFS_INCOMPAT_SEALED_VOLUME` | — | Sealed volume (Signed System Volume) | No | No | Non-goal (SSV) |

---

## Volume Optional Features (`apfs_superblock_t.apfs_optional_features`)

| Flag | Meaning | Read | Write | Notes |
|------|---------|------|-------|-------|
| `APFS_FEATURE_DEFRAG_PRERELEASE` | Defrag pre-release marker | Yes (ignored) | Yes (preserved) | Not triggered |
| `APFS_FEATURE_HARDLINK_MAP_RECORDS` | Hardlink inode map | Yes | Yes | Required for correct inode counting |
| `APFS_FEATURE_DEFRAG` | Defrag enabled | Yes (ignored) | Yes (preserved) | apfs-explorer never defragments |
| `APFS_FEATURE_STRICTATIME` | Strict atime updates | Yes | Yes | atime updated on reads |
| `APFS_FEATURE_VOLGRP_SYSTEM_INO_SPACE` | Volume group inode space | Yes | Yes | Required for System/Data split |

---

## Encryption Support

| Feature | Status | Notes |
|---------|--------|-------|
| Unencrypted volumes | Supported | Default for images created without FileVault |
| FileVault (per-volume encryption) | Non-goal | Requires keybag + user credential unwrap |
| Per-file encryption (`APFS_INODE_IS_APFS_ENCRYPTED`) | Non-goal | Inode flag detected; access denied gracefully |
| Encryption rolling | Non-goal | `APFS_INCOMPAT_ENC_ROLLED` flag present → mount refused with clear error |

Attempting to mount an encrypted volume returns `Error::EncryptedVolume` rather than
silently producing garbage data.

---

## Compression (decmpfs)

| Algorithm | Meaning | Read | Notes |
|-----------|---------|------|-------|
| `ZLIB` (type 3/4) | Inline zlib | Yes | Most common, older images |
| `LZFSE` (type 7/8) | Apple LZFSE | Yes | macOS 10.11+ default |
| `LZ4` (type 11/12) | LZ4 block | Yes | Firmware/system volumes |
| `LZBITMAP` (type 9/10) | Sparse bitmap | Yes | System volume optimization |
| `ZSTD` (type 21/22) | Zstandard | Planned | Added in recent macOS; not yet wired |

Compressed files are transparently decompressed on read. Write path always
writes uncompressed data (no re-compression on modify).

---

## macOS Version Compatibility

Volumes created or last written by these macOS versions have been tested or
are expected to be readable:

| macOS Version | APFS Format Gen | Read | Write | Notes |
|---------------|-----------------|------|-------|-------|
| macOS 11 Big Sur | v2 | Yes | Yes | |
| macOS 12 Monterey | v2 | Yes | Yes | |
| macOS 13 Ventura | v2 | Yes | Yes | |
| macOS 14 Sonoma | v2 | Yes | Yes | |
| macOS 15 Sequoia | v2 | Yes | Yes | Verified fixture source |
| macOS 26 (dev beta) | v2 | Yes | Yes | Library tests pass |

No format version 3 has been observed in the wild as of this writing.

---

## diskutil / hdiutil / asr Compatibility

| Tool + Operation | Expected Outcome |
|-----------------|-----------------|
| `hdiutil create -fs APFS` | Produces readable image (primary fixture source) |
| `hdiutil attach` (macOS) | macOS mounts it; apfs-explorer can also read same image |
| `diskutil apfs listContainers` | Output matches volumes reported by `apfs list` |
| `asr restore` target | Restored volume readable (no format divergence) |
| `diskutil apfs addVolume` | New volume appears in container; readable by apfs-explorer |
| `diskutil apfs deleteVolume` | Deleted volume absent; container superblock updated |

---

## Known Non-Goals (explicitly out of scope)

| Feature | Reason |
|---------|--------|
| FileVault encryption / decryption | Requires user credential unwrap; security boundary |
| Sealed volumes (Signed System Volume) | Hash tree verification infrastructure not planned |
| Fusion Drive (SSD+HDD tiers) | Requires dual-device block routing; no demand |
| Container resize | Online resize requires partition table changes; out of scope |
| Hibernation image format | Not an APFS on-disk format concern |
| IOUserClient kernel interface | Binary-only macOS API; clean-room boundary |

---

## Reporting Format Issues

If you encounter an APFS image that apfs-explorer cannot read correctly, please
attach the output of:

```
apfs-mount info --image /path/to/image.img
```

and open an issue with the container/volume superblock flags.
