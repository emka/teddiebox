//! Three-cell NiMH state of charge.
//!
//! NiMH has a famously flat discharge curve, so a percentage would be fiction.
//! This reports coarse buckets with hysteresis instead. Cells have no
//! protection circuit, so the low-voltage cutoff here is what stops the pack
//! being driven into cell reversal.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BatteryLevel {
    Critical,
    Low,
    Ok,
    Full,
}

#[derive(Debug, Clone, Copy)]
pub struct BatteryConfig {
    /// Pack millivolts at or above which the level is Full.
    pub full_mv: u16,
    /// Pack millivolts at or above which the level is Ok.
    pub ok_mv: u16,
    /// Pack millivolts at or above which the level is Low; below is Critical.
    pub low_mv: u16,
    /// Hard cutoff. Below this the pack must stop being discharged.
    pub cutoff_mv: u16,
    /// Millivolts added back to a reading taken while playing, to compensate
    /// for sag under load.
    ///
    /// **Uncalibrated.** The default is a placeholder until Phase B step 3
    /// measures real sag; too small and the box shuts down early, too large
    /// and the cutoff is defeated.
    pub load_offset_mv: u16,
    /// Millivolts a reading must exceed a threshold by before the level is
    /// allowed to climb back up, preventing flicker at a boundary.
    pub hysteresis_mv: u16,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        // Three cells: 1.4 V/cell charged, 1.2 V/cell nominal, 1.0 V/cell empty.
        Self {
            full_mv: 3_900,
            ok_mv: 3_500,
            low_mv: 3_200,
            cutoff_mv: 3_000,
            load_offset_mv: 150,
            hysteresis_mv: 60,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BatteryModel {
    config: BatteryConfig,
    level: BatteryLevel,
    shut_down: bool,
}

impl BatteryModel {
    pub fn new(config: BatteryConfig) -> Self {
        Self {
            config,
            level: BatteryLevel::Ok,
            shut_down: false,
        }
    }

    pub fn level(&self) -> BatteryLevel {
        self.level
    }

    /// True once the pack has fallen below the cutoff. Latching: it never
    /// clears on its own, because a pack that recovers voltage after the load
    /// is removed is still empty.
    pub fn must_shut_down(&self) -> bool {
        self.shut_down
    }

    /// Feeds a reading. Returns the new level only when it changed.
    pub fn update(&mut self, pack_mv: u16, under_load: bool) -> Option<BatteryLevel> {
        let mv = if under_load {
            pack_mv.saturating_add(self.config.load_offset_mv)
        } else {
            pack_mv
        };

        if mv < self.config.cutoff_mv {
            self.shut_down = true;
        }

        let hyst = self.config.hysteresis_mv;
        // Climbing a bucket requires clearing its threshold by the hysteresis
        // margin; falling takes effect immediately, because under-reporting
        // charge is the safe direction to be wrong in.
        let new = if mv >= self.config.full_mv.saturating_add(hyst)
            || (self.level >= BatteryLevel::Full && mv >= self.config.full_mv)
        {
            BatteryLevel::Full
        } else if mv >= self.config.ok_mv.saturating_add(hyst)
            || (self.level >= BatteryLevel::Ok && mv >= self.config.ok_mv)
        {
            BatteryLevel::Ok
        } else if mv >= self.config.low_mv.saturating_add(hyst)
            || (self.level >= BatteryLevel::Low && mv >= self.config.low_mv)
        {
            BatteryLevel::Low
        } else {
            BatteryLevel::Critical
        };

        if new == self.level {
            None
        } else {
            self.level = new;
            Some(new)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> BatteryModel {
        BatteryModel::new(BatteryConfig::default())
    }

    #[test]
    fn a_full_pack_reads_full() {
        let mut b = model();
        assert_eq!(b.update(4_000, false), Some(BatteryLevel::Full));
    }

    #[test]
    fn an_unchanged_level_reports_nothing() {
        let mut b = model();
        b.update(4_000, false);
        assert_eq!(b.update(3_950, false), None);
    }

    #[test]
    fn the_level_falls_through_the_buckets() {
        let mut b = model();
        b.update(4_000, false);
        assert_eq!(b.update(3_600, false), Some(BatteryLevel::Ok));
        assert_eq!(b.update(3_300, false), Some(BatteryLevel::Low));
        assert_eq!(b.update(3_100, false), Some(BatteryLevel::Critical));
    }

    #[test]
    fn hysteresis_stops_the_level_flickering_at_a_boundary() {
        let mut b = model();
        b.update(4_000, false);
        b.update(3_490, false); // just under the Ok threshold, so drops to Low
        assert_eq!(b.level(), BatteryLevel::Low);
        // Back above the threshold, but not by the hysteresis margin.
        assert_eq!(b.update(3_510, false), None);
        assert_eq!(b.level(), BatteryLevel::Low);
        // Clearly above it.
        assert_eq!(b.update(3_990, false), Some(BatteryLevel::Full));
    }

    #[test]
    fn a_reading_under_load_is_compensated_upward() {
        let mut b = model();
        b.update(3_300, false);
        assert_eq!(b.level(), BatteryLevel::Low);
        // 3_450 sagging under load is really about 3_600 at rest, which clears
        // the Ok threshold; uncompensated it would have stayed Low.
        assert_eq!(b.update(3_450, true), Some(BatteryLevel::Ok));
    }

    #[test]
    fn falling_below_the_cutoff_latches_shutdown() {
        let mut b = model();
        b.update(2_900, false);
        assert!(b.must_shut_down());
    }

    #[test]
    fn shutdown_does_not_clear_when_the_voltage_recovers() {
        let mut b = model();
        b.update(2_900, false);
        b.update(3_800, false);
        assert!(
            b.must_shut_down(),
            "a pack that rebounds once unloaded is still empty"
        );
    }

    #[test]
    fn the_cutoff_is_judged_on_the_compensated_reading() {
        let mut b = model();
        // 2_950 under load compensates to 3_100, which is above the cutoff.
        b.update(2_950, true);
        assert!(!b.must_shut_down());
    }
}
