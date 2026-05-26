# M7c NotebookLM — content-bearing file write (dstream + file_extent + extentref)

> **STATUS: ANSWERED + LOCKED `apfs-spec:m7c-confirmed` (2026-05-22).**
> Apple PDF p.88-89 (phys_ext), p.97 (dstream), p.98 (file_extent), p.78 (VSB
> counters). Key locks:
> - j_dstream 40B: size(logical EOF), alloced_size(block-multiple = sum of
>   extent byte lengths), crypto_id=0, total_bytes_written=size, read=0.
> - xfield order MUST be ascending x_type (NAME=4 before DSTREAM=8) — fsck
>   enforces. DSTREAM x_flags=0x20 (undocumented system flag; replicate).
> - file_extent val: len_and_kind = byte length in bits[55:0], flags[63:56]=0
>   for plain files; phys_block; crypto_id=0. Extent length is block-aligned.
> - phys_ext (extentref) val 20B: len_and_kind = kind[63:60]|block_count[59:0]
>   (kind=1 NEW), owning_obj_id=inode, refcnt=1.
> - extentref tree: variable-kv bt_flags 0x52; we COW the live tree (subtype
>   preserved verbatim — this macOS uses 0x0F, NotebookLM's "0x08" is a newer
>   beta value; real disk is the arbiter, md.6).
> - single spaceman pool (no data/metadata split); fs_alloc_count = sum of all
>   blocks reachable from the volume.
> - fsck trinity: dstream.alloced_size == Σ(file_extent byte len);
>   file_extent.phys_block in extentref with owning_obj_id+refcnt; block marked
>   allocated in spaceman bitmap.

Building on M7b (empty file creation, locked), we now write a SMALL file
WITH content (single block, <= 4096 bytes) on a scratch image, validated by
fsck + mount-readback (exact bytes) + independent Python decode.

We decoded a kernel-produced file "note.txt" (17 bytes, "hello apfs world\n")
from a real macOS-formatted scratch image. Empirical values below; we want
NotebookLM to confirm the spec source/semantics (md.6).

## Decoded kernel oracle (note.txt, inode 18, 17 bytes)

INODE (160 bytes), mode 0o100644, two xfields:
```
xf[0] type=4 (NAME)    flags=0x02 size=9   -> "note.txt\0" padded to 16
xf[1] type=8 (DSTREAM) flags=0x20 size=40  -> j_dstream (below)
xf_used_data = 56 = round_up(9,8)=16 + round_up(40,8)=40
inode field @84 (uncompressed_size) = 0   (size lives in the dstream, not here)
```
j_dstream (40 bytes):
```
@0  size                 = 17     (logical EOF)
@8  alloced_size         = 4096   (block-rounded)
@16 default_crypto_id    = 0
@24 total_bytes_written  = 17
@32 total_bytes_read     = 0
```
FILE_EXTENT record (catalog, subtype FSTREE):
```
key (16B): jkey(type=8 FILE_EXTENT, id=18) + logical_addr(u64)=0
val (24B): len_and_kind(u64)=0x1000 (=4096), phys_block_num(u64)=203, crypto_id(u64)=0
```
Extent-ref tree (VSB.extentref_tree_oid, subtype 0x0F, variable-kv, bt_flags 0x52):
```
key (8B):  jkey(type=2 EXTENT, id=phys_block_addr=203)
val (20B): len_and_kind(u64)=0x1000000000000001 (kind=1, blocks=1),
           owning_obj_id(u64)=18, refcnt(u32)=1
```
Data block 203 = "hello apfs world\n" + zero pad to 4096.

## SECTION A — j_dstream xfield

- **Q-A1**: Confirm j_dstream_t layout/offsets (size, alloced_size,
  default_crypto_id, total_bytes_written, total_bytes_read), 40 bytes total.
  Apple PDF page?
- **Q-A2**: DSTREAM xfield x_flags = 0x20. Which XF_* flag is this
  (XF_SYSTEM_FIELD?) and is it mandatory? NAME uses 0x02 (XF_DATA_DEPENDENT);
  why does DSTREAM differ?
- **Q-A3**: xfield ORDER — NAME (type 4) before DSTREAM (type 8). Is the
  order mandatory (ascending x_type), and does fsck care?
- **Q-A4**: For a 17-byte file, size=17 but alloced_size=4096 (one block).
  Confirm alloced_size is always a multiple of the block size (the sum of
  extent lengths), and size is the logical EOF. total_bytes_written = size;
  is that required or informational?
- **Q-A5**: inode field @84 — for a file WITH data it is still 0 here. Is
  @84 really "uncompressed_size" (used only for compressed files) and
  unrelated to dstream.size? Confirm we leave it 0 for normal files.

## SECTION B — j_file_extent (catalog FILE_EXTENT record)

- **Q-B1**: Confirm j_file_extent_key = jkey(type=8) + logical_addr(u64),
  and j_file_extent_val = len_and_kind(u64) + phys_block_num(u64) +
  crypto_id(u64). Apple PDF page?
- **Q-B2**: len_and_kind: we observed 0x1000 = 4096 (no high bits set). Is
  the field length-in-BYTES in bits[55:0] and flags/kind in bits[63:56]
  (FEXT_CRYPTO_ID_IS_TWEAK)? For a plain file all high bits are 0?
- **Q-B3**: The extent length is the BLOCK-ALIGNED byte length (4096) even
  though the file is 17 bytes. Confirm extents are always block-multiples.
- **Q-B4**: logical_addr is the file offset of the extent start (0 for the
  first/only extent). Confirm.

## SECTION C — extent-ref tree (physical extent records)

- **Q-C1**: Confirm j_phys_ext_key = jkey(type=2 = APFS_TYPE_EXTENT,
  id = physical_block_address) and j_phys_ext_val = len_and_kind(u64) +
  owning_obj_id(u64) + refcnt(u32) = 20 bytes. Apple PDF page?
- **Q-C2**: len_and_kind for phys_ext: 0x1<<60 | 1 → kind in bits[63:60],
  block_count in bits[59:0]. What is kind=1 (APFS_KIND_NEW)? When is
  kind=2 (APFS_KIND_UPDATE) used?
- **Q-C3**: refcnt = 1 for a singly-referenced extent. Confirm a new file's
  extent always starts at refcnt 1, and the extentref tree MUST gain this
  record (fsck cross-checks FILE_EXTENT.phys_block against extentref).
- **Q-C4**: The extent-ref tree is variable-kv (bt_flags 0x52, subtype 0x0F).
  Same insert/re-pack rules as the FSTREE catalog leaf (TOC sorted by key =
  phys_block_addr ascending)? longest_key=8, longest_val=20.

## SECTION D — allocation & spaceman

- **Q-D1**: The data block (203) is allocated from the spaceman like any
  metadata block. Confirm there is no special "data" vs "metadata" allocation
  pool for a single APFS volume (both come from the same spaceman bitmap).
- **Q-D2**: Must apfs_fs_alloc_count (VSB) increase by the data block + the
  new metadata blocks we COW? How exactly does fsck compute the expected
  fs_alloc_count (sum of all blocks reachable from the volume)?
- **Q-D3**: Ordering within the transaction: any constraint on writing the
  data block before/after the metadata? (We write all staged blocks then the
  superblock last, per M7a commit envelope.)

## SECTION E — referential integrity (fsck checks)

- **Q-E1**: List fsck's cross-checks for a file with one extent:
  inode.dstream.size vs sum of FILE_EXTENT lengths; FILE_EXTENT.phys_block
  present in extentref with matching owning_obj_id and refcnt; data block
  marked allocated in spaceman. Anything else?
- **Q-E2**: Is total_bytes_written required to equal dstream.size, and does
  fsck validate it? Does the inode need INODE flags (e.g. has-data-stream)
  beyond the DSTREAM xfield presence?

## Recommendation

Cite Apple PDF pages for j_dstream, j_file_extent, j_phys_ext, and the
extent/extentref cross-checks; confirm or correct each empirical value.
Then we lock apfs-spec:m7c-confirmed and implement write_file.
