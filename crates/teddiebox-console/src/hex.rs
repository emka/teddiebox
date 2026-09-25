//! Eight hex digits to a `u32`.
//!
//! A `const fn`, so the same code serves both the console (parsing what was
//! typed) and the build (compiling a password from the environment into the
//! image).

/// Digits a password must have. Exactly eight, because a privacy password is
/// a `u32` and a shorter value is a typo.
pub const DIGITS: usize = 8;

/// Parses exactly [`DIGITS`] hex digits. `None` for anything else at all.
///
/// Strict on purpose: a tag answers a wrong password with silence, which
/// looks the same as an empty plate or a broken antenna.
pub const fn u32_from_hex(text: &[u8]) -> Option<u32> {
    if text.len() != DIGITS {
        return None;
    }

    let mut value: u32 = 0;
    let mut at = 0;
    // A `while` and an index rather than a `for`: iterators are not const.
    while at < text.len() {
        let nibble = match text[at] {
            digit @ b'0'..=b'9' => digit - b'0',
            lower @ b'a'..=b'f' => lower - b'a' + 10,
            upper @ b'A'..=b'F' => upper - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | nibble as u32;
        at += 1;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eight_digits_become_the_number_they_spell() {
        assert_eq!(u32_from_hex(b"deadbeef"), Some(0xDEAD_BEEF));
        assert_eq!(u32_from_hex(b"0000ffff"), Some(0x0000_FFFF));
    }

    /// Leading zeros count as digits.
    #[test]
    fn leading_zeros_are_digits_like_any_other() {
        assert_eq!(u32_from_hex(b"00000001"), Some(1));
        assert_eq!(u32_from_hex(b"00000000"), Some(0));
    }

    #[test]
    fn either_case_of_letter_is_accepted() {
        assert_eq!(u32_from_hex(b"ABCDEF01"), u32_from_hex(b"abcdef01"));
    }

    /// Seven digits is a typo, not a shorter password.
    #[test]
    fn anything_but_exactly_eight_digits_is_refused() {
        assert_eq!(u32_from_hex(b"deadbee"), None);
        assert_eq!(u32_from_hex(b"deadbeef0"), None);
        assert_eq!(u32_from_hex(b""), None);
    }

    #[test]
    fn a_non_digit_is_refused_rather_than_skipped() {
        assert_eq!(u32_from_hex(b"deadbeeg"), None);
        assert_eq!(u32_from_hex(b"dead beef"), None);
        assert_eq!(u32_from_hex(b"0xdeadbe"), None);
    }

    /// A wrong compiled-in value fails the build instead of the box.
    #[test]
    fn it_can_be_computed_at_compile_time() {
        const VALUE: Option<u32> = u32_from_hex(b"12345678");
        assert_eq!(VALUE, Some(0x1234_5678));
    }
}
