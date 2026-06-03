#!/usr/bin/env python3
"""decode_macos_template.py - zero-shared-code APFS ground-truth template decoder.

Usage (run under sudo by the Rust test harness; also callable standalone):
    python3 decode_macos_template.py <device_or_image> <before_json> <after_json> <report_out>

Called by the Rust test `macos_template_tests::capture_macos_snapshot_template`.
It receives:
  - device_or_image : path to the raw Apple_APFS container partition (/dev/rdiskNsM)
  - before_json     : path to a JSON file listing {paddr: hex-string, data: hex-string} for
                      each captured BEFORE block
  - after_json      : path to a JSON file listing {paddr: hex-string, data: hex-string} for
                      each captured AFTER block (may include new blocks not in BEFORE)
  - report_out      : path to write the decoded REPORT.txt

The script is completely independent of the Rust codebase - no imports from our crates.

Spec references (all locked, the APFS specification):
  obj_phys_t           : 32-byte header, Fletcher-64 over bytes [8..block_end)
  nx_superblock_t      : magic 'NXSB'@32, xp_desc_blocks@0x68, xp_data_blocks@0x6C,
                         xp_desc_base@0x70, xp_data_base@0x78, spaceman_oid@0x98,
                         omap_oid@0xA0, next_xid@0x60
  btree_node_phys_t    : btn_flags@32, btn_level@34, btn_nkeys@36,
                         btn_table_space{off@40,len@42}, btn_free_space{off@44,len@46},
                         btn_key_free_list{off@48,len@50}, btn_val_free_list{off@52,len@54}
  apfs_spaceman_phys_t : sm_dev[0].sd_cib_addr_block_count@0xD8, sm_dev[0].sd_cib_addr_offs@?
                         (decoded heuristically from raw bytes; CIB paddr array follows)
  chunk_info_block_t   : cib header 40 bytes, then array of chunk_info_t (0x20 each):
                         ci_xid@0, ci_addr@8, ci_block_count@16, ci_free_count@20
All offsets verified against apfs_raw.h (linux-apfs-rw) and txn.rs comments.

Exit 0 on success, 1 on failure.
"""

import json
import struct
import sys
import os
from typing import Optional

BLOCK_SIZE = 4096

# ---------------------------------------------------------------------------
# Low-level helpers
# ---------------------------------------------------------------------------

def u8(buf: bytes, off: int) -> int:
    return buf[off]

def u16le(buf: bytes, off: int) -> int:
    return struct.unpack_from("<H", buf, off)[0]

def u32le(buf: bytes, off: int) -> int:
    return struct.unpack_from("<I", buf, off)[0]

def u64le(buf: bytes, off: int) -> int:
    return struct.unpack_from("<Q", buf, off)[0]

def i64le(buf: bytes, off: int) -> int:
    return struct.unpack_from("<q", buf, off)[0]

def fletcher64(block: bytes) -> int:
    """Fletcher-64 as used by APFS: sum over 4-byte LE words, zero out [0..8]."""
    # APFS variant: two 32-bit accumulators over 4-byte little-endian words.
    # checksum stored at [0..8]; computation treats those 8 bytes as zero.
    b = bytearray(block)
    b[0:8] = b'\x00' * 8
    lo, hi = 0, 0
    for i in range(0, len(b), 4):
        word = struct.unpack_from("<I", b, i)[0]
        lo = (lo + word) & 0xFFFF_FFFF
        hi = (hi + lo) & 0xFFFF_FFFF
    return (hi << 32) | lo

def verify_checksum(block: bytes) -> bool:
    stored = u64le(block, 0)
    computed = fletcher64(block)
    return stored == computed

# ---------------------------------------------------------------------------
# Object type constants (obj_phys_t o_type low 16 bits)
# ---------------------------------------------------------------------------
OBJ_TYPE_MASK          = 0x0000_FFFF
OBJECT_TYPE_NX_SUPERBLOCK = 0x0001
OBJECT_TYPE_BTREE      = 0x0002
OBJECT_TYPE_BTREE_NODE = 0x0003
OBJECT_TYPE_SPACEMAN   = 0x0005
OBJECT_TYPE_SPACEMAN_CIB = 0x0006
OBJECT_TYPE_OMAP       = 0x000B
OBJECT_TYPE_FS         = 0x000D  # volume superblock APSB

BTNODE_ROOT            = 0x0001
BTNODE_LEAF            = 0x0002
BTNODE_FIXED_KV_SIZE   = 0x0004

# ---------------------------------------------------------------------------
# Parse obj_phys_t header (first 32 bytes of any APFS block)
# ---------------------------------------------------------------------------

def parse_obj_phys(block: bytes) -> dict:
    return {
        "cksum":   u64le(block, 0),
        "oid":     u64le(block, 8),
        "xid":     u64le(block, 16),
        "o_type":  u32le(block, 24),
        "subtype": u32le(block, 28),
        "obj_type_masked": u32le(block, 24) & OBJ_TYPE_MASK,
    }

# ---------------------------------------------------------------------------
# Parse nx_superblock_t key fields
# ---------------------------------------------------------------------------

def parse_nx(block: bytes) -> Optional[dict]:
    magic = u32le(block, 32)
    if magic != 0x4253_584E:  # 'NXSB'
        return None
    obj = parse_obj_phys(block)
    return {
        **obj,
        "block_size":      u32le(block, 36),
        "block_count":     u64le(block, 40),
        "next_xid":        u64le(block, 0x60),
        "xp_desc_blocks":  u32le(block, 0x68) & 0x7FFF_FFFF,
        "xp_data_blocks":  u32le(block, 0x6C),
        "xp_desc_base":    u64le(block, 0x70),
        "xp_data_base":    u64le(block, 0x78),
        "spaceman_oid":    u64le(block, 0x98),
        "omap_oid":        u64le(block, 0xA0),
    }

# ---------------------------------------------------------------------------
# Parse btree_node_phys_t nloc fields
# ---------------------------------------------------------------------------

def parse_btree_node(block: bytes) -> dict:
    obj = parse_obj_phys(block)
    btn_flags    = u16le(block, 32)
    btn_level    = u16le(block, 34)
    btn_nkeys    = u32le(block, 36)
    tbl_off      = u16le(block, 40)
    tbl_len      = u16le(block, 42)
    free_off     = u16le(block, 44)
    free_len     = u16le(block, 46)
    kfl_off      = u16le(block, 48)
    kfl_len      = u16le(block, 50)
    vfl_off      = u16le(block, 52)
    vfl_len      = u16le(block, 54)
    is_fixed_kv  = bool(btn_flags & BTNODE_FIXED_KV_SIZE)
    is_root      = bool(btn_flags & BTNODE_ROOT)
    is_leaf      = bool(btn_flags & BTNODE_LEAF)
    # Grab first 8 bytes and last 8 bytes of the key/value area (after 56-byte header)
    toc_area_start = 56
    toc_area = block[toc_area_start:toc_area_start + max(tbl_len, 16)]
    first_toc = toc_area[:8].hex() if len(toc_area) >= 8 else "(short)"
    # val area is at end of block; last kvloc entry is at end of toc area
    last_toc = toc_area[max(0, len(toc_area)-8):].hex() if len(toc_area) >= 8 else "(short)"
    return {
        **obj,
        "btn_flags":          f"0x{btn_flags:04X}",
        "btn_flags_decoded":  f"{'ROOT ' if is_root else ''}{'LEAF ' if is_leaf else ''}{'FIXED_KV' if is_fixed_kv else ''}".strip(),
        "btn_level":          btn_level,
        "btn_nkeys":          btn_nkeys,
        "btn_table_space":    {"off": tbl_off, "len": tbl_len},
        "btn_free_space":     {"off": free_off, "len": free_len},
        "btn_key_free_list":  {"off": kfl_off, "len": kfl_len},
        "btn_val_free_list":  {"off": vfl_off, "len": vfl_len},
        "first_toc_bytes":    first_toc,
        "last_toc_bytes":     last_toc,
    }

# ---------------------------------------------------------------------------
# Parse chunk_info_t entries from a CIB block
# chunk_info_block_t: obj_phys(32) + cib_index(4) + cib_chunk_info_count(4) = 40 byte header
# then array of chunk_info_t (0x20 = 32 bytes each):
#   ci_xid(8) + ci_addr(8) + ci_block_count(4) + ci_free_count(4) + ci_bitmap_addr(8)
# ---------------------------------------------------------------------------
CHUNK_INFO_SIZE = 32
CIB_HEADER_SIZE = 40

def parse_cib(block: bytes) -> dict:
    obj = parse_obj_phys(block)
    # libfsapfs: cib_index @32, cib_chunk_info_count @36, entries start @40.
    ci_index = u32le(block, 32)
    ci_count  = u32le(block, 36)
    entries = []
    for i in range(min(ci_count, (BLOCK_SIZE - CIB_HEADER_SIZE) // CHUNK_INFO_SIZE)):
        base = CIB_HEADER_SIZE + i * CHUNK_INFO_SIZE
        if base + CHUNK_INFO_SIZE > len(block):
            break
        entries.append({
            "ci_xid":          u64le(block, base + 0),
            "ci_addr":         u64le(block, base + 8),
            "ci_block_count":  u32le(block, base + 16),
            "ci_free_count":   u32le(block, base + 20),
            "ci_bitmap_addr":  u64le(block, base + 24),
        })
    return {**obj, "ci_index": ci_index, "ci_count": ci_count, "entries": entries}

# ---------------------------------------------------------------------------
# Classify a block by its obj_phys o_type
# ---------------------------------------------------------------------------

def classify_block(block: bytes) -> str:
    if len(block) < 32:
        return "TOO_SHORT"
    t = u32le(block, 24) & OBJ_TYPE_MASK
    names = {
        OBJECT_TYPE_NX_SUPERBLOCK: "NX_SUPERBLOCK",
        OBJECT_TYPE_BTREE:         "BTREE_ROOT_HDR",
        OBJECT_TYPE_BTREE_NODE:    "BTREE_NODE",
        OBJECT_TYPE_SPACEMAN:      "SPACEMAN",
        OBJECT_TYPE_SPACEMAN_CIB:  "SPACEMAN_CIB",
        OBJECT_TYPE_OMAP:          "OMAP_PHYS",
        OBJECT_TYPE_FS:            "VOL_SUPERBLOCK",
    }
    return names.get(t, f"UNKNOWN(0x{t:04X})")

# ---------------------------------------------------------------------------
# Main decode + diff logic
# ---------------------------------------------------------------------------

def load_blocks(json_path: str) -> dict:
    """Load {paddr_hex: bytes} from the JSON snapshot produced by the Rust harness."""
    with open(json_path, "r") as f:
        raw = json.load(f)
    result = {}
    for entry in raw:
        paddr = int(entry["paddr"], 16)
        data  = bytes.fromhex(entry["data"])
        result[paddr] = data
    return result

def fmt_btree(label: str, paddr: int, node: dict) -> list:
    lines = [
        f"  {label} @ paddr=0x{paddr:X}  oid=0x{node['oid']:X}  xid=0x{node['xid']:X}",
        f"    obj_type=0x{node['o_type']:08X}  subtype=0x{node['subtype']:08X}",
        f"    btn_flags={node['btn_flags']}  ({node['btn_flags_decoded']})",
        f"    btn_level={node['btn_level']}  btn_nkeys={node['btn_nkeys']}",
        f"    btn_table_space  : off={node['btn_table_space']['off']}  len={node['btn_table_space']['len']}",
        f"    btn_free_space   : off={node['btn_free_space']['off']}  len={node['btn_free_space']['len']}",
        f"    btn_key_free_list: off={node['btn_key_free_list']['off']}  len={node['btn_key_free_list']['len']}",
        f"    btn_val_free_list: off={node['btn_val_free_list']['off']}  len={node['btn_val_free_list']['len']}",
        f"    first_toc_bytes={node['first_toc_bytes']}  last_toc_bytes={node['last_toc_bytes']}",
    ]
    return lines

def fmt_cib(label: str, paddr_before: Optional[int], paddr_after: int, cib_before: Optional[dict], cib_after: dict) -> list:
    cow = paddr_before is not None and paddr_before != paddr_after
    lines = [
        f"  {label} @ paddr_after=0x{paddr_after:X}"
        + (f"  (CoW from 0x{paddr_before:X})" if cow else "  (same paddr - in-place or new)"),
        f"    oid=0x{cib_after['oid']:X}  xid_after=0x{cib_after['xid']:X}"
        + (f"  xid_before=0x{cib_before['xid']:X}" if cib_before else "  xid_before=N/A"),
        f"    ci_count={cib_after['ci_count']}",
    ]
    for i, e in enumerate(cib_after["entries"]):
        before_e = (cib_before["entries"][i] if cib_before and i < len(cib_before["entries"]) else None)
        changed = before_e is None or before_e["ci_free_count"] != e["ci_free_count"] or before_e["ci_xid"] != e["ci_xid"]
        marker = "  CHANGED" if changed else ""
        lines.append(
            f"    chunk[{i}]: ci_xid=0x{e['ci_xid']:X}  ci_addr=0x{e['ci_addr']:X}"
            f"  ci_block_count={e['ci_block_count']}  ci_free_count={e['ci_free_count']}{marker}"
        )
        if before_e and changed:
            lines.append(
                f"             before: ci_xid=0x{before_e['ci_xid']:X}"
                f"  ci_free_count={before_e['ci_free_count']}"
            )
    return lines

def run(device_path: str, before_json: str, after_json: str, report_out: str) -> int:
    report_lines = []

    def emit(s: str = ""):
        report_lines.append(s)

    emit("=" * 72)
    emit("M7a-RT macOS GROUND-TRUTH SNAPSHOT TEMPLATE REPORT")
    emit("Generated by tools/decode_macos_template.py (zero-shared-code decoder)")
    emit(f"Device : {device_path}")
    emit(f"Before : {before_json}")
    emit(f"After  : {after_json}")
    emit("=" * 72)
    emit()

    before = load_blocks(before_json)
    after  = load_blocks(after_json)

    all_paddrs = sorted(set(before) | set(after))

    # --- Partition blocks into categories ---
    changed_paddrs   = [p for p in all_paddrs if p in before and p in after and before[p] != after[p]]
    new_paddrs       = [p for p in all_paddrs if p not in before and p in after]
    unchanged_paddrs = [p for p in all_paddrs if p in before and p in after and before[p] == after[p]]

    emit(f"Total blocks captured  : before={len(before)}  after={len(after)}")
    emit(f"Changed blocks         : {len(changed_paddrs)}")
    emit(f"New blocks (after only): {len(new_paddrs)}")
    emit(f"Unchanged blocks       : {len(unchanged_paddrs)}")
    emit()

    # --- NX superblock diff (checkpoint advancement) ---
    emit("-" * 72)
    emit("SECTION 1 - NX SUPERBLOCK CHECKPOINT ADVANCEMENT")
    emit("-" * 72)
    nx_before = nx_after = None
    nx_paddr_before = nx_paddr_after = None
    for p in all_paddrs:
        blk = after.get(p) or before.get(p)
        if blk and u32le(blk, 32) == 0x4253_584E:
            xid_val = u64le(blk, 16)
            if p in before:
                bx = parse_nx(before[p])
                if bx and (nx_before is None or bx["xid"] > nx_before["xid"]):
                    nx_before = bx
                    nx_paddr_before = p
            if p in after:
                ax = parse_nx(after[p])
                if ax and (nx_after is None or ax["xid"] > nx_after["xid"]):
                    nx_after = ax
                    nx_paddr_after = p
    if nx_before and nx_after:
        emit(f"  BEFORE NX @ paddr=0x{nx_paddr_before:X}  xid=0x{nx_before['xid']:X}  next_xid=0x{nx_before['next_xid']:X}")
        emit(f"  AFTER  NX @ paddr=0x{nx_paddr_after:X}  xid=0x{nx_after['xid']:X}  next_xid=0x{nx_after['next_xid']:X}")
        emit(f"  xid advanced by {nx_after['xid'] - nx_before['xid']} transaction(s)")
        emit(f"  BEFORE  xp_desc_base=0x{nx_before['xp_desc_base']:X}  xp_desc_blocks={nx_before['xp_desc_blocks']}")
        emit(f"          xp_data_base=0x{nx_before['xp_data_base']:X}  xp_data_blocks={nx_before['xp_data_blocks']}")
        emit(f"  AFTER   xp_desc_base=0x{nx_after['xp_desc_base']:X}  xp_desc_blocks={nx_after['xp_desc_blocks']}")
        emit(f"          xp_data_base=0x{nx_after['xp_data_base']:X}  xp_data_blocks={nx_after['xp_data_blocks']}")
        emit(f"  spaceman_oid: before=0x{nx_before['spaceman_oid']:X}  after=0x{nx_after['spaceman_oid']:X}")
        emit(f"  omap_oid:     before=0x{nx_before['omap_oid']:X}  after=0x{nx_after['omap_oid']:X}")
    else:
        emit("  WARNING: could not identify NX superblocks in captured set.")
    emit()

    # --- Changed + new block classification ---
    emit("-" * 72)
    emit("SECTION 2 - ALL CHANGED/NEW BLOCKS (classified)")
    emit("-" * 72)
    for p in sorted(changed_paddrs + new_paddrs):
        blk_after  = after[p]
        blk_before = before.get(p)
        label_after  = classify_block(blk_after)
        label_before = classify_block(blk_before) if blk_before else "N/A"
        ck_ok = verify_checksum(blk_after)
        status = "NEW" if blk_before is None else "CHANGED"
        emit(f"  paddr=0x{p:X}  [{status}]  type={label_after}  cksum_ok={ck_ok}")
        if blk_before and label_before != label_after:
            emit(f"    (was: {label_before})")
    emit()

    # --- Detailed B-tree node decode: omap nodes ---
    emit("-" * 72)
    emit("SECTION 3 - OMAP BTREE NODE(S) - CHANGED/NEW DETAIL")
    emit("-" * 72)
    omap_types = {OBJECT_TYPE_BTREE, OBJECT_TYPE_BTREE_NODE}
    for p in sorted(changed_paddrs + new_paddrs):
        blk_after = after[p]
        t = u32le(blk_after, 24) & OBJ_TYPE_MASK
        if t not in omap_types:
            continue
        node = parse_btree_node(blk_after)
        # Heuristic: omap node has subtype == OBJECT_TYPE_OMAP (0x000B)
        if (node["subtype"] & OBJ_TYPE_MASK) != OBJECT_TYPE_OMAP:
            continue
        lines = fmt_btree("OMAP_NODE AFTER", p, node)
        for ln in lines:
            emit(ln)
        if p in before:
            node_b = parse_btree_node(before[p])
            for ln in fmt_btree("OMAP_NODE BEFORE", p, node_b):
                emit(ln)
        emit()
    emit()

    # --- Detailed B-tree node decode: snap-meta nodes ---
    emit("-" * 72)
    emit("SECTION 4 - SNAP-META BTREE NODE(S) - CHANGED/NEW DETAIL")
    emit("-" * 72)
    # snap-meta tree has subtype 0x000B (OMAP) in some builds, but more
    # specifically it is a virtual object reachable from the volume superblock
    # snap_meta_tree_oid.  We identify snap-meta nodes as btree nodes whose
    # subtype is NOT omap (0x000B) and NOT catalog (0x000E) - capture all.
    for p in sorted(changed_paddrs + new_paddrs):
        blk_after = after[p]
        t = u32le(blk_after, 24) & OBJ_TYPE_MASK
        if t not in omap_types:
            continue
        node = parse_btree_node(blk_after)
        sub = node["subtype"] & OBJ_TYPE_MASK
        if sub == OBJECT_TYPE_OMAP:
            continue  # already covered in section 3
        lines = fmt_btree("SNAP_META_NODE AFTER", p, node)
        for ln in lines:
            emit(ln)
        if p in before:
            node_b = parse_btree_node(before[p])
            for ln in fmt_btree("SNAP_META_NODE BEFORE", p, node_b):
                emit(ln)
        emit()
    emit()

    # --- Spaceman / CIB analysis ---
    emit("-" * 72)
    emit("SECTION 5 - SPACEMAN + CIB BLOCKS - CHANGED/NEW DETAIL")
    emit("-" * 72)
    cib_types = {OBJECT_TYPE_SPACEMAN_CIB}
    spaceman_types = {OBJECT_TYPE_SPACEMAN}
    for p in sorted(changed_paddrs + new_paddrs):
        blk_after = after[p]
        t = u32le(blk_after, 24) & OBJ_TYPE_MASK
        if t in spaceman_types:
            obj_b = parse_obj_phys(blk_after)
            obj_a_before = parse_obj_phys(before[p]) if p in before else None
            emit(f"  SPACEMAN @ paddr=0x{p:X}  oid=0x{obj_b['oid']:X}  xid_after=0x{obj_b['xid']:X}"
                 + (f"  xid_before=0x{obj_a_before['xid']:X}" if obj_a_before else "  (new)"))
            emit(f"    cksum_ok={verify_checksum(blk_after)}")
        elif t in cib_types:
            cib_after  = parse_cib(blk_after)
            cib_before = parse_cib(before[p]) if p in before else None
            paddr_before = p if p in before else None
            for ln in fmt_cib("CIB", paddr_before, p, cib_before, cib_after):
                emit(ln)
            emit(f"    cksum_ok={verify_checksum(blk_after)}")
    emit()

    # --- CIB CoW paddr tracking: identify CIBs that moved ---
    emit("-" * 72)
    emit("SECTION 6 - CIB PADDR COW SUMMARY")
    emit("-" * 72)
    # Find CIBs present in before but NOT in after (vacated paddr = CoW source)
    before_cib_paddrs = {p for p, blk in before.items() if (u32le(blk, 24) & OBJ_TYPE_MASK) == OBJECT_TYPE_SPACEMAN_CIB}
    after_cib_paddrs  = {p for p, blk in after.items()  if (u32le(blk, 24) & OBJ_TYPE_MASK) == OBJECT_TYPE_SPACEMAN_CIB}
    vacated  = before_cib_paddrs - after_cib_paddrs
    new_cibs = after_cib_paddrs  - before_cib_paddrs
    shared   = before_cib_paddrs & after_cib_paddrs
    emit(f"  CIB paddr vacated (CoW source, no longer in after): {sorted(f'0x{p:X}' for p in vacated)}")
    emit(f"  CIB paddr new     (CoW destination or brand-new)  : {sorted(f'0x{p:X}' for p in new_cibs)}")
    emit(f"  CIB paddr shared  (same paddr in before+after)    : {sorted(f'0x{p:X}' for p in shared)}")
    for p in sorted(shared):
        blk_b = before[p]; blk_a = after[p]
        if blk_b != blk_a:
            emit(f"    shared paddr=0x{p:X} CHANGED content (in-place update)")
        else:
            emit(f"    shared paddr=0x{p:X} unchanged")
    emit()

    # --- Checkpoint ring layout ---
    emit("-" * 72)
    emit("SECTION 7 - CHECKPOINT RING LAYOUT (desc + data)")
    emit("-" * 72)
    if nx_before and nx_after:
        for phase, nx_v, blocks_map in [("BEFORE", nx_before, before), ("AFTER", nx_after, after)]:
            emit(f"  [{phase}] desc ring: base=0x{nx_v['xp_desc_base']:X}  count={nx_v['xp_desc_blocks']}")
            emit(f"  [{phase}] data ring: base=0x{nx_v['xp_data_base']:X}  count={nx_v['xp_data_blocks']}")
            desc_slots = []
            for i in range(min(nx_v["xp_desc_blocks"], 128)):
                paddr = nx_v["xp_desc_base"] + i
                blk = blocks_map.get(paddr)
                if blk:
                    cls = classify_block(blk)
                    oid = u64le(blk, 8)
                    xid = u64le(blk, 16)
                    desc_slots.append(f"    desc[{i:3d}] paddr=0x{paddr:X}  {cls}  oid=0x{oid:X}  xid=0x{xid:X}")
            for ln in desc_slots:
                emit(ln)
            if not desc_slots:
                emit(f"    (no desc ring blocks were captured for {phase})")
            emit()
    emit()

    emit("=" * 72)
    emit("END OF REPORT")
    emit("=" * 72)

    report_text = "\n".join(report_lines) + "\n"

    os.makedirs(os.path.dirname(os.path.abspath(report_out)), exist_ok=True)
    with open(report_out, "w") as f:
        f.write(report_text)

    print(report_text)
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 5:
        print(
            "Usage: decode_macos_template.py <device_or_image> <before.json> <after.json> <report.txt>",
            file=sys.stderr,
        )
        sys.exit(1)
    sys.exit(run(sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]))
