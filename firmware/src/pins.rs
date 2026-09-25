//! The only place that knows a GPIO number is a physical pin.
//!
//! `teddiebox_board` decides which pin gets which level; this applies those
//! decisions to the pins.

use esp_hal::gpio::{Level, Output, OutputConfig};
use teddiebox_board::{self as board, PinLevel};

/// The gates. The LEDs are driven by the LEDC peripheral rather than as plain
/// outputs, so they are not here — see [`crate::led`].
pub struct BoardPins<'d> {
    gate_peripherals: Output<'d>,
    gate_storage: Output<'d>,
}

impl<'d> BoardPins<'d> {
    /// Claims the pins, leaving every rail off.
    ///
    /// Each pin's starting level comes from [`board::at_reset_levels`], the
    /// only place the polarities are defined. This includes GPIO45, the
    /// VDD_SPI strapping pin, which must start low or the chip will not boot.
    pub fn new(
        gpio45: impl esp_hal::gpio::OutputPin + 'd,
        gpio47: impl esp_hal::gpio::OutputPin + 'd,
    ) -> Self {
        let cfg = OutputConfig::default();
        let levels = board::at_reset_levels();
        let level_for = |gpio: u8| -> Level {
            match levels.iter().find(|p| p.gpio == gpio) {
                Some(p) if p.high => Level::High,
                Some(_) => Level::Low,
                None => panic!("no reset level for GPIO{gpio}"),
            }
        };
        Self {
            gate_peripherals: Output::new(gpio45, level_for(board::GATE_PERIPHERALS), cfg),
            gate_storage: Output::new(gpio47, level_for(board::GATE_STORAGE), cfg),
        }
    }

    /// Applies one decision.
    pub fn apply(&mut self, pin: PinLevel) {
        let level = if pin.high { Level::High } else { Level::Low };
        match pin.gpio {
            board::GATE_PERIPHERALS => self.gate_peripherals.set_level(level),
            board::GATE_STORAGE => self.gate_storage.set_level(level),
            // The LEDs are PWM outputs on the LEDC peripheral, not plain
            // pins; see led.rs.
            other => panic!("no pin for GPIO{other}"),
        }
    }

    pub fn apply_all(&mut self, pins: &[PinLevel]) {
        for &pin in pins {
            self.apply(pin);
        }
    }
}
