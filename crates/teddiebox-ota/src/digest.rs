//! Reading and comparing the digest a manifest carries.
//!
//! **Nothing here hashes.** mbedtls is already compiled into the firmware with
//! `alg-sha256` and already holds the TLS session; a second SHA-256 would be a
//! second thing to be wrong about. What belongs on a host is the parsing and
//! the comparison, and that is all this is.

use crate::OtaError;

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Reads exactly 64 hex characters into 32 bytes.
///
/// **Exactly** — a digest one character short is not a digest with a shorter
/// value, it is a truncated line, and accepting it would compare 31 good bytes
/// and one invented one.
pub fn parse_hex32(value: &str) -> Result<[u8; 32], OtaError> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return Err(OtaError::MalformedDigest);
    }
    let mut out = [0u8; 32];
    for (i, pair) in bytes.chunks_exact(2).enumerate() {
        let hi = nibble(pair[0]).ok_or(OtaError::MalformedDigest)?;
        let lo = nibble(pair[1]).ok_or(OtaError::MalformedDigest)?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}
