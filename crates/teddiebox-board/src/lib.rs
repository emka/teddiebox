#![no_std]

//! Board power policy: which pin, at which level, in which order.
//!
//! Pure logic with no HAL types, so it can be tested on the host. The
//! firmware owns the pins and applies what this decides.
//!
//! A separate crate from `teddiebox-core` because nothing here knows about
//! stories. It describes the box as a physical object (sides, colours), and
//! `teddiebox-core` depends on it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

/// Power gate 2: the accelerometer, the codec and the LED. High enables.
///
/// Also the VDD_SPI strapping pin. If it is high at reset, the chip selects a
/// 1.8 V flash supply and will not boot. See [`Gates::release_for_reset`].
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
/// Not `Copy`: there must be only one record of the rails' state. With `Copy`,
/// two tasks could each hold a copy, and the copies could disagree.
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
    /// The two rails have opposite polarity; this is the only place that
    /// knows it.
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
    /// Only works for a restart this code starts; a watchdog or brownout reset
    /// happens without warning. Whether that is a problem depends on what the
    /// board pulls GPIO45 to, which has not been measured.
    pub fn release_for_reset(&mut self) -> [PinLevel; 2] {
        [
            self.power(Rail::Peripherals, false),
            self.power(Rail::Storage, false),
        ]
    }

    /// The three pin levels that show `colour`.
    ///
    /// Fails if the peripherals rail is off, instead of silently doing
    /// nothing. A dark LED has many possible causes; calling this too early
    /// should not be one of them.
    pub fn led(&self, colour: Colour) -> Result<[PinLevel; 3], NotPowered> {
        if !self.is_on(Rail::Peripherals) {
            return Err(NotPowered);
        }

        let (red, green, blue) = match colour {
            Colour::Off => (false, false, false),
            Colour::Red => (true, false, false),
            Colour::Green => (false, true, false),
            Colour::Blue => (false, false, true),
            Colour::Orange => (true, true, false),
            Colour::Cyan => (false, true, true),
            Colour::Magenta => (true, false, true),
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

/// The level each of the five pins must have as soon as it is claimed: both
/// gates off and every LED channel dark.
///
/// `firmware/src/pins.rs` uses this as the initial `Level` of each `Output`,
/// so the polarities are only defined here (via [`Gates::power`] and
/// [`LED_ACTIVE_HIGH`]).
///
/// GPIO45 ([`GATE_PERIPHERALS`]) must start low: it is also the VDD_SPI
/// strapping pin, and high at reset stops the chip from booting.
pub fn at_reset_levels() -> [PinLevel; 5] {
    let mut gates = Gates::at_reset();
    let peripherals_off = gates.power(Rail::Peripherals, false);
    let storage_off = gates.power(Rail::Storage, false);

    [
        peripherals_off,
        storage_off,
        PinLevel {
            gpio: LED_RED,
            high: !LED_ACTIVE_HIGH,
        },
        PinLevel {
            gpio: LED_GREEN,
            high: !LED_ACTIVE_HIGH,
        },
        PinLevel {
            gpio: LED_BLUE,
            high: !LED_ACTIVE_HIGH,
        },
    ]
}

/// The RGB LED's pins.
pub const LED_RED: u8 = 19;
pub const LED_GREEN: u8 = 18;
pub const LED_BLUE: u8 = 17;

/// Whether a lit channel is driven high.
///
/// Not documented anywhere; found by trying it on the box.
pub const LED_ACTIVE_HIGH: bool = true;

/// The colours the LED can show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    Off,
    Red,
    Green,
    Blue,
    /// Red and green together. Warns of a low battery, and is different from
    /// the red used for a fault.
    Orange,
    /// Green and blue together, for a box that is idle and on its charger.
    Cyan,
    /// Red and blue together. Used only for setup mode.
    Magenta,
}

/// The rail feeding the requested peripheral is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotPowered;

/// How bright the indicator is lit, out of 255.
///
/// Dim on purpose, because the box sits in a child's room. The LED is steady,
/// not pulsing; the colour carries the meaning.
pub const LED_DUTY: u8 = 40;

/// The codec's hardware reset line.
pub const DAC_RESET: u8 = 26;

/// Whether driving [`DAC_RESET`] high runs the codec.
///
/// The RevvoX pinout labels GPIO26 "RESET (active high)". The TLV320DAC3100's
/// own reset pin is active low, so the board inverts it: driving it high lets
/// the codec run. If the codec stops answering on I2C, check this first.
pub const DAC_RESET_RUNS_HIGH: bool = true;

/// The level that holds the codec in reset, and the one that releases it.
pub const fn dac_reset(held: bool) -> PinLevel {
    PinLevel {
        gpio: DAC_RESET,
        high: held != DAC_RESET_RUNS_HIGH,
    }
}

/// An axis of the accelerometer, named in this module's own terms.
///
/// Not the driver's type, so this crate does not depend on the driver. The
/// firmware converts between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

/// Which side of the box a click on `axis` means, if any.
///
/// **Y, positive for the box's left.** Measured with the accelerometer at
/// 400 Hz: five slaps on the left gave `Y+` four times, five on the right gave
/// `Y-` three times. Y is the only axis whose sign follows the side.
///
/// X and Z are not slaps. X is vertical when the box stands upright, so an X
/// click is the box being put down. Z gave `Z-` on both sides, so it shows how
/// hard the box was hit, not where.
///
/// At 50 Hz the side could not be detected: the impact and the box rocking
/// afterwards blurred together, and the same side gave both signs. The sample
/// rate matters more than the threshold.
pub const fn side_for_click(axis: Axis, negative: bool) -> Option<Side> {
    match axis {
        Axis::Y if negative => Some(Side::Right),
        Axis::Y => Some(Side::Left),
        Axis::X | Axis::Z => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gates with the peripherals rail on, so the LED can be lit.
    fn powered() -> Gates {
        let mut gates = Gates::at_reset();
        gates.power(Rail::Peripherals, true);
        gates
    }

    /// Each colour as the red (GPIO19), green (GPIO18) and blue (GPIO17) pin
    /// levels it drives. Literal pins, so swapped channels fail.
    #[test]
    fn each_colour_lights_its_own_channels() {
        // Given
        let gates = powered();
        let colours = [
            Colour::Off,
            Colour::Red,
            Colour::Green,
            Colour::Blue,
            Colour::Orange,
            Colour::Cyan,
            Colour::Magenta,
        ];

        // When
        let lit = colours.map(|colour| {
            let levels = gates.led(colour).expect("the rail is up");
            (colour, levels.map(|level| (level.gpio, level.high)))
        });

        // Then
        assert_eq!(
            lit,
            [
                (Colour::Off, [(19, false), (18, false), (17, false)]),
                (Colour::Red, [(19, true), (18, false), (17, false)]),
                (Colour::Green, [(19, false), (18, true), (17, false)]),
                (Colour::Blue, [(19, false), (18, false), (17, true)]),
                // No orange channel: red and green together. Blue would wash
                // it out to white; red alone would look like a fault.
                (Colour::Orange, [(19, true), (18, true), (17, false)]),
                (Colour::Cyan, [(19, false), (18, true), (17, true)]),
                (Colour::Magenta, [(19, true), (18, false), (17, true)]),
            ]
        );
    }

    /// The two rails have opposite polarity. Getting one backwards leaves the
    /// peripherals dead, which looks like a wiring fault.
    #[test]
    fn the_peripherals_rail_is_enabled_by_driving_gpio45_high() {
        // Given
        let mut gates = Gates::at_reset();

        // When
        let level = gates.power(Rail::Peripherals, true);

        // Then
        assert_eq!(
            level,
            PinLevel {
                gpio: 45,
                high: true
            }
        );
    }

    #[test]
    fn the_storage_rail_is_enabled_by_driving_gpio47_low() {
        // Given
        let mut gates = Gates::at_reset();

        // When
        let level = gates.power(Rail::Storage, true);

        // Then
        assert_eq!(
            level,
            PinLevel {
                gpio: 47,
                high: false
            }
        );
    }

    #[test]
    fn nothing_is_powered_at_reset() {
        // Given: the board just out of reset

        // When
        let gates = Gates::at_reset();

        // Then
        assert!(!gates.is_on(Rail::Peripherals));
        assert!(!gates.is_on(Rail::Storage));
    }

    /// GPIO45 is also the VDD_SPI strapping pin: high at reset stops the chip
    /// from booting. The whole array is checked, so a version that forgot the
    /// storage rail would also fail.
    #[test]
    fn releasing_for_reset_drives_gpio45_low_and_gpio47_high() {
        // Given
        let mut gates = Gates::at_reset();
        gates.power(Rail::Peripherals, true);
        gates.power(Rail::Storage, true);

        // When
        let released = gates.release_for_reset();

        // Then
        assert_eq!(
            released,
            [
                PinLevel {
                    gpio: 45,
                    high: false
                },
                PinLevel {
                    gpio: 47,
                    high: true
                },
            ]
        );
        assert!(!gates.is_on(Rail::Peripherals));
        assert!(!gates.is_on(Rail::Storage));
    }

    /// `power` returns the right pin level *and* records the change. `led()`
    /// relies on the recorded state.
    #[test]
    fn powering_a_rail_records_that_it_is_on() {
        // Given
        let mut gates = Gates::at_reset();
        let rails = |gates: &Gates| (gates.is_on(Rail::Peripherals), gates.is_on(Rail::Storage));

        // When
        gates.power(Rail::Peripherals, true);
        let peripherals_on = rails(&gates);
        gates.power(Rail::Storage, true);
        let both_on = rails(&gates);
        gates.power(Rail::Peripherals, false);
        let peripherals_off = rails(&gates);

        // Then: switching one never switches the other
        assert_eq!(peripherals_on, (true, false));
        assert_eq!(both_on, (true, true));
        assert_eq!(peripherals_off, (false, true));
    }

    /// The LED is powered by the peripherals rail, so setting a colour before
    /// that rail is on is an error, not a silent no-op.
    #[test]
    fn a_colour_cannot_be_set_before_the_rail_that_feeds_it() {
        // Given
        let gates = Gates::at_reset();

        // When
        let levels = gates.led(Colour::Red);

        // Then
        assert_eq!(levels, Err(NotPowered));
    }

    /// Literal values, so the test can disagree with the code.
    #[test]
    fn at_reset_every_rail_is_off_and_every_led_is_dark() {
        // Given: the board just out of reset

        // When
        let levels = at_reset_levels();

        // Then
        assert_eq!(
            levels,
            [
                PinLevel {
                    gpio: 45,
                    high: false
                },
                PinLevel {
                    gpio: 47,
                    high: true
                },
                PinLevel {
                    gpio: 19,
                    high: false
                },
                PinLevel {
                    gpio: 18,
                    high: false
                },
                PinLevel {
                    gpio: 17,
                    high: false
                },
            ]
        );
    }

    #[test]
    fn releasing_the_codec_drives_its_reset_line_to_the_running_level() {
        // Given
        let (released, held) = (false, true);

        // When
        let levels = [released, held].map(dac_reset);

        // Then
        assert_eq!(
            levels,
            [
                PinLevel {
                    gpio: 26,
                    high: true
                },
                PinLevel {
                    gpio: 26,
                    high: false
                },
            ]
        );
    }

    /// Measured: a slap on the left gives `Y+`, on the right `Y-`.
    #[test]
    fn a_slap_on_y_picks_a_side_by_its_sign() {
        // Given
        let (positive, negative) = (false, true);

        // When
        let sides = [positive, negative].map(|sign| side_for_click(Axis::Y, sign));

        // Then
        assert_eq!(sides, [Some(Side::Left), Some(Side::Right)]);
    }

    /// X is vertical when the box stands upright, so an X click is the box
    /// being put down. Z has the same sign on both sides. Neither is a slap.
    #[test]
    fn a_click_on_another_axis_is_not_a_slap() {
        // Given
        let clicks = [
            (Axis::X, false),
            (Axis::X, true),
            (Axis::Z, false),
            (Axis::Z, true),
        ];

        // When
        let sides = clicks.map(|(axis, negative)| side_for_click(axis, negative));

        // Then
        assert_eq!(sides, [None; 4]);
    }
}
