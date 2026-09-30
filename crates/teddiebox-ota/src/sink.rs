//! The bookkeeping for writing an image to flash, testable without flash.
//!
//! Flash is erased a whole 4 KB sector at a time, and can only be written
//! after erasing. If a sector were erased *after* being written, correct bytes
//! would be lost and the image would fail its digest check.

/// The ESP32-S3's flash sector. Erases are whole sectors, always.
pub const SECTOR: u32 = 4096;

/// Somewhere sectors can be erased and bytes written.
///
/// A trait so the logic can be tested on the host with a fake instead of
/// `esp-storage`.
pub trait FlashRegionLike {
    type Error;
    fn erase(&mut self, range: core::ops::Range<u32>) -> Result<(), Self::Error>;
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error>;
}

/// Rounds a length up to a whole number of sectors.
///
/// The last chunk of an image is rarely a whole sector. Padding wastes at most
/// 4095 bytes that nothing reads.
pub fn pad_to_sector(len: u32) -> u32 {
    if len == 0 {
        return 0;
    }
    len.div_ceil(SECTOR) * SECTOR
}

/// What can go wrong feeding bytes into a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkError<E> {
    /// The flash region itself refused.
    Flash(E),
    /// The write runs past the end of the slot it is being written into.
    ///
    /// Refused rather than cut short: a cut-short write would only show up
    /// later as a digest mismatch.
    PastSlotEnd { offset: u32, len: u32, slot: u32 },
}

/// Tracks which sectors have already been erased, so none is erased twice.
pub struct Sectors {
    slot_bytes: u32,
    /// One past the last sector erased so far, in bytes. Zero means none.
    erased_to: u32,
}

impl Sectors {
    pub fn new(slot_bytes: u32) -> Self {
        Self {
            slot_bytes,
            erased_to: 0,
        }
    }

    /// The range to erase before writing `len` bytes at `offset`, if any.
    ///
    /// `None` means the sectors are already erased, which is normal for every
    /// write after the first in a sector.
    fn erase_before(&mut self, offset: u32, len: u32) -> Option<core::ops::Range<u32>> {
        // `feed` already refuses writes that overflow or pass the slot end.
        // Even so, an overflow here is treated as the slot end, never
        // wrapped.
        let unpadded_end = offset
            .checked_add(len)
            .unwrap_or(self.slot_bytes)
            .min(self.slot_bytes);
        let end = pad_to_sector(unpadded_end).min(self.slot_bytes);
        let start = self.erased_to.max(offset / SECTOR * SECTOR);
        if end <= start {
            return None;
        }
        self.erased_to = end;
        Some(start..end)
    }

    /// Writes `bytes` at `offset`, erasing any sector this write is the first
    /// to touch.
    ///
    /// Each sector is erased exactly once, before it is written.
    pub fn feed<F: FlashRegionLike>(
        &mut self,
        flash: &mut F,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), SinkError<F::Error>> {
        let len = bytes.len() as u32;
        match offset.checked_add(len) {
            Some(end) if end <= self.slot_bytes => {}
            // The end overflowed or is past the slot. Refuse before
            // touching the flash.
            _ => {
                return Err(SinkError::PastSlotEnd {
                    offset,
                    len,
                    slot: self.slot_bytes,
                })
            }
        }
        if let Some(range) = self.erase_before(offset, len) {
            flash.erase(range).map_err(SinkError::Flash)?;
        }
        flash.write(offset, bytes).map_err(SinkError::Flash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::vec::Vec;

    #[derive(Default)]
    struct Fake {
        erased: Vec<core::ops::Range<u32>>,
        written: Vec<(u32, usize)>,
        // "erase" and "write" in call order, to check that a sector is
        // erased before it is written.
        events: Vec<&'static str>,
    }

    impl FlashRegionLike for Fake {
        type Error = ();
        fn erase(&mut self, range: core::ops::Range<u32>) -> Result<(), ()> {
            self.erased.push(range);
            self.events.push("erase");
            Ok(())
        }
        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), ()> {
            self.written.push((offset, bytes.len()));
            self.events.push("write");
            Ok(())
        }
    }

    #[test]
    fn a_write_at_the_start_erases_the_first_sector_once() {
        // Given
        let mut s = Sectors::new(0x1D0000);

        // When: a write at the start, then a second in the same sector
        let erased = [s.erase_before(0, 100), s.erase_before(100, 100)];

        // Then
        assert_eq!(erased, [Some(0..SECTOR), None]);
    }

    #[test]
    fn crossing_a_sector_boundary_erases_only_the_new_sector() {
        // Given: the first sector already erased
        let mut s = Sectors::new(0x1D0000);
        s.erase_before(0, SECTOR).unwrap();

        // When
        let erased = s.erase_before(SECTOR, 10);

        // Then
        assert_eq!(erased, Some(SECTOR..SECTOR * 2));
    }

    #[test]
    fn a_write_spanning_three_sectors_erases_all_three() {
        // Given
        let mut s = Sectors::new(0x1D0000);

        // When
        let erased = s.erase_before(0, SECTOR * 2 + 1);

        // Then
        assert_eq!(erased, Some(0..SECTOR * 3));
    }

    #[test]
    fn a_resumed_write_erases_from_where_it_resumes() {
        // Given
        let mut s = Sectors::new(0x1D0000);

        // When
        let erased = s.erase_before(SECTOR * 4, 10);

        // Then
        assert_eq!(erased, Some(SECTOR * 4..SECTOR * 5));
    }

    #[test]
    fn a_length_already_on_a_sector_boundary_is_not_padded() {
        // Given
        let length = SECTOR * 3;

        // When
        let padded = pad_to_sector(length);

        // Then
        assert_eq!(padded, SECTOR * 3);
    }

    #[test]
    fn a_short_final_chunk_is_padded_up_to_a_whole_sector() {
        // Given
        let lengths = [SECTOR * 3 + 1, 1];

        // When
        let padded = lengths.map(pad_to_sector);

        // Then
        assert_eq!(padded, [SECTOR * 4, SECTOR]);
    }

    #[test]
    fn a_zero_length_pads_to_nothing() {
        // Given
        let length = 0;

        // When
        let padded = pad_to_sector(length);

        // Then
        assert_eq!(padded, 0);
    }

    /// Checks alignment only. A bug that always erased sector 0 is caught by
    /// `a_resumed_write_lands_where_the_watermark_says_not_at_zero`.
    #[test]
    fn erased_ranges_start_and_end_on_a_sector_boundary() {
        // Given
        let mut s = Sectors::new(0x1D0000);
        let mut f = Fake::default();

        // When: three writes of awkward sizes
        s.feed(&mut f, 0, &[0u8; 100]).unwrap();
        s.feed(&mut f, 100, &[0u8; 5000]).unwrap();
        s.feed(&mut f, 5100, &[0u8; 50]).unwrap();

        // Then
        assert!(!f.erased.is_empty());
        for range in &f.erased {
            assert_eq!(range.start % SECTOR, 0);
            assert_eq!(range.end % SECTOR, 0);
        }
    }

    #[test]
    fn a_sector_is_erased_once_and_before_the_write_that_touches_it() {
        // Given
        let mut s = Sectors::new(0x1D0000);
        let mut f = Fake::default();

        // When: two writes in the same sector
        s.feed(&mut f, 0, &[0u8; 100]).unwrap();
        s.feed(&mut f, 100, &[0u8; 100]).unwrap();

        // Then
        assert_eq!(f.erased.len(), 1);
        assert_eq!(f.events, ["erase", "write", "write"]);
    }

    #[test]
    fn a_resumed_write_lands_where_the_watermark_says_not_at_zero() {
        // Given
        let mut s = Sectors::new(0x1D0000);
        let mut f = Fake::default();

        // When
        s.feed(&mut f, SECTOR * 4, &[0u8; 10]).unwrap();

        // Then
        assert_eq!(f.written, [(SECTOR * 4, 10)]);
        assert_eq!(f.erased.len(), 1);
        assert_eq!(f.erased[0], SECTOR * 4..SECTOR * 5);
    }

    #[test]
    fn a_short_final_chunk_is_written_not_dropped() {
        // Given
        let mut s = Sectors::new(0x1D0000);
        let mut f = Fake::default();

        // When
        s.feed(&mut f, 0, &[0u8; 100]).unwrap();

        // Then
        assert_eq!(f.written, [(0, 100)]);
    }

    #[test]
    fn a_write_ending_exactly_at_the_slot_end_is_accepted() {
        // Given
        let mut s = Sectors::new(8192);
        let mut f = Fake::default();

        // When
        s.feed(&mut f, 8172, &[7u8; 20]).unwrap();

        // Then
        assert_eq!(f.written, [(8172, 20)]);
        assert_eq!(f.erased.len(), 1);
        assert_eq!(f.erased[0], 4096..8192);
    }

    #[test]
    fn a_write_ending_one_byte_past_the_slot_end_is_refused() {
        // Given
        let mut s = Sectors::new(8192);
        let mut f = Fake::default();

        // When
        let fed = s.feed(&mut f, 8172, &[7u8; 21]);

        // Then
        assert_eq!(
            fed,
            Err(SinkError::PastSlotEnd {
                offset: 8172,
                len: 21,
                slot: 8192,
            })
        );
    }

    #[test]
    fn a_refused_write_touches_nothing() {
        // Given
        let mut s = Sectors::new(8192);
        let mut f = Fake::default();

        // When
        s.feed(&mut f, 8172, &[7u8; 21]).unwrap_err();

        // Then
        assert!(f.erased.is_empty());
        assert!(f.written.is_empty());
        assert!(f.events.is_empty());
    }

    #[test]
    fn an_overflowing_end_is_refused_not_wrapped() {
        // Given
        let mut s = Sectors::new(0x1D0000);
        let mut f = Fake::default();

        // When
        let fed = s.feed(&mut f, u32::MAX - 4, &[9u8; 10]);

        // Then
        assert_eq!(
            fed,
            Err(SinkError::PastSlotEnd {
                offset: u32::MAX - 4,
                len: 10,
                slot: 0x1D0000,
            })
        );
    }
}
