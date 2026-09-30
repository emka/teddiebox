//! Page-indexed I/O.
//!
//! Every TAF structure is aligned to 4096 bytes, so the parser only needs
//! "read page N". On the device that reads from the SD card; in tests it
//! indexes a slice.

use crate::{TafError, PAGE_SIZE};

pub trait PageSource {
    type Error;

    /// Reads page `index` in full. Page 0 is the header.
    fn read_page(&mut self, index: u32, buf: &mut [u8; PAGE_SIZE]) -> Result<(), Self::Error>;

    /// Total pages in the file, header included.
    fn page_count(&self) -> u32;
}

/// Host-side `PageSource` over an in-memory file. Test support only.
pub struct SlicePages<'a> {
    data: &'a [u8],
}

impl<'a> SlicePages<'a> {
    /// Fails if `data` cannot hold a complete header page.
    ///
    /// A short *last* page is fine; real Toniebox files end that way. Page 0
    /// is parsed as a full page, so a file shorter than that is truncated.
    pub fn new(data: &'a [u8]) -> Result<Self, TafError> {
        if data.len() < PAGE_SIZE {
            return Err(TafError::TruncatedFile);
        }
        Ok(Self { data })
    }
}

impl PageSource for SlicePages<'_> {
    type Error = TafError;

    fn read_page(&mut self, index: u32, buf: &mut [u8; PAGE_SIZE]) -> Result<(), TafError> {
        let start = (index as usize)
            .checked_mul(PAGE_SIZE)
            .ok_or(TafError::PageOutOfRange)?;
        if start >= self.data.len() {
            return Err(TafError::PageOutOfRange);
        }
        // The last page may be short. Fill the rest with zeros, so callers
        // always get a full page. This is safe because an Ogg page states its
        // own length in its lacing table.
        let end = (start + PAGE_SIZE).min(self.data.len());
        let present = end - start;
        buf[..present].copy_from_slice(&self.data[start..end]);
        buf[present..].fill(0);
        Ok(())
    }

    fn page_count(&self) -> u32 {
        // Rounded up: a short last page can still be read.
        self.data.len().div_ceil(PAGE_SIZE) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_requested_page() {
        // Given: three pages, the second marked
        let mut data = [0u8; PAGE_SIZE * 3];
        data[PAGE_SIZE] = 0xAB;
        let mut src = SlicePages::new(&data).unwrap();
        let mut buf = [0u8; PAGE_SIZE];

        // When
        src.read_page(1, &mut buf).unwrap();

        // Then
        assert_eq!(buf[0], 0xAB);
        assert_eq!(src.page_count(), 3);
    }

    #[test]
    fn rejects_reads_past_the_end() {
        // Given
        let data = [0u8; PAGE_SIZE];
        let mut src = SlicePages::new(&data).unwrap();
        let mut buf = [0u8; PAGE_SIZE];

        // When
        let read = src.read_page(1, &mut buf);

        // Then
        assert_eq!(read, Err(TafError::PageOutOfRange));
    }

    #[test]
    fn rejects_a_partial_page_file() {
        // Given
        let data = [0u8; 100];

        // When
        let src = SlicePages::new(&data);

        // Then
        assert!(matches!(src, Err(TafError::TruncatedFile)));
    }

    #[test]
    fn accepts_a_file_whose_final_page_is_short() {
        // Given: real Toniebox files end with a short page; `toniefile` pads
        // to a full page, so the test fixtures do not show this
        let data = [0u8; PAGE_SIZE + 100];

        // When
        let src = SlicePages::new(&data).unwrap();

        // Then
        assert_eq!(src.page_count(), 2, "the short final page still counts");
    }

    #[test]
    fn reads_a_short_final_page_zero_filled() {
        // Given: callers always get a full page; zero-filling is safe because
        // an Ogg page states its own length
        let mut data = [0u8; PAGE_SIZE + 3];
        data[PAGE_SIZE..].copy_from_slice(&[0xAB, 0xCD, 0xEF]);
        let mut src = SlicePages::new(&data).unwrap();
        let mut buf = [0xFFu8; PAGE_SIZE];

        // When
        src.read_page(1, &mut buf).unwrap();

        // Then
        assert_eq!(&buf[..3], &[0xAB, 0xCD, 0xEF]);
        assert!(buf[3..].iter().all(|&b| b == 0), "tail must be zero-filled");
    }

    #[test]
    fn still_rejects_a_read_beyond_the_last_page() {
        // Given
        let data = [0u8; PAGE_SIZE + 100];
        let mut src = SlicePages::new(&data).unwrap();
        let mut buf = [0u8; PAGE_SIZE];

        // When
        let read = src.read_page(2, &mut buf);

        // Then
        assert_eq!(read, Err(TafError::PageOutOfRange));
    }
}
