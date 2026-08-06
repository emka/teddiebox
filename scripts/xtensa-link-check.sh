#!/usr/bin/env bash
#
# Proves the device decode path resolves: builds the Rust side for the
# ESP32-S3 and merges it with the cross-built libopus, then checks what the
# result still expects from outside.
#
# `cargo check` cannot catch a wrong FFI symbol, and a host test cannot catch
# libopus needing something the device's C library does not have. This closes
# both. It stops short of producing a flashable image — see the note on
# `ld -r` below.

set -euo pipefail

TARGET=xtensa-esp32s3-none-elf
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE="$ROOT/tools/xtensa-linkcheck"
ARCHIVE="$CRATE/target/$TARGET/release/libxtensa_linkcheck.a"

# Symbols the firmware's runtime supplies, which the bare compiler toolchain
# has no reason to define. Anything appearing here that is *not* in this list
# is a new external dependency and needs a decision, not a silent pass.
#
#   __atomic_*  core's atomics; upstream LLVM does not model the ESP32-S3's
#               native atomic instructions, so it emits libcalls instead.
#   __getreent  newlib reentrancy, reached from libopus's fatal-error path.
declare -a EXPECTED_FROM_RUNTIME=(
    __atomic_load_1
    __atomic_load_2
    __atomic_load_4
    __atomic_store_4
    __getreent
)

echo "==> building the decode path for $TARGET"
(cd "$CRATE" && cargo build --release --target "$TARGET" -Z build-std=core)

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# A relocatable link, not a final one. It resolves every cross-object
# reference — which is what we are checking — without needing a memory
# layout, and without tripping over the fact that upstream rustc's Xtensa
# output cannot currently be finally linked by the ESP GNU linker (it emits
# literal pools at misaligned offsets, which ld rejects as a "dangerous
# relocation"). That is a toolchain defect, not one of ours; it is recorded
# in the design document and belongs to whoever picks up device bring-up.
echo "==> merging with libopus"
xtensa-esp32s3-elf-ld -r -u teddiebox_decode_first_frame \
    -o "$work/closure.o" "$ARCHIVE" 2>&1 | grep -v 'GNU-stack\|deprecated' || true

undefined="$(xtensa-esp32s3-elf-nm -u "$work/closure.o" | awk '{print $2}' | sort -u)"

if grep -q '^opus_' <<<"$undefined"; then
    echo "FAIL: libopus entry points are still undefined after linking:" >&2
    grep '^opus_' <<<"$undefined" >&2
    exit 1
fi
echo "    every libopus call resolved"

# Everything else must come from the C library the device toolchain ships,
# or be on the runtime list above.
libs=(
    "$(xtensa-esp32s3-elf-gcc -print-file-name=libc.a)"
    "$(xtensa-esp32s3-elf-gcc -print-file-name=libm.a)"
    "$(xtensa-esp32s3-elf-gcc -print-libgcc-file-name)"
)
provided="$(xtensa-esp32s3-elf-nm --defined-only "${libs[@]}" 2>/dev/null |
    awk '$2 ~ /^[TDBWRV]$/ {print $3}' | sort -u)"

status=0
while read -r symbol; do
    [ -n "$symbol" ] || continue
    if grep -qx "$symbol" <<<"$provided"; then
        continue
    fi
    for expected in "${EXPECTED_FROM_RUNTIME[@]}"; do
        [ "$symbol" = "$expected" ] && continue 2
    done
    echo "FAIL: $symbol is undefined and no library on the device provides it" >&2
    status=1
done <<<"$undefined"

if [ "$status" -eq 0 ]; then
    echo "    every remaining external is provided by newlib, libgcc, or the runtime"
fi
exit "$status"
