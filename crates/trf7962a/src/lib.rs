#![no_std]

//! Driver for the TI TRF7962A 13.56 MHz reader, restricted to the ISO 15693
//! operations the Toniebox needs.

pub mod regs;
pub mod slix;

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::InputPin;
use embedded_hal::spi::SpiDevice;

/// Command word bits. Getting these wrong turns a register read into a direct
/// command, so the encoding lives here and nowhere else.
const CMD_BIT: u8 = 0x80;
const READ_BIT: u8 = 0x40;
const ADDRESS_MASK: u8 = 0x1F;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    Bus(E),
    /// The IRQ line could not be read.
    Pin,
    /// The reader did not respond within the expected window.
    Timeout,
    /// A tag responded, but the response was not the expected shape.
    BadResponse,
    /// The reader's FIFO overflowed, so the bytes in it cannot be trusted.
    FifoOverflow,
    /// The tag answered with the error flag set, carrying this error code.
    /// A refused privacy password arrives this way.
    TagError(u8),
}

/// ISO 15693-3 §7.4: bit 0 of the response flags marks an error response,
/// whose payload is a one-byte error code rather than the expected data.
const RESPONSE_ERROR_FLAG: u8 = 0x01;

/// Returns the tag's error when `response` is an error response.
///
/// Without this every rejection reads as a success of the wrong shape, and a
/// wrong password becomes indistinguishable from an empty plate.
fn tag_error<E>(response: &[u8]) -> Result<(), Error<E>> {
    if response
        .first()
        .is_some_and(|f| f & RESPONSE_ERROR_FLAG != 0)
    {
        return Err(Error::TagError(response.get(1).copied().unwrap_or(0)));
    }
    Ok(())
}

/// Reader configuration applied at startup, as `(register, value)`.
///
/// Two writes are enough. Selecting the protocol presets every sub-setting
/// register the reader needs, and for ISO 15693 those presets are already the
/// values this driver wants: a 9.44 µs modulation pulse (0x06 = 0x00), a 755 µs
/// no-response window at high data rate (0x07 = 0x0E), and the 200–900 kHz
/// bandpass matching the 424 kHz subcarrier (0x0A = 0x40) — SLOS757C §6.1.2.1,
/// §6.1.2.2 and §6.1.2.5. Restating them buys nothing; tuning them belongs to a
/// specific antenna at the bench, not to a default.
pub const INIT_SEQUENCE: &[(u8, u8)] = &[
    (regs::ISO_CONTROL, regs::ISO_CONTROL_15693_HIGH),
    // The field goes on last, once the protocol is configured.
    (regs::CHIP_STATUS_CONTROL, regs::CHIP_STATUS_RF_ON),
];

/// Longest ISO 15693 response this driver handles.
pub const MAX_RESPONSE: usize = 32;

/// FIFO status register (0x1C), per SLOS757C Table 6-21.
///
/// B7 is reserved and reads zero, so an overflow check against it could never
/// fire. B6 and B5 are the level-high and level-low flags, so folding them
/// into the count claims bytes that were never received — a full-looking FIFO
/// reads as 105 bytes rather than ten.
const FIFO_OVERFLOW: u8 = 0x10;
const FIFO_COUNT_MASK: u8 = 0x0F;

/// How long the reader is given to signal that an exchange finished.
///
/// An ISO 15693 exchange at high bit rate runs roughly 5–6 ms end to end:
/// transmit, then t1 of about 320 µs, then the tag's reply at 26.48 kbit/s.
/// The budget is deliberately generous, because being too short makes a tag
/// on the plate read as no tag — the one failure that cannot be told apart
/// from a wiring fault. **Retune against a real exchange at bench step 10.**
pub const IRQ_POLL_INTERVAL_US: u32 = 200;
/// Poll count, giving a 20 ms window at the interval above.
pub const IRQ_POLL_ATTEMPTS: u32 = 100;

/// Settling time after a soft init, before the reader accepts configuration.
pub const SOFT_INIT_SETTLE_MS: u32 = 1;

pub struct Trf7962a<SPI, D, IRQ> {
    spi: SPI,
    delay: D,
    irq: IRQ,
}

impl<SPI, D, IRQ, E> Trf7962a<SPI, D, IRQ>
where
    SPI: SpiDevice<Error = E>,
    D: DelayNs,
    IRQ: InputPin,
{
    /// `irq` is the reader's interrupt line, GPIO13 on the Toniebox.
    ///
    /// The reader needs several milliseconds to complete an exchange, so a
    /// driver without a clock and an interrupt line cannot tell "no tag" from
    /// "not finished yet" — it always reads too early and reports an empty
    /// plate.
    pub fn new(spi: SPI, delay: D, irq: IRQ) -> Self {
        Self { spi, delay, irq }
    }

    pub fn release(self) -> (SPI, D, IRQ) {
        (self.spi, self.delay, self.irq)
    }

    /// Writes one register.
    pub fn write_register(&mut self, reg: u8, value: u8) -> Result<(), Error<E>> {
        self.spi
            .write(&[reg & ADDRESS_MASK, value])
            .map_err(Error::Bus)
    }

    /// Reads one register.
    ///
    /// Two bytes are clocked. The reader is still receiving the address during
    /// the first, so it can only drive the value out on the second; a one-byte
    /// transfer returns whatever MISO happened to be carrying.
    pub fn read_register(&mut self, reg: u8) -> Result<u8, Error<E>> {
        let mut buf = [0u8; 2];
        self.spi
            .transfer(&mut buf, &[(reg & ADDRESS_MASK) | READ_BIT, 0])
            .map_err(Error::Bus)?;
        Ok(buf[1])
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
        self.delay.delay_ms(SOFT_INIT_SETTLE_MS);
        self.send_command(regs::cmd::IDLE)?;
        self.delay.delay_ms(SOFT_INIT_SETTLE_MS);
        for &(reg, value) in INIT_SEQUENCE {
            self.write_register(reg, value)?;
        }
        Ok(())
    }

    /// Waits for the reader to raise its interrupt line.
    ///
    /// `Ok(false)` means the window elapsed with no assertion, which is the
    /// ordinary "nothing on the plate" case rather than a fault — the firmware
    /// polls an empty plate continuously, so that must not be an error.
    fn wait_for_response(&mut self) -> Result<bool, Error<E>> {
        for _ in 0..IRQ_POLL_ATTEMPTS {
            if self.irq.is_high().map_err(|_| Error::Pin)? {
                return Ok(true);
            }
            self.delay.delay_us(IRQ_POLL_INTERVAL_US);
        }
        Ok(false)
    }

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

        if !self.wait_for_response()? {
            return Ok(0);
        }
        // Reading the status register clears the interrupt. Leaving it set
        // would make the next exchange return instantly on a stale assertion.
        // The individual bits are not interpreted yet — see the register map
        // note in `regs`, and confirm them at bench step 10.
        let _ = self.read_register(regs::IRQ_STATUS)?;

        // B4 is the overflow flag and B3-B0 the unread byte count. Reading the
        // raw byte as a count folds in the two level flags and hands the
        // caller fabricated data.
        let status = self.read_register(regs::FIFO_STATUS)?;
        if status & FIFO_OVERFLOW != 0 {
            return Err(Error::FifoOverflow);
        }
        // The count reads as N-1: SLOS757C says so in §5.12.2 and again in
        // Table 6-21, "if 8 bytes are in the FIFO, this number is 7". This
        // runs only after the interrupt fired, so the FIFO holds at least one
        // byte and the adjustment is always defined.
        //
        // **The least verified claim in this driver.** Off by one here either
        // truncates every reply or reads one byte of rubbish past it, and no
        // mock can tell which. Confirm at bench step 10, first thing.
        let available = (status & FIFO_COUNT_MASK) as usize + 1;
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
        tag_error(&buf[..n])?;
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

    /// Fetches the tag's random number, needed to mask the password.
    pub fn get_random_number(&mut self) -> Result<u16, Error<E>> {
        let mut buf = [0u8; MAX_RESPONSE];
        let n = self.transceive(&slix::get_random_number_request(), &mut buf)?;
        if n == 0 {
            return Err(Error::Timeout);
        }
        tag_error(&buf[..n])?;
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
        tag_error(&buf[..n])
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
        // A locked tag is silent, and a tag mid-exchange can answer garbled —
        // both mean "try unlocking", so neither aborts the operation.
        match self.inventory() {
            Ok(Some(uid)) => return Ok(Some(uid)),
            Ok(None) | Err(Error::BadResponse) | Err(Error::TagError(_)) => {}
            Err(e) => return Err(e),
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
mod tests;
