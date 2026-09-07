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
            Colour::Orange => (true, true, false),
            Colour::Cyan => (false, true, true),
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
}

/// The rail feeding the requested peripheral is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotPowered;

/// Peak duty of the idle breath, out of 255.
///
/// Deliberately dim: this is a sign of life on a device that sits in a child's
/// room, not an indicator anyone needs to read across the room.
pub const BREATHE_PEAK: u8 = 40;

/// One full breath, in and out.
pub const BREATHE_PERIOD_MS: u32 = 4_000;

/// Duty at a point in the breath, rising then falling.
///
/// A triangle rather than a sine: the difference is invisible at this
/// brightness and it costs no floating point on a chip that would rather not.
pub const fn breathing_duty(now_ms: u32) -> u8 {
    let half = BREATHE_PERIOD_MS / 2;
    let phase = now_ms % BREATHE_PERIOD_MS;
    let rising = if phase < half {
        phase
    } else {
        BREATHE_PERIOD_MS - phase
    };
    ((rising * BREATHE_PEAK as u32) / half) as u8
}

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
    fn the_breath_is_symmetric_about_its_peak() {
        let quarter = BREATHE_PERIOD_MS / 4;
        assert_eq!(breathing_duty(quarter), 20);
        assert_eq!(breathing_duty(BREATHE_PERIOD_MS - quarter), 20);
    }

    /// It runs off a free-running clock, so it must keep breathing rather than
    /// latch at one brightness once that clock has been up a while.
    #[test]
    fn the_breath_repeats_across_periods() {
        assert_eq!(
            breathing_duty(BREATHE_PERIOD_MS * 7 + 1_000),
            breathing_duty(1_000)
        );
    }

    /// Never brighter than the cap, whatever the clock says.
    #[test]
    fn the_breath_never_exceeds_its_peak() {
        let mut t = 0;
        while t < BREATHE_PERIOD_MS * 2 {
            assert!(breathing_duty(t) <= BREATHE_PEAK, "at {t}");
            t += 37;
        }
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
}
