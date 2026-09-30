//! A request, left in memory that survives a software reset, to boot into
//! setup mode as if both ears were held.
//!
//! The memory is not cleared by a reset, but after power-on it holds
//! whatever it powers up with. So only one exact word counts as a request,
//! and anything else, including all zeros and all ones, boots normally.

/// The word the `setup` command leaves before restarting.
pub const REQUESTED: u32 = 0x5E70_B007;

/// Whether `word`, read at boot, asks for setup mode.
pub fn is_requested(word: u32) -> bool {
    word == REQUESTED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_left_before_the_restart_enters_setup() {
        // Given
        let word = REQUESTED;

        // When
        let enter = is_requested(word);

        // Then
        assert!(enter);
    }

    #[test]
    fn a_cleared_word_boots_normally() {
        // Given
        let word = 0;

        // When
        let enter = is_requested(word);

        // Then
        assert!(!enter);
    }

    #[test]
    fn a_word_one_bit_off_the_request_boots_normally() {
        // Given: what memory might hold after power-on, close to the request.
        let word = REQUESTED ^ 1;

        // When
        let enter = is_requested(word);

        // Then
        assert!(!enter);
    }

    #[test]
    fn the_request_is_neither_all_zeros_nor_all_ones() {
        // Given: the two states uninitialised memory most often holds.
        let blank = [0x0000_0000, 0xFFFF_FFFF];

        // When
        let entered = blank.map(is_requested);

        // Then
        assert_eq!(entered, [false, false]);
    }
}
