# M7a-RT round5b NotebookLM — capacity edge case + minimum legal encoding

Follow-up to round5a (Q-J). These two questions address the
populated-container edge cases that the prior rounds did not surface.

## Q-K: btn_free_space.off when btn_table_space.len > nkeys * 4

Round4 Q-E said preserve `btn_table_space.len` from the existing node
(kernel reserves max TOC capacity upfront for fixed-kv). Concrete
example:

The existing kernel-written omap leaf had:
- `btn_table_space.len = 256` (reserved 64 TOC slots, kvoff_t × 64 = 256)
- `btn_nkeys = 5` (only first 5 × 4 = 20 bytes of TOC used)

After our COW insert of a 6th entry, we preserve table_space:
- `btn_table_space.off = 0`
- `btn_table_space.len = 256` (still reserved, per Q-E)
- `btn_nkeys = 6`
- `key_area_start = DATA_BASE + 256 = 312`
- Each omap key is 16 bytes ⇒ 6 keys occupy 312..408
- Per Q-G: `btn_free_space.off = 6 * 16 = 96` (offset from key_area_start)

Is `96` correct here?

Or, when there are unused TOC slots (slots 6..63 of the reserved 64),
do those unused slots count as part of the "key area" (so
`btn_free_space.off` should skip them somehow)? Specifically:

(a) Does `btn_free_space.off` continue to measure from
    `DATA_BASE + btn_table_space.len` (= 312 in this example), so it
    is `6 * KEY_SIZE = 96`?
(b) OR does the kernel reserve more than `btn_table_space.len` for
    the TOC region (e.g., aligns up to some boundary), so the actual
    key area starts later than `DATA_BASE + 256`?
(c) Some other encoding the spec describes for the gap between TOC
    reserved slots and the first key?

Please cite the exact Apple PDF page that defines this edge case.

## Q-L: is btn_free_space = {0, 0} actually valid?

Our currently-proven scratch-CLEAN code (commit c4268af) leaves
`btn_free_space @44/46 = 0/0` (i.e., the bytes are simply
unchanged from the new block's `vec![0u8; bsz]` allocation, never
explicitly written).

On scratch, `fsck_apfs -n` accepts this:
- The container omap then has 1-2 entries
- VSB virtual lookup via omap succeeds
- snap-meta tree parses correctly
- fsck reports CLEAN end-to-end

On a populated container (real-USB) after this same code:
- `Snapshot metadata tree is invalid` — gone (snap-meta fix landed)
- `btn_key_free_list offset 0` — gone (Q-A fix landed)
- `btn: invalid key (0, 16)` — appears (omap TOC entry rejected)

Question:

(a) Is `btn_free_space = {0, 0}` (literal zero, NOT BTOFF_INVALID
    0xFFFF) a valid encoding per the Apple spec? I.e., does it
    correctly mean "no shared free space" / "node is at maximum
    capacity"?
(b) If `{0, 0}` is NOT spec-compliant, what is the *minimum legal*
    non-zero encoding for a fully populated (no fragmentation, no
    reserved capacity beyond used) state? Cite the page.
(c) Does the kernel-on-fresh-format use `{0, 0}` for empty/sparse
    omap leaves, or does it always write specific non-zero values
    (e.g., off pointing at first byte of empty space, len = total
    free bytes)?

## How to use

Open NotebookLM with the apfs-reference notebook. Paste this file.
Capture answers verbatim with Apple PDF page citations. Save to
`2026-05-20-m7a-write-spec-notes-round5b.md`. Then ping me.

The companion file `2026-05-20-m7a-rt-round5a-notebooklm-questions.md`
contains Q-J — ask that one separately.
