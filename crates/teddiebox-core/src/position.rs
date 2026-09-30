//! Where a story should resume, as it is written on the card.
//!
//! Plain text, like `CONFIG.TXT`, so it can be read on a laptop. Small enough
//! to fit in one sector, so a write is unlikely to be interrupted halfway.
//!
//! The number is an Ogg page index: the exact position, not the chapter.

use crate::Position;

/// Longest rendering: ten digits and a newline, which covers every `u32`.
pub const MAX_POSITION: usize = 11;

/// Writes `page` into `out`, returning how many bytes it used.
///
/// Written by hand rather than with `write!`, to avoid pulling in the
/// formatting machinery for one number.
pub fn render(page: u32, out: &mut [u8; MAX_POSITION]) -> usize {
    let mut digits = [0u8; 10];
    let mut n = page;
    let mut count = 0;
    loop {
        digits[count] = b'0' + (n % 10) as u8;
        n /= 10;
        count += 1;
        if n == 0 {
            break;
        }
    }
    for i in 0..count {
        out[i] = digits[count - 1 - i];
    }
    out[count] = b'\n';
    count + 1
}

/// Reads what [`render`] wrote.
///
/// Anything else is [`Position::Start`]. The card cannot be trusted: an
/// interrupted write can leave any bytes behind.
pub fn parse(text: &str) -> Position {
    match text.trim().parse::<u32>() {
        // Page 0 is the header, not audio, so it is never a resume point.
        // Writing 0 is how a finished story is cleared.
        Ok(0) | Err(_) => Position::Start,
        Ok(page) => Position::Exact { page },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_renders_as_its_number_and_a_newline() {
        // Given
        let mut out = [0u8; MAX_POSITION];

        // When
        let len = render(7, &mut out);

        // Then
        assert_eq!(&out[..len], b"7\n");
    }

    /// The longest rendering must fit, or the box panics writing it.
    #[test]
    fn the_largest_page_fills_the_rendering_exactly() {
        // Given
        let mut out = [0u8; MAX_POSITION];

        // When
        let len = render(u32::MAX, &mut out);

        // Then
        assert_eq!(&out[..len], b"4294967295\n");
        assert_eq!(len, MAX_POSITION);
    }

    #[test]
    fn what_was_rendered_parses_back() {
        // Given
        let mut out = [0u8; MAX_POSITION];
        let len = render(31_200, &mut out);
        let text = core::str::from_utf8(&out[..len]).unwrap();

        // When
        let position = parse(text);

        // Then
        assert_eq!(position, Position::Exact { page: 31_200 });
    }

    /// Page numbers can have more than five digits.
    #[test]
    fn a_page_late_in_a_long_story_survives_the_trip() {
        // Given
        let mut out = [0u8; MAX_POSITION];
        let len = render(4_000_000, &mut out);
        let text = core::str::from_utf8(&out[..len]).unwrap();

        // When
        let position = parse(text);

        // Then
        assert_eq!(position, Position::Exact { page: 4_000_000 });
    }

    /// Writing zero clears a finished story (the storage code cannot delete
    /// files), so it must mean the same as no file.
    #[test]
    fn zero_is_the_start_rather_than_a_page() {
        // Given
        let cleared = "0\n";

        // When
        let position = parse(cleared);

        // Then
        assert_eq!(position, Position::Start);
    }

    /// An interrupted write or a hand-edited file must fall back to the start.
    #[test]
    fn anything_unreadable_is_the_start() {
        // Given: empty, blank, words, two numbers, and a number beyond a u32
        let unreadable = ["", "\n", "twelve\n", "4 5\n", "99999999999\n"];

        // When
        let positions = unreadable.map(parse);

        // Then
        assert_eq!(positions, [Position::Start; 5]);
    }

    #[test]
    fn trailing_whitespace_is_tolerated() {
        // Given
        let texts = ["9\r\n", "9"];

        // When
        let positions = texts.map(parse);

        // Then
        assert_eq!(positions, [Position::Exact { page: 9 }; 2]);
    }
}
