//! The arithmetic a flash sink needs, with no flash in sight.
//!
//! Flash erases a whole 4 KB sector at a time and writes into an erased one.
//! Get the bookkeeping wrong and a sector is erased *after* it was written,
//! which throws away bytes that were already correct and produces an image
//! that fails its digest for a reason nothing in the download path explains.
//! So the bookkeeping lives here, where a fake can watch it.

/// The ESP32-S3's flash sector. Erases are whole sectors, always.
pub const SECTOR: u32 = 4096;

/// Somewhere sectors can be erased and bytes written.
///
/// A trait because `esp-storage`'s region cannot exist on a host, and the
/// arithmetic is the part worth being sure about.
pub trait FlashRegionLike {
    type Error;
    fn erase(&mut self, range: core::ops::Range<u32>) -> Result<(), Self::Error>;
    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error>;
}

/// Rounds a length up to a whole number of sectors.
///
/// The last chunk of an image is almost never sector-sized, and a flash write
/// of a partial sector is not portable. Padding costs at most 4095 bytes of
/// flash that the image does not use and the bootloader does not read.
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
    /// Refused rather than clamped: a silently truncated write produces an
    /// image that fails its digest, with nothing in the download path to say
    /// why. Whoever computed this offset is wrong, and the error says so.
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
    /// `None` means the sectors this write lands in are already erased —
    /// which is the ordinary case for every write after the first in a
    /// sector, and erasing anyway would discard bytes already written.
    fn erase_before(&mut self, offset: u32, len: u32) -> Option<core::ops::Range<u32>> {
        // `feed` refuses (before ever calling here) any write whose end
        // overflows u32 or exceeds `slot_bytes`, so in normal use `offset +
        // len` never overflows by the time this runs. This still can't wrap
        // on its own terms: a `checked_add` that would overflow is treated
        // as "at least the end of the slot", which lands on exactly the
        // answer the non-overflowing branch already gives once the sum is
        // clamped to `slot_bytes` — never a wrapped, wrong-sector answer.
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
    /// The erase has to happen before the write and exactly once per sector:
    /// erasing after a write throws away bytes that were already correct, and
    /// that failure surfaces as a digest mismatch with nothing in the download
    /// path to explain it.
    pub fn feed<F: FlashRegionLike>(
        &mut self,
        flash: &mut F,
        offset: u32,
        bytes: &[u8],
    ) -> Result<(), SinkError<F::Error>> {
        let len = bytes.len() as u32;
        match offset.checked_add(len) {
            Some(end) if end <= self.slot_bytes => {}
            // Either the end overflowed u32, or it lands past the slot.
            // Both are certainly past the slot, and refusing here — before
            // any erase or write — leaves the flash untouched.
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
        // Records "erase"/"write" in call order, interleaved, so a test can
        // tell that a sector's erase happened before the write that touches
        // it rather than only that both happened.
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
        let mut s = Sectors::new(0x1D0000);
        assert_eq!(s.erase_before(0, 100), Some(0..SECTOR));
        // A second write inside the same sector must not erase it again —
        // that would throw away the bytes just written.
        assert_eq!(s.erase_before(100, 100), None);
    }

    #[test]
    fn crossing_a_sector_boundary_erases_only_the_new_sector() {
        let mut s = Sectors::new(0x1D0000);
        s.erase_before(0, SECTOR).unwrap();
        assert_eq!(s.erase_before(SECTOR, 10), Some(SECTOR..SECTOR * 2));
    }

    #[test]
    fn a_write_spanning_three_sectors_erases_all_three() {
        let mut s = Sectors::new(0x1D0000);
        assert_eq!(s.erase_before(0, SECTOR * 2 + 1), Some(0..SECTOR * 3));
    }

    #[test]
    fn a_resumed_write_erases_from_where_it_resumes() {
        let mut s = Sectors::new(0x1D0000);
        assert_eq!(s.erase_before(SECTOR * 4, 10), Some(SECTOR * 4..SECTOR * 5));
    }

    #[test]
    fn a_length_already_on_a_sector_boundary_is_not_padded() {
        assert_eq!(pad_to_sector(SECTOR * 3), SECTOR * 3);
    }

    #[test]
    fn a_short_final_chunk_is_padded_up_to_a_whole_sector() {
        assert_eq!(pad_to_sector(SECTOR * 3 + 1), SECTOR * 4);
        assert_eq!(pad_to_sector(1), SECTOR);
    }

    #[test]
    fn a_zero_length_pads_to_nothing() {
        assert_eq!(pad_to_sector(0), 0);
    }

    #[test]
    fn erases_are_always_sector_aligned() {
        let mut s = Sectors::new(0x1D0000);
        let mut f = Fake::default();
        s.feed(&mut f, 0, &[0u8; 100]).unwrap();
        s.feed(&mut f, 100, &[0u8; 5000]).unwrap();
        s.feed(&mut f, 5100, &[0u8; 50]).unwrap();
        assert!(!f.erased.is_empty());
        for range in &f.erased {
            assert_eq!(range.start % SECTOR, 0);
            assert_eq!(range.end % SECTOR, 0);
        }
    }

    #[test]
    fn a_sector_is_erased_once_and_before_the_write_that_touches_it() {
        let mut s = Sectors::new(0x1D0000);
        let mut f = Fake::default();
        s.feed(&mut f, 0, &[0u8; 100]).unwrap();
        s.feed(&mut f, 100, &[0u8; 100]).unwrap();
        assert_eq!(f.erased.len(), 1);
        assert_eq!(f.events, ["erase", "write", "write"]);
    }

    #[test]
    fn a_resumed_write_lands_where_the_watermark_says_not_at_zero() {
        let mut s = Sectors::new(0x1D0000);
        let mut f = Fake::default();
        s.feed(&mut f, SECTOR * 4, &[0u8; 10]).unwrap();
        assert_eq!(f.written, [(SECTOR * 4, 10)]);
        assert_eq!(f.erased.len(), 1);
        assert_eq!(f.erased[0], SECTOR * 4..SECTOR * 5);
    }

    #[test]
    fn a_short_final_chunk_is_written_not_dropped() {
        let mut s = Sectors::new(0x1D0000);
        let mut f = Fake::default();
        s.feed(&mut f, 0, &[0u8; 100]).unwrap();
        assert_eq!(f.written, [(0, 100)]);
    }

    #[test]
    fn a_write_ending_exactly_at_the_slot_end_is_accepted() {
        let mut s = Sectors::new(8192);
        let mut f = Fake::default();
        s.feed(&mut f, 8172, &[7u8; 20]).unwrap();
        assert_eq!(f.written, [(8172, 20)]);
        assert_eq!(f.erased.len(), 1);
        assert_eq!(f.erased[0], 4096..8192);
    }

    #[test]
    fn a_write_ending_one_byte_past_the_slot_end_is_refused() {
        let mut s = Sectors::new(8192);
        let mut f = Fake::default();
        let err = s.feed(&mut f, 8172, &[7u8; 21]).unwrap_err();
        assert_eq!(
            err,
            SinkError::PastSlotEnd {
                offset: 8172,
                len: 21,
                slot: 8192,
            }
        );
    }

    #[test]
    fn a_refused_write_touches_nothing() {
        let mut s = Sectors::new(8192);
        let mut f = Fake::default();
        s.feed(&mut f, 8172, &[7u8; 21]).unwrap_err();
        assert!(f.erased.is_empty());
        assert!(f.written.is_empty());
        assert!(f.events.is_empty());
    }

    #[test]
    fn an_overflowing_end_is_refused_not_wrapped() {
        let mut s = Sectors::new(0x1D0000);
        let mut f = Fake::default();
        let err = s.feed(&mut f, u32::MAX - 4, &[9u8; 10]).unwrap_err();
        assert_eq!(
            err,
            SinkError::PastSlotEnd {
                offset: u32::MAX - 4,
                len: 10,
                slot: 0x1D0000,
            }
        );
    }
}
