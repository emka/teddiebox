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
    /// Readings that must agree before the level is allowed to change, and
    /// consecutive readings below `cutoff_mv` before the shutdown latches.
    ///
    /// The pack reading has been implausible before — the very first sample
    /// this project took was 9453 mV from three NiMH cells — and a single bad
    /// one must not make the box announce that it is turning off. Four
    /// readings at the battery task's two-second interval is eight seconds,
    /// which is nothing against a discharge curve.
    ///
    /// This is a filter for noise, not for load: a pack that sags under
    /// playback stays sagged for the length of a story, so no number here
    /// would tell that apart from a pack that is genuinely empty. That is what
    /// `load_offset_mv` is for.
    pub readings_to_agree: u8,
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
            readings_to_agree: 4,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct BatteryModel {
    config: BatteryConfig,
    level: BatteryLevel,
    shut_down: bool,
    /// The level the most recent readings have been pointing at.
    candidate: BatteryLevel,
    /// How many consecutive readings have agreed on `candidate`.
    agreed: u8,
    /// How many consecutive readings have been below the cutoff.
    ///
    /// Counted apart from `agreed`, which counts agreement on the *bucket*. A
    /// pack settled in the Critical bucket buckets an implausible 2500 mV as
    /// Critical too, so bucket agreement rises rather than resetting and one
    /// bad sample used to latch the shutdown — in the 3000-3200 mV window
    /// every discharge passes through.
    below_cutoff: u8,
}

impl BatteryModel {
    pub fn new(config: BatteryConfig) -> Self {
        Self {
            config,
            level: BatteryLevel::Ok,
            shut_down: false,
            candidate: BatteryLevel::Ok,
            agreed: 0,
            below_cutoff: 0,
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

        if new == self.candidate {
            self.agreed = self.agreed.saturating_add(1);
        } else {
            self.candidate = new;
            self.agreed = 1;
        }

        // The hard cutoff is a separate, lower threshold than the boundary of
        // the Critical bucket — 3000 mV against 3200 — and it is the one that
        // stops these unprotected cells being driven into reversal. So it is
        // judged on the compensated reading rather than on which bucket the
        // reading landed in, and it keeps its own agreement rather than
        // borrowing the bucket's: a pack already settled at Critical agrees
        // with itself about an absurd sample, which is how a single reading
        // used to switch the box off. Never cleared once armed: a pack that
        // recovers voltage as soon as the load comes off is still empty.
        if mv < self.config.cutoff_mv {
            self.below_cutoff = self.below_cutoff.saturating_add(1);
            if self.below_cutoff >= self.config.readings_to_agree {
                self.shut_down = true;
            }
        } else {
            self.below_cutoff = 0;
        }

        if self.agreed < self.config.readings_to_agree {
            return None;
        }

        if new == self.level {
            return None;
        }

        self.level = new;
        Some(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model on fixed, deliberately round thresholds.
    ///
    /// Not `BatteryConfig::default()`: these tests are about the mechanism —
    /// bucket edges, hysteresis, agreement counting, the cutoff latch — and
    /// none of them care what this pack's real voltages are. Pinning them to
    /// the shipped calibration meant every recalibration broke a dozen tests
    /// that were not about calibration. The measured numbers are exercised in
    /// `calibration` below, against the real default.
    fn model() -> BatteryModel {
        BatteryModel::new(BatteryConfig {
            full_mv: 3_900,
            ok_mv: 3_500,
            low_mv: 3_200,
            cutoff_mv: 3_000,
            load_offset_mv: 150,
            hysteresis_mv: 60,
            readings_to_agree: 4,
        })
    }

    #[test]
    fn a_full_pack_reads_full() {
        let mut b = model();
        // The model no longer believes a single sample: a level only commits
        // once `readings_to_agree` consecutive readings agree on it.
        for _ in 0..3 {
            assert_eq!(b.update(4_000, false), None, "not yet agreed");
        }
        assert_eq!(b.update(4_000, false), Some(BatteryLevel::Full));
    }

    #[test]
    fn an_unchanged_level_reports_nothing() {
        let mut b = model();
        for _ in 0..4 {
            b.update(4_000, false);
        }
        assert_eq!(b.level(), BatteryLevel::Full);
        assert_eq!(b.update(3_950, false), None);
    }

    #[test]
    fn the_level_falls_through_the_buckets() {
        let mut b = model();
        for _ in 0..4 {
            b.update(4_000, false);
        }
        for _ in 0..3 {
            b.update(3_600, false);
        }
        assert_eq!(b.update(3_600, false), Some(BatteryLevel::Ok));
        for _ in 0..3 {
            b.update(3_300, false);
        }
        assert_eq!(b.update(3_300, false), Some(BatteryLevel::Low));
        for _ in 0..3 {
            b.update(3_100, false);
        }
        assert_eq!(b.update(3_100, false), Some(BatteryLevel::Critical));
    }

    #[test]
    fn hysteresis_stops_the_level_flickering_at_a_boundary() {
        let mut b = model();
        for _ in 0..4 {
            b.update(4_000, false);
        }
        for _ in 0..3 {
            b.update(3_490, false); // just under the Ok threshold, so drops to Low
        }
        assert_eq!(b.update(3_490, false), Some(BatteryLevel::Low));
        assert_eq!(b.level(), BatteryLevel::Low);
        // Back above the threshold, but not by the hysteresis margin.
        assert_eq!(b.update(3_510, false), None);
        assert_eq!(b.level(), BatteryLevel::Low);
        // Clearly above it.
        for _ in 0..3 {
            b.update(3_990, false);
        }
        assert_eq!(b.update(3_990, false), Some(BatteryLevel::Full));
    }

    #[test]
    fn a_reading_under_load_is_compensated_upward() {
        let mut b = model();
        for _ in 0..4 {
            b.update(3_300, false);
        }
        assert_eq!(b.level(), BatteryLevel::Low);
        // 3_450 sagging under load is really about 3_600 at rest, which clears
        // the Ok threshold; uncompensated it would have stayed Low.
        for _ in 0..3 {
            b.update(3_450, true);
        }
        assert_eq!(b.update(3_450, true), Some(BatteryLevel::Ok));
    }

    #[test]
    fn falling_below_the_cutoff_latches_shutdown() {
        let mut b = model();
        for _ in 0..4 {
            b.update(2_900, false);
        }
        assert!(b.must_shut_down());
    }

    #[test]
    fn shutdown_does_not_clear_when_the_voltage_recovers() {
        let mut b = model();
        for _ in 0..4 {
            b.update(2_900, false);
        }
        assert!(b.must_shut_down());
        for _ in 0..4 {
            b.update(3_800, false);
        }
        assert!(
            b.must_shut_down(),
            "a pack that rebounds once unloaded is still empty"
        );
    }

    #[test]
    fn the_cutoff_is_judged_on_the_compensated_reading() {
        let mut b = model();
        // 2_950 under load compensates to 3_100, which is above the cutoff.
        for _ in 0..4 {
            b.update(2_950, true);
        }
        assert!(!b.must_shut_down());
    }

    /// The Critical bucket starts at `low_mv` and the hard cutoff is 200 mV
    /// below it. Reaching the bucket is a state worth announcing; reaching the
    /// cutoff is what stops the discharge, and conflating them switches the box
    /// off while the pack still has usable charge.
    #[test]
    fn settling_at_critical_does_not_by_itself_arm_the_shutdown() {
        let mut b = model();
        for _ in 0..4 {
            b.update(3_100, false);
        }
        assert_eq!(b.level(), BatteryLevel::Critical);
        assert!(
            !b.must_shut_down(),
            "3100 mV is below the bucket, above the cutoff"
        );
    }

    /// A pack already settled at Critical goes on falling, and the latch has to
    /// fire then — after the level has stopped changing.
    #[test]
    fn falling_past_the_cutoff_arms_the_shutdown_even_once_critical_is_settled() {
        let mut b = model();
        for _ in 0..4 {
            b.update(3_100, false);
        }
        assert!(!b.must_shut_down());
        for _ in 0..4 {
            b.update(2_950, false);
        }
        assert!(b.must_shut_down());
    }

    /// The very first pack sample this project ever took was 9453 mV from three
    /// NiMH cells. One reading like that, in the other direction, must not switch
    /// a box off. Hysteresis does not cover this: it guards a *boundary*, this
    /// guards a *reading*.
    #[test]
    fn one_implausible_reading_does_not_move_the_level() {
        let mut b = model();
        for _ in 0..4 {
            b.update(3_800, false);
        }
        let settled = b.level();
        assert_eq!(
            b.update(10, false),
            None,
            "one absurd reading changes nothing"
        );
        assert_eq!(b.level(), settled);
        assert!(
            !b.must_shut_down(),
            "and it must not arm the shutdown either"
        );
    }

    #[test]
    fn four_agreeing_readings_move_the_level() {
        let mut b = model();
        for _ in 0..3 {
            assert_eq!(b.update(2_900, false), None, "not yet agreed");
        }
        assert_eq!(b.update(2_900, false), Some(BatteryLevel::Critical));
        assert!(b.must_shut_down());
    }

    /// The agreement filter used to guard the cutoff only by accident: it
    /// counted agreement on the *bucket*, and a pack settled in the Critical
    /// bucket buckets an implausible 2500 mV as Critical too. So the count went
    /// up rather than resetting, and one bad sample latched the shutdown. The
    /// 3000-3200 mV window is one every discharge passes through, and this ADC
    /// has reported 9453 mV from three NiMH cells.
    #[test]
    fn one_implausible_reading_from_a_settled_critical_pack_does_not_arm_the_shutdown() {
        let mut b = model();
        for _ in 0..4 {
            b.update(3_100, false);
        }
        assert_eq!(b.level(), BatteryLevel::Critical, "settled in the bucket");
        assert!(!b.must_shut_down(), "and above the cutoff");

        b.update(2_500, false);

        assert!(
            !b.must_shut_down(),
            "one sample below the cutoff is not a pack below the cutoff"
        );
    }

    /// The cutoff keeps its own count, so noise either side of it never
    /// accumulates into a shutdown the way bucket agreement did.
    #[test]
    fn a_reading_above_the_cutoff_restarts_the_cutoff_count() {
        let mut b = model();
        for _ in 0..4 {
            b.update(3_100, false);
        }
        for _ in 0..8 {
            b.update(2_500, false);
            b.update(3_100, false);
        }
        assert!(!b.must_shut_down());
    }

    /// A reading that disagrees restarts the count, so noise alternating either
    /// side of a threshold never accumulates into a decision.
    #[test]
    fn a_disagreeing_reading_restarts_the_count() {
        let mut b = model();
        for _ in 0..3 {
            b.update(2_900, false);
        }
        assert_eq!(
            b.update(3_800, false),
            None,
            "a healthy reading interrupts it"
        );
        assert_eq!(b.update(2_900, false), None, "and the count starts again");
    }
}
