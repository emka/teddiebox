//! Page-indexed I/O.
//!
//! Every TAF structure is 4096-aligned, so the only I/O primitive the parser
//! needs is "give me page N". On device this is a block read from the SD card;
//! in tests it is a slice index. Nothing above this trait knows the difference.

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
    /// Fails if `data` is not a whole number of pages.
    pub fn new(data: &'a [u8]) -> Result<Self, TafError> {
        if data.is_empty() || !data.len().is_multiple_of(PAGE_SIZE) {
            return Err(TafError::MalformedHeader);
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
        let end = start + PAGE_SIZE;
        if end > self.data.len() {
            return Err(TafError::PageOutOfRange);
        }
        buf.copy_from_slice(&self.data[start..end]);
        Ok(())
    }

    fn page_count(&self) -> u32 {
        (self.data.len() / PAGE_SIZE) as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_requested_page() {
        let mut data = [0u8; PAGE_SIZE * 3];
        data[PAGE_SIZE] = 0xAB;
        let mut src = SlicePages::new(&data).unwrap();

        let mut buf = [0u8; PAGE_SIZE];
        src.read_page(1, &mut buf).unwrap();
        assert_eq!(buf[0], 0xAB);
        assert_eq!(src.page_count(), 3);
    }

    #[test]
    fn rejects_reads_past_the_end() {
        let data = [0u8; PAGE_SIZE];
        let mut src = SlicePages::new(&data).unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        assert_eq!(src.read_page(1, &mut buf), Err(TafError::PageOutOfRange));
    }

    #[test]
    fn rejects_a_partial_page_file() {
        assert!(SlicePages::new(&[0u8; 100]).is_err());
    }
}
