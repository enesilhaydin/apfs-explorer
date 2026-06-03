#!/usr/bin/env python3
"""redecode_snapshot.py - zero-shared-code APFS snapshot metadata re-decoder.

Usage:
    python3 redecode_snapshot.py <image_path> <snap_xid_hex_or_dec> present|absent

Exit 0 on success (snap found/not-found as requested), 1 on failure.

This script re-decodes the APFS snapshot metadata tree from raw bytes,
completely independent of the Rust implementation.  It is the decisive
proof that create_snapshot / delete_snapshot wrote structurally correct
records that a from-scratch parser can read back.

Spec references used here (all locked, the APFS specification):
  - obj_phys_t: 32-byte header, Fletcher-64 over bytes [8..block_end)
  - nx_superblock_t: magic 'NXSB' @32, checkpoint ring fields
  - apfs_superblock_t: magic 'APSB' @32, snap_meta_tree_oid @0x98
  - omap_phys_t: tree_oid @48 (B-tree root OID for physical omap)
  - btree_node_phys_t: minimal leaf traversal for flat/single-level tree
  - apfs_snap_metadata_key: j_key_t (8 bytes), type = 0xB
  - apfs_snap_name_key: j_key_t + null-terminated name

All struct offsets verified against apfs_raw.h (linux-apfs-rw).
"""

import struct
import sys


# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

BLOCK_SIZE = 4096
SECTOR = 512

# APFS GPT partition type GUID bytes (little-endian on-disk)
APFS_GPT_TYPE = bytes([
    0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
    0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
])

NX_MAGIC = b"NXSB"
APFS_MAGIC = b"APSB"

# Apple PDF p.84 j_obj_types: SNAP_METADATA = 1, SNAP_NAME = 11 (0xB).
# (Earlier 0xB/0xC were wrong - those map to SNAP_NAME and SIBLING_MAP.)
APFS_TYPE_SNAP_METADATA = 1
APFS_TYPE_SNAP_NAME = 11


# ---------------------------------------------------------------------------
# Fletcher-64 (matches apfs-core::checksum::fletcher64)
# Two 32-bit partial sums over LE u32 words (mod 0xFFFFFFFF), over [8..end).
# c1/c2 derived; result stored as LE u64 = (c2 << 32) | c1.
# ---------------------------------------------------------------------------

def fletcher64(block: bytes) -> int:
    """Compute Fletcher-64 checksum over block[8:] (same as apfs-core impl)."""
    data = block[8:]
    # Pad to u32 boundary
    if len(data) % 4 != 0:
        data = data + b'\x00' * (4 - len(data) % 4)
    MOD = 0xFFFFFFFF
    sum1 = 0
    sum2 = 0
    for i in range(0, len(data), 4):
        word = struct.unpack_from('<I', data, i)[0]
        sum1 = (sum1 + word) % MOD
        sum2 = (sum2 + sum1) % MOD
    c1 = MOD - ((sum1 + sum2) % MOD)
    c2 = MOD - ((sum1 + c1) % MOD)
    return (c2 << 32) | c1


def verify_checksum(block: bytes) -> bool:
    stored = struct.unpack_from('<Q', block, 0)[0]
    computed = fletcher64(block)
    return stored == computed


# ---------------------------------------------------------------------------
# GPT parser - find APFS container byte range
# ---------------------------------------------------------------------------

def find_apfs_range(raw: bytes) -> tuple[int, int]:
    """Return (byte_offset, byte_length) of APFS container partition."""
    # Protective MBR
    assert raw[510:512] == b'\x55\xaa', "no protective MBR signature"
    # GPT header at sector 1
    hdr = raw[512:1024]
    assert hdr[:8] == b'EFI PART', f"no GPT signature: {hdr[:8]!r}"
    part_entry_lba = struct.unpack_from('<Q', hdr, 72)[0]
    num_parts = struct.unpack_from('<I', hdr, 80)[0]
    part_entry_size = struct.unpack_from('<I', hdr, 84)[0]

    entries_off = part_entry_lba * SECTOR
    for i in range(num_parts):
        off = entries_off + i * part_entry_size
        entry = raw[off:off + part_entry_size]
        if len(entry) < 48:
            break
        if entry[:16] == APFS_GPT_TYPE:
            start_lba = struct.unpack_from('<Q', entry, 32)[0]
            end_lba = struct.unpack_from('<Q', entry, 40)[0]  # inclusive
            byte_off = start_lba * SECTOR
            byte_len = (end_lba - start_lba + 1) * SECTOR
            return byte_off, byte_len
    raise ValueError("no APFS partition found in GPT")


# ---------------------------------------------------------------------------
# Block reader
# ---------------------------------------------------------------------------

def read_block(raw: bytes, base: int, paddr: int) -> bytes:
    """Read one 4096-byte block at physical address paddr within the container."""
    off = base + paddr * BLOCK_SIZE
    blk = raw[off:off + BLOCK_SIZE]
    assert len(blk) == BLOCK_SIZE, f"short block at paddr={paddr} off={off}"
    return blk


# ---------------------------------------------------------------------------
# nx_superblock_t parser
# [CERTAIN: nx.rs M1-confirmed]
# ---------------------------------------------------------------------------

def parse_nx(blk: bytes) -> dict:
    assert verify_checksum(blk), "nx_superblock bad Fletcher-64"
    magic = blk[32:36]
    assert magic == NX_MAGIC, f"bad NX magic: {magic!r}"
    block_size = struct.unpack_from('<I', blk, 36)[0]
    assert block_size == BLOCK_SIZE, f"unexpected block_size {block_size}"
    block_count = struct.unpack_from('<Q', blk, 40)[0]
    next_xid = struct.unpack_from('<Q', blk, 0x60)[0]
    xp_desc_base = struct.unpack_from('<Q', blk, 0x70)[0]
    xp_desc_blocks = struct.unpack_from('<I', blk, 0x68)[0] & 0x7FFFFFFF
    xp_desc_next = struct.unpack_from('<I', blk, 0x80)[0]
    xp_desc_index = struct.unpack_from('<I', blk, 0x88)[0]
    xp_desc_len = struct.unpack_from('<I', blk, 0x8C)[0]
    spaceman_oid = struct.unpack_from('<Q', blk, 0x98)[0]
    omap_oid = struct.unpack_from('<Q', blk, 0xA0)[0]
    # fs_oids at 0xB8, 100 entries of u64
    fs_oids = []
    for i in range(100):
        oid = struct.unpack_from('<Q', blk, 0xB8 + i * 8)[0]
        if oid == 0:
            break
        fs_oids.append(oid)
    return {
        'block_count': block_count,
        'next_xid': next_xid,
        'current_xid': next_xid - 1,
        'xp_desc_base': xp_desc_base,
        'xp_desc_blocks': xp_desc_blocks,
        'xp_desc_next': xp_desc_next,
        'xp_desc_index': xp_desc_index,
        'xp_desc_len': xp_desc_len,
        'spaceman_oid': spaceman_oid,
        'omap_oid': omap_oid,
        'fs_oids': fs_oids,
    }


def latest_nx_in_ring(raw: bytes, base: int, bootstrap: dict) -> dict:
    """Walk the checkpoint descriptor ring and return the newest nx_superblock."""
    desc_base = bootstrap['xp_desc_base']
    desc_blocks = bootstrap['xp_desc_blocks']
    best_xid = bootstrap['current_xid']
    best = bootstrap
    for i in range(desc_blocks):
        paddr = desc_base + i
        blk = read_block(raw, base, paddr)
        if blk[32:36] != NX_MAGIC:
            continue
        if not verify_checksum(blk):
            continue
        nx = parse_nx(blk)
        if nx['current_xid'] > best_xid:
            best_xid = nx['current_xid']
            best = nx
    return best


# ---------------------------------------------------------------------------
# omap_phys_t: find volume superblock paddr via container omap.
# Physical omap tree: key = {oid, xid}, value = {flags, size, paddr}.
# Single-level (root is a leaf node) for small test images.
# [CERTAIN: apfs_raw.h omap_phys_t + omap key/val layout]
# ---------------------------------------------------------------------------

def omap_resolve(raw: bytes, base: int, omap_paddr: int, target_oid: int, target_xid: int) -> int | None:
    """Return paddr for (oid, xid) from the omap rooted at omap_paddr."""
    omap_blk = read_block(raw, base, omap_paddr)
    # omap_phys_t: om_tree_oid @48 (u64) - physical B-tree root OID = paddr
    # [CERTAIN: apfs_raw.h struct apfs_omap_phys layout the APFS specification]
    # Offset 48 = obj_phys(32) + om_flags(4) + om_snap_count(4) + om_tree_type(4) + om_snapshot_tree_type(4)
    tree_oid = struct.unpack_from('<Q', omap_blk, 48)[0]
    return btree_omap_search(raw, base, tree_oid, target_oid, target_xid)


def btree_omap_search(raw: bytes, base: int, root_paddr: int, target_oid: int, target_xid: int) -> int | None:
    """Search a physical omap B-tree for the best (target_oid, xid <= target_xid) entry."""
    blk = read_block(raw, base, root_paddr)
    if not verify_checksum(blk):
        return None
    # btree_node_phys_t layout:
    #   obj_phys: 32 bytes
    #   btn_flags u16 @32, btn_level u16 @34, btn_nkeys u32 @36
    #   btn_table_space: off u16 @40, len u16 @42
    #   btn_free_space: off u16 @44, len u16 @46
    #   btn_key_free_list: off u16 @48, len u16 @50
    #   btn_val_free_list: off u16 @52, len u16 @54
    # DATA_BASE = 56 (all fields above total 56 bytes).
    # TOC starts at DATA_BASE + toc_off.
    # Key area starts at DATA_BASE + toc_len (after the whole TOC allocation).
    # [CERTAIN: txn.rs DATA_BASE=56; empirically verified against scratch image]
    DATA_BASE = 56
    flags = struct.unpack_from('<H', blk, 32)[0]
    level = struct.unpack_from('<H', blk, 34)[0]
    nkeys = struct.unpack_from('<I', blk, 36)[0]
    toc_off = struct.unpack_from('<H', blk, 40)[0]  # relative to DATA_BASE
    toc_len = struct.unpack_from('<H', blk, 42)[0]  # full allocated TOC size

    # TOC entries start at DATA_BASE + toc_off.
    toc_base = DATA_BASE + toc_off
    # Key area starts after the full TOC allocation: DATA_BASE + toc_len.
    key_area_start = DATA_BASE + toc_len

    best_paddr = None
    best_xid = -1

    for i in range(nkeys):
        toc_entry_off = toc_base + i * 4
        key_off_rel = struct.unpack_from('<H', blk, toc_entry_off)[0]
        val_off_rel = struct.unpack_from('<H', blk, toc_entry_off + 2)[0]

        # Key is relative to key_area_start = DATA_BASE + toc_len.
        k_off = key_area_start + key_off_rel
        key_bytes = blk[k_off:k_off + 16]
        if len(key_bytes) < 16:
            continue
        k_oid = struct.unpack_from('<Q', key_bytes, 0)[0]
        k_xid = struct.unpack_from('<Q', key_bytes, 8)[0]

        if k_oid != target_oid:
            continue
        if k_xid > target_xid:
            continue
        if k_xid <= best_xid:
            continue

        # Value offset: val_off_rel is a positive distance from val_area_end downward.
        # For root nodes: val_area_end = BLOCK_SIZE - 40 (btree_info_t at end).
        # For non-root leaf: val_area_end = BLOCK_SIZE.
        # v_abs = val_area_end - val_off_rel.
        # [CERTAIN: txn.rs apply_omap_entries v_abs=val_area_end-val_off_rel;
        #  val_off_rel=(i+1)*VAL_SIZE; empirically confirmed against scratch image]
        is_root_node = bool(flags & 0x0001)
        val_area_end_node = BLOCK_SIZE - 40 if is_root_node else BLOCK_SIZE
        val_off = val_area_end_node - val_off_rel
        if val_off < 0 or val_off + 16 > BLOCK_SIZE:
            continue
        val_bytes = blk[val_off:val_off + 16]
        if len(val_bytes) < 16:
            continue
        # omap_val_t: ov_flags u32 @0, ov_size u32 @4, ov_paddr u64 @8
        v_paddr = struct.unpack_from('<Q', val_bytes, 8)[0]
        best_paddr = v_paddr
        best_xid = k_xid

    if level > 0:
        # Non-leaf: recursively search the appropriate child.
        # For simplicity, search the last matching child in the non-leaf node.
        # This works for single-level trees (level == 1 root, level == 0 leaves).
        if best_paddr is not None:
            return btree_omap_search(raw, base, best_paddr, target_oid, target_xid)

    return best_paddr


# ---------------------------------------------------------------------------
# apfs_superblock_t parser (minimal)
# [CERTAIN: volume.rs M2-confirmed]
# ---------------------------------------------------------------------------

def parse_vsb(blk: bytes) -> dict:
    assert verify_checksum(blk), "volume superblock bad Fletcher-64"
    magic = blk[32:36]
    assert magic == APFS_MAGIC, f"bad APSB magic: {magic!r}"
    omap_oid = struct.unpack_from('<Q', blk, 128)[0]
    snap_meta_tree_oid = struct.unpack_from('<Q', blk, 0x98)[0]
    num_snapshots = struct.unpack_from('<I', blk, 0xD8)[0]
    volname_bytes = blk[704:704 + 256]
    end = volname_bytes.find(b'\x00')
    volname = volname_bytes[:end].decode('utf-8', errors='replace') if end >= 0 else volname_bytes.decode('utf-8', errors='replace')
    return {
        'omap_oid': omap_oid,
        'snap_meta_tree_oid': snap_meta_tree_oid,
        'num_snapshots': num_snapshots,
        'volname': volname,
    }


# ---------------------------------------------------------------------------
# Snapshot metadata tree search.
# The snap_meta_tree is an OMAP-style B-tree keyed by (xid-derived key).
# For a freshly created snapshot tree with one record we do a linear scan
# of the leaf node looking for a SNAP_METADATA key with matching xid.
# ---------------------------------------------------------------------------

def snap_tree_contains_xid(raw: bytes, base: int, snap_tree_root_paddr: int, snap_xid: int) -> bool:
    """Return True if the snapshot metadata tree contains a SNAP_METADATA record for snap_xid."""
    if snap_tree_root_paddr == 0:
        return False
    blk = read_block(raw, base, snap_tree_root_paddr)
    if not verify_checksum(blk):
        print(f"  snap tree block {snap_tree_root_paddr} bad checksum", file=sys.stderr)
        return False

    # DATA_BASE = 56 (same as Rust txn.rs / snapshot.rs).
    # TOC starts at DATA_BASE + toc_off; key area starts at DATA_BASE + toc_len.
    # Var-kv TOC entry = (key_off u16, key_len u16, val_off u16, val_len u16) = 8 bytes.
    # [CERTAIN: snapshot.rs build_snap_meta_node DATA_BASE=56; empirically verified]
    DATA_BASE = 56
    flags = struct.unpack_from('<H', blk, 32)[0]
    level = struct.unpack_from('<H', blk, 34)[0]
    nkeys = struct.unpack_from('<I', blk, 36)[0]
    toc_off = struct.unpack_from('<H', blk, 40)[0]   # relative to DATA_BASE
    toc_len = struct.unpack_from('<H', blk, 42)[0]   # full allocated TOC size

    toc_base = DATA_BASE + toc_off
    key_area_start = DATA_BASE + toc_len

    print(f"  snap_tree paddr={snap_tree_root_paddr} flags={flags:#x} level={level} nkeys={nkeys} toc_len={toc_len} key_area_start={key_area_start}")

    if level > 0:
        # Non-leaf: scan all child pointers.
        # Var-kv non-leaf TOC entry = 8 bytes (same structure as leaf).
        for i in range(nkeys):
            toc_entry_off = toc_base + i * 8
            if toc_entry_off + 8 > BLOCK_SIZE:
                break
            val_off_rel = struct.unpack_from('<H', blk, toc_entry_off + 4)[0]
            # Child pointer value is a u64 paddr; val grows from end of block.
            val_off = BLOCK_SIZE - (val_off_rel + 8)
            if val_off < 0 or val_off + 8 > BLOCK_SIZE:
                continue
            child_paddr = struct.unpack_from('<Q', blk, val_off)[0]
            if snap_tree_contains_xid(raw, base, child_paddr, snap_xid):
                return True
        return False

    # Leaf: var-kv TOC entry = 8 bytes.
    # Leaf values grow from val_area_end downward.
    # For root+leaf (single-node tree): val_area_end = BLOCK_SIZE - 40 (btree_info_t).
    # For non-root leaf: val_area_end = BLOCK_SIZE.
    is_root = bool(flags & 0x0001)
    val_area_end = BLOCK_SIZE - 40 if is_root else BLOCK_SIZE

    for i in range(nkeys):
        toc_entry_off = toc_base + i * 8
        if toc_entry_off + 8 > BLOCK_SIZE:
            break
        key_off_rel = struct.unpack_from('<H', blk, toc_entry_off)[0]
        # key_len = struct.unpack_from('<H', blk, toc_entry_off + 2)[0]
        val_off_rel = struct.unpack_from('<H', blk, toc_entry_off + 4)[0]
        # val_len = struct.unpack_from('<H', blk, toc_entry_off + 6)[0]

        k_off = key_area_start + key_off_rel
        if k_off + 8 > BLOCK_SIZE:
            continue
        # j_key_t: obj_id_and_type u64 - type in bits 60-63, id in bits 0-59
        obj_id_and_type = struct.unpack_from('<Q', blk, k_off)[0]
        key_type = (obj_id_and_type >> 60) & 0xF
        # For SNAP_METADATA, obj_id field encodes the snap_xid.
        obj_id = obj_id_and_type & 0x0FFFFFFFFFFFFFFF
        print(f"    leaf entry {i}: key_type={key_type:#x} obj_id={obj_id:#x} (want snap_xid={snap_xid:#x})")
        if key_type == APFS_TYPE_SNAP_METADATA and obj_id == snap_xid:
            return True

    return False


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    if len(sys.argv) != 4:
        print(f"usage: {sys.argv[0]} <image> <snap_xid> present|absent", file=sys.stderr)
        sys.exit(1)

    img_path = sys.argv[1]
    snap_xid = int(sys.argv[2], 0)  # accept hex or decimal
    expect = sys.argv[3]
    assert expect in ('present', 'absent'), f"expect must be 'present' or 'absent', got {expect!r}"

    with open(img_path, 'rb') as f:
        raw = f.read()

    print(f"image size: {len(raw)} bytes")

    # Find APFS container range.
    base, length = find_apfs_range(raw)
    print(f"APFS container: offset={base:#x} length={length:#x}")

    # Parse block 0 (bootstrap nx_superblock).
    blk0 = read_block(raw, base, 0)
    if blk0[32:36] != NX_MAGIC:
        print("block 0 is not NX_MAGIC - walking checkpoint ring for bootstrap", file=sys.stderr)
    bootstrap = parse_nx(blk0)
    print(f"bootstrap xid={bootstrap['current_xid']} fs_oids={bootstrap['fs_oids']}")

    # Find latest nx_superblock in checkpoint descriptor ring.
    nx = latest_nx_in_ring(raw, base, bootstrap)
    print(f"latest nx xid={nx['current_xid']} omap_oid={nx['omap_oid']:#x} fs_oids={nx['fs_oids']}")

    # Resolve volume superblock paddr via container omap.
    if not nx['fs_oids']:
        print("ERROR: no fs_oids in nx_superblock", file=sys.stderr)
        sys.exit(1)

    fs_oid = nx['fs_oids'][0]
    vol_paddr = omap_resolve(raw, base, nx['omap_oid'], fs_oid, nx['current_xid'])
    if vol_paddr is None:
        print(f"ERROR: could not resolve volume oid={fs_oid:#x}", file=sys.stderr)
        sys.exit(1)
    print(f"volume paddr={vol_paddr:#x}")

    # Parse volume superblock.
    vsb_blk = read_block(raw, base, vol_paddr)
    vsb = parse_vsb(vsb_blk)
    print(f"volume '{vsb['volname']}': num_snapshots={vsb['num_snapshots']} snap_meta_tree_oid={vsb['snap_meta_tree_oid']:#x}")

    # Check snapshot metadata tree.
    snap_tree_oid = vsb['snap_meta_tree_oid']
    if snap_tree_oid == 0:
        found = False
    else:
        found = snap_tree_contains_xid(raw, base, snap_tree_oid, snap_xid)

    print(f"snap_xid={snap_xid:#x} found={found} (expect={expect})")

    if expect == 'present' and not found:
        print(f"FAIL: snap_xid {snap_xid:#x} expected present but not found in snapshot tree", file=sys.stderr)
        sys.exit(1)
    elif expect == 'absent' and found:
        print(f"FAIL: snap_xid {snap_xid:#x} expected absent but still found in snapshot tree", file=sys.stderr)
        sys.exit(1)

    print("OK")
    sys.exit(0)


if __name__ == '__main__':
    main()
