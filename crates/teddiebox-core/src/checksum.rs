//! CRC-32, for comparing bytes read on the box against bytes read on a laptop.
//!
//! Bench step 7 asks whether a file on the SD card checksums equal to the
//! host. The claim is "these bytes are those bytes", not a security property,
//! so this is the ordinary IEEE CRC-32 — the one `zlib`, `gzip` and PNG use —
//! which any host computes in one line. Thirty-two bits is ample for the
//! failure modes actually in play: a swapped byte order, an off-by-one block,
//! a dropped sector.
//!
//! Fed in chunks, because a file arrives from the card one block at a time and
//! the whole of it never exists in memory at once.

/// The IEEE polynomial, reflected — the form that suits a right-shifting
/// implementation, and the one behind every published CRC-32 check value.
const POLYNOMIAL: u32 = 0xEDB8_8320;

/// A CRC-32 in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc32 {
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    pub const fn new() -> Self {
        Self { state: !0 }
    }

    /// Adds the next run of bytes.
    ///
    /// Computed a bit at a time rather than through a lookup table. The table
    /// would be a kilobyte of flash to save time this does not need: the card
    /// is read over SPI, which is slower than this by orders of magnitude, so
    /// the checksum is never what the walk waits for.
    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.state ^= u32::from(byte);
            for _ in 0..8 {
                let lsb_set = self.state & 1 != 0;
                self.state >>= 1;
                if lsb_set {
                    self.state ^= POLYNOMIAL;
                }
            }
        }
    }

    /// The checksum of everything fed so far.
    pub const fn finish(&self) -> u32 {
        !self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = Crc32::new();
        crc.update(bytes);
        crc.finish()
    }

    /// The check value every CRC-32 implementation publishes. If this is
    /// right, the polynomial, the reflection and both the initial and final
    /// inversions are right too — it is the one vector that pins all four.
    #[test]
    fn the_published_check_value_is_reproduced() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// Literal, from the same published table rather than from a second
    /// implementation here: a test that computes its own expectation agrees
    /// with the code by construction and can never disagree with it.
    #[test]
    fn a_longer_known_string_matches_its_published_value() {
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
    }

    /// Not a tautology: the initial and final inversions cancel over no input,
    /// so an implementation that forgot both still passes this while failing
    /// every other test. It is here because the walk will meet a zero-length
    /// file on a real card and must not panic or report something arbitrary.
    #[test]
    fn the_empty_input_checksums_to_zero() {
        assert_eq!(crc32(b""), 0);
    }

    /// A file arrives from the card in blocks, and the answer must not depend
    /// on where the block boundaries happen to fall.
    #[test]
    fn feeding_in_pieces_matches_feeding_the_whole() {
        let mut split = Crc32::new();
        split.update(b"12345");
        split.update(b"6789");
        assert_eq!(split.finish(), 0xCBF4_3926);
    }

    /// The degenerate split: empty chunks are what a short final block looks
    /// like, and they must not disturb the running value.
    #[test]
    fn empty_chunks_do_not_disturb_the_running_value() {
        let mut crc = Crc32::new();
        crc.update(b"");
        crc.update(b"123456789");
        crc.update(b"");
        assert_eq!(crc.finish(), 0xCBF4_3926);
    }

    /// Order matters, which is what makes this a checksum rather than a sum.
    #[test]
    fn transposed_input_checksums_differently() {
        assert_ne!(crc32(b"123456789"), crc32(b"213456789"));
    }
}
