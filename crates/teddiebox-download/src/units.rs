//! Separate types for bytes and pages.
//!
//! The download writer counts bytes; the decoder's limit counts pages. Both
//! would otherwise be `u32`, so mixing them up would compile and either stall
//! the decoder or let it read audio that has not arrived.

use teddiebox_taf::PAGE_SIZE;

/// A count of bytes, as the writer counts what has been written to the card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Bytes(pub u32);

/// A count of whole pages, as the decoder's gate counts what it may read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pages(pub u32);

impl Bytes {
    /// Whole pages these bytes cover.
    ///
    /// Rounds down: a page that has only partly arrived cannot be decoded
    /// yet.
    pub const fn whole_pages(self) -> Pages {
        Pages(self.0 / PAGE_SIZE as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_part_written_page_does_not_count() {
        assert_eq!(Bytes(PAGE_SIZE as u32 - 1).whole_pages(), Pages(0));
    }

    #[test]
    fn a_byte_past_a_whole_page_does_not_count_the_next_one() {
        assert_eq!(Bytes(PAGE_SIZE as u32 + 1).whole_pages(), Pages(1));
    }

    #[test]
    fn an_exact_multiple_converts_without_truncation() {
        assert_eq!(Bytes(3 * PAGE_SIZE as u32).whole_pages(), Pages(3));
    }

    #[test]
    fn pages_order_the_same_way_the_bytes_behind_them_do() {
        assert!(Pages(1) < Pages(2));
        assert!(Bytes(1) < Bytes(PAGE_SIZE as u32));
    }
}
