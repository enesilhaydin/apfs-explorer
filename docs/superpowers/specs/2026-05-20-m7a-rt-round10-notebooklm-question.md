# M7a-RT round10 — fsck's exact rejection of snap_metadata_val

After all rounds 1-9 fixes (snap-meta key Apple-PAGE-119, omap btree
free-space + alignment, snap-meta toc_len=64, extentref_tree_type
preserved from VSB, frozen VSB OIDs preserved, om_snapshot_tree
provisioned with subtype 0x13, UNIX-epoch timestamps), fsck still
rejects. With `hdiutil attach -nomount` + direct `fsck_apfs -n` we
now see the FULL error:

```
** Checking the snapshot metadata tree.
error: snapshot name (id 3): invalid key length (8)
error: key size (8)/val size (59) is invalid
   Snapshot metadata tree is invalid.
```

The numbers `key=8, val=59` match exactly our SNAP_METADATA record's
dimensions:
- key = j_key_t (8 bytes, obj_id=3 type=0xB SNAP_METADATA)
- val = j_snap_metadata_val (50 fixed bytes + 9-byte name = 59)

fsck attributes this to "snapshot name (id 3)" — suggesting it
THINKS this record is a SNAP_NAME for snap_xid=3.

## Our snap_meta tree node bytes (verified byte-by-byte vs kernel)

```
nkeys = 2
table_space = {0, 64}    # 64-byte reserve (matches kernel)
free_space  = {27, 3842}
key_free_list = val_free_list = {0xFFFF, 0}

TOC[0]: key_off=0  key_len=8   val_off=59  val_len=59
  key bytes (8): 03 00 00 00 00 00 00 B0  (j_key_t obj_id_and_type LE)
    → obj_id = 3, type = 0xB (SNAP_METADATA)
  val bytes (59): apfs_snap_metadata_val per PDF p.117/118
    @0..8   extentref_tree_oid = 0xC3 (matches VSB pre-snap)
    @8..16  sblock_oid         = 0xC9 (frozen VSB, magic APSB, Fletcher OK)
    @16..24 create_time        = unix ns
    @24..32 change_time        = unix ns
    @32..40 inum               = 0
    @40..44 extentref_tree_type = 0x40000002 (matches VSB.apfs_extentref_tree_type @0x78)
    @44..48 flags              = 0
    @48..50 name_len           = 9
    @50..59 name               = "m7a-test\0"

TOC[1]: key_off=8  key_len=19  val_off=67  val_len=8
  key bytes (19):
    @0..8  j_key_t obj_id=0x0FFF_FFFF_FFFF_FFFF, type=0xC SNAP_NAME (PDF p.119: "always ~0ULL")
    @8..10 name_len = 9 LE
    @10..19 name = "m7a-test\0"
  val bytes (8): xid_t snap_xid = 3 LE
```

## Q-AA: which TOC entry does fsck rate as "snapshot name (id 3)"?

Both interpretations seem possible:

(a) fsck reads TOC[0] (the SNAP_METADATA record) and mis-identifies
    it as a SNAP_NAME because of some sort/ordering issue. Then
    reports key=8, val=59 as "invalid for SNAP_NAME schema".
(b) fsck reads TOC[1] (the SNAP_NAME record) correctly, but the
    `(8)` and `(59)` in the error refer to SOMETHING OTHER than
    the SNAP_NAME's own k_len/v_len (which are 19 and 8 in our
    emission).

Which one is happening?

## Q-BB: the sort-order question

Apple PDF p.123: "Snapshot metadata tree records are sorted by
transaction identifier."

For our 2 records:
- SNAP_METADATA: j_key_t obj_id=3 ⇒ tx id = 3
- SNAP_NAME:     j_key_t obj_id=~0ULL ⇒ tx id = ??? (or pulled from val.snap_xid=3?)

What is the EXACT sort key for SNAP_NAME records — `obj_id_and_type`
as a u64 (giving 0xCFFF…), or `val.snap_xid` (giving 3), or
something else? Apple PDF citation please.

If sort is by val.snap_xid, then SNAP_NAME(xid=3) and
SNAP_METADATA(xid=3) have the SAME sort key. Tie-break may then be
by type → SNAP_METADATA(0xB) first, SNAP_NAME(0xC) second (our
order). Or maybe Apple ties differently (e.g. by name).

## Q-CC: SNAP_NAME key/val format — is the name really in the key?

Apple PDF p.119 says `j_snap_name_key { j_key_t hdr; uint16_t name_len;
uint8_t name[]; }` and val is just `{ xid_t snap_xid }`. Combined
with the verbatim "obj_id is always ~0ULL" — these are LITERAL
verbatim spec quotes per round1.

But fsck reports "key length (8)" — suggesting it expects key to be
just 8 bytes (= j_key_t only), with name elsewhere. Possible
clarifications:

(a) The Apple PDF p.119 documentation is INACCURATE and the real
    SNAP_NAME format is: key = j_key_t only (8 bytes),
    val = { xid_t snap_xid; uint16_t name_len; uint8_t name[]; }
    (name moved to val).
(b) The documentation is correct but fsck reports key_len as
    "j_key_t size" (= 8) for diagnostic purposes regardless of
    actual key bytes. The real rejection is about a different
    invariant.
(c) name_len in our emission should NOT include the trailing NUL
    (so name_len=8 not 9, key=8+2+8=18).
(d) Some other structural difference.

Please verify against macOS 15 source code or by reading an actual
kernel-emitted populated snap-meta tree node.

## Q-DD: macOS 15 specific changes?

Apple may have changed snap-meta record format between APFS 1 (the
PDF version) and the current APFS variant in macOS 15. The fsck
binary on macOS 15 may expect a different schema than the PDF
documents.

Has Apple updated the APFS Reference recently with a newer schema
for snap-meta records? Or is there a `j_snap_metadata_val_t` v2 we
don't know about?

## How to use

Once NotebookLM quota resets (16:00), paste this file. Capture
answers verbatim with Apple PDF page citations. The Q-CC answer is
the most-likely unblocker — if SNAP_NAME's name lives in val not
key, our entire round1 fix was wrong direction.

The current state at d78f1d8 has all rounds 1-9 fixes preserved.
22 commits today on feat/m7a-write-snapshot.
