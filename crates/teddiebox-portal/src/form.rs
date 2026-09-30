//! Decoding `application/x-www-form-urlencoded` form bodies.

use heapless::Vec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormError {
    /// No field of that name in the body.
    NotFound,
    /// The decoded value does not fit the caller's capacity.
    TooLong,
    /// A `%` not followed by two hex digits.
    BadEscape,
}

/// Decodes one named field out of a form body.
///
/// The name must match the whole field name: `configuration=x` does not
/// match `config`.
pub fn field<const N: usize>(body: &[u8], name: &str) -> Result<Vec<u8, N>, FormError> {
    let raw = body
        .split(|&b| b == b'&')
        .find_map(|pair| {
            let (key, value) = split_once(pair, b'=')?;
            (key == name.as_bytes()).then_some(value)
        })
        .ok_or(FormError::NotFound)?;

    let mut out = Vec::new();
    let mut bytes = raw.iter().copied();
    while let Some(b) = bytes.next() {
        let decoded = match b {
            b'+' => b' ',
            b'%' => {
                let hi = bytes.next().ok_or(FormError::BadEscape)?;
                let lo = bytes.next().ok_or(FormError::BadEscape)?;
                unhex(hi)? << 4 | unhex(lo)?
            }
            other => other,
        };
        out.push(decoded).map_err(|_| FormError::TooLong)?;
    }
    Ok(out)
}

fn split_once(bytes: &[u8], sep: u8) -> Option<(&[u8], &[u8])> {
    let at = bytes.iter().position(|&b| b == sep)?;
    Some((&bytes[..at], &bytes[at + 1..]))
}

fn unhex(b: u8) -> Result<u8, FormError> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(FormError::BadEscape),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plus_is_a_space() {
        // Given
        let body = b"config=a+b";

        // When
        let got: heapless::Vec<u8, 64> = field(body, "config").unwrap();

        // Then
        assert_eq!(&got[..], b"a b");
    }

    #[test]
    fn percent_escapes_decode() {
        // Given
        let body = b"config=a%23b%26c";

        // When
        let got: heapless::Vec<u8, 64> = field(body, "config").unwrap();

        // Then
        assert_eq!(&got[..], b"a#b&c");
    }

    #[test]
    fn lower_case_hex_decodes() {
        // Given
        let body = b"config=%0d%0a";

        // When
        let got: heapless::Vec<u8, 64> = field(body, "config").unwrap();

        // Then
        assert_eq!(&got[..], b"\r\n");
    }

    #[test]
    fn a_later_field_is_found() {
        // Given
        let body = b"other=1&config=x";

        // When
        let got: heapless::Vec<u8, 64> = field(body, "config").unwrap();

        // Then
        assert_eq!(&got[..], b"x");
    }

    #[test]
    fn a_prefix_of_the_name_is_not_the_field() {
        // Given
        let body = b"configuration=x";

        // When
        let got: Result<heapless::Vec<u8, 64>, _> = field(body, "config");

        // Then
        assert_eq!(got.unwrap_err(), FormError::NotFound);
    }

    #[test]
    fn an_empty_value_is_an_empty_field_not_a_missing_one() {
        // Given
        let body = b"config=";

        // When
        let got: heapless::Vec<u8, 64> = field(body, "config").unwrap();

        // Then
        assert_eq!(&got[..], b"");
    }

    #[test]
    fn a_truncated_escape_is_refused() {
        // Given
        let body = b"config=a%2";

        // When
        let got: Result<heapless::Vec<u8, 64>, _> = field(body, "config");

        // Then
        assert_eq!(got.unwrap_err(), FormError::BadEscape);
    }

    #[test]
    fn a_non_hex_escape_is_refused() {
        // Given
        let body = b"config=a%zz";

        // When
        let got: Result<heapless::Vec<u8, 64>, _> = field(body, "config");

        // Then
        assert_eq!(got.unwrap_err(), FormError::BadEscape);
    }

    /// A value of exactly the capacity is kept. (`MAX_CONFIG` is both the
    /// decode capacity and the largest file the box writes.)
    #[test]
    fn a_value_of_exactly_the_capacity_is_kept() {
        // Given
        let body = b"config=abcd";

        // When
        let got: heapless::Vec<u8, 4> = field(body, "config").unwrap();

        // Then
        assert_eq!(&got[..], b"abcd");
    }

    #[test]
    fn a_value_past_capacity_is_refused() {
        // Given
        let body = b"config=abcdefgh";

        // When
        let got: Result<heapless::Vec<u8, 4>, _> = field(body, "config");

        // Then
        assert_eq!(got.unwrap_err(), FormError::TooLong);
    }
}
