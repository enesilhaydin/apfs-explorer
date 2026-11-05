#!/bin/sh
# Seed corpus for the open_image fuzz target (macOS: hdiutil).
#
# Two small whole-disk images: a volume holding a folder tree, a large
# file, a symlink, a hard link and an extended attribute; and the same
# volume with a snapshot taken on it.
#
#   sh scripts/fuzz-seeds.sh            # writes fuzz/corpus/open_image/
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/fuzz/corpus/open_image"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

src="$work/src"
mkdir -p "$src/dir/sub"
echo hello > "$src/a.txt"
head -c 70000 /dev/urandom > "$src/dir/big.bin"
ln -s ../a.txt "$src/dir/link"
ln "$src/a.txt" "$src/dir/hard.txt"
xattr -w com.example.note "a note" "$src/a.txt"
awk 'BEGIN { for (i = 0; i < 5000; i++) printf "x" }' > "$src/dir/sub/text.txt"

hdiutil create -quiet -size 2m -srcfolder "$src" -fs APFS -layout GPTSPUD \
    -format UDRW -volname SEED "$work/seed"
cp "$work/seed.dmg" "$work/seed-snap.dmg"
cargo run --quiet --release --manifest-path "$root/Cargo.toml" -p winfsp-host \
    --bin apfs-mount -- snapshot create --image "$work/seed-snap.dmg" --name seed-snap

mkdir -p "$out"
cp "$work/seed.dmg" "$work/seed-snap.dmg" "$out/"
echo "seeds in $out"
