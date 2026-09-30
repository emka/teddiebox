//! Measures how close the audio buffer came to running empty.
//!
//! Used to size the audio buffer from measurements and to log underruns.
//!
//! It records the levels the DMA driver reports, instead of counting bytes in
//! and out itself, so it cannot drift from the hardware.

/// A record of how full the audio buffer has been.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cushion {
    capacity: u32,
    level: u32,
    low_water: u32,
    underruns: u32,
    playing: bool,
    empty: bool,
}

impl Cushion {
    /// A cushion for a buffer of `capacity` bytes, before playback starts.
    pub const fn new(capacity: u32) -> Self {
        Self {
            capacity,
            level: 0,
            // Nothing seen yet; the first real reading lowers it.
            low_water: capacity,
            underruns: 0,
            playing: false,
            empty: false,
        }
    }

    /// Begins counting.
    ///
    /// Separate from `new` because an empty buffer before playback is normal,
    /// while an empty buffer during playback is an underrun.
    pub fn start(&mut self) {
        self.playing = true;
        self.low_water = self.capacity;
        // Only readings during playback decide whether the buffer is empty.
        self.empty = false;
    }

    /// Records the buffer's fill level as the driver reports it.
    ///
    /// Returns true when this observation is a *new* underrun.
    pub fn observe(&mut self, level: u32) -> bool {
        let level = if level > self.capacity {
            self.capacity
        } else {
            level
        };
        self.level = level;

        if !self.playing {
            return false;
        }

        if level < self.low_water {
            self.low_water = level;
        }

        // Count the change to empty, not every empty reading: ten readings
        // during one dropout are one dropout.
        let newly_empty = level == 0 && !self.empty;
        self.empty = level == 0;
        if newly_empty {
            self.underruns += 1;
        }
        newly_empty
    }

    pub const fn level(&self) -> u32 {
        self.level
    }

    /// The emptiest the buffer has been since playback started.
    ///
    /// This shows the headroom. A run that never drops below 80% was never
    /// close to failing; one that reaches 5% nearly failed.
    pub const fn low_water(&self) -> u32 {
        self.low_water
    }

    pub const fn underruns(&self) -> u32 {
        self.underruns
    }

    /// How full the buffer is now, in percent, for the log line.
    pub const fn percent(&self) -> u32 {
        self.as_percent(self.level)
    }

    /// The emptiest it has been, in percent.
    pub const fn low_water_percent(&self) -> u32 {
        self.as_percent(self.low_water)
    }

    const fn as_percent(&self, value: u32) -> u32 {
        if self.capacity == 0 {
            return 0;
        }
        // Multiply before dividing, so a small value does not round to zero.
        // The capacity is far too small to overflow.
        value * 100 / self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPACITY: u32 = 16_000;

    #[test]
    fn a_new_cushion_has_seen_nothing_go_wrong() {
        // Given
        let capacity = CAPACITY;

        // When
        let cushion = Cushion::new(capacity);

        // Then
        assert_eq!(cushion.underruns(), 0);
        assert_eq!(cushion.level(), 0);
    }

    /// Every track starts with an empty buffer; that is not an underrun.
    #[test]
    fn an_empty_buffer_before_playback_starts_is_not_an_underrun() {
        // Given
        let mut cushion = Cushion::new(CAPACITY);

        // When
        let underrun = cushion.observe(0);

        // Then
        assert!(!underrun);
        assert_eq!(cushion.underruns(), 0);
    }

    #[test]
    fn running_dry_during_playback_is_an_underrun() {
        // Given
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();
        cushion.observe(CAPACITY);

        // When
        let underrun = cushion.observe(0);

        // Then
        assert!(underrun, "the buffer just ran dry");
        assert_eq!(cushion.underruns(), 1);
    }

    /// Reading an empty buffer ten times is one dropout, not ten.
    #[test]
    fn staying_dry_is_one_underrun_rather_than_many() {
        // Given
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();
        cushion.observe(CAPACITY);

        // When
        let underruns = [0; 3].map(|level| cushion.observe(level));

        // Then
        assert_eq!(underruns, [true, false, false]);
        assert_eq!(cushion.underruns(), 1);
    }

    #[test]
    fn recovering_and_running_dry_again_is_a_second_underrun() {
        // Given: one dropout, then a recovery
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();
        cushion.observe(CAPACITY);
        cushion.observe(0);
        cushion.observe(CAPACITY / 2);

        // When
        let underrun = cushion.observe(0);

        // Then
        assert!(underrun, "dry again after recovering");
        assert_eq!(cushion.underruns(), 2);
    }

    /// The low-water mark shows how close the buffer came to running empty.
    #[test]
    fn the_low_water_mark_is_the_emptiest_it_has_been() {
        // Given
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();

        // When
        for level in [CAPACITY, CAPACITY / 4, CAPACITY / 2] {
            cushion.observe(level);
        }

        // Then
        assert_eq!(cushion.low_water(), CAPACITY / 4, "not the most recent");
    }

    #[test]
    fn the_low_water_mark_ignores_the_empty_buffer_before_playback() {
        // Given: an empty buffer seen before playback
        let mut cushion = Cushion::new(CAPACITY);
        cushion.observe(0);

        // When
        cushion.start();
        cushion.observe(CAPACITY);

        // Then
        assert_eq!(cushion.low_water(), CAPACITY);
    }

    #[test]
    fn occupancy_is_reported_as_a_percentage_for_the_log() {
        // Given
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();

        // When
        let percents = [CAPACITY / 4, CAPACITY].map(|level| {
            cushion.observe(level);
            cushion.percent()
        });

        // Then
        assert_eq!(percents, [25, 100]);
        assert_eq!(cushion.low_water_percent(), 25);
    }

    /// A driver reporting more than the buffer holds is a bug, but the log
    /// must not show more than 100%.
    #[test]
    fn a_level_beyond_capacity_is_clamped_rather_than_believed() {
        // Given
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();

        // When
        cushion.observe(CAPACITY * 2);

        // Then
        assert_eq!(cushion.level(), CAPACITY);
        assert_eq!(cushion.percent(), 100);
    }

    /// A zero capacity makes no sense, but must not cause a divide-by-zero
    /// panic on the device.
    #[test]
    fn a_zero_capacity_cushion_does_not_divide_by_zero() {
        // Given
        let mut cushion = Cushion::new(0);
        cushion.start();

        // When
        cushion.observe(0);

        // Then
        assert_eq!(cushion.percent(), 0);
        assert_eq!(cushion.low_water_percent(), 0);
    }
}
