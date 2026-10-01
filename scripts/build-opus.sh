#!/usr/bin/env bash
# Builds the static libopus that teddiebox-opus-sys links, and installs it
# under <out>/lib.
#
#   scripts/build-opus.sh <opus-source-dir> <out-dir> [device]
#
# Without `device` the library is built for the host; with it, for the
# ESP32-S3, which needs the Xtensa GCC on PATH. The result is what
# TEDDIEBOX_OPUS_LIB_DIR_<TARGET> names, with `<out-dir>/lib` as the value.
#
# Fixed point on both sides on purpose: the host build is the reference the
# device's decoded samples are compared against, and a float build would
# produce different samples. The neural extensions are float-only and
# megabytes of weights, so they stay off.
set -euo pipefail

if [ "$#" -lt 2 ] || [ "$#" -gt 3 ]; then
    echo "usage: $0 <opus-source-dir> <out-dir> [device]" >&2
    exit 2
fi

src=$(realpath "$1")
out=$(realpath -m "$2")
here=$(dirname "$(realpath "$0")")

flags=(
    -DCMAKE_BUILD_TYPE=Release
    -DCMAKE_INSTALL_PREFIX="$out"
    -DCMAKE_INSTALL_LIBDIR=lib
    -DOPUS_BUILD_SHARED_LIBRARY=OFF
    -DOPUS_BUILD_PROGRAMS=OFF
    -DOPUS_BUILD_TESTING=OFF
    -DBUILD_TESTING=OFF
    -DOPUS_FIXED_POINT=ON
    -DOPUS_ENABLE_DEEP_PLC=OFF
    -DOPUS_DRED=OFF
    -DOPUS_OSCE=OFF
    # The host build is a stand-in for the device, which has no SIMD, so
    # timing it against hand-written NEON or SSE kernels would measure the
    # wrong machine. It also keeps the build off the architecture-specific
    # assembly paths, which is what makes one recipe work for every host.
    -DOPUS_DISABLE_INTRINSICS=ON
    # Bare metal has nothing to initialise the stack guard, and a check that
    # reads an uninitialised canary is worse than no check.
    -DOPUS_STACK_PROTECTOR=OFF
)

case "${3:-host}" in
host) ;;
device) flags+=(-DCMAKE_TOOLCHAIN_FILE="$here/xtensa-esp32s3.cmake") ;;
*)
    echo "unknown target '$3': expected 'device' or nothing" >&2
    exit 2
    ;;
esac

build=$(mktemp -d)
trap 'rm -rf "$build"' EXIT

cmake -S "$src" -B "$build" "${flags[@]}"
cmake --build "$build" --parallel
cmake --install "$build"
