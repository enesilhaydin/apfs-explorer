# M7b NotebookLM — file creation (empty file in root dir) full question set

> **STATUS: ANSWERED + LOCKED `apfs-spec:m7b-confirmed` (2026-05-22).**
> All empirical values confirmed against Apple PDF. Key locks:
> - inode/drec/xfield offsets confirmed (PDF p.90-96); internal_flags 0x8000
>   = emit for kernel parity (not fsck-mandatory); write_gen starts at 1.
> - empty file: NAME xfield only, NO dstream/extent/extentref/sibling.
> - hash: CRC32C over UTF-32-LE, ASCII lowercase fold matches Apple table.
> - catalog key order: obj_id → obj_type → name_len_and_hash(LE: hash,len) →
>   name bytes. variable-kv → bt_key_size/val_size = 0; longest_key/val =
>   actual max present. TOC ordered by sorted key; key_area base =
>   DATA_BASE + table_space.len. (We keep the kernel's 8-slot reserve.)
> - VSB: bump next_obj_id (@176, monotonic), num_files (@184); fsck validates
>   num_files/num_directories and fs_alloc_count.
> - virtual catalog COW: new block keeps o_oid = root_tree_oid (virtual),
>   omap maps {oid, new_xid} → new_paddr; full checkpoint envelope as M7a.
> - **Q-E4: old previous-xid blocks should go to spaceman free queue (sm_fq),
>   NOT immediate free.** Scratch fsck is the final arbiter (md.6).

We are implementing pure-Rust APFS **file creation** (create one empty
regular file in the root directory) with copy-on-write, validated on a
scratch image with fsck + mount-readback + independent Python re-decode.

We already decoded a kernel-produced empty file ("hello.txt") from a real
macOS-formatted scratch image and have empirical byte values. We want
NotebookLM to confirm the SPEC source / exact semantics for each, so we are
not relying on a single empirical sample (md.6: spec + real-disk cross-check).

Context constants from the decoded scratch volume:
- incompatible_features = 0x1 (case-insensitive → hashed DREC keys)
- ROOT_DIR_INO_NUM = 2; the new file got inode 20; VSB next_obj_id bumped.

## SECTION A — j_inode_val (the inode value record)

Decoded empty-file inode (116 bytes):
```
@0   parent_id      = 2
@8   private_id     = 20
@16  create_time    = <now ns>
@24  mod_time       = <now>
@32  change_time    = <now + ~34us>
@40  access_time    = <now>
@48  internal_flags = 0x8000
@56  nchildren/nlink (union) = 1
@60  default_protection_class = 0
@64  write_generation_counter = 1
@68  bsd_flags = 0
@72  owner = 99
@76  group = 99
@80  mode = 0o100644 (S_IFREG | 0644)  (u16)
@82  pad1 = 0
@84  uncompressed_size = 0  (u64)
@92  xf_num_exts = 1 (u16), @94 xf_used_data = 16 (u16)
@96  x_field_t { type=4 (INO_EXT_TYPE_NAME), flags=0x02, size=10 }
@100 "hello.txt\0" padded to 16
```

- **Q-A1**: Confirm the j_inode_val field layout & offsets above against Apple
  PDF (apfs_inode_val / j_inode_val). Is @84 `uncompressed_size` or a pad?
  Apple PDF page reference?
- **Q-A2**: `internal_flags = 0x8000` — which `j_inode_flags` bit is this?
  (e.g. INODE_HAS_FINDER_INFO, INODE_WAS_EVER_PRESENT, INODE_IS_APFS_PRIVATE…)
  What is the canonical set of flags a NEWLY created regular file must have?
  Is 0x8000 mandatory or fsck-optional?
- **Q-A3**: `write_generation_counter = 1` on creation — is this required, and
  does it increment on every modification? Does fsck validate it?
- **Q-A4**: For an EMPTY file, the kernel emits ONLY the NAME xfield (type 4),
  NO DSTREAM (type 8) xfield. Confirm: a zero-length file has no j_dstream
  xfield and `uncompressed_size`/no FILE_EXTENT records. When does a DSTREAM
  xfield first appear (first byte written)?
- **Q-A5**: `x_field_t` layout {x_type u8, x_flags u8, x_size u16} and
  `xf_used_data` = sum of round_up(x_size, 8). Confirm x_flags=0x02 meaning
  (XF_DATA_DEPENDENT?) for the NAME field. Apple PDF page?
- **Q-A6**: For the NAME xfield, is `x_size` the byte length INCLUDING the
  trailing NUL (we observed 10 for "hello.txt")? And is the stored name
  NUL-terminated then zero-padded to 8 bytes?

## SECTION B — j_drec (directory entry record)

Decoded DREC for hello.txt under root dir (ino 2):
```
key: jkey=encode(type=9 DIR_REC, id=2) | name_len_and_hash=0xe25b900a | "hello.txt\0"
     name_len_and_hash: low 10 bits = 10 (len incl NUL), high 22 bits = 0x3896e4 (hash)
val (18 bytes): file_id=20 (u64) | date_added=<now> (u64) | flags=0x8 (u16)
```

- **Q-B1**: Confirm hashed key layout `j_drec_hashed_key`: jkey(8) +
  name_len_and_hash(u32: len[9:0], hash[31:10]) + name. Apple PDF page?
- **Q-B2**: The name hash. We empirically matched: **CRC32C** (poly
  0x82F63B78, init 0xFFFFFFFF, NO final xor) over the **UTF-32-LE** encoding
  of the name, take low 22 bits. Confirm the algorithm. Critically: what is
  the exact CASE-FOLD / normalization applied before hashing on a
  case-insensitive (0x1) vs normalization-insensitive (0x8) volume? Is it
  lowercasing, or Apple's specific normalizationForm? (We need this for
  non-ASCII / mixed-case names.) Apple PDF page + the apfs_hash routine?
- **Q-B3**: `j_drec_val` layout: file_id(u64) + date_added(u64) +
  flags(u16). Is `flags` purely the low-4-bit dirent type (DT_REG=8,
  DT_DIR=4) or are higher bits used? Is there a `sibling_id`/xfields tail for
  hard-linked entries (not for our single-link case)?
- **Q-B4**: Is `date_added` in the DREC the same epoch as inode timestamps
  (UNIX-epoch nanoseconds)? Should DREC.date_added equal inode.create_time?

## SECTION C — catalog (fsroot) B-tree ordering & node layout

The fsroot is a variable-kv (kvloc_t) B-tree. Single-node (ROOT|LEAF) on a
fresh volume. We need to INSERT 2 records (the DREC + the INODE) into the
sorted node.

- **Q-C1**: The canonical catalog key COMPARATOR. We observe ordering:
  primary = obj_id ascending, secondary = obj_type ascending, tertiary (for
  DIR_REC) = name. For DIR_REC tertiary, is it sorted by **name-hash** then
  name, or by name bytes directly? Apple PDF / apfs spec for the b-tree
  key order of FSTREE.
- **Q-C2**: For variable-kv leaf nodes: TOC entry kvloc_t = {k.off u16,
  k.len u16, v.off u16, v.len u16}. Keys grow up from key_area =
  DATA_BASE + table_space.off + table_space.len(?); values grow down from
  (block_end - btree_info_t(40) for root). Confirm exact key_area base and
  value_area_end formulas, and whether the TOC must be stored in sorted key
  order (vs keys physically sorted in the key area).
- **Q-C3**: `table_space` (toc) reservation for a variable-kv FSTREE node:
  on the decoded fresh node, toc_len = 64 with 4 keys. What is the rule for
  table_space.len growth as keys are added (kvloc is 8 bytes each → is it
  rounded/reserved in blocks of N)? After inserting to 6 keys, what toc_len
  does fsck expect?
- **Q-C4**: `btn_free_space` {off,len} maintenance after insert: how exactly
  are off/len recomputed? Is free_space.off the offset (from DATA_BASE) of
  the start of the gap between key-area-end and value-area-start?
- **Q-C5**: `btn_key_free_list` / `btn_val_free_list` — for a freshly built
  node we set both to {0xFFFF (BTOFF_INVALID), 0}. Is that correct when there
  are no freed slots, or must they track reclaimed key/val fragments?
- **Q-C6**: Root node `btree_info_t` footer fields after insert for FSTREE:
  bt_flags, key_size/val_size (0 for variable-kv?), longest_key, longest_val,
  key_count, node_count. What are longest_key/longest_val supposed to be for
  a catalog leaf (max over all keys/vals present)?

## SECTION D — parent directory & volume superblock updates

- **Q-D1**: When adding a child to a directory, which parent-dir inode
  fields change? We expect nchildren += 1 and mod_time/change_time updated.
  Does the kernel also set INODE_MAINTAIN_DIR_STATS / update a DIR_STATS
  xfield? Is updating nchildren mandatory for fsck?
- **Q-D2**: Volume superblock counters: `apfs_next_obj_id` (@176) bumped past
  the new inode; `apfs_num_files` (@184) += 1. Confirm offsets and that
  fsck validates these counts. Are `apfs_num_directories`, `apfs_fs_alloc_count`
  also required to be consistent?
- **Q-D3**: `apfs_next_obj_id` semantics — is the next id simply
  monotonically increasing (we take current value as the new inode id, then
  store value+1)? Or is there reservation/alignment?

## SECTION E — copy-on-write of a VIRTUAL catalog tree

The fsroot root_tree_oid is VIRTUAL, resolved via the VOLUME omap at the
volume xid. To mutate the catalog we COW the node to a new physical block
and update the mapping.

- **Q-E1**: To COW a virtual object (the fsroot root node), do we: allocate a
  new paddr, write the mutated node with obj_phys.o_xid = new_xid and
  o_oid = root_tree_oid (the VIRTUAL oid, NOT the paddr), then insert/update
  an omap record {oid=root_tree_oid, xid=new_xid} → new_paddr in the VOLUME
  omap tree? Confirm o_oid for virtual objects = the virtual oid.
- **Q-E2**: Must we bump the VOLUME superblock's o_xid and the whole
  checkpoint to a new transaction id, exactly like snapshot create did?
  (We already have a working snapshot-create COW path that updates the
  container checkpoint, spaceman, container omap. Does a catalog mutation
  follow the identical commit envelope?)
- **Q-E3**: The VOLUME omap b-tree itself — when we add/replace the
  {root_tree_oid, new_xid} record, does the omap node need COW + its own
  o_xid bump, and does om_tree_oid in the volume omap_phys get updated (as we
  do in snapshot create)? Any om_snap_count interaction (should be none for a
  plain file create with no snapshots)?
- **Q-E4**: Spaceman: the new catalog node block + any omap node blocks are
  allocated from the spaceman; the OLD catalog/omap blocks become orphaned.
  For a no-snapshot volume, are the old blocks freed immediately (bitmap +
  free_count) or queued? (Contrast with snapshot-pinned blocks from M7a.)

## SECTION F — minimal correctness / ordering

- **Q-F1**: Is there a strict ORDER in which fsck wants the new records to
  appear, or any cross-check between DREC.file_id and the INODE.private_id
  and the INODE.parent_id == dir_ino? List the referential-integrity checks
  fsck performs for a (dir → drec → inode) triple.
- **Q-F2**: Anything else mandatory we might miss for a minimal valid file:
  a SIBLING_LINK/SIBLING_MAP record (only for nlink>1?), an INODE
  data-stream id, FILE_EXTENT (only when size>0?), or an entry in the
  extent-ref tree (only when extents exist?). Confirm an empty file needs
  NONE of these.

## Recommendation

Please cite Apple PDF page numbers for the inode/drec/xfield structures and
the b-tree key order, and confirm or correct each empirical value above.
After answers, we lock `apfs-spec:m7b-confirmed` and finish the create_file
implementation + scratch proof.
