//! A test tone.
//!
//! Bench step 6 wants a clean audible tone out of the speaker. One cycle of a
//! sine, sampled so that repeating it yields exactly 1000 Hz at a
//! 48000 Hz sample rate — no phase discontinuity at the seam, because the
//! table is a whole number of cycles.

/// Samples per cycle at 48 kHz.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// The tone this produces, in hertz.
pub const TONE_HZ: u32 = SAMPLE_RATE_HZ / SINE.len() as u32;

/// Peak amplitude, a little under half full scale.
///
/// Deliberately not full scale: this is the first sound the box will ever
/// make, into a speaker nobody has tested, with a codec whose gain settings
/// have never been heard.
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
        assert_eq!(SINE[0], 0);
        // The seam: the sample after the last must be the first again.
        assert!(
            SINE[SINE.len() - 1] < 0,
            "the cycle must approach zero from below"
        );
    }

    #[test]
    fn the_peak_is_a_quarter_of_the_way_through() {
        assert_eq!(SINE[SINE.len() / 4], PEAK);
        assert_eq!(SINE[3 * SINE.len() / 4], -PEAK);
    }

    /// A tone with a DC offset heats the speaker coil and is not a tone.
    #[test]
    fn the_cycle_has_no_direct_current() {
        let sum: i32 = SINE.iter().map(|&s| i32::from(s)).sum();
        assert_eq!(sum, 0);
    }

    /// Each half is the negation of the other, which is what makes it a sine
    /// rather than something merely sine-shaped.
    #[test]
    fn the_second_half_mirrors_the_first() {
        let half = SINE.len() / 2;
        for i in 0..half {
            assert_eq!(SINE[i], -SINE[i + half], "at {i}");
        }
    }

    #[test]
    fn the_tone_is_the_rate_divided_by_the_cycle() {
        assert_eq!(TONE_HZ, 1_000);
    }
}
