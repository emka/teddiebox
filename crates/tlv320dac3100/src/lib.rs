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
/// Configures the PLL for a 48 kHz DAC from a 12 MHz BCLK, 16-bit I2S,
/// and routes the DAC to both the speaker amplifier and the headphone drivers.
pub const INIT_SEQUENCE: &[(u8, u8, u8)] = &[
    // Clocking: PLL from BCLK, CODEC_CLKIN from PLL.
    (0, page0::CLOCK_GEN_MUX, 0x07),
    (0, page0::PLL_P_R, 0x91), // PLL on, P=1, R=1
    (0, page0::PLL_J, 0x20),   // J=32
    (0, page0::PLL_D_MSB, 0x00),
    (0, page0::PLL_D_LSB, 0x00),
    (0, page0::NDAC, 0x88), // NDAC on, divide by 8
    (0, page0::MDAC, 0x82), // MDAC on, divide by 2
    (0, page0::DOSR_MSB, 0x00),
    (0, page0::DOSR_LSB, 0x80), // DOSR = 128
    // Interface: I2S, 16-bit, slave.
    (0, page0::CODEC_IF_CTRL1, 0x00),
    (0, page0::DAC_PROCESSING_BLOCK, 0x08),
    // Analog: power the output stages before unmuting.
    (1, page1::HP_DRIVERS, 0x04),
    (1, page1::HP_OUT_ROUTING, 0x44),
    (1, page1::SPK_OUT_ROUTING, 0x40),
    (1, page1::HPL_DRIVER_GAIN, 0x06),
    (1, page1::HPR_DRIVER_GAIN, 0x06),
    (1, page1::SPK_DRIVER_GAIN, 0x0C),
    (1, page1::SPK_AMP, 0x86),
    // DAC on, both channels, then unmute.
    (0, page0::DAC_DATA_PATH, 0xD4),
    (0, page0::DAC_MUTE_CTRL, 0x00),
];

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
        dac.write_reg(0, page0::DAC_MUTE_CTRL, 0x00).unwrap();
        dac.release().done();
    }

    #[test]
    fn init_writes_every_entry_of_the_sequence_in_order() {
        let mut expected: Vec<Transaction> = vec![];
        // Reset leaves page 0 selected, which is the state init starts from.
        let mut page = Some(0);
        for &(p, reg, val) in INIT_SEQUENCE {
            if page != Some(p) {
                expected.push(Transaction::write(
                    DEFAULT_ADDRESS,
                    vec![REG_PAGE_SELECT, p],
                ));
                page = Some(p);
            }
            expected.push(Transaction::write(DEFAULT_ADDRESS, vec![reg, val]));
        }

        let i2c = I2cMock::new(&expected);
        let mut dac = Tlv320Dac3100::new(i2c, DEFAULT_ADDRESS);
        // Start from a known page so the expectation above matches.
        dac.page = Some(0);

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
}
