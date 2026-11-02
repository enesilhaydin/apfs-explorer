#!/usr/bin/env python3
"""Produce decmpfs test material on macOS.

  decmpfs-samples.py author MOUNT_DIR PLAIN_DIR [--oddities]
      Creates one file per (compression method, content shape) under
      MOUNT_DIR, storing the content the way macOS tools do: in the
      com.apple.decmpfs attribute for the odd methods, in the resource fork
      for the even ones, with an empty data fork and the UF_COMPRESSED flag.
      The plain content goes to PLAIN_DIR under the same name. macOS is then
      asked to read every file back; a mismatch means the layout is wrong,
      and the script exits non-zero so that nothing downstream trusts it.
      Prints "name method size xattrs" per file, xattrs being the attribute
      names macOS lists for an ordinary caller. With --oddities, files whose
      decmpfs state is inconsistent are added as well and what macOS makes
      of each is printed as "odd name setup stat=... read=... xattrs=...";
      one of them (a resource-fork method without a fork) is corrupt by
      fsck's rules.

  decmpfs-samples.py harvest OUT_DIR [--per-method N] [--max-size BYTES]
      Walks system directories for files macOS itself stored compressed and
      copies, for up to N files per method, the decmpfs attribute
      (<n>.decmpfs), the resource fork (<n>.rsrc, when present) and the
      content macOS returns (<n>.plain). Prints "n method size path" per
      file. Nothing is modified.

Methods: 3/4 zlib, 7/8 LZVN, 9/10 plain, 11/12 LZFSE, 13/14 LZBITMAP.
"""
import ctypes
import os
import random
import struct
import sys
import zlib

MAGIC = 0x636D7066
CHUNK = 65536
UF_COMPRESSED = 0x20
XATTR_NOFOLLOW = 0x01
XATTR_CREATE = 0x02
XATTR_SHOWCOMPRESSION = 0x20
MAX_ATTR_VALUE = 3802
COMPRESSION_LZFSE = 0x801
COMPRESSION_LZVN = 0x900
COMPRESSION_LZBITMAP = 0x702
RSRC_DATA_OFFSET = 0x100
RSRC_MAP_LEN = 50

METHODS = {
    3: ("zlib", "attr"),
    4: ("zlib", "rsrc"),
    7: ("lzvn", "attr"),
    8: ("lzvn", "rsrc"),
    9: ("plain", "attr"),
    10: ("plain", "rsrc"),
    11: ("lzfse", "attr"),
    12: ("lzfse", "rsrc"),
    13: ("lzbitmap", "attr"),
    14: ("lzbitmap", "rsrc"),
}

libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
libc.setxattr.restype = ctypes.c_int
libc.setxattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]
libc.getxattr.restype = ctypes.c_ssize_t
libc.getxattr.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_uint32, ctypes.c_int]
libc.chflags.restype = ctypes.c_int
libc.chflags.argtypes = [ctypes.c_char_p, ctypes.c_uint32]
libc.listxattr.restype = ctypes.c_ssize_t
libc.listxattr.argtypes = [ctypes.c_char_p, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]


def visible_xattrs(path):
    """The attribute names macOS shows an ordinary caller, comma-joined ("-" if none)."""
    size = libc.listxattr(path.encode(), None, 0, XATTR_NOFOLLOW)
    if size <= 0:
        return "-"
    buf = ctypes.create_string_buffer(size)
    got = libc.listxattr(path.encode(), buf, size, XATTR_NOFOLLOW)
    if got <= 0:
        return "-"
    names = [n.decode() for n in buf.raw[:got].split(b"\0") if n]
    return ",".join(sorted(names)) or "-"
comp = ctypes.CDLL("/usr/lib/libcompression.dylib")
comp.compression_encode_buffer.restype = ctypes.c_size_t
comp.compression_encode_buffer.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p, ctypes.c_int]


def die(msg):
    print(msg, file=sys.stderr)
    sys.exit(1)


def setxattr(path, name, value):
    rc = libc.setxattr(path.encode(), name, value, len(value), 0, XATTR_NOFOLLOW | XATTR_CREATE)
    if rc != 0:
        die("setxattr %s on %s: errno %d" % (name.decode(), path, ctypes.get_errno()))


def getxattr(path, name):
    size = libc.getxattr(path.encode(), name, None, 0, 0, XATTR_SHOWCOMPRESSION)
    if size < 0:
        return None
    buf = ctypes.create_string_buffer(size + 1)
    got = libc.getxattr(path.encode(), name, buf, size + 1, 0, XATTR_SHOWCOMPRESSION)
    if got < 0:
        return None
    return buf.raw[:got]


def encode(data, algo):
    cap = len(data) * 2 + (1 << 16)
    dst = ctypes.create_string_buffer(cap)
    n = comp.compression_encode_buffer(dst, cap, data, len(data), None, algo)
    return dst.raw[:n] if n else None


# --- chunk encoders: one compressed chunk, or the raw form with its marker ---

def chunk_zlib(data):
    z = zlib.compress(data, 9)
    return z if len(z) < len(data) else b"\xff" + data


def chunk_lzvn(data):
    z = encode(data, COMPRESSION_LZVN)
    return z if z is not None and len(z) < len(data) else b"\x06" + data


def chunk_lzfse(data):
    z = encode(data, COMPRESSION_LZFSE)
    return z if z is not None and len(z) < len(data) else b"\xff" + data


def chunk_lzbitmap(data):
    z = encode(data, COMPRESSION_LZBITMAP)
    return z if z is not None and len(z) < len(data) else b"\xff" + data


def chunk_plain(data):
    return b"\xcc" + data


CHUNKERS = {
    "zlib": chunk_zlib,
    "lzvn": chunk_lzvn,
    "lzfse": chunk_lzfse,
    "lzbitmap": chunk_lzbitmap,
    "plain": chunk_plain,
}


# --- resource fork layouts -------------------------------------------------

def fork_flat(chunks):
    """LZVN, LZFSE, LZBITMAP and plain: a little-endian offset table, then chunks."""
    off = 4 * (len(chunks) + 1)
    table = []
    for c in chunks:
        table.append(off)
        off += len(c)
    table.append(off)
    return b"".join(struct.pack("<I", o) for o in table) + b"".join(chunks)


def fork_resource(chunks):
    """zlib and plain: a Resource Manager fork holding one 'cmpf' resource."""
    off = 4 + 8 * len(chunks)
    entries = []
    for c in chunks:
        entries.append((off, len(c)))
        off += len(c)
    body = struct.pack("<I", len(chunks))
    body += b"".join(struct.pack("<II", o, s) for o, s in entries)
    body += b"".join(chunks)
    data = struct.pack(">I", len(body)) + body
    header = struct.pack(">IIII", RSRC_DATA_OFFSET, RSRC_DATA_OFFSET + len(data), len(data), RSRC_MAP_LEN)
    # The map's leading header copy has to be zero: with the header repeated
    # there, macOS refuses the file with EINVAL (measured on macOS 26).
    rmap = b"\0" * 16 + b"\0" * 4 + b"\0\0" + b"\0\0" + struct.pack(">HH", 0x1C, 0x32)
    rmap += struct.pack(">H", 0) + b"cmpf" + struct.pack(">HH", 0, 0x0A)
    rmap += struct.pack(">H", 1) + b"\xff\xff" + b"\0" + b"\0\0\0" + b"\0" * 4
    if len(rmap) != RSRC_MAP_LEN:
        die("resource map is %d bytes" % len(rmap))
    return header + b"\0" * (RSRC_DATA_OFFSET - len(header)) + data + rmap


def store(path, method, content):
    kind, place = METHODS[method]
    chunker = CHUNKERS[kind]
    with open(path, "wb"):
        pass
    header = struct.pack("<IIQ", MAGIC, method, len(content))
    if place == "attr":
        value = header + chunker(content)
        if len(value) > MAX_ATTR_VALUE:
            die("%s: inline value of %d bytes is over the limit" % (path, len(value)))
        setxattr(path, b"com.apple.decmpfs", value)
    else:
        chunks = [chunker(content[i:i + CHUNK]) for i in range(0, len(content), CHUNK)]
        # Only zlib uses the Resource Manager layout. Plain chunks go in the
        # flat table like LZVN/LZFSE/LZBITMAP, and macOS insists on the 0xCC
        # marker in front of each of them (measured on macOS 26).
        fork = fork_resource(chunks) if kind == "zlib" else fork_flat(chunks)
        setxattr(path, b"com.apple.ResourceFork", fork)
        setxattr(path, b"com.apple.decmpfs", header)
    if libc.chflags(path.encode(), UF_COMPRESSED) != 0:
        die("chflags %s: errno %d" % (path, ctypes.get_errno()))


def text(rng, n):
    words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa"]
    out = bytearray()
    while len(out) < n:
        out += (" ".join(rng.choice(words) for _ in range(rng.randint(3, 9))) + ".\n").encode()
    return bytes(out[:n])


def shapes(rng, place):
    if place == "attr":
        return [
            ("text", text(rng, 3000)),
            ("random", rng.randbytes(300)),
            ("one", b"x"),
        ]
    return [
        ("text", text(rng, 70000)),
        ("random", rng.randbytes(70000)),
        ("mixed", text(rng, CHUNK) + rng.randbytes(CHUNK) + b"tail"),
        ("exact", text(rng, CHUNK)),
        ("big", text(rng, 300000)),
        ("tiny", b"y"),
    ]


def try_setxattr(path, name, value):
    rc = libc.setxattr(path.encode(), name, value, len(value), 0, XATTR_NOFOLLOW | XATTR_CREATE)
    return 0 if rc == 0 else ctypes.get_errno()


def try_chflags(path, flags):
    return 0 if libc.chflags(path.encode(), flags) == 0 else ctypes.get_errno()


def measure(path):
    """What macOS says about a file: stat size or errno, read length or errno."""
    try:
        stat = "stat=OK:%d" % os.stat(path).st_size
    except OSError as e:
        stat = "stat=ERR:%d" % e.errno
    data = None
    try:
        fd = os.open(path, os.O_RDONLY)
        try:
            chunks = []
            while True:
                piece = os.read(fd, 1 << 20)
                if not piece:
                    break
                chunks.append(piece)
            data = b"".join(chunks)
        finally:
            os.close(fd)
        read = "read=OK:%d" % len(data)
    except OSError as e:
        data = None
        read = "read=ERR:%d" % e.errno
    return stat, read, data


def oddities(mount, plain_dir):
    """Files whose decmpfs state is inconsistent, so that macOS can be asked
    what it does with them; prints "odd name setup stat read" per file and
    keeps whatever macOS read as the plain copy."""
    rng = random.Random(7)
    fork = text(rng, 2000)
    inline = text(rng, 300)
    good = struct.pack("<IIQ", MAGIC, 9, len(inline)) + b"\xcc" + inline
    cases = [
        # decmpfs attribute present, UF_COMPRESSED clear, data fork holds content
        ("odd_flag_clear.bin", fork, [(b"com.apple.decmpfs", good)], False),
        # the same, with the flag set: which of the two does macOS serve?
        ("odd_flag_set_with_data.bin", fork, [(b"com.apple.decmpfs", good)], True),
        # flag set, header magic wrong
        ("odd_bad_magic.bin", b"", [(b"com.apple.decmpfs", b"XXXX" + good[4:])], True),
        # flag set, header shorter than 16 bytes
        ("odd_short_header.bin", b"", [(b"com.apple.decmpfs", good[:8])], True),
        # flag set, method number nobody implements
        ("odd_unknown_method.bin", b"", [(b"com.apple.decmpfs", struct.pack("<IIQ", MAGIC, 5, 5) + b"hello")], True),
        # flag set, resource-fork method, no resource fork
        ("odd_missing_fork.bin", b"", [(b"com.apple.decmpfs", struct.pack("<IIQ", MAGIC, 8, 100))], True),
        # flag set, no decmpfs attribute at all
        ("odd_flag_without_attribute.bin", fork, [], True),
    ]
    for name, data_fork, attrs, flag in cases:
        path = os.path.join(mount, name)
        with open(path, "wb") as f:
            f.write(data_fork)
        setup = []
        for attr_name, value in attrs:
            setup.append("setxattr=%d" % try_setxattr(path, attr_name, value))
        if flag:
            setup.append("chflags=%d" % try_chflags(path, UF_COMPRESSED))
        stat, read, data = measure(path)
        if data is not None:
            with open(os.path.join(plain_dir, name), "wb") as f:
                f.write(data)
        print("odd %s %s %s %s xattrs=%s" % (name, ",".join(setup) or "-", stat, read, visible_xattrs(path)))


def author(mount, plain_dir, with_oddities):
    os.makedirs(plain_dir, exist_ok=True)
    rng = random.Random(20260912)
    failures = 0
    for method, (kind, place) in sorted(METHODS.items()):
        for shape, content in shapes(rng, place):
            name = "m%02d_%s_%s_%s.bin" % (method, kind, place, shape)
            path = os.path.join(mount, name)
            store(path, method, content)
            with open(os.path.join(plain_dir, name), "wb") as f:
                f.write(content)
            st = os.stat(path)
            try:
                with open(path, "rb") as f:
                    back = f.read()
            except OSError as e:
                failures += 1
                print("MISMATCH %s: macOS refuses to read it: %s" % (name, e.strerror), file=sys.stderr)
                continue
            if not (st.st_flags & UF_COMPRESSED) or st.st_size != len(content) or back != content:
                failures += 1
                print("MISMATCH %s: flags %#x size %d read %d" % (name, st.st_flags, st.st_size, len(back)), file=sys.stderr)
                continue
            print("%s %d %d %s" % (name, method, len(content), visible_xattrs(path)))
    if with_oddities:
        oddities(mount, plain_dir)
    if failures:
        die("%d files did not read back through macOS" % failures)


def harvest(out_dir, per_method, max_size):
    os.makedirs(out_dir, exist_ok=True)
    roots = [
        "/usr/bin", "/usr/lib", "/usr/libexec", "/usr/share", "/System/Library/CoreServices",
        "/System/Library/Frameworks", "/System/Library/PrivateFrameworks", "/System/Volumes/Preboot/Cryptexes",
        "/System/Cryptexes", "/System/Applications", "/Applications", "/Library",
    ]
    rng = random.Random(20260912)
    picked = {}
    seen = {}
    for root in roots:
        for dirpath, _dirs, files in os.walk(root):
            for f in files:
                p = os.path.join(dirpath, f)
                try:
                    st = os.lstat(p)
                except OSError:
                    continue
                if not os.path.isfile(p) or os.path.islink(p) or not (st.st_flags & UF_COMPRESSED):
                    continue
                if st.st_size > max_size or not os.access(p, os.R_OK):
                    continue
                buf = ctypes.create_string_buffer(16)
                if libc.getxattr(p.encode(), b"com.apple.decmpfs", buf, 16, 0, XATTR_SHOWCOMPRESSION) < 16:
                    if ctypes.get_errno() != 34:
                        continue
                    libc.getxattr(p.encode(), b"com.apple.decmpfs", buf, 16, 0, XATTR_SHOWCOMPRESSION)
                    full = getxattr(p, b"com.apple.decmpfs")
                    if full is None or len(full) < 16:
                        continue
                    buf = ctypes.create_string_buffer(full[:16], 16)
                _magic, method, _size = struct.unpack("<IIQ", buf.raw[:16])
                seen[method] = seen.get(method, 0) + 1
                bucket = picked.setdefault(method, [])
                if len(bucket) < per_method:
                    bucket.append(p)
                else:
                    j = rng.randrange(seen[method])
                    if j < per_method:
                        bucket[j] = p
    n = 0
    for method in sorted(picked):
        for p in picked[method]:
            d = getxattr(p, b"com.apple.decmpfs")
            if d is None:
                continue
            with open(p, "rb") as f:
                plain = f.read()
            with open(os.path.join(out_dir, "%d.decmpfs" % n), "wb") as f:
                f.write(d)
            rf = getxattr(p, b"com.apple.ResourceFork")
            if rf is not None:
                with open(os.path.join(out_dir, "%d.rsrc" % n), "wb") as f:
                    f.write(rf)
            with open(os.path.join(out_dir, "%d.plain" % n), "wb") as f:
                f.write(plain)
            print("%d %d %d %s" % (n, method, len(plain), p))
            n += 1


def main(argv):
    if len(argv) >= 4 and argv[1] == "author":
        author(argv[2], argv[3], "--oddities" in argv[4:])
    elif len(argv) >= 3 and argv[1] == "harvest":
        per_method = 200
        max_size = 16 * 1024 * 1024
        rest = argv[3:]
        while rest:
            if rest[0] == "--per-method":
                per_method = int(rest[1])
            elif rest[0] == "--max-size":
                max_size = int(rest[1])
            else:
                die(__doc__)
            rest = rest[2:]
        harvest(argv[2], per_method, max_size)
    else:
        die(__doc__)


main(sys.argv)
