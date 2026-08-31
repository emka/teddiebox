//! The only place that knows a GPIO number is a physical pin.
//!
//! `teddiebox_core::board` decides which pin goes to which level; this turns
//! those decisions into writes. It contains no policy of its own.

use esp_hal::gpio::{Level, Output, OutputConfig};
use teddiebox_core::board::{self, PinLevel};

pub struct BoardPins<'d> {
    gate_peripherals: Output<'d>,
    gate_storage: Output<'d>,
    led_red: Output<'d>,
    led_green: Output<'d>,
    led_blue: Output<'d>,
}

impl<'d> BoardPins<'d> {
    /// Claims the pins, leaving every rail off.
    ///
    /// The reset level of each pin — including GPIO45, the VDD_SPI
    /// strapping pin that must come up Low or the chip will not boot on a
    /// 1.8 V flash supply it doesn't have — comes from
    /// [`board::at_reset_levels`], not from a level chosen here. That is
    /// the one place board.rs's gate/LED polarities are known; this
    /// function only applies what it says.
    pub fn new(
        gpio45: impl esp_hal::gpio::OutputPin + 'd,
        gpio47: impl esp_hal::gpio::OutputPin + 'd,
        gpio19: impl esp_hal::gpio::OutputPin + 'd,
        gpio18: impl esp_hal::gpio::OutputPin + 'd,
        gpio17: impl esp_hal::gpio::OutputPin + 'd,
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
            led_red: Output::new(gpio19, level_for(board::LED_RED), cfg),
            led_green: Output::new(gpio18, level_for(board::LED_GREEN), cfg),
            led_blue: Output::new(gpio17, level_for(board::LED_BLUE), cfg),
        }
    }

    /// Applies one decision.
    pub fn apply(&mut self, pin: PinLevel) {
        let level = if pin.high { Level::High } else { Level::Low };
        match pin.gpio {
            board::GATE_PERIPHERALS => self.gate_peripherals.set_level(level),
            board::GATE_STORAGE => self.gate_storage.set_level(level),
            board::LED_RED => self.led_red.set_level(level),
            board::LED_GREEN => self.led_green.set_level(level),
            board::LED_BLUE => self.led_blue.set_level(level),
            other => panic!("no pin for GPIO{other}"),
        }
    }

    pub fn apply_all(&mut self, pins: &[PinLevel]) {
        for &pin in pins {
            self.apply(pin);
        }
    }
}
