#!/usr/bin/env bash
#
# Materialises `firmware/vendor/mbedtls-rs-sys`: the published crate, with one
# line removed from `gen/features.rs`.
#
# The change drops `MBEDTLS_SSL_SERVER_NAME_INDICATION` from the `TLS_CORE`
# define bundle, so this build's TLS client never writes an SNI extension. That
# has to happen here rather than in `firmware/Cargo.toml`, because `mbedtls-rs`
# names `tls-core` directly in its own dependency on `mbedtls-rs-sys` — not
# behind an optional feature — so no feature selection downstream can switch
# it off.
#
# Why fetch-and-patch rather than committing the crate: it is 33 MB unpacked
# and roughly 8.8 MB of git objects, nearly all of it upstream MbedTLS C, to
# express the removal of one line. That is a poor trade for a workaround that
# should disappear the moment upstream offers a switch for SNI — and a copy in
# the history cannot be deleted later, only added to. The tarball is pinned by
# the same SHA-256 crates.io publishes in its index, so what this produces is
# as reproducible as what cargo itself would unpack.
#
# Why it is worth a patch: `mbedtls_ssl_set_hostname` sets the name a
# certificate is verified against *and* the SNI sent to the server, and this
# define is the only thing separating the two (`ssl_client.c` guards the SNI
# write with it; `get_hostname_for_verification` does not). teddyCloud picks
# its server certificate by SNI — any non-empty value selects a secp384r1 chain
# this build has no curve for, and the handshake dies on a fatal alert. Without
# the extension it answers with the RSA certificate the box's CA on the card
# actually issued. See `firmware/src/tls.rs`.
#
# Removing this: drop the `mbedtls-rs-sys` line from `[patch.crates-io]`, the
# `vendor` recipe from the justfile, the ignore rule from .gitignore, and this
# script.

set -euo pipefail

VERSION="0.3.1"
# The `cksum` crates.io's index records for this exact tarball.
CKSUM="ed3d6fe75492994df511a63da8de669a0c59938ada3345e3d4756aec095d2ffb"

# The `TLS_CORE` bundle's entry, matched with its indentation: the same
# identifier also appears in the flat list of every known define, four spaces
# in, and that one has to stay for the config generator to keep validating it.
SNI_LINE='            "SSL_SERVER_NAME_INDICATION",'

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="$root/firmware/vendor/mbedtls-rs-sys"
stamp="$dest/.teddiebox-vendor-stamp"
want="$VERSION $CKSUM no-sni"

# Idempotent: every build runs this, and only the first one does any work.
if [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ]; then
    exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "vendoring mbedtls-rs-sys $VERSION"
# Retried: a transient error from crates.io otherwise fails a whole CI gate,
# and the checksum below still rejects anything but the expected file.
curl -fsSL --retry 3 --retry-all-errors -o "$work/crate.tar.gz" \
    "https://static.crates.io/crates/mbedtls-rs-sys/mbedtls-rs-sys-$VERSION.crate"

got="$(sha256sum "$work/crate.tar.gz" | cut -d' ' -f1)"
if [ "$got" != "$CKSUM" ]; then
    echo "mbedtls-rs-sys-$VERSION.crate is not the tarball this script was written against" >&2
    echo "  expected $CKSUM" >&2
    echo "  got      $got" >&2
    exit 1
fi

tar -xzf "$work/crate.tar.gz" -C "$work"
features="$work/mbedtls-rs-sys-$VERSION/gen/features.rs"

# Exactly one match, or stop. A silent
# no-op here would ship a build that still writes an SNI, and the only symptom
# is a fatal alert from a server that used to work.
count="$(grep -c -F -x "$SNI_LINE" "$features" || true)"
if [ "$count" != "1" ]; then
    echo "expected exactly one TLS_CORE SNI entry in gen/features.rs, found $count" >&2
    exit 1
fi
grep -v -F -x "$SNI_LINE" "$features" > "$features.new"
mv "$features.new" "$features"

rm -rf "$dest"
mkdir -p "$(dirname "$dest")"
mv "$work/mbedtls-rs-sys-$VERSION" "$dest"
echo "$want" > "$stamp"
