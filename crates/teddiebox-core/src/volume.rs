//! Volume stepping with a parental ceiling.

use crate::{Volume, MAX_VOLUME};

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
