#!/usr/bin/env bash
#
# Links the decode path into a real ESP32-S3 image.
#
# `cargo check` cannot see a mistyped FFI symbol, and no host test can see
# libopus needing something the device's C library does not have. Both only
# surface when a linker has to resolve them, which needs something that
# actually calls the code — `tools/xtensa-linkcheck` is that something.
#
# The result is never flashed. There is no memory layout here beyond the
# linker's default, and no startup code; what is being proved is that every
# symbol resolves and every relocation is representable on the target.

set -euo pipefail

TARGET=xtensa-esp32s3-none-elf
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CRATE="$ROOT/tools/xtensa-linkcheck"
ARCHIVE="$CRATE/target/$TARGET/release/libxtensa_linkcheck.a"
ENTRY=teddiebox_decode_first_frame

echo "==> building the decode path for $TARGET"
(cd "$CRATE" && cargo build --release --target "$TARGET" -Z build-std=core)

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
image="$work/decode-path.elf"

# Rooting the link at the decode entry point rather than a `main` keeps the
# reachable set to what playback actually needs, so --gc-sections can drop
# the rest and the reported size means something.
#
# The linker's output is kept back unless it fails. Linking with no runtime
# underneath makes newlib warn at length that its syscall stubs are
# unimplemented — true, expected, and the firmware's job to supply — and
# anything that actually matters here, an unresolved symbol or a relocation
# the target cannot represent, is an error rather than a warning.
echo "==> linking against libopus and the device C library"
if ! xtensa-esp32s3-elf-gcc \
    -nostartfiles \
    -Wl,--gc-sections \
    -Wl,-e,"$ENTRY" \
    -o "$image" \
    "$ARCHIVE" -lm -lc -lgcc >"$work/link.log" 2>&1; then
    echo "FAIL: the decode path does not link for $TARGET" >&2
    cat "$work/link.log" >&2
    exit 1
fi

machine="$(xtensa-esp32s3-elf-readelf -h "$image" | awk -F: '/Machine/ {print $2}' | xargs)"
if [ "$machine" != "Tensilica Xtensa Processor" ]; then
    echo "FAIL: linked image is for '$machine', not Xtensa" >&2
    exit 1
fi

if ! xtensa-esp32s3-elf-nm "$image" | grep -q " T opus_decode$"; then
    echo "FAIL: opus_decode is missing from the linked image" >&2
    exit 1
fi

echo "    linked an Xtensa image with the decoder reachable"
xtensa-esp32s3-elf-size "$image" | sed 's/^/    /'
