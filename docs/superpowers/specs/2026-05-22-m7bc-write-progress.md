# M7b/M7c — file-write engine progress (scratch-proven)

All operations below are implemented in `crates/apfs-write/src/file.rs` and
proven on a real macOS APFS scratch image via the triple oracle: `fsck_apfs`
CLEAN + macOS mount-readback + independent `tools/redecode_file.py`. Every
on-disk structure was decoded from a kernel-produced sample (md.6) and
confirmed by NotebookLM against the Apple PDF (`apfs-spec:m7b-confirmed`,
`apfs-spec:m7c-confirmed`).

## Public API (apfs_write::file)

| fn | Operation | Records touched |
|---|---|---|
| `create_file(txn, vsb, omap, parent, name)` | empty regular file | DREC + INODE(NAME only) |
| `mkdir(txn, vsb, omap, parent, name)` | empty directory | DREC(DT_DIR) + INODE(dir, no dstream) |
| `write_file(txn, vsb, omap, parent, name, content)` | file with content (multi-block) | DREC + INODE(+DSTREAM) + DSTREAM_ID + FILE_EXTENT(s) + extentref(s) + data blocks |
| `unlink(txn, vsb, omap, parent, name)` | remove file/dir | removes all of the above + frees data blocks |
| `rename(txn, vsb, omap, parent, old, new)` | rename in same dir | replaces DREC + INODE(NAME xfield); preserves dstream/extents |

All share the M7a COW commit envelope: the fsroot is a VIRTUAL object resolved
through the volume omap, so each mutation COWs the catalog leaf, remaps
`{root_tree_oid, new_xid} -> new_paddr` in the volume omap b-tree, COWs the omap
header, and stages the (virtual) volume superblock. Single-node catalog leaf
only (no b-tree split yet).

## Locked on-disk facts (md.6 + NotebookLM)

- **Name hash**: CRC32C (poly 0x82F63B78, init 0xFFFFFFFF, no final xor) over
  UTF-32-LE of the case-folded name; low 22 bits. `name_len_and_hash` =
  hash<<10 | (len incl NUL).
- **j_inode_val**: 92-byte prefix (parent@0, private@8, ts@16/24/32/40,
  internal_flags@48=0x8000, nchildren/nlink@56, write_gen@64=1, uid@72, gid@76,
  mode@80, uncompressed_size@84=0) + xfields blob (num@92, used@94,
  x_field_t[]@96 = {type,flags,size}, then 8-byte-aligned data). NAME=type4
  flags0x02; DSTREAM=type8 flags0x20 (40B: size, alloced, crypto=0,
  total_written=size, total_read=0). Xfields ascending by type.
- **j_drec hashed**: jkey(type9) + name_len_and_hash(u32) + name+NUL; val 18B =
  file_id(u64) + date_added(u64) + flags(u16; DT_REG=8, DT_DIR=4).
- **DSTREAM_ID**: jkey(type6) key; val 4B = refcnt(u32)=1. Required for any file
  with data (fsck "missing dstream id objects" without it).
- **FILE_EXTENT**: jkey(type8)+logical_addr key; val 24B = len_and_kind(byte
  length, flags hi) + phys_block + crypto=0. Extent length block-aligned.
- **extentref (phys_ext)**: jkey(type2 = phys_block) key; val 20B =
  len_and_kind(kind<<60 | block_count, kind=1 NEW) + owning_obj_id + refcnt.
  Tree is variable-kv bt_flags 0x52, sorted by phys_block.
- **Catalog comparator**: obj_id asc, obj_type asc, then DIR_REC by
  name_len_and_hash (numeric) then name bytes.
- **VSB counters** (fsck-validated): next_obj_id@0xB0 (monotonic),
  num_files@0xB8, num_directories@0xC0, fs_alloc_count@0x58 (+= data blocks).

## Crash-safety / quality

- All slicing routed through bounds-checked get/get_mut helpers (no panic in
  library paths). clippy clean. 44 lib unit tests pinned to kernel oracles.
- 7 decisive scratch tests in `tests/snapshot_scratch.rs`
  (snapshot create/delete, create_file, write_file, multi-block, mkdir, unlink,
  rename, multi-file). Gate: `APFS_WRITE_SCRATCH=1`.

## Remaining (not yet done)

- **B-tree split** (multi-node catalog): once a single 4096-byte leaf fills
  (large directories / many files), need internal nodes + child pointers +
  node split. Needs a NotebookLM round for internal-node layout. This is the
  next big milestone.
- rmdir empty-check, truncate/append (modify existing extents), hard links
  (SIBLING_LINK/MAP, refcnt > 1), non-ASCII/Unicode case-fold for the name
  hash, multi-extent fragmented files (current code handles runs but untested
  for fragmentation).
- Real-USB final proof + M7 squash-merge (the snapshot path's USB gate also
  still pending).

## Resume

Branch `feat/m7a-write-snapshot`. Run a decisive test:
`APFS_WRITE_SCRATCH=1 cargo test -p apfs-write --test snapshot_scratch <name> -- --nocapture`.
Kernel-oracle workflow: gen-scratch -> attach via hdiutil -> do op via macOS
-> detach -> decode raw bytes with a from-scratch Python script.
