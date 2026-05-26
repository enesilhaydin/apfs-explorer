# M7d NotebookLM — catalog B-tree split (internal nodes, multi-node FSTREE)

> **STATUS: ANSWERED + LOCKED `apfs-spec:m7d-confirmed` (2026-05-22).**
> All empirical values confirmed (Apple PDF p.62, 121, 124, 126, 128-131).
> Implementation locks:
> - btn_flags: ROOT=0x1, LEAF=0x2, internal-non-root=0x0, single-node=0x3.
>   btn_level: leaves 0, parent 1, root = height-1.
> - internal value = child VIRTUAL oid (8B), resolved via volume omap; variable-
>   kv (kvloc, v.len=8); pivot = exact full copy of child's first key.
> - btree_info_t footer ONLY on the root node → non-root nodes: value area ends
>   at block_size (4096), NOT 4096-40. (Fix pack/parse to take is_root.)
> - root footer: key_count = total leaf keys, node_count = total nodes,
>   longest_key/val = tree-wide max.
> - split ~50/50 by bytes; fsck checks soundness/order only, not balance.
> - leaf split: left keeps original oid (COW), right = new oid. FIRST split:
>   root_tree_oid STABLE (becomes internal), two fresh child oids.
> - new fstree-node oids from nx_superblock.nx_next_oid @0x58 (txn.alloc_oid),
>   monotonic; each node is a virtual object needing an omap {oid,xid}->paddr.
> - all COW'd nodes on the split path get the new o_xid; omap gains/updates
>   their mappings; orphaned old paddrs left allocated (fsck-tolerated, as with
>   our other COWs).
> - obj_phys (md.6 decode): ROOT node o_type=0x2 (OBJ_VIRTUAL|OBJECT_TYPE_BTREE),
>   non-root node o_type=0x3 (OBJ_VIRTUAL|OBJECT_TYPE_BTREE_NODE); o_subtype=0x0e
>   (OBJECT_TYPE_FSTREE) for both; o_oid = the node's virtual oid (root keeps
>   root_tree_oid). TOC reserve may be padded (kernel used 576) but tight
>   nkeys*8 is fsck-accepted.

Next milestone: when a single 4096-byte catalog leaf fills, split it and grow
the FSTREE into a multi-level B-tree. We forced a real kernel split (200 files
in one directory) and decoded the result; questions below confirm the spec
semantics before we implement (md.6).

## Decoded kernel oracle (200-file dir, scratch)

ROOT (internal) node:
```
flags = 0x1 (BTNODE_ROOT only, NOT LEAF)   level = 1   nkeys = 20
toc_len = 192 (20 kvloc_t used = 160; 24-slot reserve)
btree_info_t footer (root only): bt_flags=0x42, key_size=0, val_size=0,
   longest_key=31, longest_val=180, key_count=1021, node_count=21
TOC entry: key = a full catalog key (the child subtree's first key);
   value = 8 bytes = child node VIRTUAL oid (0x407, 0x410, 0x409, ...)
```
Child leaf node:
```
flags = 0x2 (BTNODE_LEAF only)   level = 0   nkeys = 71   (its own virtual oid)
```
oid allocation: new node virtual oids (0x406..0x419) come from
`nx_superblock.nx_next_oid @ 0x58` (decoded next value 0x41a), NOT the volume's
`apfs_next_obj_id` (which is for inode ids). root_tree_oid = 0x404.

## SECTION A — node flags & levels

- **Q-A1**: Confirm btn_flags: ROOT-only = 0x1, LEAF-only = 0x2, an internal
  non-root node = 0x0, and the original single node was ROOT|LEAF = 0x3.
  When the tree has exactly 2 levels the root is the only internal node, so
  internal == ROOT here; what flags does a level-1 internal node carry once a
  3rd level appears (level-2 root = 0x1, level-1 internal = 0x0)?
- **Q-A2**: btn_level: leaves = 0, their parent = 1, etc. Confirm the root's
  level equals tree height − 1, and every node stores its own level.

## SECTION B — internal node values (child pointers)

- **Q-B1**: Confirm an internal-node TOC value is the child node's **virtual
  oid** (8 bytes), resolved through the VOLUME omap at the volume xid — the
  same mechanism as the root_tree_oid. (FSTREE is a virtual, NOT physical,
  b-tree, so children are oids, not paddrs.) Apple PDF page?
- **Q-B2**: The **pivot key** stored in the parent for each child — is it an
  exact full copy of the child subtree's FIRST key (no truncation)? We observe
  the parent's key[i] equals the smallest key in child[i].
- **Q-B3**: Are internal-node keys the same variable-kv `kvloc_t` TOC layout as
  leaves (key area up from DATA_BASE+table_space, values down)? Internal values
  are fixed 8 bytes — does that change anything (still variable-kv, or does the
  kernel mark internal nodes FIXED_KV)? We observed bt_flags 0x42 (no FIXED_KV)
  on the root.

## SECTION C — btree_info_t footer (root only)

- **Q-C1**: Confirm the `btree_info_t` 40-byte footer exists ONLY on the ROOT
  node; non-root nodes (internal and leaf) have NO footer, so their value area
  ends at the full block size (4096), not 4096−40. (This changes val-area math
  for every non-root node.)
- **Q-C2**: `bt_key_count` = total number of keys across ALL leaf nodes
  (1021 observed); `bt_node_count` = total nodes in the tree including the root
  (21 = 1 root + 20 leaves). Confirm both, and that the root footer is the only
  place they live.
- **Q-C3**: `longest_key` / `longest_val` in the footer — are these the max over
  the WHOLE tree (all nodes), or just the root? We observed longest_val=180
  (an inode value) and longest_key=31 (an xattr key), which are leaf values, so
  it appears to be tree-wide.

## SECTION D — the split algorithm

- **Q-D1**: When a leaf would overflow on insert, how does the kernel choose
  the split point — by roughly half the byte usage, half the key count, or a
  fill-factor? Does fsck care about balance, or only that each node is valid and
  keys are globally sorted?
- **Q-D2**: On a leaf split, the original leaf's virtual oid — is it kept for
  the left half (and a new oid allocated for the right), or are two fresh oids
  allocated and the old node freed? (Affects omap bookkeeping.)
- **Q-D3**: When the FIRST split happens (single ROOT|LEAF node), the kernel
  creates a new ROOT internal node (new oid? or does root_tree_oid stay and the
  node is rewritten as internal?) plus the two leaves. We observe root_tree_oid
  is still 0x404 (unchanged) and it's now the internal root — so the root oid is
  STABLE across splits and only its contents change to internal. Confirm: the
  root keeps root_tree_oid; children get fresh oids from nx_next_oid.

## SECTION E — oid allocation & omap

- **Q-E1**: Confirm new FSTREE node virtual oids are drawn from
  `nx_superblock.nx_next_oid @ 0x58` (monotonic, container-wide), and the
  superblock field is bumped per allocation. Is there any alignment/reservation
  (oids look sequential 0x406, 0x407, ...)?
- **Q-E2**: Each node (root + every leaf/internal) is a VIRTUAL object that
  needs an omap record {oid, xid} → paddr in the VOLUME omap. After a split the
  volume omap b-tree gains entries for the new nodes (and itself may need to
  split if it overflows). For our scale (≤ ~20 fstree nodes) the volume omap is
  still a single node — confirm the omap is a SEPARATE physical b-tree keyed by
  {oid, xid} and that adding fstree-node mappings is the same insert we already
  do for the single root mapping.
- **Q-E3**: Within one transaction we may COW many nodes (split path from leaf
  up to root). Do all of them get o_xid = the new transaction xid, and old
  versions become unreachable (and for a no-snapshot volume, left allocated /
  leaked like our other COWs, which fsck tolerated)?

## SECTION F — fsck cross-checks for multi-node trees

- **Q-F1**: List what fsck validates for a multi-level FSTREE: global key
  ordering across leaves; each parent pivot equals child's first key; node_count
  / key_count footer accuracy; every child oid resolvable via the omap; no
  orphan nodes. Anything else (e.g. minimum fill / balance)?

## Recommendation

Cite Apple PDF pages for: internal-node value = child oid, the root-only
btree_info_t footer, nx_next_oid as the fstree-node oid source, and the
b-tree node header (btn_flags/btn_level). Confirm or correct each empirical
value. Then we lock `apfs-spec:m7d-confirmed` and implement leaf split +
internal-root growth (2-level first, then general N-level).
