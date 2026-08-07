#![no_std]

//! Driver for the TI TRF7962A 13.56 MHz reader, restricted to the ISO 15693
//! operations the Toniebox needs.

pub mod regs;

use embedded_hal::spi::SpiDevice;

/// Command word bits. Getting these wrong turns a register read into a direct
/// command, so the encoding lives here and nowhere else.
const CMD_BIT: u8 = 0x80;
const READ_BIT: u8 = 0x40;
const ADDRESS_MASK: u8 = 0x1F;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    Bus(E),
    /// The reader did not respond within the expected window.
    Timeout,
    /// A tag responded, but the response was not the expected shape.
    BadResponse,
}

pub struct Trf7962a<SPI> {
    spi: SPI,
}

impl<SPI, E> Trf7962a<SPI>
where
    SPI: SpiDevice<Error = E>,
{
    pub fn new(spi: SPI) -> Self {
        Self { spi }
    }

    pub fn release(self) -> SPI {
        self.spi
    }

    /// Writes one register.
    pub fn write_register(&mut self, reg: u8, value: u8) -> Result<(), Error<E>> {
        self.spi
            .write(&[reg & ADDRESS_MASK, value])
            .map_err(Error::Bus)
    }

    /// Reads one register.
    pub fn read_register(&mut self, reg: u8) -> Result<u8, Error<E>> {
        let mut buf = [0u8; 1];
        self.spi
            .transfer(&mut buf, &[(reg & ADDRESS_MASK) | READ_BIT])
            .map_err(Error::Bus)?;
        Ok(buf[0])
    }

    /// Issues a direct command.
    pub fn send_command(&mut self, command: u8) -> Result<(), Error<E>> {
        self.spi
            .write(&[CMD_BIT | (command & ADDRESS_MASK)])
            .map_err(Error::Bus)
    }

    /// Brings the reader up for ISO 15693 and turns the field on.
    pub fn init_iso15693(&mut self) -> Result<(), Error<E>> {
        self.send_command(regs::cmd::SOFT_INIT)?;
        self.send_command(regs::cmd::IDLE)?;
        for &(reg, value) in INIT_SEQUENCE {
            self.write_register(reg, value)?;
        }
        Ok(())
    }
}

/// Reader configuration applied at startup, as `(register, value)`.
pub const INIT_SEQUENCE: &[(u8, u8)] = &[
    (regs::ISO_CONTROL, regs::ISO_CONTROL_15693_HIGH),
    (regs::TX_PULSE_LENGTH, 0x80),
    (regs::RX_NO_RESPONSE_WAIT, 0x14),
    (regs::RX_SPECIAL_SETTINGS, 0x40),
    // The field goes on last, once the protocol is configured.
    (regs::CHIP_STATUS_CONTROL, regs::CHIP_STATUS_RF_ON),
];

#[cfg(test)]
mod tests {
    extern crate std;
    use std::{vec, vec::Vec};

    use super::*;
    use embedded_hal_mock::eh1::spi::{Mock as SpiMock, Transaction};

    #[test]
    fn a_register_write_sends_the_bare_address() {
        let expected = [
            Transaction::transaction_start(),
            Transaction::write_vec(vec![regs::ISO_CONTROL, 0x02]),
            Transaction::transaction_end(),
        ];
        let mut r = Trf7962a::new(SpiMock::new(&expected));
        r.write_register(regs::ISO_CONTROL, 0x02).unwrap();
        r.release().done();
    }

    #[test]
    fn a_register_read_sets_the_read_bit() {
        let expected = [
            Transaction::transaction_start(),
            Transaction::transfer(vec![regs::ISO_CONTROL | 0x40], vec![0x02]),
            Transaction::transaction_end(),
        ];
        let mut r = Trf7962a::new(SpiMock::new(&expected));
        assert_eq!(r.read_register(regs::ISO_CONTROL).unwrap(), 0x02);
        r.release().done();
    }

    #[test]
    fn a_direct_command_sets_the_command_bit() {
        let expected = [
            Transaction::transaction_start(),
            Transaction::write_vec(vec![0x80 | regs::cmd::SOFT_INIT]),
            Transaction::transaction_end(),
        ];
        let mut r = Trf7962a::new(SpiMock::new(&expected));
        r.send_command(regs::cmd::SOFT_INIT).unwrap();
        r.release().done();
    }

    #[test]
    fn initialisation_soft_resets_then_applies_the_sequence() {
        let mut expected: Vec<Transaction<u8>> = vec![
            Transaction::transaction_start(),
            Transaction::write_vec(vec![0x80 | regs::cmd::SOFT_INIT]),
            Transaction::transaction_end(),
            Transaction::transaction_start(),
            Transaction::write_vec(vec![0x80 | regs::cmd::IDLE]),
            Transaction::transaction_end(),
        ];
        for &(reg, val) in INIT_SEQUENCE {
            expected.push(Transaction::transaction_start());
            expected.push(Transaction::write_vec(vec![reg, val]));
            expected.push(Transaction::transaction_end());
        }

        let mut r = Trf7962a::new(SpiMock::new(&expected));
        r.init_iso15693().unwrap();
        r.release().done();
    }

    #[test]
    fn the_field_is_turned_on_last() {
        let last = INIT_SEQUENCE.last().unwrap();
        assert_eq!(
            last.0,
            regs::CHIP_STATUS_CONTROL,
            "enabling the field before the protocol is configured radiates noise"
        );
    }
}
