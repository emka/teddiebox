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
    /// GPIO45 starts Low deliberately: it is the VDD_SPI strapping pin, and
    /// coming up High would select a 1.8 V flash supply.
    pub fn new(
        gpio45: impl esp_hal::gpio::OutputPin + 'd,
        gpio47: impl esp_hal::gpio::OutputPin + 'd,
        gpio19: impl esp_hal::gpio::OutputPin + 'd,
        gpio18: impl esp_hal::gpio::OutputPin + 'd,
        gpio17: impl esp_hal::gpio::OutputPin + 'd,
    ) -> Self {
        let cfg = OutputConfig::default();
        Self {
            gate_peripherals: Output::new(gpio45, Level::Low, cfg),
            // Storage is active low, so High is off.
            gate_storage: Output::new(gpio47, Level::High, cfg),
            led_red: Output::new(gpio19, Level::Low, cfg),
            led_green: Output::new(gpio18, Level::Low, cfg),
            led_blue: Output::new(gpio17, Level::Low, cfg),
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
