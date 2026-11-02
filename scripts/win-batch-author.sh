#!/bin/sh
# Author the image the Windows write batch runs against (macOS only).
#
#   scripts/win-batch-author.sh OUT.img PLAIN_DIR
#   scripts/win-batch-author.sh /Volumes/NAME PLAIN_DIR   (a real drive)
#
# Given a mounted volume instead of an image name, the files go onto that
# volume (an empty APFS volume on a USB drive, for the batch's -Drive run).
#
# macOS formats a 128 MiB image and fills it with every kind of file the
# batch touches: plain files, a ditto-compressed file, two clone pairs
# (cp -c), two hard-link pairs, a sparse file, small files to rename over
# others, and every decmpfs method under /cmp (scripts/decmpfs-samples.py,
# whose plain copies go to PLAIN_DIR for the verifier).
#
# Then: scp OUT.img to the Windows box as C:\apfs\audit\win-batch.img, run
# scripts/win-batch.ps1 there, bring win-batch.img and win-batch.after back,
# and run scripts/win-batch-verify.py on them.
set -eu
out=$1
plain=$2
parent=""
if [ -d "$out" ]; then
  mount=$out
else
  base=${out%.img}
  rm -f "$base.dmg" "$out"
  hdiutil create -size 128m -layout GPTSPUD -fs APFS -volname WINBATCH "$base" >/dev/null
  mv "$base.dmg" "$out"
  attached=$(hdiutil attach -imagekey diskimage-class=CRawDiskImage -nobrowse "$out")
  parent=$(printf '%s\n' "$attached" | awk '$1 ~ /^\/dev\/disk[0-9]+$/ {print $1}' | head -1)
  mount=$(printf '%s\n' "$attached" | grep -o '/Volumes/.*' | head -1)
fi
python3 - "$mount" <<'EOF'
import os, subprocess, sys
m = sys.argv[1]
def pattern(n, seed):
    return bytes(((i * 7 + seed * 131) % 251) for i in range(n))
open(f"{m}/plain.bin", "wb").write(pattern(200 * 1024, 1))
open(f"{m}/plain2.bin", "wb").write(pattern(200 * 1024, 2))
src = "/tmp/win-batch-author-src"
os.makedirs(src, exist_ok=True)
open(f"{src}/compressed.bin", "wb").write(pattern(180 * 1024, 3))
subprocess.run(["ditto", "--hfsCompression", src, m], check=True)
open(f"{m}/clone_a.bin", "wb").write(pattern(160 * 1024, 4))
subprocess.run(["cp", "-c", f"{m}/clone_a.bin", f"{m}/clone_b.bin"], check=True)
open(f"{m}/clone2_a.bin", "wb").write(pattern(90 * 1024, 5))
subprocess.run(["cp", "-c", f"{m}/clone2_a.bin", f"{m}/clone2_b.bin"], check=True)
os.makedirs(f"{m}/sub", exist_ok=True)
open(f"{m}/link_a.txt", "wb").write(pattern(60 * 1024, 6))
os.link(f"{m}/link_a.txt", f"{m}/sub/link_b.txt")
open(f"{m}/link2_a.txt", "wb").write(pattern(30 * 1024, 7))
os.link(f"{m}/link2_a.txt", f"{m}/sub/link2_b.txt")
open(f"{m}/sparse.bin", "wb").write(pattern(10000, 8))
subprocess.run(["truncate", "-s", "100000", f"{m}/sparse.bin"], check=True)
open(f"{m}/other.txt", "wb").write(pattern(8000, 9))
open(f"{m}/other2.txt", "wb").write(pattern(9000, 10))
EOF
mkdir -p "$mount/cmp"
python3 "$(dirname "$0")/decmpfs-samples.py" author "$mount/cmp" "$plain" > /dev/null
sync
if [ -n "$parent" ]; then
  hdiutil detach "$parent" >/dev/null 2>&1 || hdiutil detach -force "$parent" >/dev/null
fi
echo "authored $out (plain copies of /cmp in $plain)"
