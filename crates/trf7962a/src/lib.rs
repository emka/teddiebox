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
/// Continuous address mode: the reader auto-increments through registers
/// within one transaction. Required when reading the interrupt status.
const CONTINUOUS_BIT: u8 = 0x20;
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
    /// The reader received something it could not decode, carrying its own
    /// reason: CRC, parity, byte framing or EOF, or collision (SLOS757G
    /// Table 6-29, B4 to B1). A collision or framing error points at a reader
    /// mistuned for the reply it is getting, a CRC error at a reply that
    /// arrived corrupted — worth telling apart before anyone moves an antenna.
    ReceiveError(u8),
    /// The reader's FIFO overflowed, so the bytes in it cannot be trusted.
    FifoOverflow,
    /// The tag answered with the error flag set, carrying this error code.
    /// A refused privacy password arrives this way.
    TagError(u8),
    /// The request will not fit the reader's FIFO in a single load.
    RequestTooLong,
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

/// Bytes ahead of the request in a transmit burst: two direct commands, the
/// continuous-write address, and the two length bytes.
const TRANSMIT_HEADER: usize = 5;

/// Longest request this driver can send.
///
/// The FIFO is twelve bytes (SLOS757C §5.12.2) and the whole request is loaded
/// before transmission starts, rather than being refilled from the level-low
/// interrupt as the datasheet describes for longer frames. Every frame this
/// driver sends is far shorter — inventory is three bytes, SLIX set-password
/// eight — so the simpler scheme stands, and anything longer is refused rather
/// than quietly overrunning the FIFO.
pub const MAX_REQUEST: usize = 12;

/// FIFO status register (0x1C), per SLOS757C Table 6-21.
///
/// B7 is reserved and reads zero, so an overflow check against it could never
/// fire. B6 and B5 are the level-high and level-low flags, so folding them
/// into the count claims bytes that were never received — a full-looking FIFO
/// reads as 105 bytes rather than ten.
const FIFO_OVERFLOW: u8 = 0x10;
const FIFO_COUNT_MASK: u8 = 0x0F;

/// Interrupt status bits, SLOS757G Table 6-29.
///
/// B7 marks the reader's own transmit finishing and B6 a reception; the four
/// in between are the ways a reception can fail — CRC, parity, byte framing
/// or EOF, and collision. B0, the no-response timeout, is not among them:
/// nothing answering is the ordinary empty plate, not an error.
const IRQ_TX: u8 = 0x80;
const IRQ_RX_STARTED: u8 = 0x40;
/// The FIFO wants servicing: emptying during a transmit, filling during a
/// reception. Never the end of either.
const IRQ_FIFO: u8 = 0x20;
const IRQ_ERRORS: u8 = 0x1E;

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

/// Settling time after the field comes on, before a tag can be asked anything.
///
/// A passive tag has no power of its own: it rectifies the reader's field,
/// and until its own supply has come up it cannot answer. Measured on the
/// board — the first exchange after initialisation came back silent every
/// time while the second, milliseconds later, answered — which reads exactly
/// like an empty plate and is the reason this is worth spending.
pub const FIELD_SETTLE_MS: u32 = 10;

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
    /// Reads the interrupt status register.
    ///
    /// Not an ordinary register read. SLOS757C's SPI procedure sets the
    /// continuous-address bit as well as the read bit — address `0x6C`, not
    /// `0x4C` — and follows the status byte with a dummy read of register
    /// `0x0D`, "because the reader's IRQ Status register needs an additional
    /// clock cycle to clear the register". Read the ordinary way the register
    /// does not clear, and at a bench that presents as a reader which never
    /// raises an interrupt at all.
    pub fn read_irq_status(&mut self) -> Result<u8, Error<E>> {
        let mut buf = [0u8; 3];
        self.spi
            .transfer(
                &mut buf,
                &[
                    (regs::IRQ_STATUS & ADDRESS_MASK) | READ_BIT | CONTINUOUS_BIT,
                    0,
                    0,
                ],
            )
            .map_err(Error::Bus)?;
        Ok(buf[1])
    }

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
        self.delay.delay_ms(FIELD_SETTLE_MS);
        Ok(())
    }

    /// Says whether the reader is asserting its interrupt line right now.
    ///
    /// The line and the interrupt status register can disagree, and the case
    /// where they do is the one worth naming: the reader latched an interrupt
    /// that the wiring never delivered. Separating those needs the level on
    /// its own, outside an exchange.
    pub fn irq_asserted(&mut self) -> Result<bool, Error<E>> {
        self.irq.is_high().map_err(|_| Error::Pin)
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

    /// Puts `request` on the air, as one slave-select window.
    ///
    /// SLOS757G Figure 6-20 shows the whole transmit as a single burst —
    /// reset FIFO, transmit with CRC, a continuous write from 0x1D carrying
    /// the two length bytes and then the request — and the FIFO will not take
    /// data any other way. Written a byte per transaction the FIFO byte
    /// counter stays at zero even though the length registers read back
    /// correctly, and since "data transmission begins automatically after the
    /// first byte is written into the FIFO" (§6.12.5) a FIFO that never takes
    /// a byte is a transmitter that never starts. At a bench that is a reader
    /// which raises no interrupt at all and looks like an empty plate.
    fn transmit(&mut self, request: &[u8]) -> Result<(), Error<E>> {
        let mut burst = [0u8; TRANSMIT_HEADER + MAX_REQUEST];
        burst[0] = CMD_BIT | (regs::cmd::RESET_FIFO & ADDRESS_MASK);
        burst[1] = CMD_BIT | (regs::cmd::TRANSMIT_WITH_CRC & ADDRESS_MASK);
        burst[2] = CONTINUOUS_BIT | (regs::TX_LENGTH_BYTE1 & ADDRESS_MASK);
        // The length is a 12-bit field split across 0x1D and 0x1E, with the
        // low nibble of 0x1E counting the bits of a trailing broken byte.
        burst[3] = (request.len() >> 4) as u8;
        burst[4] = ((request.len() << 4) & 0xF0) as u8;
        let end = TRANSMIT_HEADER + request.len();
        burst[TRANSMIT_HEADER..end].copy_from_slice(request);
        self.spi.write(&burst[..end]).map_err(Error::Bus)
    }

    /// Empties `out.len()` bytes out of the FIFO, as one slave-select window.
    ///
    /// SLOS757G Figure 6-23: address 0x7F — the read and continuous bits over
    /// the FIFO address — sent once, then a filler byte per byte wanted, and
    /// the reply arrives one byte behind. The FIFO refuses to be read a byte
    /// per transaction exactly as it refuses to be written that way: the byte
    /// count is right and every byte comes back 0x00, which is a tag reduced
    /// to a reply of zeroes.
    fn read_fifo(&mut self, out: &mut [u8]) -> Result<(), Error<E>> {
        let mut rx = [0u8; MAX_RESPONSE + 1];
        let mut tx = [0u8; MAX_RESPONSE + 1];
        tx[0] = (regs::FIFO & ADDRESS_MASK) | READ_BIT | CONTINUOUS_BIT;
        let len = out.len() + 1;
        self.spi
            .transfer(&mut rx[..len], &tx[..len])
            .map_err(Error::Bus)?;
        out.copy_from_slice(&rx[1..len]);
        Ok(())
    }

    /// Sends `request` and collects the tag's reply. Returns the number of
    /// bytes received, which is zero when no tag answered.
    pub fn transceive(&mut self, request: &[u8], response: &mut [u8]) -> Result<usize, Error<E>> {
        // Checked before any bus traffic: a rejected request must leave the
        // reader exactly as it was found.
        if request.len() > MAX_REQUEST {
            return Err(Error::RequestTooLong);
        }
        self.transmit(request)?;

        // Reading the status register clears the interrupt. Leaving it set
        // would make the next exchange return instantly on a stale assertion.
        //
        // A frame of five bytes or more interrupts part-way through its own
        // transmission — SLOS757G §6.12.5, "when the number of bytes in the
        // FIFO reaches 3" — to ask for more data. This driver preloads the
        // whole request, so there is never more to give and that interrupt is
        // not the end of anything. Measured for an eight-byte frame: 0xA0 at
        // 1.4 ms, then 0x80 at 1.6 ms. Acting on the first resets the FIFO
        // mid-frame, the tag receives a truncated request and says nothing,
        // and because three-byte requests never reach the threshold this
        // looked exactly like a password being refused.
        let mut status = loop {
            if !self.wait_for_response()? {
                return Ok(0);
            }
            let status = self.read_irq_status()?;
            if status & IRQ_FIFO == 0 {
                break status;
            }
        };

        // The reader interrupts twice: once when its own transmit finishes,
        // and again when the tag has answered. Reading the FIFO on the first
        // finds it empty and — through the N-1 count — reports one byte of
        // nothing, which is what a reader transmitting perfectly well looked
        // like at the bench. SLOS757G Figure 6-25 resets the FIFO between the
        // two, so the reception starts from an empty one.
        if status & IRQ_TX != 0 {
            self.send_command(regs::cmd::RESET_FIFO)?;
            if !self.wait_for_response()? {
                return Ok(0);
            }
            status = self.read_irq_status()?;
        }

        // A reply longer than eight bytes arrives in more than one piece.
        // SLOS757G §6.12.4: the reader interrupts once the ninth byte lands,
        // "before the end of the receive operation", and "this is repeated
        // until an RX complete interrupt is generated". An inventory reply is
        // ten bytes, so this is every tag read there will ever be — stopping
        // at the first interrupt returns nine and the caller rejects a good
        // tag as malformed.
        let mut received = 0;
        loop {
            // CRC, parity, framing or collision: the reader heard something
            // and could not turn it into a frame. Reading the FIFO anyway
            // hands the caller a fragment that looks like a short reply.
            if status & IRQ_ERRORS != 0 {
                return Err(Error::ReceiveError(status & IRQ_ERRORS));
            }
            // Without a reception there is nothing in the FIFO to read, and
            // the N-1 count cannot tell an empty FIFO from a one-byte one.
            if status & (IRQ_RX_STARTED | IRQ_FIFO) == 0 {
                return Ok(received);
            }

            // B4 is the overflow flag and B3-B0 the unread byte count.
            // Reading the raw byte as a count folds in the two level flags
            // and hands the caller fabricated data.
            let fifo = self.read_register(regs::FIFO_STATUS)?;
            if fifo & FIFO_OVERFLOW != 0 {
                return Err(Error::FifoOverflow);
            }
            // The count reads as N-1 — SLOS757G §6.12.2, "if 8 bytes are in
            // the FIFO, this number is 7". Confirmed at the bench: a GET
            // RANDOM NUMBER reply counted 0x02 and held the three bytes it
            // should, flags and a random number that differs every run.
            let available = (fifo & FIFO_COUNT_MASK) as usize + 1;
            let n = available.min(response.len() - received);
            self.read_fifo(&mut response[received..received + n])?;
            received += n;

            // The FIFO flag is the reader asking to be emptied again; without
            // it this interrupt was the end of the reception.
            if status & IRQ_FIFO == 0 || received == response.len() {
                return Ok(received);
            }
            if !self.wait_for_response()? {
                return Ok(received);
            }
            status = self.read_irq_status()?;
        }
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
            Ok(None)
            | Err(Error::BadResponse)
            | Err(Error::ReceiveError(_))
            | Err(Error::TagError(_)) => {}
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
