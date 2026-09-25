//! Battery and charger sensing.
//!
//! The board divides both voltages down before the ADC, each by a different
//! ratio. A wrong divider gives a plausible but wrong voltage, so the
//! arithmetic lives here where it is tested.

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
/// **Measured, not from the datasheet.** A multimeter read 3.82 V across the
/// pack while the ADC reported 3111, which with the 100k/33k divider gives
/// 1257 mV at full scale: `3820 x 4095 / (3111 x 4)`.
///
/// The datasheet value for the requested attenuation is about 3100 mV, which
/// put the pack at an impossible 9.4 V. 1257 mV is closer to the 6 dB range
/// than the 11 dB range requested, so the attenuation is probably not applied
/// as requested.
///
/// **Calibrated at one point only.** The ESP32-S3's ADC is not linear, so the
/// reading is accurate near 3.8 V and approximate elsewhere. The charger
/// channel is not calibrated at all.
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

/// Whether the charger is plugged in.
///
/// The channel is not calibrated, so this is a raw threshold between two
/// observed readings: about 1,950 with nothing connected, and full scale with
/// a charger connected.
pub const fn charger_present(raw: u16) -> bool {
    raw > 3_000
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real readings from the box, not derived from `charger_mv`.
    #[test]
    fn the_charger_is_seen_when_the_reading_rails_and_not_when_it_floats() {
        assert!(
            charger_present(4095),
            "on the charger, railed at full scale"
        );
        assert!(!charger_present(1957), "nothing connected, first reading");
        assert!(!charger_present(1933), "nothing connected, second reading");
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

    /// Literal expected values, so the test can disagree with the code.
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

    /// The calibration point: the ADC read 3111 while a multimeter read 3.82 V
    /// across the pack.
    #[test]
    fn the_measured_calibration_point_reproduces_the_meter() {
        let mv = battery_mv(3111);
        assert!(
            (3_780..=3_860).contains(&mv),
            "expected about 3820 mV, got {mv}"
        );
    }
}
