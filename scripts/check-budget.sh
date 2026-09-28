#!/usr/bin/env bash
# Fails a firmware build that the box could not run or could not receive.
#
# Stack: the stack region is whatever RAM the statics leave over, so a new
# static shrinks it without any compiler complaint. Too small a region shows
# only on the box, as a stack-guard panic in the middle of a story, and a box
# that overflows at boot needs J100 shorted to recover. The build cannot see
# how deep the stack goes, so the floor is the deepest use the box's `stack`
# command has measured (33,820 bytes, a boot and a story) plus one 4,096-byte
# page buffer, the size of the local whose second copy overflowed the stack
# with 1,020 bytes to spare. Raising the deepest use means measuring it again
# with `stack`, then changing DEEPEST_MEASURED here.
#
# Image: an image larger than its OTA slot flashes nowhere and can never be
# downloaded. espflash sizes the app image the way it will be written and
# refuses one that does not fit, against partitions.csv.

set -euo pipefail

ELF="${1:-firmware/target/xtensa-esp32s3-none-elf/release/teddiebox-firmware}"
TABLE="${TABLE:-partitions.csv}"

DEEPEST_MEASURED=33820
MARGIN=4096
floor=$((DEEPEST_MEASURED + MARGIN))

symbol() {
    local value
    value="$(xtensa-esp32s3-elf-nm "$ELF" | awk -v s="$1" '$3 == s { print $1 }')"
    [ -n "$value" ] || { echo "budget: no $1 in $ELF" >&2; exit 2; }
    echo $((16#$value))
}

region=$(($(symbol _stack_start_cpu0) - $(symbol _stack_end_cpu0)))
echo "budget: stack region $region bytes, floor $floor ($DEEPEST_MEASURED measured + $MARGIN)"
if [ "$region" -lt "$floor" ]; then
    echo "budget: the stack region is $((floor - region)) bytes below its floor." >&2
    echo "        Something added a static. Give the RAM back, or measure with" >&2
    echo "        \`stack\` on the box and update DEEPEST_MEASURED." >&2
    exit 1
fi

image="$(mktemp)"
trap 'rm -f "$image"' EXIT
# --flash-size as in scripts/flash.sh: espflash's offline default of 4 MB
# refuses this table.
if ! out="$(espflash save-image --chip esp32s3 --flash-size 8mb \
    --partition-table "$TABLE" "$ELF" "$image" 2>&1)"; then
    echo "$out" >&2
    echo "budget: espflash refused the image; see above" >&2
    exit 1
fi
echo "budget: image $(grep -o '[0-9,]*/[0-9,]* bytes, [0-9.]*%' <<< "$out") of its slot"
