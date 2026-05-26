# M7a-RT round7 NotebookLM — snap-meta value 8-byte alignment

Round6 LOCKED resolved the "byte-identical-but-rejected" anomaly: the
{0, 0} btn_free_space encoding causes silent fsck rollback to the
previous checkpoint, which made it LOOK like scratch passed. With
container omap free_space now correctly written AND snap-meta tree
table_space reserve preserved at 64, fsck reaches the snap-meta tree
and still rejects with "Snapshot metadata tree is invalid". The
remaining suspect is value placement alignment.

## The misalignment observation

Our current snap-meta tree node has:
- `btn_table_space = {0, 64}` (matches kernel reserve)
- `btn_free_space  = {27, 3842}` (= keys end at 27 bytes into key area; free gap = 3842 bytes)
- `btn_key_free_list = {0xFFFF, 0}`
- `btn_val_free_list = {0xFFFF, 0}`
- TOC[0] SNAP_METADATA: key_off=0, key_len=8, val_off=59, val_len=59
- TOC[1] SNAP_NAME:     key_off=8, key_len=19, val_off=67, val_len=8
- key_area_start = 56 + 64 = 120

Key positions (absolute):
- key[0] at 120..128, 120 % 8 = 0 ✓
- key[1] at 128..147, end 147 % 8 = 3 (UNALIGNED end of key area)

Value positions (absolute, from val_area_end=4056 backward):
- val[0] at 3997..4056, 3997 % 8 = **5** (UNALIGNED)
- val[1] at 3989..3997, 3989 % 8 = **5** (UNALIGNED)

Apple PDF page 297 explicitly says: *"Keys and values are normally
aligned to eight-byte boundaries when stored."*

But our snap-meta tree has `bt_flags & BTREE_KV_NONALIGNED (0x40)` set
in btree_info_t footer — copying from the kernel-format empty snap-meta
tree which sets exactly this. The flag name suggests "kv not aligned",
implying alignment is OPT-OUT for this tree.

## Q-M: Is BTREE_KV_NONALIGNED truly opt-out of alignment?

(a) If `BTREE_KV_NONALIGNED` is set, are keys and values allowed to
    sit at arbitrary byte offsets (any value of `key_off`, `val_off`
    is legal)?
(b) Or does fsck still require 8-byte alignment for safety despite
    the flag? Maybe the flag controls a different aspect (like the
    individual KV size padding, not the slot placement)?
(c) Is Apple PDF page 297 a "normally" rule that fsck enforces even
    when the flag is set?

Please cite Apple PDF page that disambiguates `BTREE_KV_NONALIGNED`
exact semantics.

## Q-N: Snap-meta value placement convention

For a snap-meta tree node with 2 entries (SNAP_METADATA TOC[0],
SNAP_NAME TOC[1]) where:
- SNAP_METADATA val = 59 bytes (val_off in TOC[0])
- SNAP_NAME val = 8 bytes (val_off in TOC[1])

What is the exact placement convention the kernel uses?

(a) Pack values back-to-back starting from val_area_end (= bsz-40 for
    root). val[0] = bytes 3997..4056 (val_off=59), val[1] = 3989..3997
    (val_off=67)? — This is what we do.
(b) Each value rounded up to 8-byte slot before placing the next. So
    val[0] occupies a 64-byte slot (59 padded to 64): 3992..4056 → val_off=64
    (but TOC[0].val_len would stay 59 as actual data length).
    val[1] occupies an 8-byte slot: 3984..3992 → val_off=72.
(c) Values placed at sorted-order (smaller-key-first) closer to
    val_area_end? Or larger-first?
(d) Some other convention?

Cite Apple PDF page.

## Q-O: Snap-meta key area packing — is key_end alignment required?

In our current emission key[1] ends at offset 147 (key_area_start +
key_bytes = 120 + 27). 147 is NOT 8-aligned.

(a) Is fsck strict about the END of the key area being 8-aligned
    (requiring the writer to pad key[1] to 24 bytes so end = 152)?
(b) Or is the unaligned end acceptable (with BTREE_KV_NONALIGNED set)?
(c) If padding is required, do we pad by NUL bytes (and TOC k_len
    stays at the actual key length), OR do we have to increase
    name_len to reflect the padding?

## Q-P: Validation order — what does fsck check first?

The fsck error "Snapshot metadata tree is invalid" is the only
diagnostic. To narrow down which field's invariance fails, what is
the order fsck checks?

(a) Header field bounds (table_space + free_space + free_lists make
    coherent regions)?
(b) TOC entries fall within table_space.len capacity?
(c) Each key bytes' alignment to 8?
(d) Each value bytes' alignment to 8?
(e) Inter-key sort order?
(f) Key contents match expected type (j_snap_metadata_key,
    j_snap_name_key)?
(g) Value contents per the key's record-type schema?
(h) Reachability of sblock_oid (frozen VSB) from snap_metadata_val?

Knowing the order would let us target the right invariant.

## How to use

Open NotebookLM. Paste this file. Capture answers verbatim with
Apple PDF page citations. Save into
`2026-05-20-m7a-write-spec-notes-round7.md` and ping me.

Once round7 lands, I'll implement the alignment/packing fix carefully
and re-prove scratch CLEAN + (after USB reformat) real-USB CLEAN.
