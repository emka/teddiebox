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
    /// Fails if `data` cannot hold a complete header page.
    ///
    /// A short *final* page is fine, and is what a real Toniebox file looks
    /// like: the stream's last Ogg page runs only as long as it needs to.
    /// Requiring a whole number of pages rejected every commercial `.taf`.
    /// Page 0 is different — it is parsed as a full page, so a file that
    /// cannot even hold it is truncated by any reading.
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
        // The final page may be short. Zero-filling the rest keeps the
        // fixed-size page the only thing anything above this trait sees, and
        // is safe because an Ogg page declares its own extent through its
        // lacing table — no reader reaches past the bytes that exist.
        let end = (start + PAGE_SIZE).min(self.data.len());
        let present = end - start;
        buf[..present].copy_from_slice(&self.data[start..end]);
        buf[present..].fill(0);
        Ok(())
    }

    fn page_count(&self) -> u32 {
        // Rounded up: a short final page is still a page a caller may read.
        self.data.len().div_ceil(PAGE_SIZE) as u32
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
        assert!(matches!(
            SlicePages::new(&[0u8; 100]),
            Err(TafError::TruncatedFile)
        ));
    }

    #[test]
    fn accepts_a_file_whose_final_page_is_short() {
        // What a real Toniebox file looks like: the stream's last Ogg page
        // is only as long as it needs to be, not padded out to the page
        // boundary. `toniefile` pads, so every fixture is a whole number of
        // pages and this shape went unnoticed until a commercial file was
        // read.
        let data = [0u8; PAGE_SIZE + 100];
        let src = SlicePages::new(&data).unwrap();
        assert_eq!(src.page_count(), 2, "the short final page still counts");
    }

    #[test]
    fn reads_a_short_final_page_zero_filled() {
        // Callers get a whole `[u8; PAGE_SIZE]` whatever the file's length,
        // so nothing above this trait has to know the last page is short.
        // Zero-filling is safe rather than merely convenient: an Ogg page
        // declares its own extent through its lacing table, so no reader
        // reaches past the bytes that really exist.
        let mut data = [0u8; PAGE_SIZE + 3];
        data[PAGE_SIZE..].copy_from_slice(&[0xAB, 0xCD, 0xEF]);
        let mut src = SlicePages::new(&data).unwrap();

        let mut buf = [0xFFu8; PAGE_SIZE];
        src.read_page(1, &mut buf).unwrap();

        assert_eq!(&buf[..3], &[0xAB, 0xCD, 0xEF]);
        assert!(buf[3..].iter().all(|&b| b == 0), "tail must be zero-filled");
    }

    #[test]
    fn still_rejects_a_read_beyond_the_last_page() {
        let data = [0u8; PAGE_SIZE + 100];
        let mut src = SlicePages::new(&data).unwrap();
        let mut buf = [0u8; PAGE_SIZE];
        assert_eq!(src.read_page(2, &mut buf), Err(TafError::PageOutOfRange));
    }
}
