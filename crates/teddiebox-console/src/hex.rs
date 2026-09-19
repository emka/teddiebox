//! Eight hex digits to a `u32`.
//!
//! A `const fn`, because two very different callers need the same answer: the
//! console parsing what somebody typed, and a build baking a value out of the
//! environment into the image. One of those happens at compile time, and a
//! second implementation for it would be a second thing to get wrong about a
//! credential.

/// Digits a password must have. Exactly eight, because a privacy password is
/// a `u32` and a short one is a typo rather than a small number.
pub const DIGITS: usize = 8;

/// Parses exactly [`DIGITS`] hex digits. `None` for anything else at all.
///
/// Strict on purpose. Getting a privacy password wrong matters more than
/// usual: a tag refuses a wrong one by staying silent, which is also what an
/// empty plate and a broken antenna look like.
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

    /// The leading zeros are the case a number-shaped parser loses.
    #[test]
    fn leading_zeros_are_digits_like_any_other() {
        assert_eq!(u32_from_hex(b"00000001"), Some(1));
        assert_eq!(u32_from_hex(b"00000000"), Some(0));
    }

    #[test]
    fn either_case_of_letter_is_accepted() {
        assert_eq!(u32_from_hex(b"ABCDEF01"), u32_from_hex(b"abcdef01"));
    }

    /// Seven digits is a typo, and a typo that parsed would be a password that
    /// silences every tag it touches.
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

    /// The whole reason this is a `const fn`: a build computes it, so a wrong
    /// value fails the build rather than the bench.
    #[test]
    fn it_can_be_computed_at_compile_time() {
        const VALUE: Option<u32> = u32_from_hex(b"12345678");
        assert_eq!(VALUE, Some(0x1234_5678));
    }
}
