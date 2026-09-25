//! Reading and comparing the digest a manifest carries.
//!
//! **Nothing here computes a hash.** The firmware uses mbedtls's SHA-256,
//! which it already has for TLS. Only the parsing is here, so it can be tested
//! on the host.

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
/// **Exactly** 64: a shorter value is a cut-off line, not a valid digest.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_lower_case_digest() {
        let d = parse_hex32("3f786850e387550fdab836ed7e6dc881de23001b000000000000000000000000")
            .unwrap();
        assert_eq!(d[0], 0x3f);
        assert_eq!(d[1], 0x78);
        assert_eq!(d[19], 0x1b);
        assert_eq!(d[31], 0x00);
    }

    #[test]
    fn parses_an_upper_case_digest() {
        let d = parse_hex32("3F786850E387550FDAB836ED7E6DC881DE23001B000000000000000000000000")
            .unwrap();
        assert_eq!(d[0], 0x3f);
        assert_eq!(d[19], 0x1b);
    }

    #[test]
    fn refuses_a_digest_that_is_one_character_short() {
        assert_eq!(
            parse_hex32("3f786850e387550fdab836ed7e6dc881de23001b00000000000000000000000"),
            Err(OtaError::MalformedDigest)
        );
    }

    #[test]
    fn refuses_a_digest_that_is_one_character_long() {
        assert_eq!(
            parse_hex32("3f786850e387550fdab836ed7e6dc881de23001b0000000000000000000000000"),
            Err(OtaError::MalformedDigest)
        );
    }

    #[test]
    fn refuses_a_non_hex_character() {
        assert_eq!(
            parse_hex32("3f786850e387550fdab836ed7e6dc881de23001bg0000000000000000000000"),
            Err(OtaError::MalformedDigest)
        );
    }

    #[test]
    fn refuses_an_empty_value() {
        assert_eq!(parse_hex32(""), Err(OtaError::MalformedDigest));
    }
}
