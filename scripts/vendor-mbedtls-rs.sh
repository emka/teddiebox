#!/usr/bin/env bash
#
# Materialises `firmware/vendor/mbedtls-rs`: the published crate, with one
# call added.
#
# `mbedtls-rs` 0.2.0 never calls `mbedtls_ssl_conf_max_frag_len`, and keeps the
# `mbedtls_ssl_config` it builds private, so a downstream crate cannot reach it
# — not before the handshake, which is the only time the setting has any
# effect. That leaves this box unable to say how large a TLS record it can
# take, and the size it can take is the whole problem:
#
#   - with `ssl-in-content-len-8192`, teddyCloud's CycloneSSL sends records of
#     `TLS_MAX_RECORD_LENGTH` (16384) and `mbedtls_ssl_fetch_input` refuses
#     them — "requesting more data than fits", reported as the thoroughly
#     misleading `MBEDTLS_ERR_SSL_BAD_INPUT_DATA` (-0x7100).
#   - with `ssl-in-content-len-16384`, the records fit and the *handshake* no
#     longer does: the client certificate's RSA private operation runs out of
#     heap, `MBEDTLS_ERR_RSA_PRIVATE_FAILED + MBEDTLS_ERR_MPI_ALLOC_FAILED`
#     (-0x4310).
#
# Both measured on the bench. RFC 6066's max_fragment_length is the way out of
# that squeeze rather than a tuning of it: the server is built with
# `TLS_MAX_FRAG_LEN_SUPPORT ENABLED`, so asking for 2048-byte records makes
# them fit the buffer this box can already afford.
#
# Why fetch-and-patch rather than committing the crate: the same reasoning as
# `vendor-mbedtls-rs-sys.sh`, and the same guarantee — the tarball is pinned by
# the SHA-256 crates.io publishes, so what this produces is reproducible and a
# patch that stops applying stops the build rather than silently doing nothing.
#
# Removing this: drop the `mbedtls-rs` line from `[patch.crates-io]` in
# firmware/Cargo.toml, its line from the `vendor` recipe, and this script. Do
# that the moment `mbedtls-rs` exposes the setting itself.

set -euo pipefail

VERSION="0.2.0"
# The checksum crates.io's index records for this exact tarball; it is the same
# value `firmware/Cargo.lock` carries for the crate.
CKSUM="7ffe70b1a677b6efabede0d8e762f6187e66bff5118814aee2dcbfefad187add"

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="$root/firmware/vendor/mbedtls-rs"
stamp="$dest/.teddiebox-vendor-stamp"
want="$VERSION $CKSUM max-frag-len-2048"

# Idempotent: every build runs this, and only the first one does any work.
if [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ]; then
    exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "vendoring mbedtls-rs $VERSION"
curl -fsSL -o "$work/crate.tar.gz" \
    "https://static.crates.io/crates/mbedtls-rs/mbedtls-rs-$VERSION.crate"

got="$(sha256sum "$work/crate.tar.gz" | cut -d' ' -f1)"
if [ "$got" != "$CKSUM" ]; then
    echo "mbedtls-rs-$VERSION.crate is not the tarball this script was written against" >&2
    echo "  expected $CKSUM" >&2
    echo "  got      $got" >&2
    exit 1
fi

tar -xzf "$work/crate.tar.gz" -C "$work"

# The anchor is the neighbouring setter, matched whole so that a reformatting
# upstream stops the build rather than quietly leaving the call out. A silent
# no-op here costs a download that fails on its first body read with an error
# that names the wrong thing.
python3 - "$work/mbedtls-rs-$VERSION/src/session.rs" <<'PATCH'
import sys, pathlib

path = pathlib.Path(sys.argv[1])
source = path.read_text()

anchor = """        unsafe {
            mbedtls_ssl_conf_authmode(&mut *ssl_config, conf.auth_mode().mbedtls_authmode());
        }
"""
if source.count(anchor) != 1:
    sys.exit(
        f"expected exactly one authmode block in src/session.rs, found {source.count(anchor)}"
    )

added = anchor + """
        // teddiebox: RFC 6066 max_fragment_length. Without it the peer is free
        // to send 16 KB records, which this build's input buffer cannot hold —
        // and it cannot be made to hold them, because the heap that would cost
        // is the heap the handshake's RSA private operation needs.
        //
        // 3 is MBEDTLS_SSL_MAX_FRAG_LEN_2048, and 2048 rather than the 4096
        // the input buffer could take because the extension carries one length
        // for both directions: mbedtls refuses any code above
        // MBEDTLS_TLS_EXT_ADV_CONTENT_LEN, which is the *smaller* of the two
        // content lengths, and `ssl-out-content-len-2048` is the smaller one.
        // Asking for 4096 fails the config outright with BAD_INPUT_DATA.
        //
        // The constants are plain `#define`s the bindings do not re-export.
        merr!(unsafe { mbedtls_ssl_conf_max_frag_len(&mut *ssl_config, 3) })?;
"""

path.write_text(source.replace(anchor, added))
PATCH

rm -rf "$dest"
mkdir -p "$(dirname "$dest")"
mv "$work/mbedtls-rs-$VERSION" "$dest"
echo "$want" > "$stamp"
