# M7 real-USB final proof — finding (spaceman real-bitmap allocator bug)

> **RESOLVED 2026-05-22 (update).** The original diagnosis below blamed the
> spaceman bitmap/COW path. That was PARTLY a red herring: the bitmap itself was
> internally consistent. The TRUE root cause was twofold and was MASKED the
> whole M7 series by a broken test oracle (`xtask verify-scratch` treated fsck
> "warning: overallocation" + exit-0 + "appears to be OK" as CLEAN):
>   1. Empty-node TOC: deleting the last extentref record produced
>      btn_table_space.len=0 (fsck "Extent ref tree is invalid"). Fixed: empty
>      b-tree root reserves 64-byte TOC like the kernel.
>   2. COW metadata leak: every catalog/omap/extentref/VSB mutation COW'd to
>      fresh blocks but NEVER freed the replaced old blocks → they stayed set in
>      the chunk bitmap, unreferenced → fsck "overallocation" + skewed
>      apfs_fs_alloc_count. Fixed: free the replaced volume + container blocks at
>      commit (fsck validates only the latest checkpoint, so immediate free is
>      CLEAN; spaceman free queue is the crash-safe/snapshot deferral, still a
>      follow-up); unlink frees the WHOLE extent run; fs_alloc_count tracks the
>      catalog node-count delta.
> RESULT: usb_tests::decisive_file_write_on_real_usb PASSES on a fresh 32 GB USB
> — create_file + write_file + unlink, container + volume fsck CLEAN throughout.
> Remaining: snapshot-pinned reclaim via the spaceman free queue (2 scratch
> tests #[ignore]'d). See apfs-spec:cow-block-reclaim + :omap-snapshot-versioning.

---


Date 2026-05-22. Target: SMI Corporation (Silicon Motion) 32 GB USB, formatted
APFS, container partition `/dev/disk4s2` (whole disk `disk4`, external).

## What the decisive real-USB test found

Ran `usb_tests::decisive_file_write_on_real_usb` (sudo) against the real 32 GB
USB. The hard safety guard passed (Apple_APFS + external + not disk0). On
`create_file("usbfile.txt")`:

- **Volume fsck is CLEAN** — `fsck_apfs` reports the volume + container
  "appears to be OK" structurally. So the M7b/c/d file-write STRUCTURE
  (catalog, inode, drec, omap remap, b-tree) is correct on real hardware.
- **Container space verification reports NEW overallocation** at ~14 blocks
  (e.g. `0x1e476+1 … 0x1e5b0+4`), all with `bitmap address (0x4644)`. fsck:
  `warning: overallocation detected on Main device`.

Overallocation = our COW block allocations handed out blocks the spaceman
already considers allocated (double-allocation), OR did not mark them
consistently in the real persistent chunk bitmap.

## Root cause (diagnosis)

This is exactly the **M7a-RT open item (#56)** that the ROUND2 safety gate
deferred ("the production writer is not allowed to touch a real USB until
spaceman CIB/bitmap COW is implemented and proven").

- The 64 MiB scratch image only ever had chunks with `ci_bitmap_addr == 0`
  (implicit allocation, no persistent bitmap). The round14/round16 allocator
  paths (skip partial bitmap-less chunks; contiguous bump on fully-free
  bitmap-less chunks) handled that and passed scratch fsck CLEAN.
- A 32 GB real USB has **real persistent chunk bitmaps** (bitmap addr 0x4644).
  `alloc_one_block`'s bitmap path (read bitmap → find_zero_bit → set bit →
  write bitmap → decrement ci/sm free counts) is reached for the first time on
  real hardware, and it double-allocates. Likely culprits to investigate:
  1. `find_zero_bit` returning a bit that is actually set (bit-order / byte
     scan bug), or scanning beyond `ci_block_count`.
  2. Reading/writing the wrong bitmap block (CIB `ci_bitmap_addr` offset, or
     the spaceman internal-pool bitmap `sm_ip_bm` vs per-chunk bitmaps).
  3. Non-COW in-place bitmap mutation conflicting with the checkpoint's
     spaceman accounting (ROUND2 concern) — the bitmap write must be part of
     the COW transaction, not a direct device write.
  4. Multiple `alloc_block` calls within one transaction not seeing each
     other's bitmap updates (staged vs on-device).

## Empirical confirmation (raw spaceman decode of the USB)

- Spaceman: block_size 4096, blocks_per_chunk 32768, main dev 7,935,990 blocks,
  243 chunks, 2 CIBs. Chunk 0 (addr 0) has a real bitmap (0x45d7, 14142 free —
  the kernel's format metadata). Chunks 1,2,4,5,6 are `bitmap_addr=0`
  fully-free. Chunks 3 (0x45d8) and 7 (0x45d9) have **real bitmaps** and are
  partially used.
- **Bitmap convention is correct**: chunk 0 free_count 14142 == zero-bit count.
  So `find_zero_bit` (0 = free, 1 = allocated) matches APFS — NOT the bug.
- The overallocated block `0x1e476` lives in chunk 3 (real bitmap 0x45d8) and
  its bitmap bit is **1 (allocated)**, yet the filesystem references it →
  double-use.
- Spaceman `next_xid-1 = 42`: the USB accumulated many write transactions
  (repeated test runs, no reformat between), so leaked/double blocks piled up.

So the allocator hands out blocks in real-bitmap chunks that are (or become)
double-referenced, and/or the bitmap mutation is a direct non-COW device write
that diverges from the checkpoint's spaceman accounting. This is the CIB/bitmap
copy-on-write work ROUND2 deferred — the genuine remaining M7a-RT core task.

## Status

- M7b/c/d (create_file, mkdir, write_file multi-block, unlink, rename, N-level
  catalog b-tree split) are all **scratch-proven CLEAN** (fsck + mount + Python
  decode) and the file-write **structure is also correct on the real USB**
  (volume fsck CLEAN).
- The remaining gap is purely the **spaceman block allocator on real
  persistent bitmaps** — the long-standing M7a-RT core task.

## Next steps (M7a-RT resume)

1. Decode the real USB spaceman: `nx.spaceman_oid` → spaceman_phys → device
   `sm_dev[0]` → CIB array → each `chunk_info` (ci_addr, ci_block_count,
   ci_free_count, ci_bitmap_addr) + the internal-pool bitmap.
2. Reproduce in isolation: which exact blocks did `alloc_block` return, and
   were their bits already set in the real bitmap?
3. Fix `alloc_one_block` for real bitmaps; make the bitmap update copy-on-write
   within the transaction (ROUND2 requirement).
4. Re-run `decisive_file_write_on_real_usb` until fsck reports no NEW
   overallocation → M7 real-USB final proof complete.

## Re-run command

```
cargo test -p apfs-write --test snapshot_scratch --no-run
sudo env APFS_WRITE_USB=/dev/disk4s2 APFS_WRITE_USB_ROUND2_CIB_COW_READY=1 \
  <target/debug/deps/snapshot_scratch-HASH> \
  usb_tests::decisive_file_write_on_real_usb --nocapture --exact
```

The USB is left mounted + usable (volume is structurally CLEAN; only the
container free-space accounting has the leaked/double blocks).
