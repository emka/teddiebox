//! Volume stepping with a parental ceiling.

use crate::{Volume, MAX_VOLUME};

/// The level, in whole dB, that one volume step asks of the codec.
///
/// The ladder is anchored on the only listening the box has had rather than
/// on a curve: a Tonie at -12 dB was reported as "120% of the max volume", so
/// the ceiling sits under it, and -35 dB is the level the bench has played
/// stories at for weeks, so the step a box boots on lands beside it. Seven dB
/// apart, because equal dB steps are what equal loudness steps sound like.
///
/// Zero is silence rather than one more rung. `Tlv320Dac3100::set_volume_db`
/// takes whole dB and its register floor is -63.5, so -63 is as quiet as the
/// codec goes without muting it.
///
/// A step above [`MAX_VOLUME`] is answered with the ceiling. [`Volume`] is a
/// tuple struct with a public field and nothing in the type prevents one, and
/// the ceiling is the only safe answer beside a child's head.
pub fn db_for(volume: Volume) -> i8 {
    const LEVELS: [i8; MAX_VOLUME as usize + 1] = [-63, -43, -36, -29, -22, -15];
    LEVELS[(volume.0.min(MAX_VOLUME)) as usize]
}

#[derive(Debug, Clone, Copy)]
pub struct VolumeModel {
    current: u8,
    limit: u8,
}

impl VolumeModel {
    /// `limit` is clamped into `0..=MAX_VOLUME`. Starts at half the limit so a
    /// freshly booted box is audible without being startling.
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

    /// The ceiling is set from the one listening test there has been. A first
    /// Tonie played at -12 dB was, in the listener's words, "120% of the max
    /// volume", so the loudest step the ears can reach sits below it.
    #[test]
    fn the_loudest_step_is_quieter_than_the_level_that_was_too_loud() {
        assert_eq!(db_for(Volume(MAX_VOLUME)), -15);
    }

    /// `VolumeModel::new` starts a box at half its limit, so this is the level
    /// a freshly booted box plays at. -35 dB is what the bench has actually
    /// listened to and found reasonable; landing within a dB of it means the
    /// default is evidence rather than taste.
    #[test]
    fn the_step_a_box_boots_at_is_the_level_the_bench_listened_to() {
        assert_eq!(db_for(Volume(MAX_VOLUME / 2)), -36);
    }

    /// Zero is silence, not one more rung. The codec's own floor is -63.5 dB
    /// and `set_volume_db` takes whole dB, so -63 is as quiet as it goes.
    #[test]
    fn the_lowest_step_is_the_codecs_floor_rather_than_another_rung() {
        assert_eq!(db_for(Volume(0)), -63);
    }

    /// Equal dB steps are what equal loudness steps sound like. A ladder that
    /// bunched up at one end would give the ears a dead zone.
    #[test]
    fn the_audible_steps_are_evenly_spaced() {
        assert_eq!(db_for(Volume(1)), -43);
        assert_eq!(db_for(Volume(2)), -36);
        assert_eq!(db_for(Volume(3)), -29);
        assert_eq!(db_for(Volume(4)), -22);
        assert_eq!(db_for(Volume(5)), -15);
    }

    /// `Volume` is a tuple struct with a public field, so nothing in the type
    /// stops a caller handing over a step that does not exist. Answering the
    /// ceiling is the only safe reading beside someone's head.
    #[test]
    fn a_step_above_the_maximum_is_answered_with_the_ceiling() {
        assert_eq!(db_for(Volume(MAX_VOLUME + 1)), -15);
        assert_eq!(db_for(Volume(255)), -15);
    }

    #[test]
    fn starts_at_half_the_limit() {
        assert_eq!(VolumeModel::new(4).current(), Volume(2));
    }

    #[test]
    fn stepping_up_increases_by_one() {
        let mut v = VolumeModel::new(4);
        assert_eq!(v.up(), Some(Volume(3)));
        assert_eq!(v.current(), Volume(3));
    }

    #[test]
    fn stepping_up_stops_at_the_parental_limit() {
        let mut v = VolumeModel::new(2);
        v.up();
        assert_eq!(v.current(), Volume(2));
        assert_eq!(v.up(), None, "at the ceiling, nothing changes");
        assert_eq!(v.current(), Volume(2));
    }

    #[test]
    fn stepping_down_stops_at_silence() {
        let mut v = VolumeModel::new(2);
        v.down();
        assert_eq!(v.current(), Volume(0));
        assert_eq!(v.down(), None);
    }

    #[test]
    fn a_limit_above_the_maximum_is_clamped() {
        assert_eq!(VolumeModel::new(250).limit(), MAX_VOLUME);
    }
}
