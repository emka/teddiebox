//! CRC-32, for checking that a file read on the box matches the same file
//! read on a laptop.
//!
//! This is the standard IEEE CRC-32 used by `zlib`, `gzip` and PNG, so any
//! computer can compute it easily. It catches errors like swapped bytes, an
//! off-by-one block or a dropped sector; it is not a security check.
//!
//! Fed in chunks, because a file is read from the card one block at a time.

/// The IEEE polynomial, bit-reversed for a right-shifting implementation.
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
    /// Computed a bit at a time, without a lookup table. A table would cost a
    /// kilobyte of flash, and reading the card is far slower than this anyway.
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

    /// The standard CRC-32 check value. It confirms the polynomial, the bit
    /// order and both inversions.
    #[test]
    fn the_published_check_value_is_reproduced() {
        // Given
        let check_input = b"123456789";

        // When
        let crc = crc32(check_input);

        // Then
        assert_eq!(crc, 0xCBF4_3926);
    }

    /// A published value, not one computed here, so the test can disagree
    /// with the code.
    #[test]
    fn a_longer_known_string_matches_its_published_value() {
        // Given
        let text = b"The quick brown fox jumps over the lazy dog";

        // When
        let crc = crc32(text);

        // Then
        assert_eq!(crc, 0x414F_A339);
    }

    /// A real card can hold empty files; they must checksum to zero.
    #[test]
    fn the_empty_input_checksums_to_zero() {
        // Given
        let empty = b"";

        // When
        let crc = crc32(empty);

        // Then
        assert_eq!(crc, 0);
    }

    /// The result must not depend on where the block boundaries fall.
    #[test]
    fn feeding_in_pieces_matches_feeding_the_whole() {
        // Given: the check input, split where a block boundary might fall
        let mut split = Crc32::new();

        // When
        split.update(b"12345");
        split.update(b"6789");

        // Then
        assert_eq!(split.finish(), 0xCBF4_3926);
    }

    /// Empty chunks must not change the result.
    #[test]
    fn empty_chunks_do_not_disturb_the_running_value() {
        // Given
        let mut crc = Crc32::new();

        // When
        crc.update(b"");
        crc.update(b"123456789");
        crc.update(b"");

        // Then
        assert_eq!(crc.finish(), 0xCBF4_3926);
    }

    /// Byte order changes the result.
    #[test]
    fn transposed_input_checksums_differently() {
        // Given
        let (original, transposed) = (b"123456789", b"213456789");

        // When
        let crcs = (crc32(original), crc32(transposed));

        // Then
        assert_ne!(crcs.0, crcs.1);
    }
}
