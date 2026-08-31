#![no_std]

//! Driver for the TI TLV320DAC3100 stereo DAC with integrated class-D
//! speaker amplifier.

pub mod regs;

use embedded_hal::i2c::I2c;
use regs::{page0, page1, REG_PAGE_SELECT};

/// Default 7-bit address with ADDR tied low.
pub const DEFAULT_ADDRESS: u8 = 0x18;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    Bus(E),
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

    /// Applies `INIT_SEQUENCE`.
    pub fn init(&mut self) -> Result<(), Error<E>> {
        for &(page, reg, value) in INIT_SEQUENCE {
            self.write_reg(page, reg, value)?;
        }
        Ok(())
    }
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
pub const INIT_SEQUENCE: &[(u8, u8, u8)] = &[
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
    // Headset detection is off after reset, so without this the jack reads as
    // permanently empty. 16 ms debounce, the reset default.
    (0, page0::HEADSET_DETECT, 0x80),
    // Analog: power the output stages before unmuting.
    (1, page1::HP_DRIVERS, 0x04),
    (1, page1::OUTPUT_MIXER_ROUTING, 0x44),
    // Route each analog volume control to its driver at 0 dB. Without D7 the
    // mixer reaches no amplifier at all, and the reset gain is -78 dB.
    (1, page1::HPL_ANALOG_VOLUME, 0x80),
    (1, page1::HPR_ANALOG_VOLUME, 0x80),
    (1, page1::SPK_ANALOG_VOLUME, 0x80),
    (1, page1::HPL_DRIVER_GAIN, 0x06),
    (1, page1::HPR_DRIVER_GAIN, 0x06),
    (1, page1::SPK_DRIVER_GAIN, 0x0C),
    (1, page1::SPK_AMP, 0x86),
    // DAC on, both channels, then unmute.
    (0, page0::DAC_DATA_PATH, 0xD4),
    (0, page0::DAC_MUTE_CTRL, 0x00),
];

/// Volume register step size, in half-decibels.
const VOLUME_MIN_CODE: i16 = -127; // -63.5 dB
const VOLUME_MAX_CODE: i16 = 48; //  +24 dB
/// D6-D5 of the headset-detection register report what is plugged in: 00 for
/// nothing, 01 for a headset without a microphone, 11 for one with. Anything
/// non-zero is a jack, which is all this driver needs to know.
const HEADSET_DETECTED: u8 = 0x60;

impl<I2C, E> Tlv320Dac3100<I2C>
where
    I2C: I2c<Error = E>,
{
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

    /// True while a jack is inserted. The firmware mutes the speaker on this.
    ///
    /// Detection only reports anything once `INIT_SEQUENCE` has enabled it;
    /// the reset state of the register is disabled, reading a constant 00.
    pub fn headphones_connected(&mut self) -> Result<bool, Error<E>> {
        let v = self.read_reg(0, page0::HEADSET_DETECT)?;
        Ok(v & HEADSET_DETECTED != 0)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::{vec, vec::Vec};

    use super::*;
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
            w(vec![0x43, 0x80]), // headset detection on, 16 ms debounce
            w(vec![0x00, 0x01]), // select page 1
            w(vec![0x1F, 0x04]), // headphone drivers
            w(vec![0x23, 0x44]), // DAC to output mixer routing
            w(vec![0x24, 0x80]), // left analog volume to HPL: routed, 0 dB
            w(vec![0x25, 0x80]), // right analog volume to HPR: routed, 0 dB
            w(vec![0x26, 0x80]), // left analog volume to speaker: routed, 0 dB
            w(vec![0x28, 0x06]), // HPL driver gain
            w(vec![0x29, 0x06]), // HPR driver gain
            w(vec![0x2A, 0x0C]), // speaker driver gain
            w(vec![0x20, 0x86]), // speaker amp on
            w(vec![0x00, 0x00]), // back to page 0
            w(vec![0x3F, 0xD4]), // DAC data path: on, both channels
            w(vec![0x40, 0x00]), // unmute
        ];

        let mut dac = Tlv320Dac3100::new(I2cMock::new(&expected), DEFAULT_ADDRESS);
        dac.init().unwrap();
        dac.release().done();
    }

    #[test]
    fn the_sequence_unmutes_only_after_the_output_stages_are_powered() {
        let mute_at = INIT_SEQUENCE
            .iter()
            .position(|&(_, r, _)| r == page0::DAC_MUTE_CTRL)
            .expect("sequence must unmute");
        let amp_at = INIT_SEQUENCE
            .iter()
            .position(|&(p, r, _)| p == 1 && r == page1::SPK_AMP)
            .expect("sequence must power the speaker amp");
        assert!(
            amp_at < mute_at,
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
}
