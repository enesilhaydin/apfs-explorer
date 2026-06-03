#!/usr/bin/env python3
"""redecode_file.py - zero-shared-code APFS file-existence re-decoder.

Usage:
    python3 redecode_file.py <image_path> <name> present|absent

Exit 0 on success (file found/not-found as requested), 1 on failure.

Independently re-decodes the APFS catalog (fsroot) B-tree from raw bytes and
checks that a directory entry record (DIR_REC) for <name> exists AND that the
inode it points to (INODE with private_id == drec.file_id) is present. This is
the decisive proof that create_file wrote structurally correct records that a
from-scratch parser (sharing no code with the Rust writer) can read back.

Spec anchors (the APFS specification):
  - j_key_t: obj_id_and_type, type = bits[63:60], id = bits[59:0]
  - APFS_TYPE_INODE = 3, APFS_TYPE_DIR_REC = 9
  - j_drec_hashed_key: jkey(8) + name_len_and_hash(u32) + name(UTF-8+NUL)
  - j_drec_val: file_id(u64) + date_added(u64) + flags(u16)
  - variable-kv leaf node (kvloc_t TOC), root footer btree_info_t = 40 bytes
"""

import struct
import sys

BLOCK_SIZE = 4096
SECTOR = 512
NX_MAGIC = b"NXSB"
APFS_MAGIC = b"APSB"
APFS_GPT_TYPE = bytes([
    0xEF, 0x57, 0x34, 0x7C, 0x00, 0x00, 0xAA, 0x11,
    0xAA, 0x11, 0x00, 0x30, 0x65, 0x43, 0xEC, 0xAC,
])
APFS_TYPE_INODE = 3
APFS_TYPE_DIR_REC = 9
DATA_BASE = 56
BTREE_INFO_SIZE = 40


def u16(b, o): return struct.unpack_from('<H', b, o)[0]
def u32(b, o): return struct.unpack_from('<I', b, o)[0]
def u64(b, o): return struct.unpack_from('<Q', b, o)[0]


def find_apfs_range(raw):
    assert raw[510:512] == b'\x55\xaa', "no protective MBR"
    hdr = raw[512:1024]
    assert hdr[:8] == b'EFI PART', "no GPT signature"
    part_entry_lba = u64(hdr, 72)
    num_parts = u32(hdr, 80)
    part_entry_size = u32(hdr, 84)
    entries_off = part_entry_lba * SECTOR
    for i in range(num_parts):
        off = entries_off + i * part_entry_size
        entry = raw[off:off + part_entry_size]
        if len(entry) < 48:
            break
        if entry[:16] == APFS_GPT_TYPE:
            start = u64(entry, 32)
            return start * SECTOR
    raise ValueError("no APFS partition")


def main():
    if len(sys.argv) != 4:
        print(f"usage: {sys.argv[0]} <image> <name> present|absent", file=sys.stderr)
        sys.exit(1)
    img, name, expect = sys.argv[1], sys.argv[2], sys.argv[3]
    assert expect in ('present', 'absent')
    raw = open(img, 'rb').read()
    base = find_apfs_range(raw)

    def blk(p):
        return raw[base + p * BLOCK_SIZE: base + (p + 1) * BLOCK_SIZE]

    # Latest volume superblock (highest o_xid).
    best = None
    off = base
    while True:
        p = raw.find(APFS_MAGIC, off)
        if p < 0:
            break
        v = raw[p - 32:p - 32 + BLOCK_SIZE]
        if best is None or u64(v, 16) > best[0]:
            best = (u64(v, 16), v)
        off = p + 4
    vsb = best[1]
    vol_xid = u64(vsb, 16)
    root_tree_oid = u64(vsb, 136)
    vol_omap = u64(vsb, 128)
    print(f"VSB xid={vol_xid} root_tree_oid={root_tree_oid:#x} "
          f"next_obj_id={u64(vsb, 176)} num_files={u64(vsb, 184)}")

    # Volume omap (fixed-kv) maps fstree-node virtual oids -> paddr.
    om = blk(vol_omap)
    omt = u64(om, 48)
    onode = blk(omt)
    onk = u32(onode, 36)
    oto = u16(onode, 40)
    otl = u16(onode, 42)
    oka = DATA_BASE + oto + otl
    ove = BLOCK_SIZE - BTREE_INFO_SIZE

    def omap_resolve(oid):
        best, bx = None, -1
        for i in range(onk):
            te = DATA_BASE + oto + i * 4
            k = oka + u16(onode, te)
            if u64(onode, k) == oid and u64(onode, k + 8) <= vol_xid \
                    and u64(onode, k + 8) > bx:
                bx = u64(onode, k + 8)
                best = u64(onode, ove - u16(onode, te + 2) + 8)
        return best

    APFS_TYPE_FILE_EXTENT = 8
    leaf_records = []  # (type, oid, key_bytes, val_bytes)

    def walk(oid):
        paddr = omap_resolve(oid)
        if paddr is None:
            print(f"ERROR: oid {oid:#x} not in omap", file=sys.stderr)
            sys.exit(1)
        n = blk(paddr)
        is_root = u16(n, 32) & 1
        level = u16(n, 34)
        nk = u32(n, 36)
        to = u16(n, 40)
        tl = u16(n, 42)
        ka = DATA_BASE + to + tl
        ve = BLOCK_SIZE - (BTREE_INFO_SIZE if is_root else 0)
        for i in range(nk):
            te = DATA_BASE + to + i * 8
            ko = u16(n, te)
            kl = u16(n, te + 2)
            vo = u16(n, te + 4)
            vl = u16(n, te + 6)
            k = ka + ko
            v = ve - vo
            if level > 0:
                walk(u64(n, v))  # child virtual oid
            else:
                leaf_records.append((n[k:k + kl], n[v:v + vl]))

    _rp = omap_resolve(root_tree_oid)
    _rlevel = u16(blk(_rp), 34) if _rp is not None else -1
    walk(root_tree_oid)
    print(f"catalog: root level={_rlevel}, {len(leaf_records)} leaf records "
          f"collected (multi-node walk)")

    file_id = None
    inode_ids = set()
    extents = {}  # file_id -> (logical_addr, byte_len, phys_block)
    for (kb, vb) in leaf_records:
        oidtype = u64(kb, 0)
        t = (oidtype >> 60) & 0xF
        oid = oidtype & 0x0FFF_FFFF_FFFF_FFFF
        if t == APFS_TYPE_DIR_REC and len(kb) >= 12:
            nm = kb[12:].split(b'\x00')[0].decode('utf-8', 'replace')
            if nm == name:
                file_id = u64(vb, 0)
                print(f"  DIR_REC '{nm}' under dir {oid} -> file_id={file_id}")
        if t == APFS_TYPE_INODE:
            inode_ids.add(oid)
        if t == APFS_TYPE_FILE_EXTENT:
            logical = u64(kb, 8)
            byte_len = u64(vb, 0) & 0x00FF_FFFF_FFFF_FFFF
            phys = u64(vb, 8)
            extents[oid] = (logical, byte_len, phys)

    found = file_id is not None and file_id in inode_ids
    # For a content file, report the FILE_EXTENT + data-block prefix.
    if found and file_id in extents:
        logical, byte_len, phys = extents[file_id]
        data = blk(phys)
        prefix = data[:32].split(b'\x00')[0]
        print(f"  FILE_EXTENT logical={logical} len={byte_len} phys_block={phys} "
              f"data_prefix={prefix!r}")
    print(f"name={name!r} found={found} (file_id={file_id}, "
          f"inode_present={file_id in inode_ids if file_id is not None else False}) "
          f"expect={expect}")

    if expect == 'present' and not found:
        print(f"FAIL: {name} expected present but not found", file=sys.stderr)
        sys.exit(1)
    if expect == 'absent' and found:
        print(f"FAIL: {name} expected absent but found", file=sys.stderr)
        sys.exit(1)
    print("OK")
    sys.exit(0)


if __name__ == '__main__':
    main()
