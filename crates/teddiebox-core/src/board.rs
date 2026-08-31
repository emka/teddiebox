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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

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
}
