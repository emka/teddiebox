//! A test tone.
//!
//! One cycle of a sine wave. Repeated at a 48000 Hz sample rate it gives
//! exactly 1000 Hz, with no click where it repeats, because the table holds
//! exactly one cycle.

/// The rate the cycle is played at.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// The tone this produces, in hertz.
pub const TONE_HZ: u32 = SAMPLE_RATE_HZ / SINE.len() as u32;

/// Peak amplitude, a little under half full scale.
///
/// Not full scale, to be safe on an untested speaker and codec setup.
pub const PEAK: i16 = 16000;

/// One cycle of a sine wave.
pub static SINE: [i16; 48] = [
    0, 2088, 4141, 6123, 8000, 9740, 11314, 12694, 13856, 14782, 15455, 15863, 16000, 15863, 15455,
    14782, 13856, 12694, 11314, 9740, 8000, 6123, 4141, 2088, 0, -2088, -4141, -6123, -8000, -9740,
    -11314, -12694, -13856, -14782, -15455, -15863, -16000, -15863, -15455, -14782, -13856, -12694,
    -11314, -9740, -8000, -6123, -4141, -2088,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_whole_cycle_starts_and_ends_at_the_zero_crossing() {
        // Given
        let cycle = &SINE;

        // When
        let (first, last) = (cycle[0], cycle[cycle.len() - 1]);

        // Then: the sample after the last is the first again, so the seam is
        // smooth only if the cycle approaches zero from below.
        assert_eq!(first, 0);
        assert!(last < 0, "the cycle must approach zero from below");
    }

    #[test]
    fn the_peak_is_a_quarter_of_the_way_through() {
        // Given
        let cycle = &SINE;

        // When
        let quarters = (cycle[cycle.len() / 4], cycle[3 * cycle.len() / 4]);

        // Then
        assert_eq!(quarters, (PEAK, -PEAK));
    }

    /// A DC offset would heat the speaker coil.
    #[test]
    fn the_cycle_has_no_direct_current() {
        // Given
        let cycle = &SINE;

        // When
        let sum: i32 = cycle.iter().map(|&s| i32::from(s)).sum();

        // Then
        assert_eq!(sum, 0);
    }

    /// The second half is the negative of the first.
    #[test]
    fn the_second_half_mirrors_the_first() {
        // Given
        let (first, second) = SINE.split_at(SINE.len() / 2);

        // When
        let second_negated: [i16; SINE.len() / 2] = core::array::from_fn(|i| -second[i]);

        // Then
        assert_eq!(first, second_negated);
    }

    #[test]
    fn the_tone_is_one_kilohertz() {
        // Given: a cycle of SINE's length, repeated at SAMPLE_RATE_HZ

        // When
        let tone = TONE_HZ;

        // Then
        assert_eq!(tone, 1_000);
    }
}
