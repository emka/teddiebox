//! Turns a download's arbitrary chunks into writes flash will accept.

use crate::sink::{FlashRegionLike, Sectors, SinkError};

/// Holds the staging buffer at a word-aligned address.
///
/// `esp-storage` copies a slice that is not word-aligned in memory through a
/// 4096-byte buffer on the stack.
#[repr(align(4))]
struct Aligned<const N: usize>([u8; N]);

/// Buffers an image as it arrives and writes it to a slot in aligned pieces.
///
/// Download chunks are whatever size the TLS record gave, but
/// `esp-storage` refuses a write unless its offset and length are both
/// multiples of four. So every write but the last is exactly `N` bytes, and
/// [`ImageWriter::finish`] pads the last to the next multiple of four with
/// `0xFF`, the value erased flash already holds.
///
/// `N` must be a non-zero multiple of four.
pub struct ImageWriter<const N: usize> {
    sectors: Sectors,
    stage: Aligned<N>,
    /// How many bytes of `stage` hold image bytes not yet written.
    staged: usize,
    /// Where in the slot the staged bytes belong.
    written: u32,
}

impl<const N: usize> ImageWriter<N> {
    pub fn new(slot_bytes: u32) -> Self {
        const { assert!(N > 0 && N.is_multiple_of(4)) };
        Self {
            sectors: Sectors::new(slot_bytes),
            stage: Aligned([0xFF; N]),
            staged: 0,
            written: 0,
        }
    }

    /// Takes the next bytes of the image, writing each full staging buffer.
    pub fn push<F: FlashRegionLike>(
        &mut self,
        flash: &mut F,
        mut bytes: &[u8],
    ) -> Result<(), SinkError<F::Error>> {
        while !bytes.is_empty() {
            let take = (N - self.staged).min(bytes.len());
            self.stage.0[self.staged..self.staged + take].copy_from_slice(&bytes[..take]);
            self.staged += take;
            bytes = &bytes[take..];
            if self.staged == N {
                self.write_stage(flash, N)?;
            }
        }
        Ok(())
    }

    /// Writes what is still staged and returns the image's length in bytes.
    ///
    /// The length excludes the padding.
    pub fn finish<F: FlashRegionLike>(
        &mut self,
        flash: &mut F,
    ) -> Result<u32, SinkError<F::Error>> {
        let length = self.written + self.staged as u32;
        if self.staged > 0 {
            let padded = self.staged.next_multiple_of(4);
            self.stage.0[self.staged..padded].fill(0xFF);
            self.write_stage(flash, padded)?;
        }
        Ok(length)
    }

    fn write_stage<F: FlashRegionLike>(
        &mut self,
        flash: &mut F,
        len: usize,
    ) -> Result<(), SinkError<F::Error>> {
        self.sectors
            .feed(flash, self.written, &self.stage.0[..len])?;
        self.written += len as u32;
        self.staged = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::vec;
    use std::vec::Vec;

    /// A slot held in memory, erased to `0xFF` like real flash.
    struct Memory {
        bytes: Vec<u8>,
        /// `(offset, len, address % 4)` of every write, in order.
        writes: Vec<(u32, usize, usize)>,
    }

    impl Memory {
        fn new(len: usize) -> Self {
            Self {
                bytes: vec![0u8; len],
                writes: Vec::new(),
            }
        }
    }

    impl FlashRegionLike for Memory {
        type Error = ();
        fn erase(&mut self, range: core::ops::Range<u32>) -> Result<(), ()> {
            self.bytes[range.start as usize..range.end as usize].fill(0xFF);
            Ok(())
        }
        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), ()> {
            self.writes
                .push((offset, bytes.len(), bytes.as_ptr() as usize % 4));
            self.bytes[offset as usize..offset as usize + bytes.len()].copy_from_slice(bytes);
            Ok(())
        }
    }

    fn image(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    #[test]
    fn chunks_of_any_size_land_in_the_slot_byte_for_byte() {
        let data = image(3000);
        let mut flash = Memory::new(8192);
        let mut writer = ImageWriter::<512>::new(8192);
        for chunk in data.chunks(1373) {
            writer.push(&mut flash, chunk).unwrap();
        }
        writer.finish(&mut flash).unwrap();
        assert_eq!(&flash.bytes[..3000], &data[..]);
    }

    #[test]
    fn every_write_starts_ends_and_sits_on_a_word_boundary() {
        let mut flash = Memory::new(8192);
        let mut writer = ImageWriter::<512>::new(8192);
        for chunk in image(3001).chunks(1373) {
            writer.push(&mut flash, chunk).unwrap();
        }
        writer.finish(&mut flash).unwrap();
        for &(offset, len, address) in &flash.writes {
            assert_eq!((offset % 4, len % 4, address), (0, 0, 0));
        }
    }

    #[test]
    fn the_last_write_is_padded_with_erased_flash() {
        let mut flash = Memory::new(8192);
        let mut writer = ImageWriter::<512>::new(8192);
        writer.push(&mut flash, &[0u8; 3001]).unwrap();
        writer.finish(&mut flash).unwrap();
        assert_eq!(flash.writes.last(), Some(&(2560, 444, 0)));
        assert_eq!(&flash.bytes[3001..3004], &[0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn finish_reports_the_image_length_without_the_padding() {
        let mut flash = Memory::new(8192);
        let mut writer = ImageWriter::<512>::new(8192);
        writer.push(&mut flash, &[0u8; 3001]).unwrap();
        assert_eq!(writer.finish(&mut flash), Ok(3001));
    }

    #[test]
    fn an_image_that_fills_the_slot_exactly_is_written() {
        let mut flash = Memory::new(8192);
        let mut writer = ImageWriter::<512>::new(8192);
        writer.push(&mut flash, &[1u8; 8192]).unwrap();
        assert_eq!(writer.finish(&mut flash), Ok(8192));
    }

    #[test]
    fn an_image_one_byte_larger_than_the_slot_is_refused() {
        let mut flash = Memory::new(8192);
        let mut writer = ImageWriter::<512>::new(8192);
        writer.push(&mut flash, &[1u8; 8192]).unwrap();
        writer.push(&mut flash, &[1u8]).unwrap();
        assert_eq!(
            writer.finish(&mut flash),
            Err(SinkError::PastSlotEnd {
                offset: 8192,
                len: 4,
                slot: 8192,
            })
        );
    }

    proptest::proptest! {
        #[test]
        fn any_split_of_an_image_lands_intact(
            len in 0usize..6000,
            cuts in proptest::collection::vec(1usize..2000, 1..20),
        ) {
            let data = image(len);
            let mut flash = Memory::new(8192);
            let mut writer = ImageWriter::<512>::new(8192);
            let mut at = 0;
            for cut in cuts.iter().cycle() {
                if at >= len {
                    break;
                }
                let end = (at + cut).min(len);
                writer.push(&mut flash, &data[at..end]).unwrap();
                at = end;
            }
            proptest::prop_assert_eq!(writer.finish(&mut flash), Ok(len as u32));
            proptest::prop_assert!(flash.bytes[..len] == data[..]);
        }
    }
}
