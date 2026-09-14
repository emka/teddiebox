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
    pub fn erase_before(&mut self, offset: u32, len: u32) -> Option<core::ops::Range<u32>> {
        let end = pad_to_sector(offset + len).min(self.slot_bytes);
        let start = self.erased_to.max(offset / SECTOR * SECTOR);
        if end <= start {
            return None;
        }
        self.erased_to = end;
        Some(start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::vec::Vec;

    // Unused in this task's tests: nothing here drives a write through
    // `FlashRegionLike` yet, that lands with the real sink in a later task.
    // Kept so the fake the doc comment above promises actually exists.
    #[allow(dead_code)]
    #[derive(Default)]
    struct Fake {
        erased: Vec<core::ops::Range<u32>>,
        written: Vec<(u32, usize)>,
    }

    impl FlashRegionLike for Fake {
        type Error = ();
        fn erase(&mut self, range: core::ops::Range<u32>) -> Result<(), ()> {
            self.erased.push(range);
            Ok(())
        }
        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), ()> {
            self.written.push((offset, bytes.len()));
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
}
