#!/usr/bin/env python3
"""Compare what macOS reads from the Windows-modified image with what Windows saw.

usage: win-batch-verify.py IMAGE|VOLUME AFTER_MANIFEST
IMAGE is the image file; VOLUME is where macOS mounted a real drive the batch
ran on (e.g. /Volumes/WINBATCH), checked with `diskutil verifyVolume`.
AFTER_MANIFEST lines: "<sha256>  <size>  <relative path>" as the PowerShell scenario wrote them.
"""
import hashlib, os, subprocess, sys

img, manifest = sys.argv[1], sys.argv[2]
on_volume = os.path.isdir(img)

def attach(extra):
    out = subprocess.run(["hdiutil", "attach", "-imagekey", "diskimage-class=CRawDiskImage", "-nobrowse"] + extra + [img],
                         capture_output=True, text=True, check=True).stdout
    parent = next(l.split()[0] for l in out.splitlines() if l.split() and l.split()[0].startswith("/dev/disk") and "s" not in l.split()[0][len("/dev/disk"):])
    mount = next((l.split("\t")[-1].strip() for l in out.splitlines() if "/Volumes/" in l), "")
    return parent, mount

def detach(parent):
    for _ in range(8):
        if subprocess.run(["hdiutil", "detach", parent], capture_output=True).returncode == 0:
            return
    subprocess.run(["hdiutil", "detach", "-force", parent])

# 1. strict fsck
if on_volume:
    fsck = subprocess.run(["diskutil", "verifyVolume", img], capture_output=True, text=True)
else:
    parent, _ = attach(["-nomount"])
    rdev = parent.replace("/dev/disk", "/dev/rdisk") + "s1"
    fsck = subprocess.run(["fsck_apfs", "-n", rdev], capture_output=True, text=True)
    detach(parent)
report = fsck.stdout + fsck.stderr
complaints = [l for l in report.splitlines() if any(k in l.lower() for k in ("warning:", "overallocation", "underallocation", "is not valid"))]
print("FSCK exit", fsck.returncode, "complaints", len(complaints))
for c in complaints[:20]:
    print("  ", c)

# 2. macOS view vs Windows view
if on_volume:
    parent, mount = None, img
else:
    parent, mount = attach(["-readonly"])
seen = {}
for dp, dn, fn in os.walk(mount):
    dn[:] = [d for d in dn if not d.startswith(".")]
    for f in fn:
        if f.startswith("."):
            continue
        p = os.path.join(dp, f)
        if os.path.islink(p):
            continue
        rel = os.path.relpath(p, mount).replace(os.sep, "/")
        try:
            data = open(p, "rb").read()
            seen[rel] = (hashlib.sha256(data).hexdigest(), len(data), data)
        except OSError as e:
            seen[rel] = ("ERROR:%s" % e.strerror, -1, b"")
want = {}
for line in open(manifest, encoding="utf-8-sig"):
    line = line.rstrip("\r\n")
    if not line:
        continue
    h, size, rel = line.split("  ", 2)
    want[rel] = (h, int(size))
problems = []
for rel, (h, size) in sorted(want.items()):
    got = seen.get(rel)
    if got is None:
        problems.append("%s: Windows listed it, macOS does not see it" % rel)
    elif (got[0], got[1]) != (h, size):
        problems.append("%s: Windows %s/%d, macOS %s/%d" % (rel, h[:12], size, got[0][:12], got[1]))
for rel in sorted(set(seen) - set(want)):
    problems.append("%s: macOS sees it, Windows did not list it" % rel)
print("FILES windows", len(want), "macos", len(seen), "problems", len(problems))
for p in problems[:30]:
    print("  ", p)

def pattern(n, seed):
    return bytes(((i * 7 + seed * 131) % 251) for i in range(n))

# 3. semantic checks against what the scenario did
checks = [
    ("plain.bin", pattern(200 * 1024, 1) + bytes(307200 - 200 * 1024)),
    ("plain2.bin", pattern(200 * 1024, 2)[:153723]),
    ("sparse.bin", pattern(10000, 8) + bytes(200000 - 10000)),
    ("compressed.bin", pattern(180 * 1024, 3) + bytes(200000 - 180 * 1024)),
    ("clone_b.bin", pattern(160 * 1024, 4)),
    ("clone2_a.bin", bytes(((i * 11) % 251) for i in range(70000))),
    ("clone2_b.bin", pattern(8000, 9)),
    ("sub/link_b.txt", pattern(60 * 1024, 6)),
    ("sub/link2_b.txt", pattern(9000, 10)),
    ("link2_a.txt", b"through the link"),
    ("cmp/m13_lzbitmap_attr_text.bin", b"replaced on windows"),
]
plain_dir = "/tmp/apfs-win-batch-plain"
m12 = bytearray(open(os.path.join(plain_dir, "m12_lzfse_rsrc_big.bin"), "rb").read())
m12[1000:6000] = bytes(((i * 3) % 251) for i in range(5000))
checks.append(("cmp/m12_lzfse_rsrc_big.bin", bytes(m12)))
checks.append(("cmp/m14_lzbitmap_rsrc_text.bin", open(os.path.join(plain_dir, "m14_lzbitmap_rsrc_text.bin"), "rb").read() + b"appended on windows"))
checks.append(("cmp/m04_zlib_rsrc_big.bin", open(os.path.join(plain_dir, "m04_zlib_rsrc_big.bin"), "rb").read()[:70001]))
checks.append(("cmp/m10_plain_rsrc_text.bin", open(os.path.join(plain_dir, "m09_plain_attr_text.bin"), "rb").read()))
checks.append(("cmp/lzvn_copy.bin", open(os.path.join(plain_dir, "m08_lzvn_rsrc_text.bin"), "rb").read()))
bad = 0
for rel, expect in checks:
    got = seen.get(rel)
    if got is None:
        print("  MISSING", rel); bad += 1
    elif got[2] != expect:
        print("  DIFFERS", rel, "macOS len", got[1], "expected", len(expect)); bad += 1
for gone in ("clone_a.bin", "link_a.txt", "other.txt", "other2.txt", "cmp/m07_lzvn_attr_random.bin", "cmp/m09_plain_attr_text.bin"):
    if gone in seen:
        print("  STILL PRESENT", gone); bad += 1
print("SEMANTIC checks", len(checks), "bad", bad)
if parent:
    detach(parent)
