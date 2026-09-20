#![no_std]

//! Driver for the TI TLV320DAC3100 stereo DAC with integrated class-D
//! speaker amplifier.

pub mod regs;

use embedded_hal::delay::DelayNs;
use embedded_hal::i2c::I2c;
use regs::{page0, page1, page3, REG_PAGE_SELECT};

/// Default 7-bit address with ADDR tied low.
pub const DEFAULT_ADDRESS: u8 = 0x18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    Bus(E),
    /// The output stages did not report themselves off in time. Cutting the
    /// supply now will be heard, so the caller is told rather than left to
    /// assume the codec is quiet.
    StillPowered,
}

pub struct Tlv320Dac3100<I2C> {
    i2c: I2C,
    address: u8,
    page: Option<u8>,
}

impl<I2C, E> Tlv320Dac3100<I2C>
where
    I2C: I2c<Error = E>,
{
    pub fn new(i2c: I2C, address: u8) -> Self {
        Self {
            i2c,
            address,
            // Unknown until the first select, so the first write always sets it.
            page: None,
        }
    }

    pub fn release(self) -> I2C {
        self.i2c
    }

    /// Forgets which page is selected, so the next access selects one again.
    ///
    /// The page cache is the driver's belief about state it does not own. Call
    /// this after anything that resets the codec behind its back — the RESET
    /// line on GPIO26, or the board's power gate cycling on idle. A stale
    /// cache does not fail: it silently writes the right value to the wrong
    /// page, which is why this is on the public surface rather than a rule to
    /// remember.
    pub fn invalidate_page(&mut self) {
        self.page = None;
    }

    /// Selects `page`, skipping the write when it is already active.
    fn select_page(&mut self, page: u8) -> Result<(), Error<E>> {
        if self.page == Some(page) {
            return Ok(());
        }
        self.i2c
            .write(self.address, &[REG_PAGE_SELECT, page])
            .map_err(Error::Bus)?;
        self.page = Some(page);
        Ok(())
    }

    fn write_reg(&mut self, page: u8, reg: u8, value: u8) -> Result<(), Error<E>> {
        self.select_page(page)?;
        self.i2c
            .write(self.address, &[reg, value])
            .map_err(Error::Bus)
    }

    /// Issues a software reset. The caller must already have released the
    /// hardware RESET line (GPIO26 on the Toniebox) and waited for the codec
    /// to come out of reset.
    pub fn reset(&mut self) -> Result<(), Error<E>> {
        self.write_reg(0, page0::SOFTWARE_RESET, 0x01)?;
        // The page register resets to 0 along with everything else.
        self.page = Some(0);
        Ok(())
    }

    /// Configures the codec and leaves it silent.
    ///
    /// The output stages are configured but not powered, and the speaker is
    /// muted: a box that is not playing anything should not be driving a
    /// speaker, and powering these stages is what it used to click on.
    /// `start_output` is what makes it audible.
    pub fn init<D: DelayNs>(&mut self, delay: &mut D) -> Result<(), Error<E>> {
        self.apply(delay, INIT_ANALOG, &[])
    }

    /// Writes `analog`, waits `DRIVER_RAMP_MS`, then writes `dac`.
    ///
    /// The wait is the whole point, so it is a step of this operation rather
    /// than something a caller is trusted to remember — SLAS671C §6.3.10.14
    /// puts it between powering the output drivers and powering the DAC, and
    /// skipping it means the DAC unmutes into drivers that are still ramping.
    pub fn apply<D: DelayNs>(
        &mut self,
        delay: &mut D,
        analog: &[(u8, u8, u8)],
        dac: &[(u8, u8, u8)],
    ) -> Result<(), Error<E>> {
        for &(page, reg, value) in analog {
            self.write_reg(page, reg, value)?;
        }
        delay.delay_ms(DRIVER_RAMP_MS);
        for &(page, reg, value) in dac {
            self.write_reg(page, reg, value)?;
        }
        Ok(())
    }

    fn read_reg(&mut self, page: u8, reg: u8) -> Result<u8, Error<E>> {
        self.select_page(page)?;
        let mut buf = [0u8; 1];
        self.i2c
            .write_read(self.address, &[reg], &mut buf)
            .map_err(Error::Bus)?;
        Ok(buf[0])
    }

    /// Sets the digital volume on both DAC channels.
    pub fn set_volume_db(&mut self, db: i8) -> Result<(), Error<E>> {
        let code = (i16::from(db) * 2).clamp(VOLUME_MIN_CODE, VOLUME_MAX_CODE) as i8 as u8;
        self.write_reg(0, page0::DAC_LEFT_VOLUME, code)?;
        self.write_reg(0, page0::DAC_RIGHT_VOLUME, code)
    }

    pub fn set_muted(&mut self, muted: bool) -> Result<(), Error<E>> {
        // Bits 3 and 2 mute the left and right DAC channels.
        let value = if muted { 0x0C } else { 0x00 };
        self.write_reg(0, page0::DAC_MUTE_CTRL, value)
    }

    /// Powers the output path up, in the order the datasheet gives.
    ///
    /// Separate from `init` because powering these stages is what the box
    /// clicked on, and nothing should pay for that until there is audio to
    /// hear. The speaker is unmuted last, which is the one step in the whole
    /// sequence already measured to be silent.
    pub fn start_output<D: DelayNs>(&mut self, delay: &mut D) -> Result<(), Error<E>> {
        for &(page, reg, value) in INIT_DAC {
            self.write_reg(page, reg, value)?;
        }
        self.write_reg(1, page1::SPK_AMP, SPK_AMP_UP)?;
        self.unmute_speaker(delay)
    }

    /// Powers the output path down again, muting before anything moves.
    pub fn stop_output(&mut self) -> Result<(), Error<E>> {
        self.mute_speaker()?;
        self.write_reg(1, page1::SPK_AMP, SPK_AMP_DOWN)?;
        self.write_reg(0, page0::DAC_MUTE_CTRL, DAC_MUTED)?;
        self.write_reg(0, page0::DAC_DATA_PATH, DAC_DOWN)
    }

    /// Mutes the class-D driver.
    pub fn mute_speaker(&mut self) -> Result<(), Error<E>> {
        self.write_reg(1, page1::SPK_DRIVER_GAIN, SPK_GAIN_MUTED)
    }

    /// Unmutes the class-D driver and waits for the gain to take effect.
    ///
    /// The last step of a start-up, and deliberately so: the speaker is muted
    /// through everything before it, because unmuting it while the drivers
    /// ramp and the DAC comes up is — measured by ear — what made the box
    /// click when it powered on.
    ///
    /// SLAS671C Table 6-110 gives D0 of the same register as "all programmed
    /// gains to the Class-D driver have been applied", so the wait is on the
    /// part rather than on a guess. Running on regardless after the poll
    /// budget is the right failure: a box that plays is better than one that
    /// refuses to finish starting.
    pub fn unmute_speaker<D: DelayNs>(&mut self, delay: &mut D) -> Result<(), Error<E>> {
        self.write_reg(1, page1::SPK_DRIVER_GAIN, SPK_GAIN_UNMUTED)?;
        for _ in 0..GAIN_POLLS {
            if self.read_reg(1, page1::SPK_DRIVER_GAIN)? & SPK_GAINS_APPLIED != 0 {
                return Ok(());
            }
            delay.delay_ms(GAIN_POLL_MS);
        }
        Ok(())
    }

    /// Runs the codec's own power-down and waits for it to finish.
    ///
    /// Setting D7 of page 1 register 46 hands the sequence to the codec, which
    /// — with `HP_POP_REMOVAL` D7 set in `INIT_ANALOG` — takes the amplifiers
    /// down before the DAC. That ordering is the datasheet's stated way to
    /// "optimize power-down POP", and it only helps if the supply is still
    /// there while it happens, so this waits for the stages to report off
    /// rather than returning as soon as the write lands.
    pub fn power_down<D: DelayNs>(&mut self, delay: &mut D) -> Result<(), Error<E>> {
        self.write_reg(1, page1::MICBIAS, SOFTWARE_POWER_DOWN)?;
        for _ in 0..POWER_DOWN_POLLS {
            let f = self.power_flags()?;
            if !f.left_dac && !f.right_dac && !f.hpl_driver && !f.left_class_d && !f.right_class_d {
                return Ok(());
            }
            delay.delay_ms(POWER_DOWN_POLL_MS);
        }
        Err(Error::StillPowered)
    }

    /// What the codec says actually powered up.
    ///
    /// Writing a power-up bit is a request; this register is the answer. A
    /// silent output with every configuration register correct is the case
    /// this exists for — it separates "we asked wrongly" from "it declined".
    pub fn power_flags(&mut self) -> Result<PowerFlags, Error<E>> {
        let v = self.read_reg(0, page0::DAC_FLAGS)?;
        Ok(PowerFlags {
            left_dac: v & 0x80 != 0,
            hpl_driver: v & 0x20 != 0,
            left_class_d: v & 0x10 != 0,
            right_dac: v & 0x08 != 0,
            right_class_d: v & 0x01 != 0,
        })
    }

    /// True while a jack is inserted. The firmware mutes the speaker on this.
    ///
    /// Detection only reports anything once `INIT_SEQUENCE` has enabled it;
    /// the reset state of the register is disabled, reading a constant 00.
    pub fn headphones_connected(&mut self) -> Result<bool, Error<E>> {
        let v = self.read_reg(0, page0::HEADSET_DETECT)?;
        Ok(v & HEADSET_DETECTED != 0)
    }
}

/// Which output stages report themselves powered up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerFlags {
    pub left_dac: bool,
    pub right_dac: bool,
    pub hpl_driver: bool,
    pub left_class_d: bool,
    pub right_class_d: bool,
}

/// The power-on configuration, as `(page, register, value)`.
///
/// Kept as data so it can be compared line by line against the datasheet.
/// Routes the DAC to both the speaker amplifier and the headphone drivers, and
/// clocks a 48 kHz DAC from BCLK alone — the board wires no MCLK, only DIN,
/// BCLK and WCLK.
///
/// The clocking is a chain of exact integers, and every link is checked here
/// because a single wrong one is inaudible until it is a wrong pitch:
///
/// - **BCLK = 1.536 MHz**, from 48 kHz x 16 bits x 2 channels. This is a
///   requirement on the I2S peripheral, not an observation: if it is ever
///   configured for 32-bit slots, BCLK doubles and `J` must halve to 32.
/// - **PLL = 1.536 MHz x J=64 = 98.304 MHz**, with P=1, R=1 and D=0. SLAS671C
///   requires 80 MHz <= PLL_CLKIN x J.D x R/P <= 110 MHz and 4 <= R x J <= 259;
///   both hold. The previous J=32 gave 49.152 MHz, under the floor, so the PLL
///   never locked at all.
/// - **fS = 98.304 MHz / (NDAC=8 x MDAC=2 x DOSR=128) = 48000 Hz** exactly.
pub const INIT_ANALOG: &[(u8, u8, u8)] = &[
    // Clocking: PLL from BCLK, CODEC_CLKIN from PLL.
    (0, page0::CLOCK_GEN_MUX, 0x07),
    (0, page0::PLL_P_R, 0x91), // PLL on, P=1, R=1
    (0, page0::PLL_J, 0x40),   // J=64
    (0, page0::PLL_D_MSB, 0x00),
    (0, page0::PLL_D_LSB, 0x00),
    (0, page0::NDAC, 0x88), // NDAC on, divide by 8
    (0, page0::MDAC, 0x82), // MDAC on, divide by 2
    (0, page0::DOSR_MSB, 0x00),
    (0, page0::DOSR_LSB, 0x80), // DOSR = 128
    // Interface: I2S, 16-bit, slave.
    (0, page0::CODEC_IF_CTRL1, 0x00),
    (0, page0::DAC_PROCESSING_BLOCK, 0x08),
    // The debounce below counts on a clock this register chooses, and the
    // reset choice is an external MCLK the board does not wire — see
    // `the_headset_debounce_is_clocked_from_the_internal_oscillator`. Written
    // before detection is enabled, because a debounce with no clock is the
    // failure that looks exactly like an unwired jack.
    (3, page3::TIMER_CLOCK, 0x01),
    // Headset detection is off after reset, so without this the jack reads as
    // permanently empty. 128 ms debounce rather than the reset 16 ms: a plug
    // pushed in slowly makes and breaks the switch, and at 16 ms the flaps
    // land in different polls and cross the speaker between them.
    (0, page0::HEADSET_DETECT, 0x8C),
    // Analog, in the order SLAS671C §6.3.10.14 gives: route the DAC to the
    // output amplifier (d), unmute and set the gain of the output drivers
    // (e), and only then power the drivers up (f). Powering a driver first —
    // as this sequence did — brings it up against the reset state, no routing
    // and -78 dB, and then steps it to 0 dB once the gain arrives. The
    // speaker reproduces that step.
    (1, page1::OUTPUT_MIXER_ROUTING, 0x44),
    // Route each analog volume control to its driver at 0 dB. Without D7 the
    // mixer reaches no amplifier at all, and the reset gain is -78 dB.
    (1, page1::HPL_ANALOG_VOLUME, 0x80),
    (1, page1::HPR_ANALOG_VOLUME, 0x80),
    (1, page1::SPK_ANALOG_VOLUME, 0x80),
    (1, page1::HPL_DRIVER_GAIN, 0x06),
    (1, page1::HPR_DRIVER_GAIN, 0x06),
    // 6 dB, the lowest the class-D stage offers, and unmuted. 12 dB was
    // painfully loud on a bench with the box open; real content can raise it
    // deliberately rather than inheriting it.
    (1, page1::SPK_DRIVER_GAIN, SPK_GAIN_MUTED),
    // Pop removal, written rather than inherited. D7 set orders a software
    // power-down to take the amplifiers down before the DAC — Table 6-101's
    // own "this is to optimize power-down POP". D6-D3 (driver power-on time,
    // 304 ms) and D2-D1 (gain ramp step, 3.9 ms) keep their reset values, so
    // this changes the power-down and nothing about the power-up.
    (1, page1::HP_POP_REMOVAL, 0xBE),
    // The drivers last. D7 and D6 power the HPL and HPR drivers; D2 is
    // reserved and must be 1. Common mode stays at its 1.35 V reset value.
    (1, page1::HP_DRIVERS, 0xC4),
    // Configured but *not* powered: D7 clear. Powering the class-D amplifier
    // is, measured by ear, the loudest part of what the box used to do when it
    // started, and it is audible whether or not the driver in front of it is
    // muted. D6-D1 keep their reset value as the datasheet requires.
    (1, page1::SPK_AMP, SPK_AMP_DOWN),
];

/// Class-D driver, 6 dB, muted — D2 clear. The speaker spends the whole
/// start-up like this: unmuting it any earlier is, measured by ear on the
/// box, the click that start-up made.
pub const SPK_GAIN_MUTED: u8 = 0x00;
/// The same 6 dB, unmuted. Written last, once there is nothing left to expose.
pub const SPK_GAIN_UNMUTED: u8 = 0x04;
/// D0 of the class-D driver register: all programmed gains have been applied.
const SPK_GAINS_APPLIED: u8 = 0x01;
/// How long between reads while waiting for the class-D gains to apply.
pub const GAIN_POLL_MS: u32 = 5;
/// How many such reads before going on regardless.
pub const GAIN_POLLS: u32 = 40;

/// D7 of page 1 register 46: device software power down enabled.
const SOFTWARE_POWER_DOWN: u8 = 0x80;
/// How long between reads of the power flags while the codec shuts down.
pub const POWER_DOWN_POLL_MS: u32 = 20;
/// How many such reads before giving up. Twenty-five covers half a second,
/// comfortably past the ramp the drivers came up on.
pub const POWER_DOWN_POLLS: u32 = 25;

/// How long the output drivers are given to finish ramping.
///
/// SLAS671C Table 6-101 gives the reset driver power-on time as `0111`, which
/// is 304 ms, and a ramp-up step time of 3.9 ms. §6.3.10.14 step 4 says to
/// wait that out before the DAC is powered. Measured on the board: the HPL
/// driver reads as unpowered immediately after the analog block is written
/// and powered 400 ms later — the long-standing `hpl=false` was this ramp
/// being read through, not a driver declining to come up.
pub const DRIVER_RAMP_MS: u32 = 400;

/// Class-D amplifier configured and powered; D6-D1 are reserved and hold the
/// reset value the datasheet insists on.
pub const SPK_AMP_UP: u8 = 0x86;
/// The same, powered down.
pub const SPK_AMP_DOWN: u8 = 0x06;
/// DAC powered, both channels routed to their own side.
pub const DAC_UP: u8 = 0xD4;
/// The same, both channels powered down.
pub const DAC_DOWN: u8 = 0x14;
/// D3 and D2 mute the left and right DAC channels.
pub const DAC_MUTED: u8 = 0x0C;
pub const DAC_UNMUTED: u8 = 0x00;

/// Applied when there is something to play, never at start-up.
///
/// Powering the DAC accounts for most of what remains of the start-up click
/// once the amplifier is left alone, so it waits here with it.
pub const INIT_DAC: &[(u8, u8, u8)] = &[
    (0, page0::DAC_DATA_PATH, DAC_UP),
    (0, page0::DAC_MUTE_CTRL, DAC_UNMUTED),
];

// The volume register counts in half-decibel steps, two's complement, which
// is why the codes below are twice the decibel figures beside them.
const VOLUME_MIN_CODE: i16 = -127; // -63.5 dB
const VOLUME_MAX_CODE: i16 = 48; //  +24 dB
/// D6-D5 of the headset-detection register report what is plugged in: 00 for
/// nothing, 01 for a headset without a microphone, 11 for one with. Anything
/// non-zero is a jack, which is all this driver needs to know.
const HEADSET_DETECTED: u8 = 0x60;

#[cfg(test)]
mod tests {
    extern crate std;
    use std::{vec, vec::Vec};

    use super::*;
    use embedded_hal_mock::eh1::delay::{CheckedDelay, Transaction as DelayTransaction};
    use embedded_hal_mock::eh1::i2c::{Mock as I2cMock, Transaction};

    #[test]
    fn reset_writes_page_zero_then_the_reset_register() {
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::SOFTWARE_RESET, 0x01]),
        ];
        let i2c = I2cMock::new(&expected);
        let mut dac = Tlv320Dac3100::new(i2c, DEFAULT_ADDRESS);

        dac.reset().unwrap();
        dac.release().done();
    }

    #[test]
    fn the_page_register_is_not_rewritten_when_it_is_already_selected() {
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::SOFTWARE_RESET, 0x01]),
            // Second page-0 write must not repeat the page select.
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_MUTE_CTRL, 0x00]),
        ];
        let i2c = I2cMock::new(&expected);
        let mut dac = Tlv320Dac3100::new(i2c, DEFAULT_ADDRESS);

        dac.reset().unwrap();
        dac.set_muted(false).unwrap();
        dac.release().done();
    }

    /// Every byte `init` puts on the wire, written out by hand.
    ///
    /// This is the only artefact in the crate that a datasheet can be diffed
    /// against, so it must not be derived from `INIT_SEQUENCE` — a test that
    /// reads the same table as the code cannot disagree with it, and that is
    /// precisely how a shifted register map ships green. Changing the driver
    /// means changing these literals deliberately, in the same commit.
    #[test]
    fn init_puts_exactly_this_sequence_on_the_bus() {
        let w = |bytes: Vec<u8>| Transaction::write(DEFAULT_ADDRESS, bytes);
        let expected = [
            w(vec![0x00, 0x00]), // select page 0
            w(vec![0x04, 0x07]), // clock gen mux: PLL from BCLK
            w(vec![0x05, 0x91]), // PLL P/R: on, P=1, R=1
            w(vec![0x06, 0x40]), // PLL J=64
            w(vec![0x07, 0x00]), // PLL D, MSB
            w(vec![0x08, 0x00]), // PLL D, LSB
            w(vec![0x0B, 0x88]), // NDAC on, /8
            w(vec![0x0C, 0x82]), // MDAC on, /2
            w(vec![0x0D, 0x00]), // DOSR MSB
            w(vec![0x0E, 0x80]), // DOSR LSB = 128
            w(vec![0x1B, 0x00]), // interface: I2S, 16-bit, slave
            w(vec![0x3C, 0x08]), // DAC processing block
            w(vec![0x00, 0x03]), // select page 3
            w(vec![0x10, 0x01]), // debounce clocked from the internal oscillator
            w(vec![0x00, 0x00]), // back to page 0
            w(vec![0x43, 0x8C]), // headset detection on, 128 ms debounce
            w(vec![0x00, 0x01]), // select page 1
            w(vec![0x23, 0x44]), // DAC to output mixer routing
            w(vec![0x24, 0x80]), // left analog volume to HPL: routed, 0 dB
            w(vec![0x25, 0x80]), // right analog volume to HPR: routed, 0 dB
            w(vec![0x26, 0x80]), // left analog volume to speaker: routed, 0 dB
            w(vec![0x28, 0x06]), // HPL driver gain
            w(vec![0x29, 0x06]), // HPR driver gain
            w(vec![0x2A, 0x00]), // speaker driver gain: 6 dB, muted
            w(vec![0x21, 0xBE]), // pop removal: power down amps before the DAC
            w(vec![0x1F, 0xC4]), // headphone drivers: HPL and HPR powered up
            w(vec![0x20, 0x06]), // speaker amp configured but NOT powered
                                 // Nothing else: no DAC, no amplifier, no unmute. A box that is
                                 // not playing anything drives nothing.
        ];

        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        let mut delay = CheckedDelay::new(&[DelayTransaction::delay_ms(DRIVER_RAMP_MS)]);
        dac.init(&mut delay).unwrap();
        dac.release().done();
        delay.done();
    }

    /// SLAS671C Table 6-79 note (1): the headset-detection debounce is clocked
    /// from "the 1 MHz reference clock defined in Page 3 / Register 16", and
    /// Table 6-118 gives that register's reset value as D7 = 1 — external
    /// MCLK. This board wires no MCLK, only DIN, BCLK and WCLK, so the reset
    /// value leaves the debounce with no clock at all and detection can fail
    /// in a way indistinguishable from a jack that reaches no codec pin.
    ///
    /// `0x01` clears D7 to select the internal oscillator and leaves the
    /// divider field at its reset value: exactly one bit changes.
    #[test]
    fn the_headset_debounce_is_clocked_from_the_internal_oscillator() {
        let &(_, _, value) = INIT_ANALOG
            .iter()
            .find(|&&(p, r, _)| p == 3 && r == page3::TIMER_CLOCK)
            .expect("the debounce has no clock until page 3 register 16 is written");
        assert_eq!(
            value & 0x80,
            0x00,
            "D7 set asks for an MCLK this board does not wire"
        );
        assert_eq!(value, 0x01);
    }

    /// D7 enables detection and D4-D2 set the debounce, whose reset `000` is
    /// 16 ms. A jack is not pushed in cleanly — a slow plug makes and breaks
    /// the switch several times — and at 16 ms each of those can land in a
    /// different poll, which would cross the speaker and back between them.
    /// `011` is 128 ms, comfortably longer than a hand.
    #[test]
    fn detection_debounces_for_longer_than_a_slow_hand() {
        let &(_, _, value) = INIT_ANALOG
            .iter()
            .find(|&&(p, r, _)| p == 0 && r == page0::HEADSET_DETECT)
            .expect("the sequence must enable headset detection");
        assert_eq!(value & 0x80, 0x80, "D7 clear leaves detection off");
        assert_eq!(value & 0x1C, 0x0C, "D4-D2 = 011 is the 128 ms debounce");
        assert_eq!(value, 0x8C);
    }

    /// SLAS671C §6.3.10.14 orders the analog block deliberately: (d) route the
    /// DAC to the output amplifier, (e) unmute and set the gain of the output
    /// drivers, and only then (f) power up the output drivers. A driver
    /// powered before its routing exists comes up against the reset state —
    /// no routing and −78 dB — and is then slammed to 0 dB once the gain
    /// arrives, which is a step the speaker reproduces.
    /// SLAS671C §6.3.10.14 step 4: after powering the output drivers, "apply
    /// waiting time determined by the de-pop settings and the soft-stepping
    /// settings of the driver gain" — and only then power up the DAC.
    ///
    /// The wait is not optional padding. Table 6-101 gives the reset driver
    /// power-on time as 0111, 304 ms, so without it the DAC is powered and
    /// unmuted while the output drivers are still ramping, and the speaker
    /// hears the rest of that ramp. Measured on the board: the HPL driver
    /// reports itself unpowered when read straight after init and powered
    /// 400 ms later, which is the ramp this waits out.
    /// SLAS671C Table 6-101, page 1 / register 33 D7: with it set, a software
    /// power-down takes the DAC down "only after HP and SP amplifiers are
    /// completely powered down. This is to optimize power-down POP". Its reset
    /// value is 0, which powers everything down together — and this driver was
    /// inheriting that.
    /// Measured by ear on the box: muting the class-D driver for the whole of
    /// the start-up sequence is what stops it clicking. So the speaker comes
    /// up muted and is unmuted last, once the drivers have ramped and the DAC
    /// is running — everything the unmute would otherwise expose.
    /// Measured by ear, bisecting the start-up a register at a time: leaving
    /// the class-D amplifier unpowered is what silences it, leaving the DAC
    /// unpowered accounts for most of the rest, and the headphone drivers
    /// contribute nothing. Powering an output stage is audible whether or not
    /// the driver in front of it is muted, so the only cure is not to power
    /// it until there is something to play.
    #[test]
    fn the_start_up_leaves_the_output_stages_unpowered() {
        let &(_, _, amp) = INIT_ANALOG
            .iter()
            .find(|&&(p, r, _)| p == 1 && r == page1::SPK_AMP)
            .expect("the sequence must configure the speaker amplifier");
        assert_eq!(
            amp & 0x80,
            0x00,
            "D7 set powers the class-D amp, which clicks"
        );
        assert!(
            !INIT_ANALOG
                .iter()
                .any(|&(p, r, _)| p == 0 && r == page0::DAC_DATA_PATH),
            "the DAC must not be powered by the start-up sequence"
        );
    }

    /// Starting the output is the inverse, and its order is the datasheet's:
    /// the DAC comes up, then the amplifier, and the speaker is unmuted last
    /// — the one step already measured to be silent.
    #[test]
    fn starting_the_output_powers_the_dac_then_the_amplifier_then_unmutes() {
        let w = |bytes: Vec<u8>| Transaction::write(DEFAULT_ADDRESS, bytes);
        let expected = [
            w(vec![0x00, 0x00]), // page 0
            w(vec![0x3F, 0xD4]), // DAC on, both channels
            w(vec![0x40, 0x00]), // digital unmute
            w(vec![0x00, 0x01]), // page 1
            w(vec![0x20, 0x86]), // class-D amplifier powered
            w(vec![0x2A, 0x04]), // speaker unmuted, last
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x2A], vec![0x05]),
        ];

        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        let mut delay = CheckedDelay::new(&[]);
        assert_eq!(dac.start_output(&mut delay), Ok(()));
        dac.release().done();
        delay.done();
    }

    /// Stopping unwinds it: muted first, so nothing that follows is heard.
    #[test]
    fn stopping_the_output_mutes_before_it_unpowers_anything() {
        let w = |bytes: Vec<u8>| Transaction::write(DEFAULT_ADDRESS, bytes);
        let expected = [
            w(vec![0x00, 0x01]), // page 1
            w(vec![0x2A, 0x00]), // speaker muted first
            w(vec![0x20, 0x06]), // class-D amplifier down
            w(vec![0x00, 0x00]), // page 0
            w(vec![0x40, 0x0C]), // digital mute
            w(vec![0x3F, 0x14]), // DAC down
        ];

        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        assert_eq!(dac.stop_output(), Ok(()));
        dac.release().done();
    }

    #[test]
    fn the_speaker_comes_up_muted() {
        let &(_, _, value) = INIT_ANALOG
            .iter()
            .find(|&&(p, r, _)| p == 1 && r == page1::SPK_DRIVER_GAIN)
            .expect("the sequence must set the speaker driver");
        assert_eq!(
            value & 0x04,
            0x00,
            "D2 set unmutes the class-D driver during start-up, which clicks"
        );
    }

    /// SLAS671C Table 6-110: D0 of the class-D driver register reads back
    /// whether "all programmed gains to the Class-D driver have been applied".
    /// Unmuting is the last thing the sequence does, so the codec is only
    /// really up once that clears — waiting on the part rather than on a
    /// guessed delay.
    #[test]
    fn unmuting_the_speaker_waits_for_its_gains_to_be_applied() {
        let w = |bytes: Vec<u8>| Transaction::write(DEFAULT_ADDRESS, bytes);
        let r =
            |reg: u8, value: u8| Transaction::write_read(DEFAULT_ADDRESS, vec![reg], vec![value]);
        let expected = [
            w(vec![0x00, 0x01]), // page 1
            w(vec![0x2A, 0x04]), // 6 dB, unmuted
            r(0x2A, 0x04),       // D0 clear: gains not applied yet
            r(0x2A, 0x05),       // D0 set: applied
        ];

        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        let mut delay = CheckedDelay::new(&[DelayTransaction::delay_ms(GAIN_POLL_MS)]);

        assert_eq!(dac.unmute_speaker(&mut delay), Ok(()));
        dac.release().done();
        delay.done();
    }

    #[test]
    fn the_pop_removal_register_orders_the_power_down_after_the_amplifiers() {
        let &(_, _, value) = INIT_ANALOG
            .iter()
            .find(|&&(p, r, _)| p == 1 && r == page1::HP_POP_REMOVAL)
            .expect("the de-pop settings must be written rather than inherited");
        assert_eq!(
            value & 0x80,
            0x80,
            "D7 clear powers the DAC down alongside the amplifiers, which pops"
        );
    }

    /// The power-down is a request the codec services over time, so it is not
    /// finished when the write returns. Cutting the rail while the class-D
    /// stage is still powered is exactly the pop this exists to avoid, so the
    /// driver waits for the flags to go clear rather than for a guessed delay.
    #[test]
    fn powering_down_waits_for_the_output_stages_to_report_off() {
        let w = |bytes: Vec<u8>| Transaction::write(DEFAULT_ADDRESS, bytes);
        let r =
            |reg: u8, value: u8| Transaction::write_read(DEFAULT_ADDRESS, vec![reg], vec![value]);
        let expected = [
            w(vec![0x00, 0x01]), // page 1
            w(vec![0x2E, 0x80]), // device software power down enabled
            w(vec![0x00, 0x00]), // page 0 to read the flags
            r(0x25, 0x98),       // still powered: DACs and left class-D
            r(0x25, 0x00),       // everything off
        ];

        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        let mut delay = CheckedDelay::new(&[DelayTransaction::delay_ms(POWER_DOWN_POLL_MS)]);

        assert_eq!(dac.power_down(&mut delay), Ok(()));
        dac.release().done();
        delay.done();
    }

    /// A codec that never reports itself off must not stall a reboot forever.
    #[test]
    fn powering_down_gives_up_rather_than_waiting_for_ever() {
        let w = |bytes: Vec<u8>| Transaction::write(DEFAULT_ADDRESS, bytes);
        let mut expected = vec![
            w(vec![0x00, 0x01]),
            w(vec![0x2E, 0x80]),
            w(vec![0x00, 0x00]),
        ];
        for _ in 0..POWER_DOWN_POLLS {
            expected.push(Transaction::write_read(
                DEFAULT_ADDRESS,
                vec![0x25],
                vec![0x98],
            ));
        }

        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        let mut delay = CheckedDelay::new(&vec![
            DelayTransaction::delay_ms(POWER_DOWN_POLL_MS);
            POWER_DOWN_POLLS as usize
        ]);

        assert_eq!(dac.power_down(&mut delay), Err(Error::StillPowered));
        dac.release().done();
        delay.done();
    }

    #[test]
    fn the_dac_is_powered_only_after_the_drivers_have_finished_ramping() {
        let w = |bytes: Vec<u8>| Transaction::write(DEFAULT_ADDRESS, bytes);
        let mut dac = Tlv320Dac3100::new(
            I2cMock::new(&[
                w(vec![0x00, 0x01]),
                w(vec![0x20, 0x86]),
                w(vec![0x00, 0x00]),
                w(vec![0x3F, 0xD4]),
            ]),
            DEFAULT_ADDRESS,
        );
        let mut delay = CheckedDelay::new(&[DelayTransaction::delay_ms(DRIVER_RAMP_MS)]);

        dac.apply(
            &mut delay,
            &[(1, page1::SPK_AMP, 0x86)],
            &[(0, page0::DAC_DATA_PATH, 0xD4)],
        )
        .unwrap();

        dac.release().done();
        delay.done();
    }

    #[test]
    fn every_output_driver_is_powered_only_after_its_routing_and_gain() {
        let at = |page: u8, reg: u8| {
            INIT_ANALOG
                .iter()
                .position(|&(p, r, _)| p == page && r == reg)
                .unwrap_or_else(|| panic!("sequence must write page {page} register {reg:#04x}"))
        };

        let routing = at(1, page1::OUTPUT_MIXER_ROUTING);
        for (name, gain, driver) in [
            (
                "headphone",
                at(1, page1::HPL_DRIVER_GAIN),
                at(1, page1::HP_DRIVERS),
            ),
            (
                "speaker",
                at(1, page1::SPK_DRIVER_GAIN),
                at(1, page1::SPK_AMP),
            ),
        ] {
            assert!(
                routing < driver,
                "the {name} driver is powered before the DAC is routed to it"
            );
            assert!(
                gain < driver,
                "the {name} driver is powered before its gain is set"
            );
        }
    }

    #[test]
    fn the_sequence_unmutes_only_after_the_output_stages_are_powered() {
        // Now structural rather than positional: the amp is in the analog
        // table and the unmute in the one applied after the ramp, so the
        // ordering holds by construction.
        assert!(
            INIT_ANALOG
                .iter()
                .any(|&(p, r, _)| p == 1 && r == page1::SPK_AMP),
            "the speaker amp belongs to the analog block"
        );
        assert!(
            INIT_DAC.iter().any(|&(_, r, _)| r == page0::DAC_MUTE_CTRL),
            "unmuting must wait until after the drivers have ramped"
        );
        assert!(
            !INIT_ANALOG
                .iter()
                .any(|&(_, r, _)| r == page0::DAC_MUTE_CTRL),
            "unmuting before the amp is powered produces an audible pop"
        );
    }

    #[test]
    fn zero_decibels_writes_the_zero_code_to_both_channels() {
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_LEFT_VOLUME, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_RIGHT_VOLUME, 0x00]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        dac.set_volume_db(0).unwrap();
        dac.release().done();
    }

    #[test]
    fn negative_decibels_are_encoded_as_twos_complement_half_steps() {
        // The register is 0.5 dB per step, two's complement. -6 dB is -12 steps.
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_LEFT_VOLUME, 0xF4]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_RIGHT_VOLUME, 0xF4]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        dac.set_volume_db(-6).unwrap();
        dac.release().done();
    }

    #[test]
    fn volume_is_clamped_to_the_registers_range() {
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_LEFT_VOLUME, 0x81]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_RIGHT_VOLUME, 0x81]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        // -100 dB is below the -63.5 dB floor, so it must clamp to 0x81.
        dac.set_volume_db(-100).unwrap();
        dac.release().done();
    }

    #[test]
    fn muting_sets_both_mute_bits() {
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_MUTE_CTRL, 0x0C]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        dac.set_muted(true).unwrap();
        dac.release().done();
    }

    #[test]
    fn a_headset_without_a_microphone_counts_as_connected() {
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![page0::HEADSET_DETECT], vec![0x20]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        assert!(dac.headphones_connected().unwrap());
        dac.release().done();
    }

    #[test]
    fn invalidating_the_page_makes_the_next_access_select_it_again() {
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![page0::HEADSET_DETECT], vec![0x00]),
            // The caller has since driven the RESET line, or the board's power
            // gate cycled, so the codec is back on page 0 and the cache lies.
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![page0::HEADSET_DETECT], vec![0x20]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        dac.headphones_connected().unwrap();
        dac.invalidate_page();
        assert!(dac.headphones_connected().unwrap());
        dac.release().done();
    }

    #[test]
    fn headphone_detect_reports_absence_when_the_flag_is_clear() {
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![page0::HEADSET_DETECT], vec![0x00]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        assert!(!dac.headphones_connected().unwrap());
        dac.release().done();
    }

    /// The bit positions are the point of this function, so they are asserted
    /// against a literal register value rather than a mask expression.
    #[test]
    fn the_power_flags_decode_each_stage_separately() {
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x25], vec![0b1001_1000]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        assert_eq!(
            dac.power_flags().unwrap(),
            PowerFlags {
                left_dac: true,
                right_dac: true,
                hpl_driver: false,
                left_class_d: true,
                right_class_d: false,
            }
        );
        dac.release().done();
    }

    #[test]
    fn nothing_powered_reads_as_nothing_powered() {
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x25], vec![0x00]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        let flags = dac.power_flags().unwrap();
        assert!(!flags.left_dac && !flags.left_class_d);
        dac.release().done();
    }
}

/// Register overrides applied to a start-up sequence before it is written.
///
/// Which register value stops a box clicking on start-up is a question for the
/// ear, and a reflash between guesses makes that loop minutes long. This is
/// what lets one be typed at a console instead: the answer is a register, a
/// page and a byte, and the sequences in this module are what they land on.
///
/// Six slots, because the question being asked is "which one of these is it",
/// not "what would a whole different codec setup look like" — a table long
/// enough to hold a second start-up sequence would hide a mistake rather than
/// catch it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overrides {
    slots: [Option<(u8, u8, u8)>; OVERRIDE_SLOTS],
}

/// How many registers may be overridden at once.
pub const OVERRIDE_SLOTS: usize = 6;

impl Default for Overrides {
    fn default() -> Self {
        Self::new()
    }
}

impl Overrides {
    pub const fn new() -> Self {
        Self {
            slots: [None; OVERRIDE_SLOTS],
        }
    }

    /// Records an override, or answers `false` if there is no room.
    ///
    /// An override of a register already overridden replaces it. Keeping both
    /// would fill the table with one register's history and then refuse the
    /// next register — which is the opposite of what somebody stepping a value
    /// up and down is asking for.
    pub fn set(&mut self, page: u8, register: u8, value: u8) -> bool {
        if let Some(slot) = self
            .slots
            .iter_mut()
            .find(|slot| matches!(slot, Some((p, r, _)) if *p == page && *r == register))
        {
            *slot = Some((page, register, value));
            return true;
        }
        if let Some(slot) = self.slots.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some((page, register, value));
            return true;
        }
        false
    }

    pub fn clear(&mut self) {
        self.slots = [None; OVERRIDE_SLOTS];
    }

    /// Copies `table` into `out`, replacing the value of any register that has
    /// an override.
    ///
    /// An override naming a register the table does not contain does nothing.
    /// It is not an error: the sequence a value belongs to is the caller's
    /// business, and this is called once per sequence with the same table of
    /// overrides.
    ///
    /// # Panics
    ///
    /// If `out` is shorter than `table`. Both are compiled-in sequences at
    /// every call site, so a short buffer is a build-time mistake.
    pub fn apply<'a>(
        &self,
        table: &[(u8, u8, u8)],
        out: &'a mut [(u8, u8, u8)],
    ) -> &'a [(u8, u8, u8)] {
        out[..table.len()].copy_from_slice(table);
        for entry in out[..table.len()].iter_mut() {
            for (page, register, value) in self.slots.iter().flatten() {
                if entry.0 == *page && entry.1 == *register {
                    entry.2 = *value;
                }
            }
        }
        &out[..table.len()]
    }
}

#[cfg(test)]
mod override_tests {
    use super::*;

    const TABLE: &[(u8, u8, u8)] = &[(0, 0x3F, 0xD4), (1, 0x2A, 0x06), (0, 0x40, 0x0C)];

    #[test]
    fn an_override_replaces_the_value_for_its_register_only() {
        let mut overrides = Overrides::new();
        assert!(overrides.set(1, 0x2A, 0x86));

        let mut out = [(0, 0, 0); 8];
        assert_eq!(
            overrides.apply(TABLE, &mut out),
            &[(0, 0x3F, 0xD4), (1, 0x2A, 0x86), (0, 0x40, 0x0C)]
        );
    }

    /// The same register on a different page is a different register.
    #[test]
    fn an_override_on_another_page_leaves_the_table_alone() {
        let mut overrides = Overrides::new();
        assert!(overrides.set(0, 0x2A, 0x86));

        let mut out = [(0, 0, 0); 8];
        assert_eq!(overrides.apply(TABLE, &mut out), TABLE);
    }

    /// Stepping one value up and down must not consume the table.
    #[test]
    fn overriding_the_same_register_twice_takes_one_slot() {
        let mut overrides = Overrides::new();
        for value in 0..(OVERRIDE_SLOTS as u8 + 4) {
            assert!(overrides.set(1, 0x2A, value), "slot {value} refused");
        }

        let mut out = [(0, 0, 0); 8];
        assert_eq!(
            overrides.apply(TABLE, &mut out)[1].2,
            OVERRIDE_SLOTS as u8 + 3
        );
    }

    #[test]
    fn a_seventh_register_is_refused_rather_than_dropped_quietly() {
        let mut overrides = Overrides::new();
        for register in 0..OVERRIDE_SLOTS as u8 {
            assert!(overrides.set(0, register, 1));
        }
        assert!(!overrides.set(0, OVERRIDE_SLOTS as u8, 1));
    }

    #[test]
    fn clearing_puts_the_table_back() {
        let mut overrides = Overrides::new();
        overrides.set(1, 0x2A, 0x86);
        overrides.clear();

        let mut out = [(0, 0, 0); 8];
        assert_eq!(overrides.apply(TABLE, &mut out), TABLE);
    }

    /// An override for a register this sequence does not carry is not an
    /// error: `cset` is typed once and both sequences are run through it.
    #[test]
    fn an_override_for_an_absent_register_does_nothing() {
        let mut overrides = Overrides::new();
        overrides.set(9, 0x11, 0x22);

        let mut out = [(0, 0, 0); 8];
        assert_eq!(overrides.apply(TABLE, &mut out), TABLE);
    }
}
