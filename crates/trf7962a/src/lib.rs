#![no_std]

//! Driver for the TI TRF7962A 13.56 MHz reader, restricted to the ISO 15693
//! operations the Toniebox needs.

pub mod regs;
pub mod slix;

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

/// Longest ISO 15693 response this driver handles.
pub const MAX_RESPONSE: usize = 32;

impl<SPI, E> Trf7962a<SPI>
where
    SPI: SpiDevice<Error = E>,
{
    /// Sends `request` and collects the tag's reply. Returns the number of
    /// bytes received, which is zero when no tag answered.
    pub fn transceive(&mut self, request: &[u8], response: &mut [u8]) -> Result<usize, Error<E>> {
        self.send_command(regs::cmd::RESET_FIFO)?;

        // The length is split across two registers as a 12-bit field.
        self.write_register(regs::TX_LENGTH_BYTE1, (request.len() >> 4) as u8)?;
        self.write_register(regs::TX_LENGTH_BYTE2, ((request.len() << 4) & 0xF0) as u8)?;

        for &b in request {
            self.write_register(regs::FIFO, b)?;
        }
        self.send_command(regs::cmd::TRANSMIT_WITH_CRC)?;

        let available = self.read_register(regs::FIFO_STATUS)? as usize;
        let n = available.min(response.len());
        for slot in response.iter_mut().take(n) {
            *slot = self.read_register(regs::FIFO)?;
        }
        Ok(n)
    }

    /// Runs a single-slot inventory. `Ok(None)` means nothing answered, which
    /// includes the case of a tag held in privacy mode.
    pub fn inventory(&mut self) -> Result<Option<[u8; 8]>, Error<E>> {
        const REQUEST: [u8; 3] = [0x26, 0x01, 0x00];
        let mut buf = [0u8; MAX_RESPONSE];
        let n = self.transceive(&REQUEST, &mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        // flags(1) + DSFID(1) + UID(8)
        if n < 10 {
            return Err(Error::BadResponse);
        }
        let mut uid = [0u8; 8];
        // The tag sends the UID least-significant byte first.
        for (i, slot) in uid.iter_mut().enumerate() {
            *slot = buf[9 - i];
        }
        Ok(Some(uid))
    }
}

impl<SPI, E> Trf7962a<SPI>
where
    SPI: SpiDevice<Error = E>,
{
    /// Fetches the tag's random number, needed to mask the password.
    pub fn get_random_number(&mut self) -> Result<u16, Error<E>> {
        let mut buf = [0u8; MAX_RESPONSE];
        let n = self.transceive(&slix::get_random_number_request(), &mut buf)?;
        if n == 0 {
            return Err(Error::Timeout);
        }
        // flags(1) + RN low + RN high
        if n < 3 {
            return Err(Error::BadResponse);
        }
        Ok(u16::from_le_bytes([buf[1], buf[2]]))
    }

    /// Sends the privacy password, masked with `random`.
    pub fn set_password(&mut self, password: u32, random: u16) -> Result<(), Error<E>> {
        let mut buf = [0u8; MAX_RESPONSE];
        let n = self.transceive(&slix::set_password_request(password, random), &mut buf)?;
        if n == 0 {
            return Err(Error::Timeout);
        }
        Ok(())
    }

    /// Takes a tag out of privacy mode so it will answer inventory.
    pub fn unlock_privacy(&mut self, password: u32) -> Result<(), Error<E>> {
        let random = self.get_random_number()?;
        self.set_password(password, random)
    }

    /// Unlocks, then inventories. This is the operation the firmware calls;
    /// a locked tag is invisible to `inventory` alone.
    pub fn inventory_unlocked(&mut self, password: u32) -> Result<Option<[u8; 8]>, Error<E>> {
        // A tag already out of privacy mode answers inventory directly, so try
        // that first and only pay for the unlock exchange when it is needed.
        if let Some(uid) = self.inventory()? {
            return Ok(Some(uid));
        }
        match self.unlock_privacy(password) {
            Ok(()) => self.inventory(),
            // Nothing on the plate at all.
            Err(Error::Timeout) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

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

    /// Builds the SPI transactions for a transceive of `request` that returns
    /// `response`, so tests describe intent rather than bus mechanics.
    fn transceive_transactions(request: &[u8], response: &[u8]) -> Vec<Transaction<u8>> {
        let mut t = vec![
            Transaction::transaction_start(),
            Transaction::write_vec(vec![0x80 | regs::cmd::RESET_FIFO]),
            Transaction::transaction_end(),
            Transaction::transaction_start(),
            Transaction::write_vec(vec![regs::TX_LENGTH_BYTE1, (request.len() >> 4) as u8]),
            Transaction::transaction_end(),
            Transaction::transaction_start(),
            Transaction::write_vec(vec![
                regs::TX_LENGTH_BYTE2,
                ((request.len() << 4) & 0xF0) as u8,
            ]),
            Transaction::transaction_end(),
        ];
        for &b in request {
            t.push(Transaction::transaction_start());
            t.push(Transaction::write_vec(vec![regs::FIFO, b]));
            t.push(Transaction::transaction_end());
        }
        t.push(Transaction::transaction_start());
        t.push(Transaction::write_vec(vec![
            0x80 | regs::cmd::TRANSMIT_WITH_CRC,
        ]));
        t.push(Transaction::transaction_end());
        t.push(Transaction::transaction_start());
        t.push(Transaction::transfer(
            vec![regs::FIFO_STATUS | 0x40],
            vec![response.len() as u8],
        ));
        t.push(Transaction::transaction_end());
        for &b in response {
            t.push(Transaction::transaction_start());
            t.push(Transaction::transfer(vec![regs::FIFO | 0x40], vec![b]));
            t.push(Transaction::transaction_end());
        }
        t
    }

    #[test]
    fn inventory_returns_the_uid_a_tag_reported() {
        // Flags 0x26 = high data rate, inventory, one slot. Command 0x01,
        // mask length 0x00.
        let request = [0x26u8, 0x01, 0x00];
        // Response: flags, DSFID, then the UID least-significant byte first.
        let response = [0x00u8, 0x00, 0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01];
        let expected = transceive_transactions(&request, &response);

        let mut r = Trf7962a::new(SpiMock::new(&expected));
        let uid = r.inventory().unwrap().expect("a tag answered");
        // Reported most-significant byte first, the order printed on a figure.
        assert_eq!(uid, [1, 2, 3, 4, 5, 6, 7, 8]);
        r.release().done();
    }

    #[test]
    fn inventory_reports_no_tag_when_the_fifo_stays_empty() {
        let request = [0x26u8, 0x01, 0x00];
        let expected = transceive_transactions(&request, &[]);

        let mut r = Trf7962a::new(SpiMock::new(&expected));
        assert_eq!(
            r.inventory().unwrap(),
            None,
            "an empty FIFO means no tag, or a tag still in privacy mode"
        );
        r.release().done();
    }

    #[test]
    fn unlocking_fetches_a_random_number_then_sends_the_masked_password() {
        let random_response = [0x00u8, 0xCD, 0xAB]; // flags, then RN low, high
        let mut expected =
            transceive_transactions(&slix::get_random_number_request(), &random_response);
        // Password 0 masked with RN 0xABCD.
        expected.extend(transceive_transactions(
            &slix::set_password_request(0, 0xABCD),
            &[0x00],
        ));

        let mut r = Trf7962a::new(SpiMock::new(&expected));
        r.unlock_privacy(0).unwrap();
        r.release().done();
    }

    #[test]
    fn unlocking_fails_cleanly_when_no_tag_answers() {
        let expected = transceive_transactions(&slix::get_random_number_request(), &[]);
        let mut r = Trf7962a::new(SpiMock::new(&expected));
        assert_eq!(r.unlock_privacy(0), Err(Error::Timeout));
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
