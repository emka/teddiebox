#!/usr/bin/env bash
#
# Materialises `firmware/vendor/mbedtls-rs-sys`: the published crate, with one
# line of its `Cargo.toml` changed.
#
# `mbedtls-rs-sys` 0.2.0 declares `esp-hal = "~1.1.0"`, and `~1.1.0` means
# `>=1.1.0, <1.2.0`. This firmware is on `esp-hal 1.2.0-rc.0` (see the
# `[patch.crates-io]` block in firmware/Cargo.toml for why), so enabling the
# crate's `esp32s3` feature — the one that routes SHA, RSA and AES onto the
# chip's accelerators — cannot resolve. Every one of the nine `esp-hal` APIs
# those hooks use exists unchanged in 1.2, so the bound is the entire problem.
#
# Why fetch-and-patch rather than committing the crate: it is 33 MB unpacked
# and roughly 8.8 MB of git objects, nearly all of it upstream MbedTLS C, to
# express a change of eleven characters. That is a poor trade for a workaround
# that should disappear the moment upstream widens the bound — and a copy in
# the history cannot be deleted later, only added to. The tarball is pinned by
# the same SHA-256 crates.io publishes in its index, so what this produces is
# as reproducible as what cargo itself would unpack.
#
# Removing this: drop the `mbedtls-rs-sys` line from `[patch.crates-io]`, the
# `vendor` recipe from the justfile, the ignore rule from .gitignore, and this
# script.

set -euo pipefail

VERSION="0.2.0"
# The `cksum` crates.io's index records for this exact tarball.
CKSUM="d49d6c43db5aae2ef895972de7a9414cafb649fab288933e74bf7739c0fffe55"

# The bound as published, and what it has to become. A comma-separated
# requirement is an AND, so there is no way to name both 1.1 and 1.2.0-rc.0:
# cargo admits a pre-release only when some comparator names a pre-release of
# that same version, which rules out `>=1.1, <1.3.0` and `>=1.1.0-rc.0, <1.3.0`
# alike. This firmware only ever wants 1.2, so the narrow form is the honest
# one to write here.
BOUND_BEFORE='version = "~1.1.0"'
BOUND_AFTER='version = ">=1.2.0-rc.0, <1.3.0"'

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="$root/firmware/vendor/mbedtls-rs-sys"
stamp="$dest/.teddiebox-vendor-stamp"
want="$VERSION $CKSUM $BOUND_AFTER"

# Idempotent: every build runs this, and only the first one does any work.
if [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ]; then
    exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "vendoring mbedtls-rs-sys $VERSION"
curl -fsSL -o "$work/crate.tar.gz" \
    "https://static.crates.io/crates/mbedtls-rs-sys/mbedtls-rs-sys-$VERSION.crate"

got="$(sha256sum "$work/crate.tar.gz" | cut -d' ' -f1)"
if [ "$got" != "$CKSUM" ]; then
    echo "mbedtls-rs-sys-$VERSION.crate is not the tarball this script was written against" >&2
    echo "  expected $CKSUM" >&2
    echo "  got      $got" >&2
    exit 1
fi

tar -xzf "$work/crate.tar.gz" -C "$work"
manifest="$work/mbedtls-rs-sys-$VERSION/Cargo.toml"

# The bound appears once, under `[dependencies.esp-hal]`. If it ever appears
# elsewhere, or not at all, this script is out of date and a silent no-op
# would be worse than a stop.
count="$(grep -c -F "$BOUND_BEFORE" "$manifest" || true)"
if [ "$count" != "1" ]; then
    echo "expected exactly one '$BOUND_BEFORE' in the published manifest, found $count" >&2
    exit 1
fi
sed -i "s|$BOUND_BEFORE|$BOUND_AFTER|" "$manifest"

rm -rf "$dest"
mkdir -p "$(dirname "$dest")"
mv "$work/mbedtls-rs-sys-$VERSION" "$dest"
echo "$want" > "$stamp"
