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
    /// power now would be audible.
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
    /// Call this after anything that resets the codec without the driver
    /// knowing: the RESET line on GPIO26, or the board's power gate turning
    /// off. A stale page cache would silently write to the wrong page.
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
    /// muted, because powering them causes a click. `start_output` makes the
    /// codec audible.
    pub fn init<D: DelayNs>(&mut self, delay: &mut D) -> Result<(), Error<E>> {
        self.apply(delay, INIT_ANALOG, &[])
    }

    /// Writes `analog`, waits `DRIVER_RAMP_MS`, then writes `dac`.
    ///
    /// SLAS671C §6.3.10.14 requires this wait between powering the output
    /// drivers and powering the DAC; without it the DAC unmutes into drivers
    /// that are still ramping up.
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
    /// Separate from `init` because powering these stages clicks, so it only
    /// happens when there is audio to play. The speaker is unmuted last,
    /// which is silent.
    ///
    /// `speaker` decides whether the class-D amplifier is powered and
    /// unmuted. With headphones in, it stays off; [`Self::resume_speaker`]
    /// turns it on if the plug is pulled. The speaker is muted first, so the
    /// amplifier is never powered while unmuted.
    pub fn start_output<D: DelayNs>(
        &mut self,
        delay: &mut D,
        speaker: bool,
    ) -> Result<(), Error<E>> {
        self.mute_speaker()?;
        for &(page, reg, value) in INIT_DAC {
            self.write_reg(page, reg, value)?;
        }
        if !speaker {
            return Ok(());
        }
        self.write_reg(1, page1::SPK_AMP, SPK_AMP_UP)?;
        self.unmute_speaker(delay)
    }

    /// Brings the class-D stage back for a story that is already playing.
    ///
    /// Used when headphones are unplugged during a story. `start_output` does
    /// not power the amplifier when headphones are in, because powering it
    /// clicks even when muted. This moves that click to the moment the plug is
    /// pulled.
    ///
    /// Safe if the amplifier is already powered: writing `SPK_AMP_UP` again
    /// does not change anything audible.
    pub fn resume_speaker<D: DelayNs>(&mut self, delay: &mut D) -> Result<(), Error<E>> {
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
    /// The last step of starting output: unmuting earlier, while the drivers
    /// ramp and the DAC powers up, causes a click.
    ///
    /// SLAS671C Table 6-110 says D0 of the same register means "all
    /// programmed gains to the Class-D driver have been applied", so this
    /// waits for it. If it never comes, carry on anyway: playing is better
    /// than failing to start.
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
    /// Setting D7 of page 1 register 46 starts the codec's own power-down,
    /// which (with `HP_POP_REMOVAL` D7 set in `INIT_ANALOG`) turns off the
    /// amplifiers before the DAC, to reduce the power-down pop. The supply
    /// must stay on until it finishes, so this waits for the stages to
    /// report off.
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
    /// Writing a power-up bit is a request; this register shows the result.
    /// Useful when the output is silent although the configuration looks
    /// right.
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
    /// Reads the live status flag, not `HEADSET_DETECT`'s type field. The
    /// type field keeps showing the last headset type after the plug is
    /// removed. SLAS671C Table 6-63 gives D4 here as the current state.
    ///
    /// Only works once `INIT_ANALOG` has enabled detection in
    /// `HEADSET_DETECT`.
    pub fn headphones_connected(&mut self) -> Result<bool, Error<E>> {
        let v = self.read_reg(0, page0::INTERRUPT_FLAGS_DAC)?;
        Ok(v & HEADSET_INSERTED != 0)
    }

    /// The headset-detection register itself, unmasked.
    ///
    /// For debugging: D7 shows whether detection is enabled, which tells an
    /// empty socket apart from detection that was never turned on.
    pub fn headset_detect_raw(&mut self) -> Result<u8, Error<E>> {
        self.read_reg(0, page0::HEADSET_DETECT)
    }

    /// The live status register itself, unmasked.
    ///
    /// Printed next to `headset_detect_raw`. After a plug is pulled they
    /// differ: register 67 still shows the last headset, while D4 of
    /// register 46 is already 0.
    pub fn headset_status_raw(&mut self) -> Result<u8, Error<E>> {
        self.read_reg(0, page0::INTERRUPT_FLAGS_DAC)
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
/// Kept as data so it can be compared line by line with the datasheet.
/// Routes the DAC to both the speaker amplifier and the headphone drivers,
/// and clocks a 48 kHz DAC from BCLK alone: the board has no MCLK, only DIN,
/// BCLK and WCLK.
///
/// The clock chain:
///
/// - **BCLK = 1.536 MHz**, from 48 kHz x 16 bits x 2 channels. The I2S
///   peripheral must use 16-bit slots; with 32-bit slots BCLK doubles and `J`
///   must halve to 32.
/// - **PLL = 1.536 MHz x J=64 = 98.304 MHz**, with P=1, R=1 and D=0. SLAS671C
///   requires 80 MHz <= PLL_CLKIN x J.D x R/P <= 110 MHz and 4 <= R x J <= 259;
///   both hold.
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
    // The headset debounce needs a clock, chosen here. The default is an
    // external MCLK, which the board does not have; see
    // `the_headset_debounce_is_clocked_from_the_internal_oscillator`. Set
    // before detection is enabled.
    (3, page3::TIMER_CLOCK, 0x01),
    // Headset detection is off after reset. 128 ms debounce rather than the
    // default 16 ms: a plug pushed in slowly makes and breaks contact, and at
    // 16 ms the output would switch back and forth.
    (0, page0::HEADSET_DETECT, 0x8C),
    // Analog, in the order SLAS671C §6.3.10.14 gives: route the DAC to the
    // output amplifier (d), unmute and set the gain of the output drivers
    // (e), and only then power the drivers up (f). Powering a driver first
    // would start it with no routing at -78 dB and then jump to 0 dB, which
    // the speaker plays as a click.
    //
    // MICBIAS: the bias voltage headset detection measures against (SLAS671C
    // Figure 6-17), at 2 V, the lowest setting. By default it is off, and then
    // the MICDET pin floats: it keeps the charge from the last plug and
    // reports a headset in an empty socket.
    //
    // D3 is why the value is 0x09 rather than 0x01. With D3 clear, the bias
    // only turns on once a headset is detected, so for about 1.7 s after
    // start-up the box thought headphones were in. With D3 set, the bias is
    // always on and the first reading is valid. An empty socket draws no
    // current from it.
    //
    // Written here explicitly, so `cset 1 2e <v>` can change it from the
    // console.
    (1, page1::MICBIAS, 0x09),
    (1, page1::OUTPUT_MIXER_ROUTING, 0x44),
    // Route each analog volume control to its driver at 0 dB. Without D7 the
    // mixer reaches no amplifier at all, and the reset gain is -78 dB.
    (1, page1::HPL_ANALOG_VOLUME, 0x80),
    (1, page1::HPR_ANALOG_VOLUME, 0x80),
    (1, page1::SPK_ANALOG_VOLUME, 0x80),
    (1, page1::HPL_DRIVER_GAIN, 0x06),
    (1, page1::HPR_DRIVER_GAIN, 0x06),
    // 6 dB, the lowest the class-D stage offers, and muted until
    // `start_output`. 12 dB was painfully loud.
    (1, page1::SPK_DRIVER_GAIN, SPK_GAIN_MUTED),
    // Pop removal. D7 makes a software power-down turn off the amplifiers
    // before the DAC ("to optimize power-down POP", Table 6-101). D6-D3
    // (driver power-on time, 304 ms) and D2-D1 (gain ramp step, 3.9 ms) keep
    // their reset values, so power-up is unchanged.
    (1, page1::HP_POP_REMOVAL, 0xBE),
    // The drivers last. D7 and D6 power the HPL and HPR drivers; D2 is
    // reserved and must be 1. Common mode stays at its 1.35 V reset value.
    (1, page1::HP_DRIVERS, 0xC4),
    // Configured but *not* powered (D7 clear). Powering the class-D amplifier
    // causes the loudest click, even when muted. D6-D1 keep their reset
    // value, as the datasheet requires.
    (1, page1::SPK_AMP, SPK_AMP_DOWN),
];

/// Class-D driver, 6 dB, muted (D2 clear). The speaker stays muted through
/// start-up; unmuting earlier causes a click.
pub const SPK_GAIN_MUTED: u8 = 0x00;
/// The same 6 dB, unmuted. Written last.
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
/// How many such reads before giving up: half a second, well past the
/// drivers' ramp time.
pub const POWER_DOWN_POLLS: u32 = 25;

/// How long the output drivers are given to finish ramping.
///
/// SLAS671C Table 6-101 gives the default driver power-on time as `0111`,
/// 304 ms, with a ramp step of 3.9 ms. §6.3.10.14 step 4 says to wait for it
/// before powering the DAC. Measured: the HPL driver reads as off right after
/// the analog block is written, and on 400 ms later.
pub const DRIVER_RAMP_MS: u32 = 400;

/// Class-D amplifier configured and powered. D6-D1 are reserved and keep the
/// reset value the datasheet requires.
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

/// Applied when there is something to play, never at start-up, because
/// powering the DAC also clicks.
pub const INIT_DAC: &[(u8, u8, u8)] = &[
    (0, page0::DAC_DATA_PATH, DAC_UP),
    (0, page0::DAC_MUTE_CTRL, DAC_UNMUTED),
];

// The volume register counts in half-decibel steps, two's complement, which
// is why the codes below are twice the decibel figures beside them.
const VOLUME_MIN_CODE: i16 = -127; // -63.5 dB
const VOLUME_MAX_CODE: i16 = 48; //  +24 dB
/// D4 of `INTERRUPT_FLAGS_DAC`: a headphone plug is inserted.
pub const HEADSET_INSERTED: u8 = 0x10;

#[cfg(test)]
mod tests {
    extern crate std;
    use std::{vec, vec::Vec};

    use super::*;
    use embedded_hal_mock::eh1::delay::{CheckedDelay, Transaction as DelayTransaction};
    use embedded_hal_mock::eh1::i2c::{Mock as I2cMock, Transaction};

    #[test]
    fn reset_writes_page_zero_then_the_reset_register() {
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::SOFTWARE_RESET, 0x01]),
        ];
        let i2c = I2cMock::new(&expected);
        let mut dac = Tlv320Dac3100::new(i2c, DEFAULT_ADDRESS);

        // When
        dac.reset().unwrap();

        // Then: the bus saw exactly the expected transactions
        dac.release().done();
    }

    #[test]
    fn the_page_register_is_not_rewritten_when_it_is_already_selected() {
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::SOFTWARE_RESET, 0x01]),
            // Second page-0 write must not repeat the page select.
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_MUTE_CTRL, 0x00]),
        ];
        let i2c = I2cMock::new(&expected);
        let mut dac = Tlv320Dac3100::new(i2c, DEFAULT_ADDRESS);

        // When
        dac.reset().unwrap();
        dac.set_muted(false).unwrap();

        // Then: the bus saw exactly the expected transactions
        dac.release().done();
    }

    /// Every byte `init` puts on the wire, written out by hand.
    ///
    /// Not derived from `INIT_ANALOG`, so the test can disagree with the code
    /// and can be checked against the datasheet. Changing the driver means
    /// changing these literals too.
    #[test]
    fn init_puts_exactly_this_sequence_on_the_bus() {
        // Given
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
            w(vec![0x2E, 0x09]), // MICBIAS at 2 V, up even on an empty jack
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
        ];

        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        let mut delay = CheckedDelay::new(&[DelayTransaction::delay_ms(DRIVER_RAMP_MS)]);

        // When
        dac.init(&mut delay).unwrap();

        // Then: the bus saw exactly these transactions and nothing else: no
        // DAC, no amplifier, no unmute. A box that is not playing anything
        // drives nothing.
        dac.release().done();
        delay.done();
    }

    /// SLAS671C Table 6-79 note (1): the headset-detection debounce uses "the
    /// 1 MHz reference clock defined in Page 3 / Register 16", whose reset
    /// value (Table 6-118) selects external MCLK (D7 = 1). This board has no
    /// MCLK, so the debounce would have no clock and detection could fail.
    ///
    /// `0x01` clears D7 to select the internal oscillator; the divider keeps
    /// its reset value.
    #[test]
    fn the_headset_debounce_is_clocked_from_the_internal_oscillator() {
        // Given
        let sequence = INIT_ANALOG;

        // When
        let &(_, _, value) = sequence
            .iter()
            .find(|&&(p, r, _)| p == 3 && r == page3::TIMER_CLOCK)
            .expect("the debounce has no clock until page 3 register 16 is written");

        // Then
        assert_eq!(
            value & 0x80,
            0x00,
            "D7 set asks for an MCLK this board does not wire"
        );
        assert_eq!(value, 0x01);
    }

    /// D7 enables detection and D4-D2 set the debounce (reset `000` = 16 ms).
    /// A plug pushed in slowly makes and breaks contact several times; at
    /// 16 ms the output would switch back and forth. `011` is 128 ms.
    #[test]
    fn detection_debounces_for_longer_than_a_slow_hand() {
        // Given
        let sequence = INIT_ANALOG;

        // When
        let &(_, _, value) = sequence
            .iter()
            .find(|&&(p, r, _)| p == 0 && r == page0::HEADSET_DETECT)
            .expect("the sequence must enable headset detection");

        // Then
        assert_eq!(value & 0x80, 0x80, "D7 clear leaves detection off");
        assert_eq!(value & 0x1C, 0x0C, "D4-D2 = 011 is the 128 ms debounce");
        assert_eq!(value, 0x8C);
    }

    /// Found by ear, one register at a time: leaving the class-D amplifier
    /// unpowered at start-up removes most of the click, and leaving the DAC
    /// unpowered removes most of the rest. Powering them clicks even when
    /// muted, so they are only powered when there is something to play.
    #[test]
    fn the_start_up_leaves_the_output_stages_unpowered() {
        // Given
        let sequence = INIT_ANALOG;

        // When
        let &(_, _, amp) = sequence
            .iter()
            .find(|&&(p, r, _)| p == 1 && r == page1::SPK_AMP)
            .expect("the sequence must configure the speaker amplifier");
        let powers_the_dac = sequence
            .iter()
            .any(|&(p, r, _)| p == 0 && r == page0::DAC_DATA_PATH);

        // Then
        assert_eq!(
            amp & 0x80,
            0x00,
            "D7 set powers the class-D amp, which clicks"
        );
        assert!(
            !powers_the_dac,
            "the DAC must not be powered by the start-up sequence"
        );
    }

    /// The reverse of stopping, in the datasheet's order: the DAC, then the
    /// amplifier, and the speaker is unmuted last, which is silent.
    ///
    /// It also mutes *first*: unplugging headphones while nothing plays
    /// unmutes the speaker, and powering the amplifier behind an unmuted
    /// speaker clicks.
    #[test]
    fn starting_the_output_mutes_first_then_powers_the_dac_and_the_amplifier() {
        // Given
        let w = |bytes: Vec<u8>| Transaction::write(DEFAULT_ADDRESS, bytes);
        let expected = [
            w(vec![0x00, 0x01]), // page 1
            w(vec![0x2A, 0x00]), // speaker muted before anything is powered
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

        // When
        let result = dac.start_output(&mut delay, true);

        // Then
        assert_eq!(result, Ok(()));
        dac.release().done();
        delay.done();
    }

    /// With headphones in, the speaker amplifier stays unpowered, because
    /// powering it clicks even when muted. `resume_speaker` powers it when
    /// the plug is pulled (see the next test).
    ///
    /// The speaker is still muted first, whatever an earlier unplug left.
    #[test]
    fn starting_the_output_for_headphones_leaves_the_speaker_unpowered() {
        // Given
        let w = |bytes: Vec<u8>| Transaction::write(DEFAULT_ADDRESS, bytes);
        let expected = [
            w(vec![0x00, 0x01]), // page 1
            w(vec![0x2A, 0x00]), // speaker muted before anything is powered
            w(vec![0x00, 0x00]), // page 0
            w(vec![0x3F, 0xD4]), // DAC on, both channels
            w(vec![0x40, 0x00]), // digital unmute
        ];

        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        let mut delay = CheckedDelay::new(&[]);

        // When
        let result = dac.start_output(&mut delay, false);

        // Then: nothing more on the bus, no amplifier and no unmute
        assert_eq!(result, Ok(()));
        dac.release().done();
        delay.done();
    }

    /// When the plug is pulled during a story, the speaker must come back,
    /// and its amplifier may not be powered yet. Power first, then unmute,
    /// as in `start_output`: unmuting before the amplifier is up clicks.
    ///
    /// If the amplifier is already powered, writing `SPK_AMP_UP` again is
    /// silent.
    #[test]
    fn resuming_the_speaker_powers_the_amplifier_before_it_unmutes() {
        // Given
        let w = |bytes: Vec<u8>| Transaction::write(DEFAULT_ADDRESS, bytes);
        let expected = [
            w(vec![0x00, 0x01]), // page 1
            w(vec![0x20, 0x86]), // class-D amplifier powered
            w(vec![0x2A, 0x04]), // and only then unmuted
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x2A], vec![0x05]),
        ];

        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        let mut delay = CheckedDelay::new(&[]);

        // When
        let result = dac.resume_speaker(&mut delay);

        // Then
        assert_eq!(result, Ok(()));
        dac.release().done();
        delay.done();
    }

    /// Stopping mutes first, so nothing after it is heard.
    #[test]
    fn stopping_the_output_mutes_before_it_unpowers_anything() {
        // Given
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

        // When
        let result = dac.stop_output();

        // Then
        assert_eq!(result, Ok(()));
        dac.release().done();
    }

    #[test]
    fn the_speaker_comes_up_muted() {
        // Given
        let sequence = INIT_ANALOG;

        // When
        let &(_, _, value) = sequence
            .iter()
            .find(|&&(p, r, _)| p == 1 && r == page1::SPK_DRIVER_GAIN)
            .expect("the sequence must set the speaker driver");

        // Then
        assert_eq!(
            value & 0x04,
            0x00,
            "D2 set unmutes the class-D driver during start-up, which clicks"
        );
    }

    /// SLAS671C Table 6-110: D0 of the class-D driver register shows whether
    /// "all programmed gains to the Class-D driver have been applied". The
    /// driver waits for it instead of guessing a delay.
    #[test]
    fn unmuting_the_speaker_waits_for_its_gains_to_be_applied() {
        // Given
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

        // When
        let result = dac.unmute_speaker(&mut delay);

        // Then
        assert_eq!(result, Ok(()));
        dac.release().done();
        delay.done();
    }

    /// Headset detection measures the MICDET pin against MICBIAS (SLAS671C
    /// Figure 6-17), set in page 1 register 46. It must be in the sequence so
    /// `cset` can override it from the console.
    #[test]
    fn the_bias_the_detector_senses_against_is_part_of_the_sequence() {
        // Given
        let sequence = INIT_ANALOG;

        // When
        let writes_micbias = sequence
            .iter()
            .any(|&(p, r, _)| p == 1 && r == page1::MICBIAS);

        // Then
        assert!(
            writes_micbias,
            "an override for a register the sequence never writes does nothing"
        );
    }

    #[test]
    fn the_pop_removal_register_orders_the_power_down_after_the_amplifiers() {
        // Given
        let sequence = INIT_ANALOG;

        // When
        let &(_, _, value) = sequence
            .iter()
            .find(|&&(p, r, _)| p == 1 && r == page1::HP_POP_REMOVAL)
            .expect("the de-pop settings must be written rather than inherited");

        // Then
        assert_eq!(
            value & 0x80,
            0x80,
            "D7 clear powers the DAC down alongside the amplifiers, which pops"
        );
    }

    /// The power-down takes time. Cutting power while the class-D stage is
    /// still on would pop, so the driver waits for the flags to clear.
    #[test]
    fn powering_down_waits_for_the_output_stages_to_report_off() {
        // Given
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

        // When
        let result = dac.power_down(&mut delay);

        // Then
        assert_eq!(result, Ok(()));
        dac.release().done();
        delay.done();
    }

    /// A codec that never reports itself off must not stall a reboot forever.
    #[test]
    fn powering_down_gives_up_rather_than_waiting_for_ever() {
        // Given
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

        // When
        let result = dac.power_down(&mut delay);

        // Then
        assert_eq!(result, Err(Error::StillPowered));
        dac.release().done();
        delay.done();
    }

    #[test]
    fn the_dac_is_powered_only_after_the_drivers_have_finished_ramping() {
        // Given
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

        // When
        dac.apply(
            &mut delay,
            &[(1, page1::SPK_AMP, 0x86)],
            &[(0, page0::DAC_DATA_PATH, 0xD4)],
        )
        .unwrap();

        // Then: the bus saw exactly the expected transactions
        dac.release().done();
        delay.done();
    }

    #[test]
    fn every_output_driver_is_powered_only_after_its_routing_and_gain() {
        // Given
        let at = |page: u8, reg: u8| {
            INIT_ANALOG
                .iter()
                .position(|&(p, r, _)| p == page && r == reg)
                .unwrap_or_else(|| panic!("sequence must write page {page} register {reg:#04x}"))
        };

        // When
        let routing = at(1, page1::OUTPUT_MIXER_ROUTING);
        let drivers = [
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
        ];

        // Then
        for (name, gain, driver) in drivers {
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
        // Given: the amplifier is in the analog table and the unmute in the
        // table applied after the ramp, so the order is guaranteed
        let (analog, after_the_ramp) = (INIT_ANALOG, INIT_DAC);

        // When
        let amp_in_analog = analog
            .iter()
            .any(|&(p, r, _)| p == 1 && r == page1::SPK_AMP);
        let unmute_after_ramp = after_the_ramp
            .iter()
            .any(|&(_, r, _)| r == page0::DAC_MUTE_CTRL);
        let unmute_in_analog = analog.iter().any(|&(_, r, _)| r == page0::DAC_MUTE_CTRL);

        // Then
        assert!(amp_in_analog, "the speaker amp belongs to the analog block");
        assert!(
            unmute_after_ramp,
            "unmuting must wait until after the drivers have ramped"
        );
        assert!(
            !unmute_in_analog,
            "unmuting before the amp is powered produces an audible pop"
        );
    }

    #[test]
    fn zero_decibels_writes_the_zero_code_to_both_channels() {
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_LEFT_VOLUME, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_RIGHT_VOLUME, 0x00]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        // When
        dac.set_volume_db(0).unwrap();

        // Then: the bus saw exactly the expected transactions
        dac.release().done();
    }

    #[test]
    fn negative_decibels_are_encoded_as_twos_complement_half_steps() {
        // Given
        // The register is 0.5 dB per step, two's complement. -6 dB is -12 steps.
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_LEFT_VOLUME, 0xF4]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_RIGHT_VOLUME, 0xF4]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        // When
        dac.set_volume_db(-6).unwrap();

        // Then: the bus saw exactly the expected transactions
        dac.release().done();
    }

    #[test]
    fn volume_is_clamped_to_the_registers_range() {
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_LEFT_VOLUME, 0x81]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_RIGHT_VOLUME, 0x81]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        // When: -100 dB is below the -63.5 dB floor, so it must clamp to 0x81
        dac.set_volume_db(-100).unwrap();

        // Then: the bus saw exactly the expected transactions
        dac.release().done();
    }

    #[test]
    fn muting_sets_both_mute_bits() {
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![page0::DAC_MUTE_CTRL, 0x0C]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        // When
        dac.set_muted(true).unwrap();

        // Then: the bus saw exactly the expected transactions
        dac.release().done();
    }

    #[test]
    fn an_inserted_jack_reads_as_connected() {
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x2E], vec![0x10]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        // When
        let connected = dac.headphones_connected().unwrap();

        // Then
        assert!(connected);
        dac.release().done();
    }

    /// Register 67's type field keeps showing the last headset after the plug
    /// is removed (measured on the box). Register 46's live status bit
    /// (SLAS671C Table 6-63) shows whether one is in now. Both are printed
    /// for debugging.
    #[test]
    fn the_live_status_register_is_reported_byte_for_byte() {
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![0x00, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x2E], vec![0x10]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        // When
        let result = dac.headset_status_raw();

        // Then
        assert_eq!(result, Ok(0x10));
        dac.release().done();
    }

    #[test]
    fn a_pulled_plug_reads_as_absent() {
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x2E], vec![0x00]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        // When
        let connected = dac.headphones_connected().unwrap();

        // Then
        assert!(!connected);
        dac.release().done();
    }

    #[test]
    fn invalidating_the_page_makes_the_next_access_select_it_again() {
        // Given: page 0 selected and cached, then the codec reset behind the
        // driver's back, so it is back on page 0 and the cache is wrong
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x2E], vec![0x00]),
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x2E], vec![0x10]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        dac.headphones_connected().unwrap();

        // When
        dac.invalidate_page();
        let connected = dac.headphones_connected().unwrap();

        // Then
        assert!(connected);
        dac.release().done();
    }

    /// The raw byte, including the enable bit, tells an empty socket apart
    /// from detection that was never turned on.
    #[test]
    fn the_raw_headset_register_is_reported_byte_for_byte() {
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![0x00, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x43], vec![0x8C]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        // When
        let result = dac.headset_detect_raw();

        // Then
        assert_eq!(result, Ok(0x8C));
        dac.release().done();
    }

    /// Checked against a literal register value, not a mask expression.
    #[test]
    fn the_power_flags_decode_each_stage_separately() {
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x25], vec![0b1001_1000]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        // When
        let flags = dac.power_flags().unwrap();

        // Then
        assert_eq!(
            flags,
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
        // Given
        let expected = [
            Transaction::write(DEFAULT_ADDRESS, vec![REG_PAGE_SELECT, 0x00]),
            Transaction::write_read(DEFAULT_ADDRESS, vec![0x25], vec![0x00]),
        ];
        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);

        // When
        let flags = dac.power_flags().unwrap();

        // Then
        assert!(!flags.left_dac && !flags.left_class_d);
        dac.release().done();
    }
}

/// Register overrides applied to a start-up sequence before it is written.
///
/// Lets register values be changed from the console, to find by ear which
/// ones cause a click, without reflashing.
///
/// Six slots: enough to try a few registers, not to replace the whole
/// sequence.
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
    /// Overriding the same register again replaces the old value instead of
    /// using another slot.
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
    /// An override for a register not in the table does nothing. This is
    /// called once per sequence with the same overrides.
    ///
    /// # Panics
    ///
    /// If `out` is shorter than `table`. Both are fixed at compile time, so
    /// this is a programming mistake.
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
    extern crate std;
    use std::vec::Vec;

    use super::*;

    const TABLE: &[(u8, u8, u8)] = &[(0, 0x3F, 0xD4), (1, 0x2A, 0x06), (0, 0x40, 0x0C)];

    #[test]
    fn an_override_replaces_the_value_for_its_register_only() {
        // Given
        let mut overrides = Overrides::new();
        assert!(overrides.set(1, 0x2A, 0x86));

        // When
        let mut out = [(0, 0, 0); 8];
        let applied = overrides.apply(TABLE, &mut out);

        // Then
        assert_eq!(
            applied,
            &[(0, 0x3F, 0xD4), (1, 0x2A, 0x86), (0, 0x40, 0x0C)]
        );
    }

    /// The same register on a different page is a different register.
    #[test]
    fn an_override_on_another_page_leaves_the_table_alone() {
        // Given
        let mut overrides = Overrides::new();
        assert!(overrides.set(0, 0x2A, 0x86));

        // When
        let mut out = [(0, 0, 0); 8];
        let applied = overrides.apply(TABLE, &mut out);

        // Then
        assert_eq!(applied, TABLE);
    }

    /// Stepping one value up and down must not consume the table.
    #[test]
    fn overriding_the_same_register_twice_takes_one_slot() {
        // Given
        let mut overrides = Overrides::new();

        // When: one register stepped through more values than there are slots
        let taken: Vec<bool> = (0..OVERRIDE_SLOTS as u8 + 4)
            .map(|value| overrides.set(1, 0x2A, value))
            .collect();

        // Then: every value was taken, and the last one applies
        assert!(taken.iter().all(|&set| set), "{taken:?}");
        let mut out = [(0, 0, 0); 8];
        assert_eq!(
            overrides.apply(TABLE, &mut out)[1].2,
            OVERRIDE_SLOTS as u8 + 3
        );
    }

    #[test]
    fn a_seventh_register_is_refused_rather_than_dropped_quietly() {
        // Given: every slot taken
        let mut overrides = Overrides::new();
        for register in 0..OVERRIDE_SLOTS as u8 {
            assert!(overrides.set(0, register, 1));
        }

        // When
        let taken = overrides.set(0, OVERRIDE_SLOTS as u8, 1);

        // Then
        assert!(!taken);
    }

    #[test]
    fn clearing_puts_the_table_back() {
        // Given
        let mut overrides = Overrides::new();
        overrides.set(1, 0x2A, 0x86);

        // When
        overrides.clear();

        // Then
        let mut out = [(0, 0, 0); 8];
        assert_eq!(overrides.apply(TABLE, &mut out), TABLE);
    }

    /// An override for a register not in this sequence is not an error:
    /// the same overrides are applied to both sequences.
    #[test]
    fn an_override_for_an_absent_register_does_nothing() {
        // Given
        let mut overrides = Overrides::new();
        overrides.set(9, 0x11, 0x22);

        // When
        let mut out = [(0, 0, 0); 8];
        let applied = overrides.apply(TABLE, &mut out);

        // Then
        assert_eq!(applied, TABLE);
    }
}
