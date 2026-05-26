# Filesystem feature matrix — NTFS/FAT32 vs apfs-explorer vs Apple APFS

Three-way comparison so we can see (a) every action a Windows user
expects to work on a "normal" drive, (b) what our APFS-over-WinFsp
stack supports today, (c) what the APFS reference implementation provides (symbol-
level, clean-room). The "Gap" column is the actionable list — the reference
implementation has it, we do not, so we either need to add it or document that we
intentionally do not implement it for v1.

NTFS/FAT32 column reflects Win32 API + cmd/PowerShell typical
behaviour, not the obscure corner of every option. Apple APFS column
references the APFS specification (Apple PDF + linux-apfs-rw cross-check).
No third-party binary code is copied — only symbol names
and the algorithmic role they imply.

Legend: ✅ supported, ⚠️ partial, ❌ missing, n/a not applicable to
APFS.

---

## A — File data operations

| Operation                                  | NTFS | FAT32 | apfs-explorer | Apple APFS (symbol) | Gap |
|--------------------------------------------|:----:|:-----:|:-------------:|---------------------|:---:|
| CreateFile (CREATE_NEW)                    | ✅   | ✅    | ✅            | `_apfs_create_doc_id_tree_if_needed`-adjacent | — |
| CreateFile (CREATE_ALWAYS, overwrite)      | ✅   | ✅    | ✅            | overwrite cb        | — |
| CreateFile (OPEN_ALWAYS)                   | ✅   | ✅    | ✅            | open cb             | — |
| ReadFile sequential                        | ✅   | ✅    | ✅            | read path           | — |
| ReadFile random (FILE_FLAG_RANDOM_ACCESS)  | ✅   | ✅    | ✅            | `_apfs_bc_optimizer_callback` heuristic | — |
| WriteFile sequential                       | ✅   | ✅    | ✅            | WBC chain           | perf |
| WriteFile random / sparse                  | ✅   | ⚠️    | ⚠️ no-sparse  | sparse extent path  | M8 |
| SetEndOfFile (truncate to N>0)             | ✅   | ✅    | ✅            | `_fext_cut`         | — |
| SetEndOfFile (extend with zeros)           | ✅   | ✅    | ✅ (memzero)  | sparse extend       | M8 |
| SetAllocationSize                          | ✅   | ⚠️    | ✅ (accept)   | spaceman reserve    | — |
| FlushFileBuffers                           | ✅   | ✅    | ✅            | `_fs_tx_*` + sync   | — |
| Memory-mapped writes                       | ✅   | ✅    | ⚠️ untested   | UPL (`_apfs_create_upl`) | test |
| FILE_DELETE_ON_CLOSE                       | ✅   | ✅    | ✅            | cleanup-delete      | — |
| Atomic save (temp+rename)                  | ✅   | ✅    | ✅            | rename in tx        | — |
| Open with FILE_FLAG_NO_BUFFERING           | ✅   | ✅    | ⚠️            | wbc_bypass          | — |

## B — File metadata + attributes

| Operation                                  | NTFS | FAT32 | apfs-explorer | Apple APFS (symbol) | Gap |
|--------------------------------------------|:----:|:-----:|:-------------:|---------------------|:---:|
| Get/Set FILE_ATTRIBUTE_READONLY            | ✅   | ✅    | ✅            | mode 0o200 bit      | — |
| FILE_ATTRIBUTE_HIDDEN                      | ✅   | ✅    | ❌            | `_apfs_*hidden*`?   | add |
| FILE_ATTRIBUTE_SYSTEM                      | ✅   | ✅    | ❌            | bsd flags           | add |
| FILE_ATTRIBUTE_ARCHIVE                     | ✅   | ✅    | ❌            | bsd flags           | add |
| FILE_ATTRIBUTE_COMPRESSED (transparent)    | ✅   | ❌    | ⚠️ read-only  | decmpfs (we have read) | encode |
| FILE_ATTRIBUTE_ENCRYPTED (EFS)             | ✅   | ❌    | ❌            | FileVault crypto    | M9+ |
| FILE_ATTRIBUTE_SPARSE_FILE                 | ✅   | ❌    | ❌            | `_apfs_check_for_spillover`? | M8 |
| Get/Set creation time (FILETIME)           | ✅   | ✅    | ✅            | btime               | — |
| Get/Set last access time                   | ✅   | ✅    | ✅            | atime               | — |
| Get/Set last write time                    | ✅   | ✅    | ✅            | mtime               | — |
| Get/Set change time (ctime)                | ✅   | ❌    | ✅            | ctime               | — |
| Owner / group (BSD)                        | n/a  | n/a   | ⚠️ read-only  | uid/gid             | wire |
| Security descriptor (NTFS ACL)             | ✅   | ❌    | ⚠️ stub       | `_apfs_check_seal`? | M9+ |
| Extended attributes (EA)                   | ⚠️   | ❌    | ✅ read only  | xattr tree          | write |
| Alternate Data Streams (ADS)               | ✅   | ❌    | n/a APFS      | n/a                 | docu |

## C — Directory operations

| Operation                                  | NTFS | FAT32 | apfs-explorer | Apple APFS (symbol) | Gap |
|--------------------------------------------|:----:|:-----:|:-------------:|---------------------|:---:|
| CreateDirectory                            | ✅   | ✅    | ✅            | mkdir               | — |
| RemoveDirectory (empty)                    | ✅   | ✅    | ✅            | rmdir               | — |
| Remove recursive (cmd rmdir /s, PS -R)     | ✅   | ✅    | ✅            | walk + unlink loop  | — |
| Rename / Move (same parent)                | ✅   | ✅    | ✅            | rename in tx        | — |
| Move across directory (different parent)   | ✅   | ✅    | ✅            | move_entry          | — |
| Move across volume                         | ✅   | ✅    | n/a single    | container-cross     | n/a |
| Directory enum (FindFirstFile + Next)      | ✅   | ✅    | ✅            | drec iter           | — |
| Order-based marker resume                  | ✅   | ✅    | ✅            | tree iter           | — |
| Hash-collision name siblings               | ✅   | n/a   | ⚠️ untested   | `_apfs_check_and_alloc_alt_name` | test |
| Dir change notification (USN/FindFirstChangeNotification) | ✅ | ⚠️ | ❌ | `_apfs_bp_cleaner` events? | M9+ |
| Watch subtree                              | ✅   | ❌    | ❌            | —                   | M9+ |
| Junction / mount point                     | ✅   | ❌    | ❌            | n/a (firmlinks)     | docu |
| Symbolic link                              | ✅   | ❌    | ❌            | symlink inode mode  | add |
| Hard link (cross-dir)                      | ✅   | ❌    | ❌            | sibling chain       | add |

## D — Volume / container operations

| Operation                                  | NTFS | FAT32 | apfs-explorer | Apple APFS (symbol) | Gap |
|--------------------------------------------|:----:|:-----:|:-------------:|---------------------|:---:|
| GetVolumeInformation (name, fs, serial)    | ✅   | ✅    | ✅            | VSB read            | — |
| SetVolumeLabel                             | ✅   | ✅    | ✅            | VSB write           | — |
| GetDiskFreeSpaceEx (total / free)          | ✅   | ✅    | ✅            | spaceman query      | precise |
| Volume serial number                       | ✅   | ✅    | ⚠️ zero       | nx_uuid             | wire |
| Defragment (FSCTL_GET_VOLUME_BITMAP)       | ✅   | ✅    | ❌            | n/a (COW)           | docu |
| TRIM / FSCTL_FILE_LEVEL_TRIM               | ✅   | ❌    | ❌            | TRIM on free        | add |
| Online resize / extend                     | ✅   | ⚠️    | ❌            | container grow      | M9+ |
| Quotas                                     | ✅   | ❌    | ❌            | space-share model   | n/a |
| Encryption (BitLocker)                     | ✅   | ❌    | ❌            | FileVault container | M9+ |
| Snapshot (VSS / Time Machine)              | ✅   | ❌    | ✅ internal   | snap_meta_tree      | expose |
| Snapshot delete                            | ✅   | ❌    | ✅ internal   | snap delete + drain | expose |
| Snapshot mount as drive                    | ✅   | ❌    | ❌            | snap-mount path     | M9 |
| Disk image (.vhdx mount)                   | ⚠️   | ⚠️    | ❌            | n/a (we are the image-side) | M4b+ |

## E — Concurrency / safety

| Operation                                  | NTFS | FAT32 | apfs-explorer | Apple APFS (symbol) | Gap |
|--------------------------------------------|:----:|:-----:|:-------------:|---------------------|:---:|
| Byte-range lock (LockFileEx)               | ✅   | ✅    | ❌            | vnode lock          | M9+ |
| Mandatory locks                            | ⚠️   | ❌    | ❌            | —                   | n/a |
| Atomic rename (no-replace)                 | ✅   | ✅    | ⚠️            | rename + drec check | add |
| Atomic exchange (NTFS RenameInformationEx) | ✅   | ❌    | ❌            | —                   | M9+ |
| Crash-safe transaction                     | ✅   | ⚠️    | ✅ COW        | `_fs_tx_freeze/thaw`| — |
| Power-loss tolerance                       | ✅   | ⚠️    | ✅ proven     | checkpoint two-NX   | — |

## F — APFS-native features (do we expose to Windows?)

| Operation                                  | Apple APFS (symbol)                | apfs-explorer | Gap |
|--------------------------------------------|------------------------------------|:-------------:|:---:|
| Clone file (`clonefile()`)                 | `_apfs_clone_internal`, `_fext_cloner` | ❌         | maybe |
| Per-file compression (decmpfs)             | decompression in `apfs::read`      | ⚠️ read-only | encode |
| Sealed system volume (signed checksum)     | `_apfs_check_seal`, `_apfs_announce_hash_mismatch` | n/a | docu |
| Container space-sharing across volumes     | `_apfs_check_for_spillover`        | n/a (single vol) | docu |
| Fusion drive WBC tiering                   | `_nx_fusion_release_wbc_range` + sysctls | ❌  | n/a |
| Crypto rolling                             | `_apfs_commit_update_volume_key`   | ❌            | M9+ |
| Snapshot CoW reservation                   | `_apfs_check_snapshots_extents`    | ✅ basic     | strengthen |
| TimeMachine local snapshots                | snap-meta tree                     | ✅ engine    | expose |
| Document ID tree                           | `_apfs_create_doc_id_tree_if_needed` | ❌         | M9+ |
| Read-only snapshot pinning                 | `_apfs_check_rosp`                 | ✅ engine    | wire |
| Purgatory (delayed reclaim)                | `_apfs_cleanup_purgatory_*`        | ✅ sm_fq     | — |

---

## Gap summary (what to add, in M8 / M9 order)

**M8 (perf-leaning, this branch)**
1. ✅ Allocator: free-runs tree (done planning, impl pending)
2. ⏭ Transaction batching (`_fs_tx_enter/leave`) — cleanup batching
3. ⏭ Write-back coalescing (`_apfs_wbc_*`) — write-side cache
4. ⏭ `_fext_cut` — partial truncate without RAM buffer

**M8 — correctness completers**
5. Hash-collision sibling chain (`_apfs_check_and_alloc_alt_name`)
6. Memory-mapped write test pass
7. Volume serial number from nx_uuid
8. Atomic-rename no-replace semantic
9. SetAllocationSize precise reservation

**M9 (feature parity beyond perf)**
10. Hidden / System / Archive attribute persist
11. Symbolic link + hard-link siblings
12. Sparse file (transparent holes)
13. Snapshot expose to user (Win shadow-copy compatible)
14. Clone file (Windows ReFS reflink equivalent)
15. EA (xattr) write support
16. ACL / security descriptor write
17. USN-equivalent change notifications
18. TRIM on free
19. Online container resize
20. FileVault / encryption (large scope)
21. Clonefile + cp -c semantic
22. Document-ID tree

**Intentionally out of scope (document, not implement)**
- Alternate Data Streams (APFS doesn't have)
- Quotas (single-volume container doesn't need)
- Fusion-drive tier (only Apple HW)
- BitLocker / cross-FS encryption
- Multi-volume cross-container move
- Junctions (firmlinks are APFS internal)

## Process for filling each row

Per row marked `Gap = M8 / M9`, in the relevant phase:
1. Identify the apfs-write or winfsp-host site that would handle it.
2. Cross-check the reference symbol in linux-apfs-rw/apfsprogs; capture algorithm in
   `apfs-implementation-notes.md` (one section per row).
3. Implement in Rust from linux-apfs-rw + the captured algorithm in
   English — never paste Apple code.
4. Scratch test in apfs-write tests/, Windows oracle.
5. Tick the row.
