//! The chapter a story should resume at, as it is written on the card.
//!
//! Text, like `CONFIG.TXT`, so a card can be read on a laptop when the box
//! does something surprising. Small enough to sit inside one sector, which is
//! what keeps the window for a torn write short.

use crate::Position;

/// Longest rendering: five digits and a newline.
pub const MAX_POSITION: usize = 6;

/// Writes `chapter` into `out`, returning how many bytes it used.
///
/// Hand-rolled rather than `write!`: this crate is `no_std` and a formatter
/// would pull in machinery for one number.
pub fn render(chapter: u16, out: &mut [u8; MAX_POSITION]) -> usize {
    let mut digits = [0u8; 5];
    let mut n = chapter;
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
/// Anything else is [`Position::Start`]. The card is not a trusted input — a
/// torn write leaves whatever the sector held — and no byte on it should be
/// able to strand a figure at a chapter its story does not have.
pub fn parse(text: &str) -> Position {
    match text.trim().parse::<u16>() {
        Ok(0) | Err(_) => Position::Start,
        Ok(chapter) => Position::Chapter(chapter),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chapter_renders_as_its_number_and_a_newline() {
        let mut out = [0u8; MAX_POSITION];
        let len = render(7, &mut out);
        assert_eq!(&out[..len], b"7\n");
    }

    #[test]
    fn what_was_rendered_parses_back() {
        let mut out = [0u8; MAX_POSITION];
        let len = render(312, &mut out);
        let text = core::str::from_utf8(&out[..len]).unwrap();
        assert_eq!(parse(text), Position::Chapter(312));
    }

    /// Zero is how a finished story is cleared, and it has to mean the same
    /// thing as no file at all — `storage` has no delete, so this is what
    /// clearing looks like.
    #[test]
    fn zero_is_the_start_rather_than_chapter_zero() {
        assert_eq!(parse("0\n"), Position::Start);
    }

    /// The card is not a trusted input. A torn write, a truncated sector or a
    /// file somebody edited by hand must not strand a figure.
    #[test]
    fn anything_unreadable_is_the_start() {
        assert_eq!(parse(""), Position::Start);
        assert_eq!(parse("\n"), Position::Start);
        assert_eq!(parse("twelve\n"), Position::Start);
        assert_eq!(parse("4 5\n"), Position::Start);
        assert_eq!(parse("999999\n"), Position::Start, "beyond a u16");
    }

    #[test]
    fn trailing_whitespace_is_tolerated() {
        assert_eq!(parse("9\r\n"), Position::Chapter(9));
        assert_eq!(parse("9"), Position::Chapter(9));
    }
}
