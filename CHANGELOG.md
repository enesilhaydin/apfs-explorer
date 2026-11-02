# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.1.0] - 2026-11-02

Every change below was arbitrated the same way: a failing test on a
macOS-formatted image, the behaviour measured on macOS (or in Apple's own
implementation), the fix, and a strict `fsck_apfs` (warnings count) at the
end. The Windows host was rebuilt and its write batch re-run against macOS
after each group.

### Added

- `apfs_core::gpt::find_apfs_container` finds the APFS container in a
  whole-disk image (it lived in the mount host), and the `list_dir`
  example (`cargo run -p apfs --example list_dir -- disk.img /`) shows a
  volume read from either kind of image.
- `list_snapshots` returns a volume's snapshots (xid, name, creation
  time), oldest first.
- `write_range` writes a byte range of a file without rewriting the file:
  only the blocks the write touches get new blocks (their old content with
  the new bytes over it), the extent around them is split and neighbours
  that continue each other are joined, and the replaced blocks lose the
  file's reference (a clone or a snapshot keeps them). A block the same
  transaction gave the file is rewritten where it is. Compressed files and
  streams shared with a macOS clone are declined, and the caller rewrites
  them whole. 4 KiB into a 64 MiB file: 9 ms.
- `unshare_stream` gives a file that shares its data stream with a macOS
  clone a stream of its own by reference: the extents are recorded again
  under a new id and their blocks gain a reference, nothing is copied.
  Range writes and cuts then work on it, and also on any stream named by
  one inode under another id.
- Cutting a file short inside an extent is a metadata change too: the
  extents past the new end go, the one across it is shortened, and only
  the block holding the new end is rewritten (with zeros past it). It used
  to read and rewrite the whole file: cutting a 64 MiB file on Windows
  takes 17 ms instead of 103 ms, without holding the file in memory.
- Hard links: create a second name, remove one name, list the names a file is
  reachable through, keep a link a link across rename and move; the link
  count is reported.
- Clones share physical extents by reference count; a clone keeps the source's
  extended attributes and its birth, modification and access times, a sparse
  source clones sparse, and removing or rewriting one side leaves the other
  its content.
- Snapshots: take, delete and revert on volumes with multi-level object maps;
  deleting a snapshot releases only the extents it alone was keeping and drops
  the object-map versions no remaining view can reach.
- Every decmpfs method macOS writes is read (zlib, LZVN, LZFSE, LZBITMAP,
  raw), reads are ranged, and compressed files can be resized and rewritten
  (rewriting drops the compression).
- Large containers: the space manager's address blocks are followed and a
  space manager longer than one block is carried through a commit.
- Volume size and free space are reported the way macOS reports them; volume
  roles are decoded as flags plus an enumeration; symbolic links open on
  Windows.
- Reader API: attribute flags, sibling maps, data-stream ids, object-map
  entries, the inode write-generation counter; an absent path is a `NotFound`
  error rather than an empty result.
- Sealed volumes (signed system volumes, cryptex images) are read: headerless
  catalog nodes, relative child ids and the separate file-extent tree. A
  macOS OS cryptex (6139 files, 18 GB) reads byte-identical to macOS.

### Changed

- Reading a file that was edited in places keeps its read-ahead. Edits are
  written elsewhere (copy-on-write), and a reader going through the file
  in order dips there and comes back; the mount had one 1 MiB window, which
  each dip moved and the way back refilled, two 1 MiB reads per edit. It
  now keeps four windows, and reads ahead only once a reader has gone 16
  KiB in order, so a dip of a block or two is read as asked. A 256 MiB
  file with 1000 scattered edits read in 4 KiB requests on a USB drive:
  15.9 s -> 6.2 s.
- The Fletcher-64 checksum reduces its sums once per 4096 words instead
  of twice per word (the same result, pinned by a test against the
  per-word form). Every node a write rebuilds is checksummed, and it was
  the costliest step of a small write: creating, sizing, writing and
  timestamping 6000 small files took 3.05 s in the library, now 1.93 s.
- After an operation that changes no name (a write, a resize, new times
  or attributes, an extended attribute), the mount keeps its cached paths
  and listings instead of dropping them. Every operation dropped them, so
  each of the five operations Windows makes to copy a file in was
  followed by lookups walking from the root; their time fell from 317 to
  110 ms over 1000 files.
- A transaction's new blocks follow each other: an allocation takes the
  block right after the transaction's last one when it is free, else the
  start of a free run with room to keep going (1 MiB), and best fit only
  after that. Best fit put each one-block allocation in a different hole
  left by copy-on-write, so a commit made a device write per block, and
  a USB drive takes up to 50 ms for a small random write. 300 scattered
  4 KiB writes into a 1.5 GiB file on a USB drive: commit 11.8 s -> 0.29 s.
- The app shows its version, the commit it was built from and the build
  date under its title, and logs them at start; `apfs-mount --version`
  prints the same, and every mount's log starts with it. The version is
  the product's (`tauri.conf.json`, which `xtask set-version` sets);
  `APFS_BUILD_COMMIT` names the commit when building from a source export.
- `snapshot list`, `delete` and `revert` take `--drive N` as `create`
  already did, so a drive with snapshots can be made writable from
  Windows. On a drive the subcommands open it unshared and refuse it while
  a mount (or anything else) holds it, instead of writing under the mount.
- A run of blocks is taken whole from any chunk with room for it before
  the allocator falls back to the largest hole of the first chunk with
  free space. Holes left by earlier deletes cut new large files into
  scraps, each a device write of its own: copying 256 MiB to a used USB
  drive made 1591 writes at 6.8 MB/s; now 469 writes at 23 MB/s.
- File content is read through its own path (`BlockDevice::read_content_at`)
  and never enters the mount's metadata cache, whatever the size of the
  reads: a program reading 4 KiB at a time filled the cache with file
  data, and the mount held 68-97 MB while copying and hashing large files
  on a USB drive; now 18 MB.
- Writes to a physical drive no longer ask for write-through. Every write
  was a forced unit access, which a USB stick served at 12 MB/s; without it
  the same stick takes 30 MB/s. Durability still comes from the commit's
  two flush barriers.
- A range write no longer copies its data: blocks the write covers whole
  go to the disk straight from the caller's buffer, and only the two edge
  blocks get buffers of their own. A 32 MiB write (the size Windows hands
  over for a large file) held 64 MiB; the mount's private memory while
  copying a 256 MiB file fell from 39 MB to 4 MB, and the copy ran twice as
  fast.
- A directory listing is kept sorted by name, and Windows reads it a page
  at a time (`FsView::read_dir_after`): each listing call used to copy and
  sort the whole directory. Listing a folder of 20 000 files: 2.2 s, now
  112 ms (Get-ChildItem 4.5 s, now 0.7 s).
- The mount's block cache keeps metadata only. File content read in order
  comes through one 1 MiB read-ahead window, other content reads go to the
  disk as asked: reading a large file no longer fills the 64 MiB cache with
  it (the mount used 75 MB after reading one, now 18 MB). The cache finds
  its oldest block without scanning all of them on every hit.
- The app lists drives every 3 s only while its window is shown, and
  refreshes as soon as it is shown again. Each listing is two WMI queries
  (~40 ms of WMI service CPU), which a window hidden in the tray kept
  paying; the ten log lines each listing wrote (~110 000 a day) now log at
  debug.
- Both mounts ask WinFsp to pass single-name directory queries through
  (`PassQueryDirectoryFileName`) instead of having it list the whole
  directory to find one name: copying 300 small files into a folder takes
  ~485 ms instead of ~613 ms. Filtered listings (`dir *.txt`) stay complete.
- Names are hashed with the full Unicode case fold macOS applies (ß, ligatures,
  final sigma), a different spelling of an existing name is that name, case is
  not folded on case-sensitive volumes, and entry names are limited to 255
  characters.
- Attributes follow macOS: Finder info is exactly 32 bytes, names are at most
  127 bytes, `com.apple.system.*` is refused, a resource fork is always a
  stream, values up to 3800 bytes are embedded as long as the whole record
  stays within 3936 bytes, an attribute stream carries no
  data-stream-id record, and the symlink target attribute is file-system
  owned and cannot be set or removed by name.
- Timestamps follow macOS: a move keeps the file's modification time, setting
  or removing an attribute moves its change time, a clone keeps the source's
  times, an explicit modification time earlier than the birth time pulls the
  birth time back, and the write-generation counter steps on content changes
  of files and entry changes of directories.
- Object counters are kept per kind (files, directories, symlinks, other) and
  the space manager's reserved-volume sum stays in step.
- What is not implemented is refused with a clear reason instead of
  misbehaving: encrypted volumes, Fusion containers, sealed and unhashed
  volumes, directories that maintain directory statistics, block sizes other
  than 4096, symlink targets longer than 1023 bytes.

- Catalog lookups descend only into the part of the tree that can hold the
  object asked for, instead of walking every node.
- Reads keep parsed catalog nodes and object-map resolutions between
  lookups, find a name in a directory by key and remember resolved path
  prefixes: listing and stat of a 5000-entry directory dropped from 2.7 s
  to 20 ms.
- Writes rewrite only the nodes on the paths to the records they change, in
  the catalog, the extent-reference tree and the volume object map (nodes
  split when full and are dropped when empty; separators stay the first key
  of their child), and read only the records they need. A metadata operation
  on a volume of thousands of files no longer reads and writes the whole
  catalog. Volumes with snapshots still rebuild the catalog in full; the
  rebuild remains as the reference the path-local writer is tested against.
- A commit issues two durability barriers instead of three: the bootstrap
  superblock copy at block 0 no longer gets its own.
- One transaction can carry many operations: a node an earlier operation of
  the transaction wrote is rewritten where it is, blocks a later operation
  replaced are never written, a reader built on the open transaction sees
  its changes before the commit, and an operation that fails half way
  (`Transaction::atomically`) rolls back alone. 300 creates with writes,
  300 renames or 300 removals in one checkpoint take 28-63 ms, against
  53-72 ms for macOS on the same image.
- The Windows mount groups write operations: each runs at once and reports
  its own result, lookups see it immediately, and the group is committed
  after 100 ms without operations, after one second at most, at 4096
  operations or 64 MiB of staged blocks, on a flush and at unmount (also when the process is stopped
  from the console). Measured through WinFsp on the same image: copying 300
  small files 6.6 s -> 0.5 s, renaming them 2.8 s -> 0.35 s, removing 150
  1.2 s -> 0.2 s. A commit's time is almost all its two waits on the disk,
  so the group's caps bound the memory its staged blocks take (file data
  included) rather than cutting a burst into several commits.
- A name is looked up by its hash: the reader reads the few directory records
  carrying the name's hash instead of listing the directory, and the
  Windows host resolves the name a write operation targets the same way
  (`FsView::entry` returns the name as stored). After a write the listings
  are dropped, so every operation on a large directory used to list it again.
- An operation after the first in a transaction rewrites the volume
  superblock and the volume object-map header where the transaction staged
  them, instead of taking two new blocks for them: 49 renames after a first
  one take no new blocks (98 before), so an open write group holds fewer
  blocks in memory and data written between operations stays contiguous.
- File data is written to the device as it is written, not held in memory
  until the commit: no checkpoint names a newly allocated block before the
  commit, so a crash leaves the bytes in free space, and an operation that
  fails puts back the blocks it rewrote. Writes of 1 MiB and more go at
  once; smaller ones gather in a 4 MiB buffer and reach the device as runs
  of consecutive blocks, so a USB drive's raw node gets a few large writes
  rather than one per small file. A transaction that wrote 4 MiB holds
  under 1 MiB; the Windows mount's peak working set while editing a 64 MiB
  file fell from 88 MB to 19 MB.
- A commit writes staged blocks at consecutive addresses in one device
  write (up to 8 MiB) instead of one write per block: a 64 MiB file is
  written in 88 ms instead of 343 ms (macOS: 113 ms), with 121 device
  writes instead of 16441.
- The Windows mount writes each write to the volume as it comes, through
  `write_range`, instead of holding the whole file in the handle until it
  is closed: on a 64 MiB file, appending one byte takes 26 ms instead of
  260 ms, writing 4 KiB into the middle 30 ms instead of 252 ms, and the
  mount's peak memory fell from 206 MB to 88 MB. A write error now reaches
  the program that wrote, and every handle reads what any handle wrote.
  A file sharing a macOS clone's stream is first given its own by
  reference, and a compressed file is written plain once (as macOS does
  on the first write), so those take range writes too.
- The Windows mount lets the kernel answer file, directory and security
  queries from what it was told in the last second, and cache file data,
  flushed to the mount when a handle closes (`APFS_INFO_TIMEOUT_MS`, 0
  turns it off). Renaming 300 files takes 258 ms and removing 150 takes
  135 ms (NTFS: 265 and 125).
- The Windows mount counts what it does when `APFS_STATS` names a file:
  calls and time per callback, write operations, commits and device reads,
  writes and flushes, written there every second and at unmount.
- A volume's index is its slot in the container: a volume after a deleted
  one keeps the index macOS gives it.
- Snapshot names are checked: empty, longer than 255 bytes, holding a NUL or
  already taken is refused.

### Fixed

- Mounting from the app works again. Its launcher classes were limited to
  Administrators and SYSTEM (anyone could otherwise get raw access to a
  drive through them), but the app still ran as the plain user, so every
  mount was refused. The app now asks for elevation once when it opens;
  mounts and unmounts need no further prompt, and the drive letters stay
  visible to the normal Explorer.
- The app's log is written. Its folder is writable by Administrators
  only, and the app, not elevated, logged nothing and said nothing about
  it; it now falls back to `%LOCALAPPDATA%\APFS Explorer\logs`, names the
  folder it used at start, and records failed mounts and unmounts with
  the reason.
- A drive pulled out while mounted ends its mount. The drive letter went,
  but `apfs-mount` stayed running against a device that no longer existed
  until something stopped it. It now checks the drive every second and,
  once it is gone, unmounts without trying to commit to it and exits with
  code 3. Measured on a USB drive pulled during a stream of writes, three
  times: every file finished more than 0.6 s before the pull read back
  intact, nothing after the first lost file survived, and no file was
  left half-written.
- `APFS_INFO_TIMEOUT_MS` is capped at 60 s. Its largest value made WinFsp
  cache file data, the setting that hung copies of 4 GiB in the kernel;
  the stuck copies could not be ended and held their unwritten pages (20
  GB on the test machine) until Windows restarted.
- Deleting a snapshot drains a free queue whose leaves macOS filled. A
  leaf below the queue's root has no footer and holds 144 entries, and
  macOS fills it that far; the drain held every leaf to a root's 142 and
  rewrote each one even with nothing due in it, so the deletion failed
  ("sm_fq drain leaf: Full") on a USB drive macOS had mounted. A leaf is
  now held to its own room and left alone when nothing in it is due.
- `apfs_fs_alloc_count` stays right when a write rebuilds the whole catalog
  (every write under a snapshot does). The object map is rebuilt with it,
  and on the first write after a snapshot it grows, because every catalog
  node gains a second mapping; that growth was never counted. On a USB
  drive of 120 000 files, fsck_apfs found the volume's block count 50 low
  ("apfs_fs_alloc_count is not valid"); a test volume of 3000 files showed
  the same on its first delete.
- Deleting a snapshot after a write into the middle of a file under it no
  longer fails. The write leaves the live extent-reference tree a record
  for the replaced blocks inside the snapshot's longer record, and the
  merge at deletion combined only records that started at the same block,
  so the rebuilt tree had two overlapping records and the deletion was
  refused. Records are now split where either side starts or ends one.
- A read-only mount refuses writes. WinFsp's read-only volume flag does
  not stop the write callbacks, and the mount never checked: on a mount
  without `--writable`, programs were told their writes had succeeded
  while the commit failed on the read-only disk handle and the writes were
  lost, and a writable request the snapshot guard had turned read-only
  wrote to the disk anyway. Every write callback now answers that the
  medium is write-protected.
- Reading one kind of record of an object (a stat, an xattr, a sibling
  link) walks only the part of the catalog that holds that kind. A
  directory's entries share its id, so a stat of a directory read the
  whole directory: 90 device reads in a folder of 3000 files, and on
  Windows, which opens the folder for every file created in it, filling
  a folder of 10 000 files took 13.9 s; now 5.5 s.
- The declared minimum Rust version is what the code needs: 1.87 for the
  library crates and `apfs-mount`, 1.88 for the app (its Tauri
  dependencies). It said 1.77, which cannot read the lock file, and the
  README and an earlier entry said 1.91.
- A directory holding more than about 500 entries listed only its first
  ~500 on Windows (Explorer, `dir`, Get-ChildItem); the rest still opened
  by name. Each listing call ended with WinFsp's end-of-directory marker,
  even when the buffer had filled first. Single-name queries, which a copy
  makes for every file, are now answered by one lookup
  (`GetDirInfoByName`) instead of a pass over the directory: copying 2000
  small files into one folder takes 3.6 s.
- The mount log (`apfs-mount.log`), which every run appends to, is moved
  to `apfs-mount.log.1` when a mount starts and finds it past 8 MiB; it
  had no bound before. The app keeps a week of its daily log files instead
  of all of them.
- The mount no longer logs a warning for every directory on the way to
  each file it opens (WinFsp asks whether each one is a symlink). It wrote
  ~260 KB per 300-file copy and had grown one test machine's log to 90 MB;
  renaming 300 files is ~8% faster without it.
- A physical drive is sized by its exact length, not by its geometry.
  The geometry counts whole cylinders, up to ~8 MB short of the disk, and
  macOS runs the APFS partition to the last usable sector, so mounting such
  a drive failed with "partition extends past the end of the device".
- The `snapshot`, `clone` and `volume list` commands open GPT disk images
  (as `hdiutil` and disk dumps produce) by finding the APFS partition,
  where they used to fail with a checksum error. `snapshot list` prints
  each snapshot's name and xid (it read the wrong superblock fields and
  found no entries), and `--xid` takes the `0x` form it prints.
- Tree nodes keep their table of contents in whole steps of eight
  entries, as macOS does. A table cut to its exact record count passed
  fsck, but macOS editing such a node moved records between siblings
  without moving the separator above them: after it removed some 200
  files we wrote in one directory, it could list files it could not open,
  and fsck found the catalog invalid ("invalid key order").
- A volume's running totals of blocks allocated and freed follow every
  change of its allocation count. They never moved, so once macOS removed
  files we wrote, the freed total passed the allocated one and fsck warned
  ("apfs_total_blocks_freed should not be greater than
  apfs_total_blocks_alloced").
- The reader asks the device for whole blocks only. It read the exact
  bytes of a range, so a raw disk node (macOS `/dev/rdiskN`), which takes
  only whole sectors, refused the tail of most files and the pieces of
  compressed ones.
- A Windows handle reads back what it wrote: a read on a handle with
  content not yet written to the volume is answered from that content.
  Writing, seeking back and reading through one handle returned the bytes
  from before the write.
- Several operations in one transaction each build on the ones before
  them: every operation starts from the volume superblock the transaction
  last staged, not the copy read before it began, which lost every change
  but the last.
- Clones and shared blocks: removing or replacing an attribute on one side
  of a clone no longer frees blocks the other side still reads; a clone of a
  file whose shared extent macOS has split takes a reference on every piece;
  a clone of a file with Finder info is marked as carrying it.
- Crash safety: every pool block is written before the superblock, the
  checkpoint is sealed in the descriptor ring before block 0 is refreshed,
  readers pick the newest ring superblock that agrees with block 0, a torn
  overwrite never touches the previous checkpoint's blocks, and the previous
  checkpoint's allocator state is never rewritten in place.
- The post-commit read-back covers the space manager and every ephemeral
  object; allocator and object-map blocks are checksum-verified before being
  edited; a free that cannot be placed fails the transaction instead of
  drifting the counts; a checkpoint map is verified before it is consumed.
- Container object map rebuilds keep every record with its flags and size;
  volume object map rebuilds carry tombstone flags and sizes.
- Hardening against crafted images: bounded block addresses, GPT table
  bounds, B-tree node geometry and level checks, loop guards in every tree
  walk, allocation caps from untrusted sizes, and a validated object map
  header (flags, tree type, pending-revert window).
- Windows host: writes, names and errors are no longer lost on the mount
  path, the running system disk is never opened for writing, an
  allocation-size call no longer undoes the truncate before it, and the
  deferred-operation queue is gone (operations apply immediately).

## [1.0.0-rc3] - 2026-06-02

### Added

- Multi-node volume omap read + write + free (#157): the catalog rewrite path
  now descends and rebuilds a split volume omap b-tree (table_space reserve
  matching macOS), so writes on volumes with thousands of files stay fsck-clean.
- Windows-runnable catalog self-verifier: `apfs_write::file::verify_fsroot` plus
  an env-gated (`APFS_SELF_VERIFY=1`) `self_verify` in winfsp-host that checks
  the fsroot after each committed op and FREEZES the mount on any structural
  violation (never propagates a pre-existing corruption).

### Fixed

- Catalog key comparator parity with Apple `apfs_key_compare` (verified against
  apfs.kext): per-type ordering - DIR_REC by hash then name, FILE_EXTENT /
  SIBLING_LINK numeric, XATTR by name (was a flat byte compare).

### Investigated

- #156 real-USB fsroot corruption - VERDICT: not our write-path. A 3-stage
  real-hardware handoff experiment with an independent verifier confirmed our
  output is structurally clean (all leaves monotonic), a clean macOS remount is
  clean, and a mid-write yank is clean (APFS crash-safety). The observed
  corruption sat in a macOS `.Spotlight-V100` cache dir with an Apple in-place
  inserter signature our packer cannot produce.

## [1.0.0-rc2] - 2026-05-26

### Added

- Windows installer: WiX Burn bundle chains the WinFsp MSI when the prerequisite
  is absent. Silent install parameters (`/qn /norestart`) suppress the WinFsp
  dialog. Product MSI gains Add/Remove Programs icon and Start Menu shortcut.
- Tray UI: minimal menu with per-volume actions (Open in Explorer, Eject,
  Re-mount Read-Only), auto-mount toggle, and default-read-only toggle.
  Settings persist to `%APPDATA%\APFS Explorer\config.toml`.
- Dirty indicator: tray polls a status file written by each mount process
  every 2 seconds; shows "unflushed writes" when batched ops are pending.
- CLI subcommands documented in `man/apfs-mount.1` and shell completions
  (bash, zsh, fish, PowerShell).
- Project hygiene: NOTICE, CONTRIBUTING, SECURITY, CHANGELOG, dependabot.yml,
  GitHub Actions CI + release workflows, bug/feature issue templates.
- Documentation: library usage guide, tested platforms matrix, APFS format
  compatibility matrix, icon design briefs.

### Fixed

- Cross-compile build error in winfsp-host (windows crate 0.58 API change).
- xtask registry probe now compares `WIN32_ERROR` values without `.map_err`
  on the bare type.

### Deferred

- Windows "Safely Remove Hardware" integration: WinFsp 2.1 SDK / winfsp-sys
  0.12.1 does not expose the `RemovableMedia` bitfield in
  `FSP_FSCTL_VOLUME_PARAMS`. Tracked for re-evaluation when WinFsp 2.2 stable
  ships or winfsp-sys updates its bindings.
- Hard links and `FSCTL_DUPLICATE_EXTENTS_TO_FILE`: blocked upstream
  (winfsp-rs 0.13 trait lacks `set_link`; `control` callback lacks a
  handle-to-path resolver).

## [1.0.0-rc1] - 2026-05-17

### Added

- M11 catalog incremental write path: delta-only B-tree node updates, reducing
  write amplification on large directories.
- M12 omap incremental patch in rename fast path: avoids full omap rewrite on
  single-object renames.
- Windows benchmark suite validated against physical APFS volumes (empirical
  performance floor confirmed).
- WinFsp EA bridge: extended attributes round-trip through Windows EA API.

### Fixed

17 bug fixes across the write path, including:
- debug-log spam on hot paths removed.
- Parent-chain EINVAL on deep directory renames corrected.
- Write-seed silent data loss on power-fail scenario fixed.
- Cache invalidate-on-error now clears stale entries correctly.
- `set_file_size` slow-fallback no longer silently drops the final block.
- `HANDLE` RAII wrapper prevents handle leak on early return.
- `getattr` timestamps corrected (Windows epoch 1601 base removed; real mtime
  reported).
- Case-insensitive lookup now returns consistent results for NFD-normalized names.

### Changed

- Minimum supported Rust version: 1.91.

## [0.9.9-rc1] - 2026-02-28

### Added

- M9 milestone features:
  - Extended attribute stream read/write (xattr data fork).
  - UUID resolver for volume cross-reference.
  - NFD case-fold normalization for case-insensitive volumes.
  - Clone (copy-on-write reflink) support.
  - Snapshot revert (`apfs snapshot revert <name>`).
  - CLI subcommands: `mount`, `unmount`, `snapshot`, `info`, `fsck-lite`.
  - WinFsp EA bridge for Windows extended attribute interoperability.

### Fixed

- Snapshot deletion drain (`sm_fq`) now safe with multiple simultaneous snapshots.
- Symlink round-trip through Windows reparse points preserves target path.

## [0.9.0-pre-windows-bench] - 2025-11-15

### Added

- Complete read path (B-tree traversal, extent resolution,
  directory enumeration, inode stat).
- WinFsp host crate (`winfsp-host`) initial integration.
- Windows installer skeleton (WiX Burn bundle, `xtask package`).
- Cargo workspace restructured: `apfs-core`, `apfs-write`, `winfsp-host`,
  `installer`, `xtask`.

### Changed

- Error handling migrated to `thiserror`; no `unwrap` in library paths.

## [0.5.0] - 2025-07-01

### Added

- M4-M8 milestones:
  - M4: Container/volume superblock parse, checkpoint selection.
  - M5: Object map B-tree traversal.
  - M6: Catalog B-tree: inode, directory record, dentry lookup.
  - M7: Extent-based file data read, compressed file detection.
  - M8: Write transaction skeleton, space manager allocation, free-run bitmap.

