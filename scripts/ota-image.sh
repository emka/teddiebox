#!/usr/bin/env bash
# Writes an update the box will accept: the app image and its manifest.
#
# The manifest's version must be exactly the version inside the image, or
# the box refuses it (and an older firmware would reflash it on every boot).
# So the version is read out of the image's ESP-IDF application descriptor,
# never typed or taken from git: magic word 0xABCD5432 at offset 0x20, the
# NUL-padded version at 0x30..0x50.
#
# Publish both files side by side in the directory `update_url` names.

set -euo pipefail

ELF="${1:-firmware/target/xtensa-esp32s3-none-elf/release/teddiebox-firmware}"
OUT="${2:-target/ota}"
TABLE="${TABLE:-partitions.csv}"
IMAGE_NAME=teddiebox.bin
MANIFEST_NAME=teddiebox.txt

mkdir -p "$OUT"
image="$OUT/$IMAGE_NAME"
# --flash-size as in scripts/flash.sh: espflash's offline default of 4 MB
# refuses this table.
espflash save-image --chip esp32s3 --flash-size 8mb \
    --partition-table "$TABLE" "$ELF" "$image" > /dev/null

magic="$(od -An -tx4 -j $((0x20)) -N4 "$image" | tr -d ' ')"
if [ "$magic" != "abcd5432" ]; then
    echo "ota-image: no application descriptor at 0x20 in $image" >&2
    exit 1
fi
version="$(dd if="$image" bs=1 skip=$((0x30)) count=32 status=none | tr -d '\0')"
if [ -z "$version" ] || [ "${#version}" -gt 31 ]; then
    echo "ota-image: version '$version' is empty or longer than 31 bytes" >&2
    exit 1
fi
case "$version" in
*-dirty)
    echo "ota-image: warning: $version was built from uncommitted changes" >&2
    ;;
esac

length="$(stat -c %s "$image")"
sha256="$(sha256sum "$image" | cut -d' ' -f1)"

cat > "$OUT/$MANIFEST_NAME" << EOF
version = $version
sha256 = $sha256
length = $length
image = $IMAGE_NAME
EOF

echo "ota-image: $version, $length bytes, in $OUT/"
