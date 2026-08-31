//! Board power policy: which pin, at which level, in which order.
//!
//! Pure logic with no HAL types, so the decisions can be tested on the host.
//! The firmware owns the pins and applies what this decides; it decides
//! nothing itself.

/// Power gate 2: the accelerometer, the codec and the LED. High enables.
///
/// Also the VDD_SPI strapping pin. High at reset selects the internal 1.8 V
/// flash supply and the chip will not boot, which is why
/// [`Gates::release_for_reset`] exists.
pub const GATE_PERIPHERALS: u8 = 45;

/// Power gate 1: the SD card and the NFC reader. **Low** enables.
pub const GATE_STORAGE: u8 = 47;

/// A power gate, named for what it feeds rather than for its polarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rail {
    /// LIS3DH, TLV320DAC3100 and the RGB LED.
    Peripherals,
    /// SD card and TRF7962A.
    Storage,
}

/// One pin driven to one level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinLevel {
    pub gpio: u8,
    pub high: bool,
}

/// Which rails are on, and what driving them costs.
///
/// Deliberately not `Copy`: this is the one authoritative record of what the
/// board's rails are doing. `Copy` would let two tasks each hold a value
/// that believes itself authoritative, and only one of them would be right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gates {
    peripherals_on: bool,
    storage_on: bool,
}

impl Gates {
    /// The state the board comes out of reset in: everything off.
    pub const fn at_reset() -> Self {
        Self {
            peripherals_on: false,
            storage_on: false,
        }
    }

    pub const fn is_on(&self, rail: Rail) -> bool {
        match rail {
            Rail::Peripherals => self.peripherals_on,
            Rail::Storage => self.storage_on,
        }
    }

    /// Switches a rail, returning the pin level that does it.
    ///
    /// The polarity inversion between the two rails lives here and nowhere
    /// else.
    pub fn power(&mut self, rail: Rail, on: bool) -> PinLevel {
        match rail {
            Rail::Peripherals => {
                self.peripherals_on = on;
                PinLevel {
                    gpio: GATE_PERIPHERALS,
                    high: on,
                }
            }
            Rail::Storage => {
                self.storage_on = on;
                PinLevel {
                    gpio: GATE_STORAGE,
                    high: !on,
                }
            }
        }
    }

    /// Everything off, for a deliberate restart.
    ///
    /// Only a restart this code initiates can be handled; a watchdog or
    /// brownout reset does not ask. Whether that is dangerous depends on
    /// what the board pulls GPIO45 to, which is an unmeasured assumption.
    pub fn release_for_reset(&mut self) -> [PinLevel; 2] {
        [
            self.power(Rail::Peripherals, false),
            self.power(Rail::Storage, false),
        ]
    }

    /// The three pin levels that show `colour`.
    ///
    /// Fails rather than doing nothing when the peripherals rail is down: a
    /// dark LED is the expected output of a great many faults, so it must
    /// not also be the output of a caller ordering mistake.
    pub fn led(&self, colour: Colour) -> Result<[PinLevel; 3], NotPowered> {
        if !self.is_on(Rail::Peripherals) {
            return Err(NotPowered);
        }

        let (red, green, blue) = match colour {
            Colour::Off => (false, false, false),
            Colour::Red => (true, false, false),
            Colour::Green => (false, true, false),
            Colour::Blue => (false, false, true),
        };

        Ok([
            PinLevel {
                gpio: LED_RED,
                high: red == LED_ACTIVE_HIGH,
            },
            PinLevel {
                gpio: LED_GREEN,
                high: green == LED_ACTIVE_HIGH,
            },
            PinLevel {
                gpio: LED_BLUE,
                high: blue == LED_ACTIVE_HIGH,
            },
        ])
    }
}

/// The discrete RGB LED, per the hardware inventory.
pub const LED_RED: u8 = 19;
pub const LED_GREEN: u8 = 18;
pub const LED_BLUE: u8 = 17;

/// Whether a lit channel is driven high.
///
/// **Assumed, not measured.** The ears are documented active-low; the LED's
/// polarity is documented nowhere. Bench step 1 settles it, and if it is
/// wrong the only change is this constant.
pub const LED_ACTIVE_HIGH: bool = true;

/// What the LED can show during bring-up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    Off,
    Red,
    Green,
    Blue,
}

/// The rail feeding the requested peripheral is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotPowered;

#[cfg(test)]
mod tests {
    use super::*;

    /// The two rails are wired with opposite polarity. Getting one backwards
    /// leaves the peripheral dead, which is indistinguishable from a wiring
    /// fault and is diagnosed over a UART that itself needs a rail.
    #[test]
    fn the_peripherals_rail_is_enabled_by_driving_gpio45_high() {
        let mut gates = Gates::at_reset();
        assert_eq!(
            gates.power(Rail::Peripherals, true),
            PinLevel {
                gpio: 45,
                high: true
            }
        );
    }

    #[test]
    fn the_storage_rail_is_enabled_by_driving_gpio47_low() {
        let mut gates = Gates::at_reset();
        assert_eq!(
            gates.power(Rail::Storage, true),
            PinLevel {
                gpio: 47,
                high: false
            }
        );
    }

    #[test]
    fn nothing_is_powered_at_reset() {
        let gates = Gates::at_reset();
        assert!(!gates.is_on(Rail::Peripherals));
        assert!(!gates.is_on(Rail::Storage));
    }

    /// GPIO45 is the VDD_SPI strapping pin as well as the gate: left high
    /// across a reset it selects a 1.8 V flash supply and the chip will not
    /// boot.
    #[test]
    fn releasing_for_reset_drives_gpio45_low() {
        let mut gates = Gates::at_reset();
        gates.power(Rail::Peripherals, true);

        let released = gates.release_for_reset();

        assert!(
            released.iter().any(|p| p.gpio == 45 && !p.high),
            "the strapping pin must be released: {released:?}"
        );
        assert!(!gates.is_on(Rail::Peripherals));
    }

    /// `power` returns the right pin level *and* records the change. Without
    /// this, an implementation that emitted correct levels but never updated
    /// its own state would pass every other test here — and `led()` in the
    /// next task refuses to work when it believes the rail is down.
    #[test]
    fn powering_a_rail_records_that_it_is_on() {
        let mut gates = Gates::at_reset();

        gates.power(Rail::Peripherals, true);
        assert!(gates.is_on(Rail::Peripherals));
        assert!(!gates.is_on(Rail::Storage), "rails are independent");

        gates.power(Rail::Storage, true);
        assert!(gates.is_on(Rail::Storage));

        gates.power(Rail::Peripherals, false);
        assert!(!gates.is_on(Rail::Peripherals));
        assert!(
            gates.is_on(Rail::Storage),
            "switching one must not switch the other"
        );
    }

    /// The LED is fed by the peripherals rail, so asking for a colour before
    /// that rail is up cannot work. Silently doing nothing would present as
    /// a dead LED during the one bring-up step whose only output is the LED.
    #[test]
    fn a_colour_cannot_be_set_before_the_rail_that_feeds_it() {
        let gates = Gates::at_reset();
        assert_eq!(gates.led(Colour::Red), Err(NotPowered));
    }

    #[test]
    fn red_lights_only_the_red_channel() {
        let mut gates = Gates::at_reset();
        gates.power(Rail::Peripherals, true);

        assert_eq!(
            gates.led(Colour::Red),
            Ok([
                PinLevel {
                    gpio: 19,
                    high: true
                },
                PinLevel {
                    gpio: 18,
                    high: false
                },
                PinLevel {
                    gpio: 17,
                    high: false
                },
            ])
        );
    }

    #[test]
    fn off_darkens_every_channel() {
        let mut gates = Gates::at_reset();
        gates.power(Rail::Peripherals, true);

        let levels = gates.led(Colour::Off).unwrap();
        assert!(levels.iter().all(|p| !p.high));
    }
}
