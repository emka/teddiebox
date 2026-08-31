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
/// The nominal figure for the highest attenuation setting. The ESP32-S3's ADC
/// is not linear and ESP-IDF corrects it with a per-chip calibration curve;
/// this is a straight line through the nominal endpoints, which is enough to
/// tell a charging pack from a flat one and not enough to trust to the
/// millivolt. **Bench step 3 compares these against a multimeter across a real
/// charge and discharge, and the correction belongs here when it does.**
const FULL_SCALE_MV: u32 = 3100;

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

    #[test]
    fn a_full_scale_reading_is_the_rail_times_its_divider() {
        assert_eq!(battery_mv(ADC_MAX), 12_400);
        assert_eq!(charger_mv(ADC_MAX), 6_200);
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
        assert_eq!(battery_mv(2048), 6_201);
        assert_eq!(charger_mv(1024), 1_550);
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
}
