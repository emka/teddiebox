#![no_std]

//! Board power policy: which pin, at which level, in which order.
//!
//! Pure logic with no HAL types, so the decisions can be tested on the host.
//! The firmware owns the pins and applies what this decides; it decides
//! nothing itself.
//!
//! Its own crate rather than a module of `teddiebox-core`, because it has
//! nothing to do with the reducer that crate exists for: nothing here knows
//! what a story is. What it does own is the vocabulary for the box as a
//! physical object — which side, which colour — so the reducer depends on
//! this and not the other way round.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
}

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

/// The level every one of the five pins this module knows about must hold
/// the instant it is claimed, before any policy has run: both gates off and
/// every LED channel dark.
///
/// `firmware/src/pins.rs` applies this array as the reset `Level` of each
/// `Output`. It must not hardcode these levels itself — this function, built
/// from [`Gates::power`] and [`LED_ACTIVE_HIGH`], is the one place the
/// gate/LED polarities are known, matching the promise made on
/// [`Gates::power`] and [`led`](Gates::led).
///
/// GPIO45 ([`GATE_PERIPHERALS`]) is also the VDD_SPI strapping pin: High at
/// reset selects the internal 1.8 V flash supply and the chip will not
/// boot, so it MUST come up Low, which is what "off" already means for that
/// gate.
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
    /// Red and green together. The warning colour for a pack that is running
    /// out, kept distinct from the red this box uses for a fault.
    Orange,
    /// Green and blue together, for a box that is idle and on its charger.
    Cyan,
    /// Red and blue together. Setup mode, and nothing else — the one state
    /// where the box is deliberately not a teddy bear.
    Magenta,
}

/// The rail feeding the requested peripheral is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotPowered;

/// How bright the indicator is lit, out of 255.
///
/// Deliberately dim: this is a sign of life on a device that sits in a child's
/// room, not an indicator anyone needs to read across the room. It is steady —
/// the colour carries the meaning, and a box that pulses in the dark is a box
/// that gets turned to face the wall.
pub const LED_DUTY: u8 = 40;

/// The codec's hardware reset line.
pub const DAC_RESET: u8 = 26;

/// Whether driving [`DAC_RESET`] high runs the codec.
///
/// The RevvoX pinout labels GPIO26 "RESET (active high)", which is ambiguous:
/// the TLV320DAC3100's own pin is an active-low RESET, so "active high" most
/// plausibly means the board inverts it and high releases the part. **Assumed,
/// not measured.** If the codec never acknowledges on I2C, invert this one
/// constant before suspecting anything else.
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
/// Not the driver's type: this crate depends on `heapless` and nothing else,
/// and a driver type here would point the dependency the wrong way. The
/// firmware owns both and translates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

/// Which side of the box a click on `axis` means, if any.
///
/// **Y, positive for the box's left.** Measured at the bench 2026-09-13 with
/// the part running at 400 Hz: five slaps on the left face gave `Y+` four
/// times, five on the right face gave `Y-` three times. Y is the only axis
/// that flips with the side.
///
/// X and Z are deliberately not slaps. X is *vertical* when the box stands
/// as a child uses it — gravity reads `-16000` on it — so an X click is the
/// box being set down, not struck. Z showed `Z-` on both faces alike, so it
/// carries how hard the box was pushed, not which side took the blow.
///
/// At 50 Hz none of this was visible: the engine could not separate the
/// impact from the box rocking afterwards, and the same face produced both
/// signs. The rate matters more than the threshold here.
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

    /// Orange is the low-pack warning, and this LED has no orange channel —
    /// it is red and green lit together. A mapping that lights only one of
    /// them is the failure this pins: it would show red, which this box also
    /// uses for a fault, so a tired battery would read as a broken story.
    #[test]
    fn orange_lights_the_red_and_green_channels_together() {
        let mut gates = Gates::at_reset();
        gates.power(Rail::Peripherals, true);
        let levels = gates.led(Colour::Orange).expect("the rail is up");
        for level in levels {
            let lit = level.high == LED_ACTIVE_HIGH;
            match level.gpio {
                LED_RED => assert!(lit, "red is half of orange"),
                LED_GREEN => assert!(lit, "green is the other half"),
                LED_BLUE => assert!(!lit, "blue would wash it out to white"),
                other => panic!("unexpected channel {other}"),
            }
        }
    }

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
    /// boot. Asserted against the whole returned array, literally: a
    /// `release_for_reset` that forgot the storage rail entirely would still
    /// leave GPIO45 low, so checking only for GPIO45 would not catch it.
    #[test]
    fn releasing_for_reset_drives_gpio45_low_and_gpio47_high() {
        let mut gates = Gates::at_reset();
        gates.power(Rail::Peripherals, true);
        gates.power(Rail::Storage, true);

        let released = gates.release_for_reset();

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

    /// Without this, swapping `LED_GREEN` and `LED_BLUE` in `led()` would
    /// pass every test above: `red_lights_only_the_red_channel` never
    /// touches these two, and `off_darkens_every_channel` expects both
    /// false regardless of which gpio each is attached to.
    #[test]
    fn green_lights_only_the_green_channel() {
        let mut gates = Gates::at_reset();
        gates.power(Rail::Peripherals, true);

        assert_eq!(
            gates.led(Colour::Green),
            Ok([
                PinLevel {
                    gpio: 19,
                    high: false
                },
                PinLevel {
                    gpio: 18,
                    high: true
                },
                PinLevel {
                    gpio: 17,
                    high: false
                },
            ])
        );
    }

    /// See `green_lights_only_the_green_channel`: this is the other half of
    /// the swap it would not catch alone.
    #[test]
    fn blue_lights_only_the_blue_channel() {
        let mut gates = Gates::at_reset();
        gates.power(Rail::Peripherals, true);

        assert_eq!(
            gates.led(Colour::Blue),
            Ok([
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
                    high: true
                },
            ])
        );
    }

    /// Literal, not recomputed from the constants this function is built
    /// from: the test must be able to disagree with the code.
    #[test]
    fn at_reset_every_rail_is_off_and_every_led_is_dark() {
        assert_eq!(
            at_reset_levels(),
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
        assert_eq!(
            dac_reset(false),
            PinLevel {
                gpio: 26,
                high: true
            }
        );
        assert_eq!(
            dac_reset(true),
            PinLevel {
                gpio: 26,
                high: false
            }
        );
    }

    /// Measured 2026-09-13: left face gives `Y+`, right face `Y-`.
    #[test]
    fn a_slap_on_y_picks_a_side_by_its_sign() {
        assert_eq!(side_for_click(Axis::Y, false), Some(Side::Left));
        assert_eq!(side_for_click(Axis::Y, true), Some(Side::Right));
    }

    /// X is vertical when the box stands upright, so an X click is the box
    /// being set down. Z read the same sign on both faces, so it says how hard
    /// rather than which side. Neither is a slap.
    #[test]
    fn a_click_on_another_axis_is_not_a_slap() {
        assert_eq!(side_for_click(Axis::X, false), None);
        assert_eq!(side_for_click(Axis::X, true), None);
        assert_eq!(side_for_click(Axis::Z, false), None);
        assert_eq!(side_for_click(Axis::Z, true), None);
    }
}
