# M7a write-spec notes — ROUND 2 (real-USB-surfaced ambiguities, md.6 LOCKED)

The near-empty scratch image hid two write-side on-disk encodings our READER
never interprets. A populated real USB container exposed them via
`fsck_apfs -n`. md.6 NotebookLM answers (notebook `apfs-reference-apple-file-syst`)
are DEFINITIVE (Apple doc explicit, not silent). Cross-validate against
`apfs-spec:linux-apfs-rw` (writer reference) for the step-by-step algorithm
(the Apple doc gives field semantics + offsets but no pseudocode). Final
arbiter remains `fsck_apfs -n` CLEAN on scratch AND the real USB.

## Q-A — `btree_node_phys_t` free-list / table-space construction (LOCKED)

Field offsets (after the 32-byte `obj_phys` header; each is an `nloc_t`
`{off: u16, len: u16}` = 4 bytes):

| field              | offset |
|--------------------|--------|
| `btn_table_space`  | 40     |
| `btn_free_space`   | 44     |
| `btn_key_free_list`| 48     |
| `btn_val_free_list`| 52     |
| `btn_data` (start) | 56     |

For a COMPACTED node with `N` entries and NO internal fragmentation:

- `btn_table_space`: `off = 0` (TOC offset is measured from the start of
  `btn_data`). `len = N * sizeof(toc_entry)` where `toc_entry` is
  `kvoff_t` (4 bytes) when `BTNODE_FIXED_KV_SIZE` is set, else `kvloc_t`
  (8 bytes).
- `btn_free_space`: `off` = distance from the start of the key area to the
  start of the shared free space = the combined total size of all `N`
  keys (no fragmentation ⇒ exactly the sum of key sizes). `len` = total
  size of the shared free space between the key area and the value area.
- `btn_key_free_list` / `btn_val_free_list`: empty ⇒ `off = 0xFFFF`
  (`BTOFF_INVALID`), `len = 0`. (Plain `0` for `off` is INVALID and is
  exactly what the kernel rejected: `invalid btn_key_free_list offset 0`.)

Per-tree TOC entry type (CRITICAL — the prior broken fix conflated these):
- OMAP B-tree nodes: `omap_key_t`/`omap_val_t` are fixed size ⇒ node has
  `BTNODE_FIXED_KV_SIZE`; TOC array of 4-byte `kvoff_t`.
- Snapshot-metadata tree: `j_snap_metadata_val_t` has a variable-length
  `name` ⇒ NOT `BTNODE_FIXED_KV_SIZE`; TOC array of 8-byte `kvloc_t`.

## Q-B — Space Manager CIB copy-on-write (LOCKED)

1. The chunk-info block (`OBJECT_TYPE_SPACEMAN_CIB`) MUST be copy-on-write
   to a NEW physical address every transaction. In-place mutation of an
   existing CIB (flipping bitmap bits, decrementing `ci_free_count`) is
   FORBIDDEN — old checkpoints still reference the old CIBs (read-only
   history protection).
2. Back-patching a PREVIOUS checkpoint's on-disk ephemeral spaceman is NOT
   supported and is WRONG: it permanently breaks that object's
   immutability and its Fletcher-64. Ephemeral objects are mutated in RAM
   only and always written fresh, at a new location, as part of a NEW
   checkpoint. ⇒ `sync_prev_spaceman_free_count` (which patches the prior
   checkpoint's spaceman on disk) is a spec violation and must be removed.
   Correct model: each write transaction creates entirely NEW CIB
   object(s) + a NEW spaceman object + a NEW current checkpoint superblock,
   chained together; never touch the prior checkpoint.

This is the documented cause of the `overallocation … bitmap address` +
`checkpoint failed consistency check`: validation found a mismatch because
the old structures were mutated instead of fully COW-chained anew.

## Dual-evidence (md.6 mandatory, before trusting the implementation)

Controlled known value = decode what macOS ITSELF writes:
- On the real USB, run `diskutil apfs createSnapshot` (kernel-authored).
  Python (zero shared code) decode: the kernel-written OMAP leaf node's
  four nloc fields (assert the Q-A construction) and the CIB — capture the
  CIB block address across two checkpoints (assert it CHANGES = COW, per
  Q-B). Our writer must produce byte-equivalent structure; `fsck_apfs -n`
  CLEAN on scratch AND real USB is the final arbiter.
