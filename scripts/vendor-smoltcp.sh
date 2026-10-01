#!/usr/bin/env bash
#
# Materialises `firmware/vendor/smoltcp`: the published crate with
# `scripts/smoltcp-per-address-arp.patch` applied.
#
# smoltcp silences its *whole* neighbor cache for one second after sending any
# ARP request. The box's first connection after association needs two: the
# gateway, because it answers DNS, and then teddyCloud, a different host on the
# same link. The second one waited out the rest of that second: 1.03 s of
# every request (measured), against 0.01 s without the wait. The patch keeps
# the one-second silence but applies it per address, which is what it is for:
# not asking the same silent neighbor over and over. Upstream smoltcp still
# applies it to the whole cache:
# https://github.com/smoltcp-rs/smoltcp/issues/1209
#
# Fetched and patched rather than committed, for the reasons
# `scripts/vendor-mbedtls-rs-sys.sh` gives: the tarball is pinned by the
# SHA-256 crates.io records (the `checksum` in firmware/Cargo.lock), and the
# change itself is the reviewable diff beside this script.
#
# Removing this: drop the `smoltcp` line from `[patch.crates-io]` in
# firmware/Cargo.toml, this script and its patch from the `vendor` recipe and
# from scripts/, and the neighbor tests from the `test` recipe.

set -euo pipefail

VERSION="0.13.1"
# The `checksum` firmware/Cargo.lock and crates.io's index record for this tarball.
CKSUM="5f73d40463bba65efc9adc6370b56df76d563cc46e2482bba58351b4afb7535e"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
patch_file="$root/scripts/smoltcp-per-address-arp.patch"
dest="$root/firmware/vendor/smoltcp"
stamp="$dest/.teddiebox-vendor-stamp"
want="$VERSION $CKSUM $(sha256sum "$patch_file" | cut -d' ' -f1) standalone quiet"

# Idempotent: every build runs this, and only the first one after the patch
# changes does any work.
if [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ]; then
    exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "vendoring smoltcp $VERSION"
# Retried: a transient error from crates.io otherwise fails a whole CI gate,
# and the checksum below still rejects anything but the expected file.
curl -fsSL --retry 3 --retry-all-errors -o "$work/crate.tar.gz" \
    "https://static.crates.io/crates/smoltcp/smoltcp-$VERSION.crate"

got="$(sha256sum "$work/crate.tar.gz" | cut -d' ' -f1)"
if [ "$got" != "$CKSUM" ]; then
    echo "smoltcp-$VERSION.crate is not the tarball this script was written against" >&2
    echo "  expected $CKSUM" >&2
    echo "  got      $got" >&2
    exit 1
fi

tar -xzf "$work/crate.tar.gz" -C "$work"
# --forward, and fail on any error: a half-applied patch would still silence
# the whole cache, and the only symptom would be a slow first connection.
patch --quiet --forward -p1 -d "$work/smoltcp-$VERSION" < "$patch_file"

# Its own workspace root, so `just test` can run its tests from where it sits:
# otherwise cargo finds firmware/ or the repository root above it and refuses
# a package that is not their member. As a path dependency of the firmware it
# is unaffected — cargo ignores a dependency's `[workspace]` table.
printf '\n[workspace]\n' >> "$work/smoltcp-$VERSION/Cargo.toml"

# A path dependency is built as local code, so its lints are not capped as a
# registry crate's are, and every firmware build would print thirteen
# upstream warnings (unused imports and variables behind features the box
# does not enable).
printf '\n[lints.rust]\nwarnings = "allow"\n' >> "$work/smoltcp-$VERSION/Cargo.toml"

rm -rf "$dest"
mkdir -p "$(dirname "$dest")"
mv "$work/smoltcp-$VERSION" "$dest"
echo "$want" > "$stamp"
