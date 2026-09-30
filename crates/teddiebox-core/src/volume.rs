//! Volume steps with a parental limit.

use crate::{Output, Volume, MAX_VOLUME};

/// How much quieter each headphone step is than the same speaker step, in
/// whole dB.
///
/// **An untested guess**, based roughly on the difference between a small
/// speaker across a room and a headphone in an ear. To tune it, change this
/// number and the test that checks it together.
pub const HEADPHONE_OFFSET_DB: i8 = 12;

/// The level, in whole dB, that one volume step asks of the codec.
///
/// The speaker steps are based on listening, not on a formula: -12 dB was
/// much too loud, so the loudest step is below it, and -35 dB was a
/// comfortable level, so the starting step is close to it. Steps are 7 dB
/// apart, because equal dB steps sound like equal loudness steps.
///
/// The headphone steps are the same, [`HEADPHONE_OFFSET_DB`] quieter.
///
/// Step 0 is silence on both outputs. `Tlv320Dac3100::set_volume_db` takes
/// whole dB and the codec's lowest level is -63.5 dB, so -63 is the quietest
/// level without muting. The offset is not applied to it, since there is
/// nothing lower.
///
/// A step above [`MAX_VOLUME`] gets the loudest step. [`Volume`] has a public
/// field, so the type cannot prevent such a value, and the loudest step is
/// the only safe answer.
pub const fn db_for(output: Output, volume: Volume) -> i8 {
    const SPEAKER: [i8; MAX_VOLUME as usize + 1] = [-63, -43, -36, -29, -22, -15];
    // Written out rather than computed, so the levels are easy to read and
    // step 0 can be the codec's minimum on both.
    const HEADPHONES: [i8; MAX_VOLUME as usize + 1] = [-63, -55, -48, -41, -34, -27];
    // `Ord::min` is not const, and the firmware calls this in a `const` to get
    // the codec's start-up level.
    let step = if volume.0 > MAX_VOLUME {
        MAX_VOLUME
    } else {
        volume.0
    };
    match output {
        Output::Speaker => SPEAKER[step as usize],
        Output::Headphones => HEADPHONES[step as usize],
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VolumeModel {
    current: u8,
    limit: u8,
}

impl VolumeModel {
    /// `limit` is clamped to `0..=MAX_VOLUME`. Starts at half the limit, so a
    /// freshly booted box is audible without being too loud.
    pub fn new(limit: u8) -> Self {
        let limit = limit.min(MAX_VOLUME);
        Self {
            current: limit / 2,
            limit,
        }
    }

    pub fn current(&self) -> Volume {
        Volume(self.current)
    }

    pub fn limit(&self) -> u8 {
        self.limit
    }

    /// Returns the new volume, or `None` if already at the ceiling.
    pub fn up(&mut self) -> Option<Volume> {
        if self.current >= self.limit {
            return None;
        }
        self.current += 1;
        Some(Volume(self.current))
    }

    /// Returns the new volume, or `None` if already silent.
    pub fn down(&mut self) -> Option<Volume> {
        if self.current == 0 {
            return None;
        }
        self.current -= 1;
        Some(Volume(self.current))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Tonie played at -12 dB was much too loud, so the loudest step is
    /// below it.
    #[test]
    fn the_loudest_step_is_quieter_than_the_level_that_was_too_loud() {
        // Given
        let loudest = Volume(MAX_VOLUME);

        // When
        let level = db_for(Output::Speaker, loudest);

        // Then
        assert_eq!(level, -15);
    }

    /// `VolumeModel::new` starts at half the limit, so this is the start-up
    /// level. It is within 1 dB of -35 dB, a level found comfortable in
    /// listening tests.
    #[test]
    fn the_step_a_box_boots_at_is_the_level_the_bench_listened_to() {
        // Given
        let start_up = Volume(MAX_VOLUME / 2);

        // When
        let level = db_for(Output::Speaker, start_up);

        // Then
        assert_eq!(level, -36);
    }

    /// Step 0 is silence. The codec's minimum is -63.5 dB and `set_volume_db`
    /// takes whole dB, so -63 is the quietest level.
    #[test]
    fn the_lowest_step_is_the_codecs_floor_rather_than_another_rung() {
        // Given
        let silence = Volume(0);

        // When
        let level = db_for(Output::Speaker, silence);

        // Then
        assert_eq!(level, -63);
    }

    /// Equal dB steps sound like equal loudness steps.
    #[test]
    fn the_audible_steps_are_evenly_spaced() {
        // Given
        let audible = [1, 2, 3, 4, 5].map(Volume);

        // When
        let levels = audible.map(|step| db_for(Output::Speaker, step));

        // Then
        assert_eq!(levels, [-43, -36, -29, -22, -15]);
    }

    /// `Volume` has a public field, so a caller can pass a step that does not
    /// exist. The loudest step is the only safe answer.
    #[test]
    fn a_step_above_the_maximum_is_answered_with_the_ceiling() {
        // Given
        let too_loud = [Volume(MAX_VOLUME + 1), Volume(255)];

        // When
        let levels = too_loud.map(|step| db_for(Output::Speaker, step));

        // Then
        assert_eq!(levels, [-15, -15]);
    }

    /// Each headphone step is quieter than the speaker's. Written out as
    /// literals, so the test can disagree with the code.
    #[test]
    fn the_headphone_ladder_is_the_speakers_own_steps_made_quieter() {
        // Given
        let audible = [1, 2, 3, 4, 5].map(Volume);

        // When
        let levels = audible.map(|step| db_for(Output::Headphones, step));

        // Then
        assert_eq!(levels, [-55, -48, -41, -34, -27]);
    }

    /// The offset is not applied to silence: the codec cannot go below
    /// -63.5 dB, so -75 dB is not possible.
    #[test]
    fn headphone_silence_is_the_codecs_floor_too() {
        // Given
        let silence = Volume(0);

        // When
        let level = db_for(Output::Headphones, silence);

        // Then
        assert_eq!(level, -63);
    }

    /// The two outputs' steps are a fixed distance apart. Step 0 is skipped
    /// because the offset is not applied to silence.
    #[test]
    fn the_two_ladders_stay_the_same_distance_apart() {
        // Given
        let audible: [Volume; MAX_VOLUME as usize] = core::array::from_fn(|i| Volume(i as u8 + 1));

        // When
        let distances =
            audible.map(|step| db_for(Output::Speaker, step) - db_for(Output::Headphones, step));

        // Then
        assert_eq!(distances, [HEADPHONE_OFFSET_DB; MAX_VOLUME as usize]);
    }

    /// The same for headphones, where a too-loud level matters most.
    #[test]
    fn a_step_above_the_maximum_is_answered_with_the_headphone_ceiling() {
        // Given
        let too_loud = [Volume(MAX_VOLUME + 1), Volume(255)];

        // When
        let levels = too_loud.map(|step| db_for(Output::Headphones, step));

        // Then
        assert_eq!(levels, [-27, -27]);
    }

    #[test]
    fn starts_at_half_the_limit() {
        // Given
        let limit = 4;

        // When
        let model = VolumeModel::new(limit);

        // Then
        assert_eq!(model.current(), Volume(2));
    }

    /// The firmware powers the codec up at this step's level, computed at
    /// compile time, so it must match the reducer's start-up step.
    #[test]
    fn a_box_with_no_parental_limit_starts_on_the_middle_step() {
        // Given
        let no_limit = MAX_VOLUME;

        // When
        let model = VolumeModel::new(no_limit);

        // Then
        assert_eq!(model.current(), Volume(2));
    }

    #[test]
    fn stepping_up_increases_by_one() {
        // Given
        let mut v = VolumeModel::new(4);

        // When
        let stepped = v.up();

        // Then
        assert_eq!(stepped, Some(Volume(3)));
        assert_eq!(v.current(), Volume(3));
    }

    #[test]
    fn stepping_up_stops_at_the_parental_limit() {
        // Given
        let mut v = VolumeModel::new(2);
        v.up();
        assert_eq!(v.current(), Volume(2));

        // When
        let stepped = v.up();

        // Then
        assert_eq!(stepped, None, "at the ceiling, nothing changes");
        assert_eq!(v.current(), Volume(2));
    }

    #[test]
    fn stepping_down_stops_at_silence() {
        // Given
        let mut v = VolumeModel::new(2);
        v.down();
        assert_eq!(v.current(), Volume(0));

        // When
        let stepped = v.down();

        // Then
        assert_eq!(stepped, None);
    }

    #[test]
    fn a_limit_above_the_maximum_is_clamped() {
        // Given
        let too_high = 250;

        // When
        let model = VolumeModel::new(too_high);

        // Then
        assert_eq!(model.limit(), MAX_VOLUME);
    }
}
