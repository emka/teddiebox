//! Three-cell NiMH state of charge.
//!
//! NiMH voltage barely changes over most of a discharge, so a percentage would
//! be meaningless. This reports coarse levels with hysteresis instead. The
//! cells have no protection circuit, so the cutoff here is what stops the
//! battery being over-discharged (which can reverse a cell).
//!
//! A measured discharge went from 3943 mV to 3680 mV over eight hours, then
//! dropped 250 mV in the last two and a half. There are about 25 minutes
//! between the first useful warning and the box losing power.
//!
//! **Limitation.** The voltage at which the box stops working depends on what
//! it is doing: it once rebooted mid-story at 3656 mV, then idled from
//! 3663 mV for two and a half hours. Steady playback lowers the reading by
//! only 9 mV, so the difference is short current spikes that a sample every
//! two seconds does not see. One threshold cannot both allow use of the flat
//! part of the curve and prevent a story from causing a brownout. Deciding
//! whether to *start* playing would need a higher threshold than deciding
//! whether to stay awake; that is not done here.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BatteryLevel {
    Critical,
    Low,
    Ok,
    Full,
}

#[derive(Debug, Clone, Copy)]
pub struct BatteryConfig {
    /// Battery millivolts at or above which the level is Full.
    pub full_mv: u16,
    /// Battery millivolts at or above which the level is Ok.
    pub ok_mv: u16,
    /// Battery millivolts at or above which the level is Low; below is Critical.
    pub low_mv: u16,
    /// Hard cutoff. Below this the battery must stop being discharged.
    pub cutoff_mv: u16,
    /// Millivolts added back to a reading taken while playing, to compensate
    /// for sag under load.
    ///
    /// Measured: 9 mV median over seven playback sessions, 14 mV worst. A value
    /// that is too large lifts readings taken during a story above the
    /// cutoff, so the cutoff never fires.
    ///
    /// Steady sag is not what causes brownouts; short current spikes are, and
    /// no value here covers them. See the module docs.
    pub load_offset_mv: u16,
    /// Millivolts a reading must exceed a threshold by before the level is
    /// allowed to climb back up, preventing flicker at a boundary.
    pub hysteresis_mv: u16,
    /// Readings that must agree before the level is allowed to change, and
    /// consecutive readings below `cutoff_mv` before the shutdown latches.
    ///
    /// The ADC has returned impossible readings (9453 mV from three NiMH
    /// cells), and one bad reading must not turn the box off. Four readings,
    /// two seconds apart, take eight seconds, which is short compared to a
    /// discharge.
    ///
    /// This filters noise, not load: sag under playback lasts the whole story.
    /// `load_offset_mv` handles load.
    pub readings_to_agree: u8,
}

impl Default for BatteryConfig {
    fn default() -> Self {
        // Measured on this box's battery over two full discharge runs (13.6 h,
        // 3943 mV down to 3407 mV).
        //
        // These are not the textbook NiMH values. A NiMH cell counts as empty
        // at 1.0 V (3000 mV for the battery), but this box stops booting at about
        // 3410 mV while the cells still hold charge. A cutoff below the
        // brownout voltage could never fire.
        Self {
            full_mv: 3_800,
            ok_mv: 3_650,
            low_mv: 3_550,
            cutoff_mv: 3_480,
            load_offset_mv: 10,
            hysteresis_mv: 25,
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
    /// Counted separately from `agreed`, which counts agreement on the level.
    /// A battery already at Critical would also place a bogus 2500 mV reading in
    /// Critical, so `agreed` would keep rising and one bad reading could
    /// trigger the shutdown.
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

    /// True once the battery has fallen below the cutoff. Never clears, because a
    /// battery whose voltage recovers once the load is removed is still empty.
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

        let new = self.classify(mv);
        if new == self.candidate {
            self.agreed = self.agreed.saturating_add(1);
        } else {
            self.candidate = new;
            self.agreed = 1;
        }

        self.track_cutoff(mv);

        if self.agreed < self.config.readings_to_agree || new == self.level {
            return None;
        }

        self.level = new;
        Some(new)
    }

    /// The level a load-compensated reading falls into.
    ///
    /// Rising to a higher level needs the threshold plus the hysteresis
    /// margin. Falling happens at the threshold, because reporting too
    /// little charge is the safer mistake.
    fn classify(&self, mv: u16) -> BatteryLevel {
        let hyst = self.config.hysteresis_mv;
        if mv >= self.config.full_mv.saturating_add(hyst)
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
        }
    }

    /// The cutoff is a separate threshold, below the start of Critical. It
    /// uses the load-compensated reading and its own agreement count (see
    /// `below_cutoff`). Once set, `shut_down` is never cleared.
    fn track_cutoff(&mut self, mv: u16) {
        if mv < self.config.cutoff_mv {
            self.below_cutoff = self.below_cutoff.saturating_add(1);
            if self.below_cutoff >= self.config.readings_to_agree {
                self.shut_down = true;
            }
        } else {
            self.below_cutoff = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model on fixed, deliberately round thresholds.
    ///
    /// Not `BatteryConfig::default()`: these tests are about the logic
    /// (levels, hysteresis, agreement, the cutoff), not the real battery's
    /// voltages, so recalibrating must not break them. The real values are
    /// tested in `calibration` below.
    fn model() -> BatteryModel {
        BatteryModel::new(BatteryConfig {
            full_mv: 3_900,
            ok_mv: 3_500,
            low_mv: 3_200,
            cutoff_mv: 3_000,
            load_offset_mv: 150,
            hysteresis_mv: 60,
            readings_to_agree: READINGS_TO_AGREE,
        })
    }

    /// The agreement count of both the test model and the shipped one.
    const READINGS_TO_AGREE: u8 = 4;

    /// Feeds one reading as often as the model needs to agree on it, and
    /// returns what the last of them reported.
    fn settle(b: &mut BatteryModel, mv: u16, under_load: bool) -> Option<BatteryLevel> {
        let mut last = None;
        for _ in 0..READINGS_TO_AGREE {
            last = b.update(mv, under_load);
        }
        last
    }

    #[test]
    fn a_full_pack_reads_full() {
        // Given
        let mut b = model();

        // When: a level only changes once enough readings in a row agree
        let reported = [4_000; 4].map(|mv| b.update(mv, false));

        // Then
        assert_eq!(reported, [None, None, None, Some(BatteryLevel::Full)]);
    }

    #[test]
    fn an_unchanged_level_reports_nothing() {
        // Given
        let mut b = model();
        settle(&mut b, 4_000, false);
        assert_eq!(b.level(), BatteryLevel::Full);

        // When: below Full's threshold plus its margin, for as long as a
        // change would need
        let reported = settle(&mut b, 3_950, false);

        // Then
        assert_eq!(reported, None);
    }

    #[test]
    fn the_level_falls_through_the_buckets() {
        // Given
        let mut b = model();
        settle(&mut b, 4_000, false);

        // When
        let reported = [3_600, 3_300, 3_100].map(|mv| settle(&mut b, mv, false));

        // Then
        assert_eq!(
            reported,
            [
                Some(BatteryLevel::Ok),
                Some(BatteryLevel::Low),
                Some(BatteryLevel::Critical),
            ]
        );
    }

    #[test]
    fn hysteresis_stops_the_level_flickering_at_a_boundary() {
        // Given: Full, then just under the Ok threshold, so Low
        let mut b = model();
        settle(&mut b, 4_000, false);
        assert_eq!(settle(&mut b, 3_490, false), Some(BatteryLevel::Low));

        // When: back above the threshold, but not by the hysteresis margin,
        // for as long as a change would need
        let reported = settle(&mut b, 3_510, false);

        // Then
        assert_eq!(reported, None);
        assert_eq!(b.level(), BatteryLevel::Low);
    }

    #[test]
    fn a_reading_clearly_above_a_threshold_raises_the_level() {
        // Given: Full, then just under the Ok threshold, so Low
        let mut b = model();
        settle(&mut b, 4_000, false);
        assert_eq!(settle(&mut b, 3_490, false), Some(BatteryLevel::Low));

        // When
        let reported = settle(&mut b, 3_990, false);

        // Then
        assert_eq!(reported, Some(BatteryLevel::Full));
    }

    #[test]
    fn a_reading_under_load_is_compensated_upward() {
        // Given
        let mut b = model();
        settle(&mut b, 3_300, false);
        assert_eq!(b.level(), BatteryLevel::Low);

        // When: 3_450 sagging under load is really about 3_600 at rest, which
        // clears the Ok threshold; uncompensated it would have stayed Low
        let reported = settle(&mut b, 3_450, true);

        // Then
        assert_eq!(reported, Some(BatteryLevel::Ok));
    }

    #[test]
    fn falling_below_the_cutoff_latches_shutdown() {
        // Given
        let mut b = model();

        // When
        settle(&mut b, 2_900, false);

        // Then
        assert!(b.must_shut_down());
    }

    #[test]
    fn shutdown_does_not_clear_when_the_voltage_recovers() {
        // Given
        let mut b = model();
        settle(&mut b, 2_900, false);
        assert!(b.must_shut_down());

        // When
        settle(&mut b, 3_800, false);

        // Then
        assert!(
            b.must_shut_down(),
            "a battery that rebounds once unloaded is still empty"
        );
    }

    #[test]
    fn the_cutoff_is_judged_on_the_compensated_reading() {
        // Given
        let mut b = model();

        // When: 2_950 under load compensates to 3_100, above the cutoff
        settle(&mut b, 2_950, true);

        // Then
        assert!(!b.must_shut_down());
    }

    /// Critical starts at `low_mv`; the cutoff is 200 mV lower. Reaching
    /// Critical is worth a warning. Only the cutoff turns the box off, since
    /// the battery still has usable charge above it.
    #[test]
    fn settling_at_critical_does_not_by_itself_arm_the_shutdown() {
        // Given
        let mut b = model();

        // When
        settle(&mut b, 3_100, false);

        // Then
        assert_eq!(b.level(), BatteryLevel::Critical);
        assert!(
            !b.must_shut_down(),
            "3100 mV is below the bucket, above the cutoff"
        );
    }

    /// A battery already at Critical keeps falling; the shutdown must still
    /// trigger even though the level no longer changes.
    #[test]
    fn falling_past_the_cutoff_arms_the_shutdown_even_once_critical_is_settled() {
        // Given
        let mut b = model();
        settle(&mut b, 3_100, false);
        assert!(!b.must_shut_down());

        // When
        settle(&mut b, 2_950, false);

        // Then
        assert!(b.must_shut_down());
    }

    /// The ADC has returned impossible readings (9453 mV from three NiMH
    /// cells), and one such reading must not change the level. Hysteresis
    /// does not help here: it protects a *threshold*, this protects against a
    /// bad *reading*.
    #[test]
    fn one_implausible_reading_does_not_move_the_level() {
        // Given
        let mut b = model();
        settle(&mut b, 3_800, false);
        let settled = b.level();

        // When
        let reported = b.update(10, false);

        // Then
        assert_eq!(reported, None, "one absurd reading changes nothing");
        assert_eq!(b.level(), settled);
    }

    /// Nor may one impossible reading turn the box off.
    #[test]
    fn one_implausible_reading_does_not_arm_the_shutdown() {
        // Given
        let mut b = model();
        settle(&mut b, 3_800, false);

        // When
        b.update(10, false);

        // Then
        assert!(!b.must_shut_down());
    }

    /// A battery already at Critical would also place a bogus 2500 mV reading in
    /// Critical. That single reading must not trigger the shutdown.
    #[test]
    fn one_implausible_reading_from_a_settled_critical_pack_does_not_arm_the_shutdown() {
        // Given
        let mut b = model();
        settle(&mut b, 3_100, false);
        assert_eq!(b.level(), BatteryLevel::Critical, "settled in the bucket");
        assert!(!b.must_shut_down(), "and above the cutoff");

        // When
        b.update(2_500, false);

        // Then
        assert!(
            !b.must_shut_down(),
            "one sample below the cutoff is not a battery below the cutoff"
        );
    }

    /// The cutoff keeps its own count, so noise on either side of it never
    /// adds up to a shutdown.
    #[test]
    fn a_reading_above_the_cutoff_restarts_the_cutoff_count() {
        // Given
        let mut b = model();
        settle(&mut b, 3_100, false);

        // When
        for _ in 0..8 {
            b.update(2_500, false);
            b.update(3_100, false);
        }

        // Then
        assert!(!b.must_shut_down());
    }

    /// A reading that disagrees restarts the count, so noise alternating either
    /// side of a threshold never accumulates into a decision.
    #[test]
    fn a_disagreeing_reading_restarts_the_count() {
        // Given: one reading short of agreeing
        let mut b = model();
        for _ in 0..3 {
            b.update(2_900, false);
        }

        // When: a healthy reading interrupts it, then the count starts again
        let reported = [3_800, 2_900].map(|mv| b.update(mv, false));

        // Then
        assert_eq!(reported, [None, None]);
    }

    /// The shipped calibration, against the battery it was measured on.
    ///
    /// Every millivolt value here is a real reading from two measured
    /// discharge runs, not derived from the config, so these tests can
    /// disagree with it.
    mod calibration {
        use super::*;

        fn measured() -> BatteryModel {
            BatteryModel::new(BatteryConfig::default())
        }

        /// The box rebooted at 3_410 mV eleven times during a measured run. The cutoff must fire
        /// before that.
        #[test]
        fn the_shutdown_latches_above_the_voltage_the_box_browns_out_at() {
            // Given
            let mut b = measured();

            // When
            settle(&mut b, 3_410, false);

            // Then
            assert!(
                b.must_shut_down(),
                "the battery browns out here; the cutoff has to be above it"
            );
        }

        #[test]
        fn a_pack_at_the_brownout_floor_is_not_still_called_ok() {
            // Given
            let mut b = measured();

            // When
            settle(&mut b, 3_410, false);

            // Then
            assert_eq!(b.level(), BatteryLevel::Critical);
        }

        /// Playing lowers the reading by about 9 mV (measured), so a reading during a story is
        /// close to the true value. An offset large enough to hide the brownout voltage would stop
        /// the cutoff from ever firing.
        #[test]
        fn the_load_offset_does_not_lift_a_dying_pack_over_the_cutoff() {
            // Given
            let mut b = measured();

            // When
            settle(&mut b, 3_410, true);

            // Then
            assert!(
                b.must_shut_down(),
                "a battery at the floor is at the floor, story or no story"
            );
        }

        /// A measured run fell from 3_570 to 3_407 mV in the 40 minutes after this voltage, so this
        /// must already read Low.
        #[test]
        fn the_level_drops_before_the_curve_does() {
            // Given
            let mut b = measured();

            // When
            settle(&mut b, 3_550, false);

            // Then
            assert_eq!(b.level(), BatteryLevel::Low);
        }

        /// 3_943 mV: the first reading of a measured discharge, once the surface charge had gone.
        #[test]
        fn a_pack_just_off_the_charger_reads_full() {
            // Given
            let mut b = measured();

            // When
            settle(&mut b, 3_943, false);

            // Then
            assert_eq!(b.level(), BatteryLevel::Full);
        }

        /// The battery spends most of its life here; calling any of it Low would make the warning
        /// meaningless.
        #[test]
        fn the_eight_hour_plateau_reads_ok_throughout() {
            // Given
            let mut b = measured();
            let plateau = [3_900, 3_800, 3_750, 3_700, 3_680];

            // When
            let levels = plateau.map(|mv| {
                settle(&mut b, mv, false);
                (mv, b.level())
            });

            // Then
            for (mv, level) in levels {
                assert!(
                    level >= BatteryLevel::Ok,
                    "{mv} mV is mid-plateau, not a warning"
                );
            }
        }
    }
}
