//! How close the audio buffer came to running dry.
//!
//! Bench step 8 asks for several minutes of playback with zero underruns and
//! the buffer occupancy logged, and design §5 says the cushion must be sized
//! "from measurement, not from this estimate". This is the thing that
//! measures it.
//!
//! It records levels the DMA driver reports rather than keeping its own count
//! of bytes in and out. A parallel tally would be a second opinion that can
//! drift from the hardware, and a cushion that disagrees with the buffer it is
//! describing is worse than no cushion at all.

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
            // Nothing has been seen yet, so the emptiest so far is "as full as
            // it gets" — any real observation lowers it.
            low_water: capacity,
            underruns: 0,
            playing: false,
            empty: false,
        }
    }

    /// Begins counting.
    ///
    /// Separate from construction because an empty buffer before playback is
    /// normal — it is the state every track starts in — while an empty buffer
    /// during playback is the fault step 8 is looking for.
    pub fn start(&mut self) {
        self.playing = true;
        self.low_water = self.capacity;
        // Whatever the buffer was before playback, the first observation
        // during it decides whether it is dry.
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

        // Count the transition into dry, not each look at a dry buffer: ten
        // polls during one dropout are one dropout.
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
    /// This is the headroom measurement. A run that never drops below, say,
    /// 80% was never close to failing; one that touches 5% passed by luck.
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
        // Scaled before dividing, so a small buffer does not round to zero;
        // capacity is bytes of audio, far below the overflow point.
        value * 100 / self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPACITY: u32 = 16_000;

    #[test]
    fn a_new_cushion_has_seen_nothing_go_wrong() {
        let cushion = Cushion::new(CAPACITY);
        assert_eq!(cushion.underruns(), 0);
        assert_eq!(cushion.level(), 0);
    }

    /// An empty buffer before playback is every track's starting state, not a
    /// fault. Counting it would make every run report an underrun it did not
    /// have.
    #[test]
    fn an_empty_buffer_before_playback_starts_is_not_an_underrun() {
        let mut cushion = Cushion::new(CAPACITY);
        assert!(!cushion.observe(0));
        assert_eq!(cushion.underruns(), 0);
    }

    #[test]
    fn running_dry_during_playback_is_an_underrun() {
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();
        cushion.observe(CAPACITY);
        assert!(cushion.observe(0), "the buffer just ran dry");
        assert_eq!(cushion.underruns(), 1);
    }

    /// The gap this closes: polling a dry buffer ten times is one dropout, not
    /// ten. Counting observations rather than transitions would make a single
    /// glitch look like a catastrophe and hide how often it really happened.
    #[test]
    fn staying_dry_is_one_underrun_rather_than_many() {
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();
        cushion.observe(CAPACITY);
        assert!(cushion.observe(0));
        assert!(!cushion.observe(0));
        assert!(!cushion.observe(0));
        assert_eq!(cushion.underruns(), 1);
    }

    #[test]
    fn recovering_and_running_dry_again_is_a_second_underrun() {
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();
        cushion.observe(CAPACITY);
        cushion.observe(0);
        cushion.observe(CAPACITY / 2);
        assert!(cushion.observe(0), "dry again after recovering");
        assert_eq!(cushion.underruns(), 2);
    }

    /// The measurement design §5 asks for: not whether it survived, but by how
    /// much.
    #[test]
    fn the_low_water_mark_is_the_emptiest_it_has_been() {
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();
        cushion.observe(CAPACITY);
        cushion.observe(CAPACITY / 4);
        cushion.observe(CAPACITY / 2);
        assert_eq!(cushion.low_water(), CAPACITY / 4, "not the most recent");
    }

    #[test]
    fn the_low_water_mark_ignores_the_empty_buffer_before_playback() {
        let mut cushion = Cushion::new(CAPACITY);
        cushion.observe(0);
        cushion.start();
        cushion.observe(CAPACITY);
        assert_eq!(cushion.low_water(), CAPACITY);
    }

    #[test]
    fn occupancy_is_reported_as_a_percentage_for_the_log() {
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();
        cushion.observe(CAPACITY / 4);
        assert_eq!(cushion.percent(), 25);
        cushion.observe(CAPACITY);
        assert_eq!(cushion.percent(), 100);
        assert_eq!(cushion.low_water_percent(), 25);
    }

    /// A driver reporting more than the buffer holds is a bug somewhere, but
    /// it must not produce a percentage above 100 in the log and send someone
    /// hunting the wrong fault.
    #[test]
    fn a_level_beyond_capacity_is_clamped_rather_than_believed() {
        let mut cushion = Cushion::new(CAPACITY);
        cushion.start();
        cushion.observe(CAPACITY * 2);
        assert_eq!(cushion.level(), CAPACITY);
        assert_eq!(cushion.percent(), 100);
    }

    /// A zero-capacity cushion is nonsense, but dividing by it in a log line
    /// would panic on the device, which is a worse outcome than a useless
    /// number.
    #[test]
    fn a_zero_capacity_cushion_does_not_divide_by_zero() {
        let mut cushion = Cushion::new(0);
        cushion.start();
        cushion.observe(0);
        assert_eq!(cushion.percent(), 0);
        assert_eq!(cushion.low_water_percent(), 0);
    }
}
