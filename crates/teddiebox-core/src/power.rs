//! Battery and charger sensing.
//!
//! The board divides both rails down before the ADC sees them, with a
//! different ratio each. Getting a divider wrong reads as a plausible voltage
//! rather than an obvious fault, which is why the arithmetic lives here.

/// Battery sense, divided by four.
pub const BATTERY_PIN: u8 = 9;
/// Charger sense, divided by two.
pub const CHARGER_PIN: u8 = 8;

const BATTERY_DIVIDER: u32 = 4;
const CHARGER_DIVIDER: u32 = 2;

/// Full scale of a 12-bit conversion.
pub const ADC_MAX: u16 = 4095;

/// Millivolts at full scale.
///
/// **Measured, not nominal.** On 2026-09-01 a multimeter read 3.82 V across the
/// pack while the ADC reported raw 3111, which with the documented 100k/33k
/// divider gives 1257 mV at full scale:
/// `3820 x 4095 / (3111 x 4)`.
///
/// The nominal figure for the attenuation this driver requests would be around
/// 3100 mV, and using it put the pack at 9.4 V — impossible for three NiMH
/// cells. 1257 mV is close to the 6 dB range rather than the 11 dB one asked
/// for, so either the attenuation is not applied as requested or the nominal
/// endpoints are badly wrong. The measurement is trusted over the theory.
///
/// **This is one point on a line.** The ESP32-S3's ADC is not linear, and step
/// 3's criterion is that the millivolts track a meter across a real charge and
/// discharge. Until that has been done, treat the reading as good near 3.8 V
/// and approximate elsewhere. The charger channel is **not** calibrated at all
/// — nobody has put a meter on it.
const FULL_SCALE_MV: u32 = 1257;

const fn scaled_mv(raw: u16, divider: u32) -> u32 {
    let raw = if raw > ADC_MAX { ADC_MAX } else { raw };
    (raw as u32 * FULL_SCALE_MV * divider) / ADC_MAX as u32
}

/// Pack voltage in millivolts.
pub const fn battery_mv(raw: u16) -> u32 {
    scaled_mv(raw, BATTERY_DIVIDER)
}

/// Charger input voltage in millivolts.
pub const fn charger_mv(raw: u16) -> u32 {
    scaled_mv(raw, CHARGER_DIVIDER)
}

/// Warn here. Three NiMH cells nominal 3.6 V; below this the box should say so
/// while it still has the power to say it.
/// Whether the charger is plugged in.
///
/// The channel is uncalibrated, so this is a threshold between two observed
/// states rather than a voltage with a meaning: nothing connected reads about
/// 1,950 counts, and a connected charger rails the channel at full scale.
/// Half way between them is far from either, which is the most this reading
/// can honestly support.
pub const fn charger_present(raw: u16) -> bool {
    raw > 3_000
}

pub const LOW_BATTERY_MV: u32 = 3_300;
/// Shut down here, before the flash writes start failing.
pub const CRITICAL_BATTERY_MV: u32 = 3_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackState {
    Healthy,
    Low,
    Critical,
}

/// Judges the pack against the documented chemistry.
pub const fn pack_state(mv: u32) -> PackState {
    if mv < CRITICAL_BATTERY_MV {
        PackState::Critical
    } else if mv <= LOW_BATTERY_MV {
        PackState::Low
    } else {
        PackState::Healthy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both readings are from this bench, not from the divider arithmetic:
    /// the channel is uncalibrated, so what it is worth is exactly the two
    /// states it has been observed in. Deriving these from `charger_mv` would
    /// make the test agree with the code by construction.
    #[test]
    fn the_charger_is_seen_when_the_reading_rails_and_not_when_it_floats() {
        assert!(
            charger_present(4095),
            "on the charger, railed at full scale"
        );
        assert!(!charger_present(1957), "nothing connected, 2026-09-03");
        assert!(!charger_present(1933), "nothing connected, 2026-09-07");
    }

    #[test]
    fn a_full_scale_reading_is_the_rail_times_its_divider() {
        assert_eq!(battery_mv(ADC_MAX), 5_028);
        assert_eq!(charger_mv(ADC_MAX), 2_514);
    }

    #[test]
    fn a_zero_reading_is_zero_millivolts() {
        assert_eq!(battery_mv(0), 0);
        assert_eq!(charger_mv(0), 0);
    }

    /// Literal expectations, not the formula restated: a test that recomputes
    /// what the code computes cannot disagree with it.
    #[test]
    fn a_midscale_reading_converts_with_its_own_divider() {
        assert_eq!(battery_mv(2048), 2_514);
        assert_eq!(charger_mv(1024), 628);
    }

    /// A reading above full scale is a broken driver, not a 20 V battery.
    #[test]
    fn a_reading_beyond_full_scale_is_clamped() {
        assert_eq!(battery_mv(9_999), battery_mv(ADC_MAX));
    }

    /// Three NiMH cells. The pack sits near 3.6 V and the spec calls for a low
    /// battery warning, so the thresholds live here rather than in the caller.
    #[test]
    fn the_pack_is_judged_against_the_documented_chemistry() {
        assert_eq!(pack_state(4_000), PackState::Healthy);
        assert_eq!(pack_state(LOW_BATTERY_MV), PackState::Low);
        assert_eq!(pack_state(LOW_BATTERY_MV - 1), PackState::Low);
        assert_eq!(pack_state(CRITICAL_BATTERY_MV - 1), PackState::Critical);
    }

    /// The calibration point itself: the raw count the ADC actually reported
    /// while a multimeter read 3.82 V across the pack.
    #[test]
    fn the_measured_calibration_point_reproduces_the_meter() {
        let mv = battery_mv(3111);
        assert!(
            (3_780..=3_860).contains(&mv),
            "expected about 3820 mV, got {mv}"
        );
    }
}
