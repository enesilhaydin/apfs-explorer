# M7a-RT round3 NotebookLM questions (md.6 LOCKED protocol)

> Status: PENDING NotebookLM. The questions below block populated-real-USB
> COW write correctness. Two implementation attempts that guessed answers
> regressed scratch (`Snapshot metadata tree is invalid`). Per md.6, stop
> and ask the authoritative source.
>
> Source the user should ask: NotebookLM with the full Apple File System
> Reference loaded (notebook `apfs-reference-apple-file-syst`). Cross-check
> with `apfs-spec:linux-apfs-rw` (apfs.h, apfs_raw.h, btree.c, spaceman.c,
> transaction.c) and `apfs-spec:libfsapfs` (Apple File System (APFS).asciidoc)
> in our committed KB.
>
> After answers come back, lock them in
> `docs/superpowers/specs/2026-05-20-m7a-write-spec-notes-round3.md` and
> implement the writer to match. Re-prove on scratch (regression guard) +
> real-USB (the actual arbiter). `fsck_apfs -n` CLEAN is the final test.

Today's real-USB state on `/dev/disk15s2` after the snap-meta tree fix
(`e8aa896`) and the omap-btree free-list sentinel fix (`c4268af`) landed:

- ✅ `Snapshot metadata tree is invalid` — GONE
- ✅ `btn: invalid btn_key_free_list offset 0` — GONE
- ❌ `error: btn: invalid key (0, 16)` on omap btree root (oid 18631,
     xid 0x9A, subtype 0xb=OBJECT_TYPE_OMAP, flags 0x7=ROOT|LEAF|FIXED_KV_SIZE)
- ❌ `overallocation (0x48b4+21) bitmap address (0x45d7)` — grows every
     run, confirming in-place CIB bitmap mutation (Q-B violation)

We attempted to capture macOS's own byte-level snapshot template
(`tools/decode_macos_template.py` + `tests/snapshot_scratch.rs
macos_template_tests::capture_macos_snapshot_template`). On macOS 15.7.4
the legacy `diskutil apfs createSnapshot` verb is removed; we switched
to `tmutil localsnapshot` but it is a silent no-op on this external
APFS volume (xid did not advance). We have a high-fidelity capture
harness ready to use once we resolve the snapshot-creation path on
modern macOS — but for now the authoritative spec answers below are
the unblocker.

## Q-C series — Container OMAP B-tree insertion (populated leaf)

### Q-C1 fixed-kv TOC entry struct

For a B-tree node with `BTNODE_FIXED_KV_SIZE` set (container omap leaf
is the canonical example), confirm the exact on-disk TOC entry struct:

```c
struct kvoff_t {
    uint16_t key_off;
    uint16_t val_off;
} __attribute__((packed));  // 4 bytes per entry
```

- Apple PDF page reference?
- Is this distinct from `kvloc_t` (8 bytes) used by variable-kv trees
  like the snap-meta tree?

### Q-C2 key_off reference point

In a fixed-kv leaf, when the TOC entry's `key_off` is read, what is it
measured FROM?

(a) From DATA_BASE (= byte 56 of the node, i.e. start of node body
    after btree_node_phys header)?
(b) From the start of the key area (= DATA_BASE + btn_table_space.len)?

Please cite the exact Apple PDF page that says this. Cross-check with
linux-apfs-rw `btree.c` for the kernel's interpretation.

### Q-C3 val_off reference point and direction

In a fixed-kv leaf, when the TOC entry's `val_off` is read:

(a) For a ROOT node (BTNODE_ROOT flag set, has 40-byte btree_info_t at
    end of block): is `val_off` measured from `(bsz - 40)` and grows
    toward LOWER addresses? That is, the absolute byte offset of the
    value is `v_abs = (bsz - 40) - val_off`?
(b) For a non-root leaf (no btree_info footer): is `val_off` measured
    from `bsz` and grows toward LOWER addresses? That is,
    `v_abs = bsz - val_off`?

We have observed `v_abs = val_area_end - val_off` empirically on
scratch where val_area_end depends on whether the node is root. Please
confirm or correct.

### Q-C4 TOC sort requirement after insertion

After we insert a new `(oid, xid) → paddr` record into a populated
fixed-kv omap leaf where existing entries occupy specific slots:

(a) MUST TOC entries appear in sorted key order, with the new entry
    inserted at the correct sorted position (shifting later TOC entries
    to make room)?
(b) OR may TOC entries appear in insertion order, with sort discovered
    on read by parsing the keys?
(c) When sorting omap keys: by `(obj_id ASC, xid ASC)` — confirm?

Cite Apple PDF page + linux-apfs-rw `btree.c` lookup/insert code.

### Q-C5 sparse leaf placement vs compaction

If the existing leaf has gaps (a previous deletion left holes in the
key or value area or in the TOC), when we INSERT a new entry can we:

(a) Place the new key at the end of the packed-key region without
    compacting (leaving the holes)?
(b) MUST we compact (relocate keys+values to eliminate holes) on every
    insert?
(c) Some hybrid (compact only when free-list is non-empty)?

What is the kernel's actual behaviour? linux-apfs-rw transaction.c
should have the answer.

### Q-C6 exact header field values for compacted N-key leaf

For a compacted fixed-kv leaf with N keys (no fragmentation,
no free-list entries, root with btree_info_t footer), please give the
EXACT values for these 4 nloc_t fields:

```
@40 btn_table_space:  off=?    len=?
@44 btn_free_space:   off=?    len=?
@48 btn_key_free_list: off=?   len=?
@52 btn_val_free_list: off=?   len=?
```

Variables: N (key count), KEY_SIZE=16 (omap), VAL_SIZE=16 (omap),
DATA_BASE=56, BTREE_INFO_SIZE=40, bsz=4096.

Especially: is `btn_free_space.off` measured from DATA_BASE or from
key_area_start? And what is `btn_free_space.len`?

Round2 LOCKED Q-A established `btn_key_free_list/btn_val_free_list =
{off=0xFFFF, len=0}` for empty lists — confirm this also applies to
the container omap btree (not just the snap-meta tree).

## Q-D series — Spaceman CIB copy-on-write per transaction

### Q-D1 spaceman_phys_t exact byte layout

Starting from byte 0 of the spaceman block (= start of obj_phys
header), please give the EXACT byte layout of `spaceman_phys_t` up
to and including `sm_dev[APFS_SD_MAIN].sm_addr_offset`:

```
@0  obj_phys           (32 bytes)
@32 sm_block_size?     (...)
... ?
```

Specifically:
- What is the offset of `sm_dev[APFS_SD_MAIN]`?
- Within that, what is the offset of `sm_addr_offset` (the byte offset
  WITHIN the spaceman block where the CIB paddr array starts)?
- What is the offset of `sm_cib_count`?

Cite Apple PDF page + linux-apfs-rw `apfs_raw.h` struct `apfs_spaceman_phys`.

### Q-D2 CIB COW policy per transaction

(a) Does EVERY transaction COW EVERY CIB block (allocating a fresh
    paddr per CIB, regardless of whether its bitmap actually changed)?
(b) OR only CIBs whose bitmap was actually mutated this transaction?

(c) Where is the OLD CIB paddr released? Options:
    - Into the spaceman free queue `sm_fq[APFS_SFQ_MAIN]` for deferred
      reclamation by the reaper?
    - Into the spaceman's main bitmap immediately?
    - Into a per-checkpoint orphan list?

Cite Apple PDF page + linux-apfs-rw `spaceman.c`.

### Q-D3 CIB-managed bitmap block COW

`chunk_info_t.ci_bitmap_addr` points to the actual allocation bitmap
block for that chunk. When a CIB is COW'd:

(a) Is the bitmap block ALSO COW'd to a new paddr every txn?
(b) OR does the bitmap stay at the same paddr (shared between
    checkpoints, immutable)?
(c) OR is it overwritten in place (legal because old checkpoint
    doesn't reference the bitmap directly)?

### Q-D4 sm_free_count update timing

When a snapshot-create allocates 5 new blocks for
`{live-VSB, frozen-VSB, snap-meta tree node, new-extentref-tree root,
new-omap-btree root}`:

(a) Does `spaceman_phys.sm_dev[MAIN].sm_free_count` decrease by 5
    atomically in the NEW spaceman block written at commit time?
(b) Is the decrement deferred to a later checkpoint (orphan/reaper)?
(c) Does it require a per-chunk update too (ci_free_count) for each
    chunk that lost a block?

### Q-D5 internal-pool (IP) bitmap rotation

linux-apfs-rw has a `write_dirty_ip_bitmaps` pattern that rotates
internal-pool bitmaps before serialising the spaceman:

(a) What is the relationship between IP bitmaps and main-pool CIB
    bitmaps? Different bitmap blocks for different allocation classes?
(b) Why rotate the IP bitmap (move dirty bitmap to a new offset slot)
    rather than overwrite in place?
(c) Is this rotation also needed for main-pool CIB bitmaps, or only
    for IP?

### Q-D6 sm_fq free queue record format

When blocks are released to `sm_fq[APFS_SFQ_MAIN]` (whose
`sfq_tree_oid` points to a B-tree):

(a) What is the EXACT on-disk record format for one entry?
(b) Is the key `(xid: u64, paddr: u64)` packed?
(c) Is the value present at all (Apple says "ghost" records for the
    free queue), or is the key-only encoding signalled by some flag?
(d) Cite Apple PDF page + linux-apfs-rw `apfs_raw.h` definition.

## How the user should use this

1. Open NotebookLM with the apfs-reference notebook.
2. Paste each Q-C / Q-D question one at a time (or as a single batch).
3. Capture answers verbatim. Cite Apple PDF page numbers.
4. Save the answers back into a new file
   `docs/superpowers/specs/2026-05-20-m7a-write-spec-notes-round3.md`
   keyed by question label (Q-C1, Q-D1, ...).
5. Then ping me; I will lock the answers into `apfs-spec:m7a-confirmed`
   KB and implement the writer to match — scratch regression + real-USB
   `fsck_apfs -n` CLEAN are the final gates.
