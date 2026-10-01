#![no_std]

//! Driver for the TI TRF7962A 13.56 MHz reader, restricted to the ISO 15693
//! operations the Toniebox needs.

pub mod regs;
pub mod slix;

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::InputPin;
use embedded_hal::spi::SpiDevice;

/// Command word bits. A wrong bit turns a register read into a direct
/// command, so the encoding is only defined here.
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
    /// The reader received something it could not decode. The value gives
    /// the reason: CRC, parity, byte framing or EOF, or collision (SLOS757G
    /// Table 6-29, B4 to B1). A collision or framing error suggests the reader
    /// is mistuned; a CRC error suggests a corrupted reply.
    ReceiveError(u8),
    /// The reader's FIFO overflowed, so the bytes in it cannot be trusted.
    FifoOverflow,
    /// The tag answered with the error flag set, carrying this error code.
    /// A refused privacy password arrives this way.
    TagError(u8),
    /// The request will not fit the reader's FIFO in a single load.
    RequestTooLong,
    /// `read_memory`'s output length was not a whole number of four-byte
    /// blocks.
    BadLength,
}

/// ISO 15693-3 §7.4: bit 0 of the response flags marks an error response,
/// whose payload is a one-byte error code rather than the expected data.
const RESPONSE_ERROR_FLAG: u8 = 0x01;

/// Returns the tag's error when `response` is an error response.
///
/// Without this, a rejection would look like a malformed success, and a
/// wrong password like a box with no figure.
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
/// Two writes are enough. Selecting the protocol presets the other registers,
/// and for ISO 15693 the presets are what this driver wants: a 9.44 µs
/// modulation pulse (0x06 = 0x00), a 755 µs no-response window at high data
/// rate (0x07 = 0x0E), and the 200–900 kHz bandpass for the 424 kHz
/// subcarrier (0x0A = 0x40); SLOS757C §6.1.2.1, §6.1.2.2 and §6.1.2.5.
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
/// The FIFO is twelve bytes (SLOS757C §5.12.2), and the whole request is
/// loaded before transmission starts, rather than refilled during it as the
/// datasheet describes for longer frames. Every frame this driver sends is
/// shorter (inventory is three bytes, SLIX set-password eight), and anything
/// longer is refused.
pub const MAX_REQUEST: usize = 12;

/// Non-addressed request flags for a plain ISO 15693-3 command.
///
/// ISO 15693-3 §7.3.1 puts the Address flag in bit 6; setting it would
/// require sending the tag's eight-byte UID.
///
/// The same value as `slix::FLAGS` but a separate constant: READ SINGLE BLOCK
/// is a standard ISO 15693-3 command, not one of NXP's custom commands, so
/// its flags should not change if `slix` ever needs different ones.
const FLAGS: u8 = 0x02;

/// READ SINGLE BLOCK: ISO 15693-3's command code for reading four bytes of
/// tag memory by block number.
const CMD_READ_SINGLE_BLOCK: u8 = 0x20;

/// FIFO status register (0x1C), per SLOS757C Table 6-21.
///
/// B7 is reserved and reads zero. B4 is the overflow flag. B6 and B5 are the
/// level flags and must be masked out of the count, or a FIFO holding ten
/// bytes would read as 105.
const FIFO_OVERFLOW: u8 = 0x10;
const FIFO_COUNT_MASK: u8 = 0x0F;

/// Interrupt status bits, SLOS757G Table 6-29.
///
/// B7 means the reader's own transmit finished and B6 a reception. B4-B1 are
/// reception errors: CRC, parity, byte framing or EOF, and collision. B0, the
/// no-response timeout, is not an error: no answer just means a box with no figure.
const IRQ_TX: u8 = 0x80;
const IRQ_RX_STARTED: u8 = 0x40;
/// The FIFO wants servicing: emptying during a transmit, filling during a
/// reception. Never the end of either.
const IRQ_FIFO: u8 = 0x20;
const IRQ_ERRORS: u8 = 0x1E;

/// How long the reader is given to signal that an exchange finished.
///
/// An ISO 15693 exchange at high bit rate takes about 5–6 ms: transmit, then
/// t1 of about 320 µs, then the tag's reply at 26.48 kbit/s. Too short a
/// window makes a tag on the box look like no tag, so it has headroom.
pub const IRQ_POLL_INTERVAL_US: u32 = 200;
/// Poll count, giving a 10 ms window at the interval above.
///
/// **Measured:** the slowest reply from a real Tonie was **20 polls, about
/// 4 ms** (see `slowest_reply_polls`). 50 leaves two and a half times that.
///
/// Kept short because a box with no figure waits the whole window on every poll,
/// and the task blocks meanwhile: with 100 polls, playback had 49 audible
/// glitches in 70 s. Measure again before lowering it further.
pub const IRQ_POLL_ATTEMPTS: u32 = 50;

/// Quiet time the air is left after a tag has answered, before the next
/// request may go out — ISO 15693-3 §9.1's t2, 4192 carrier periods at
/// 13.56 MHz.
///
/// A tag is not ready to listen the moment it finishes replying, and the
/// TRF7962A does not enforce the gap. Without it, the second of two exchanges
/// in a row (as in a privacy unlock) got no answer about half the time.
///
/// Rounded up from 309.1 µs; the delay is a minimum.
pub const T2_QUIET_US: u32 = 320;

/// How long the field is held down to reset the tags standing in it.
///
/// A tag that has refused a password answers nothing until it is reset by
/// losing power (SL2S2602 §9.5.3.2: "it will not execute any following
/// command until a Power-On Reset (POR) (RF reset) is executed"). A passive
/// tag is powered only by the reader's field, and its stored charge lasts
/// microseconds, so a few milliseconds is plenty.
pub const FIELD_OFF_MS: u32 = 5;

/// Settling time after a soft init, before the reader accepts configuration.
pub const SOFT_INIT_SETTLE_MS: u32 = 1;

/// Settling time after the field comes on, before a tag can be asked anything.
///
/// A passive tag is powered by the reader's field and cannot answer until its
/// supply is up. Without this wait, the first exchange after start-up always
/// got no answer (measured on the board).
pub const FIELD_SETTLE_MS: u32 = 10;

pub struct Trf7962a<SPI, D, IRQ> {
    spi: SPI,
    delay: D,
    irq: IRQ,
    /// The most interrupt polls any answered exchange has needed, to size the
    /// reply window from measurements. See `slowest_reply_polls`.
    slowest_reply_polls: u32,
}

impl<SPI, D, IRQ, E> Trf7962a<SPI, D, IRQ>
where
    SPI: SpiDevice<Error = E>,
    D: DelayNs,
    IRQ: InputPin,
{
    /// `irq` is the reader's interrupt line, GPIO13 on the Toniebox.
    ///
    /// An exchange takes several milliseconds, so the driver needs a delay and
    /// the interrupt line to tell "no tag" from "not finished yet".
    pub fn new(spi: SPI, delay: D, irq: IRQ) -> Self {
        Self {
            spi,
            delay,
            irq,
            slowest_reply_polls: 0,
        }
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
    /// Two bytes are clocked: the reader receives the address during the
    /// first and sends the value during the second.
    pub fn read_register(&mut self, reg: u8) -> Result<u8, Error<E>> {
        let mut buf = [0u8; 2];
        self.spi
            .transfer(&mut buf, &[(reg & ADDRESS_MASK) | READ_BIT, 0])
            .map_err(Error::Bus)?;
        Ok(buf[1])
    }

    /// Reads the interrupt status register.
    ///
    /// Not an ordinary register read. SLOS757C's SPI procedure sets the
    /// continuous-address bit as well as the read bit (address `0x6C`, not
    /// `0x4C`) and follows the status byte with a dummy read of register
    /// `0x0D`, "because the reader's IRQ Status register needs an additional
    /// clock cycle to clear the register". Read the ordinary way, the register
    /// does not clear, and the reader seems to never raise an interrupt.
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
        self.delay.delay_ms(FIELD_SETTLE_MS);
        Ok(())
    }

    /// Power-cycles every tag in the field, by taking the field away.
    ///
    /// A tag sent a wrong password stops answering everything, GET RANDOM
    /// NUMBER included, until it loses power (SL2S2602 §9.5.3.2). The only way
    /// to reset it is to turn the field off. Confirmed on the board.
    pub fn reset_tags(&mut self) -> Result<(), Error<E>> {
        self.write_register(regs::CHIP_STATUS_CONTROL, regs::CHIP_STATUS_RF_OFF)?;
        self.delay.delay_ms(FIELD_OFF_MS);
        self.write_register(regs::CHIP_STATUS_CONTROL, regs::CHIP_STATUS_RF_ON)?;
        self.delay.delay_ms(FIELD_SETTLE_MS);
        Ok(())
    }

    /// Says whether the reader is asserting its interrupt line right now.
    ///
    /// For debugging: if the status register shows an interrupt but the line
    /// is low, the wiring did not deliver it.
    pub fn irq_asserted(&mut self) -> Result<bool, Error<E>> {
        self.irq.is_high().map_err(|_| Error::Pin)
    }

    /// Waits for the reader to raise its interrupt line.
    ///
    /// `Ok(false)` means the window passed without an interrupt: the normal
    /// "nothing on the box" case, not an error.
    fn wait_for_response(&mut self) -> Result<bool, Error<E>> {
        for polled in 0..IRQ_POLL_ATTEMPTS {
            if self.irq.is_high().map_err(|_| Error::Pin)? {
                if polled > self.slowest_reply_polls {
                    self.slowest_reply_polls = polled;
                }
                return Ok(true);
            }
            self.delay.delay_us(IRQ_POLL_INTERVAL_US);
        }
        Ok(false)
    }

    /// The most interrupt polls any answered exchange has needed so far.
    ///
    /// Multiplied by `IRQ_POLL_INTERVAL_US`, this is how long the slowest
    /// reply took. Use it to size `IRQ_POLL_ATTEMPTS`. Unanswered exchanges are
    /// not counted.
    ///
    /// A window shorter than this makes a tag on the box look like no tag,
    /// so leave plenty of headroom.
    pub fn slowest_reply_polls(&self) -> u32 {
        self.slowest_reply_polls
    }

    /// Puts `request` on the air, as one slave-select window.
    ///
    /// SLOS757G Figure 6-20 shows the transmit as one burst: reset FIFO,
    /// transmit with CRC, a continuous write from 0x1D with the two length
    /// bytes, then the request. The FIFO does not accept data any other way:
    /// written one byte per transaction, it stays empty, and since
    /// transmission only starts when the first byte enters the FIFO (§6.12.5),
    /// nothing is sent.
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
    /// SLOS757G Figure 6-23: address 0x7F (read and continuous bits plus the
    /// FIFO address) sent once, then one filler byte per byte wanted; the
    /// reply arrives one byte behind. Read one byte per transaction, every
    /// byte comes back 0x00.
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
        // Checked before any bus traffic, so a rejected request changes
        // nothing.
        if request.len() > MAX_REQUEST {
            return Err(Error::RequestTooLong);
        }
        self.transmit(request)?;

        // Reading the status register clears the interrupt; otherwise the
        // next exchange would return at once.
        //
        // A frame of five bytes or more interrupts during its own transmission
        // ("when the number of bytes in the FIFO reaches 3", SLOS757G
        // §6.12.5) to ask for more data. The whole request is already loaded,
        // so that interrupt (IRQ_FIFO) is skipped. Measured for an eight-byte
        // frame: 0xA0 at 1.4 ms, then 0x80 at 1.6 ms. Acting on the first
        // would reset the FIFO mid-frame, and the tag would not answer.
        let mut status = loop {
            if !self.wait_for_response()? {
                return Ok(0);
            }
            let status = self.read_irq_status()?;
            if status & IRQ_FIFO == 0 {
                break status;
            }
        };

        // The reader interrupts twice: when its transmit finishes, and when
        // the tag has answered. At the first, the FIFO is empty but its N-1
        // count would report one byte. SLOS757G Figure 6-25 resets the FIFO
        // between the two, so reception starts empty.
        if status & IRQ_TX != 0 {
            self.send_command(regs::cmd::RESET_FIFO)?;
            if !self.wait_for_response()? {
                return Ok(0);
            }
            status = self.read_irq_status()?;
        }

        // A reply longer than eight bytes arrives in pieces. SLOS757G §6.12.4:
        // the reader interrupts when the ninth byte arrives, "before the end
        // of the receive operation", and "this is repeated until an RX
        // complete interrupt is generated". An inventory reply is ten bytes,
        // so this always happens.
        let mut received = 0;
        loop {
            // CRC, parity, framing or collision: the reader received
            // something it could not decode. Do not read the FIFO.
            if status & IRQ_ERRORS != 0 {
                return Err(Error::ReceiveError(status & IRQ_ERRORS));
            }
            // No reception means nothing to read, and the N-1 count cannot
            // tell an empty FIFO from one holding a byte.
            if status & (IRQ_RX_STARTED | IRQ_FIFO) == 0 {
                return Ok(received);
            }

            // B4 is the overflow flag and B3-B0 the unread byte count. The
            // level flags must be masked out of the count.
            let fifo = self.read_register(regs::FIFO_STATUS)?;
            if fifo & FIFO_OVERFLOW != 0 {
                return Err(Error::FifoOverflow);
            }
            // The count reads as N-1 (SLOS757G §6.12.2: "if 8 bytes are in
            // the FIFO, this number is 7"). Confirmed on the hardware.
            let available = (fifo & FIFO_COUNT_MASK) as usize + 1;
            let n = available.min(response.len() - received);
            self.read_fifo(&mut response[received..received + n])?;
            received += n;

            // The FIFO flag means more data is coming; without it, the
            // reception is complete.
            if status & IRQ_FIFO == 0 || received == response.len() {
                self.delay.delay_us(T2_QUIET_US);
                return Ok(received);
            }
            if !self.wait_for_response()? {
                self.delay.delay_us(T2_QUIET_US);
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

    /// Puts a tag back into privacy mode, where it answers nothing but GET
    /// RANDOM NUMBER until the password is presented again.
    ///
    /// Reversible with `unlock_privacy` and the same password (SL2S2602
    /// §9.5.3.9).
    pub fn enable_privacy(&mut self, password: u32) -> Result<(), Error<E>> {
        let random = self.get_random_number()?;
        let mut buf = [0u8; MAX_RESPONSE];
        let n = self.transceive(&slix::enable_privacy_request(password, random), &mut buf)?;
        if n == 0 {
            return Err(Error::Timeout);
        }
        tag_error(&buf[..n])
    }

    /// Whether anything is on the box, without unlocking it.
    ///
    /// SL2S5002 §1.3: a label in privacy mode "will not respond to any command
    /// except the command GET RANDOM NUMBER, until it next receives the correct
    /// Privacy password". So this is the only command a locked Tonie answers,
    /// and a quick one: a tag replies in about 6 ms.
    ///
    /// It only says *something is there*, not *what*: the reply is a random
    /// number. Use `inventory_unlocked` to identify the figure.
    ///
    /// Unlocked tags answer this too, so the caller does not need to know
    /// the tag's state.
    pub fn tag_present(&mut self) -> Result<bool, Error<E>> {
        match self.get_random_number() {
            Ok(_) => Ok(true),
            Err(Error::Timeout) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Unlocks, then runs an inventory. This is what the firmware calls; a
    /// locked tag does not answer `inventory` alone.
    ///
    /// `passwords` are tried in order. A Toniebox figure and a tag with NXP's
    /// factory default can both be read; put the expected one first.
    pub fn inventory_unlocked(&mut self, passwords: &[u32]) -> Result<Option<[u8; 8]>, Error<E>> {
        // An unlocked tag answers inventory directly, so try that first. A
        // locked tag is silent and a tag mid-exchange may answer garbled;
        // both mean "try unlocking".
        match self.inventory() {
            Ok(Some(uid)) => return Ok(Some(uid)),
            Ok(None)
            | Err(Error::BadResponse)
            | Err(Error::ReceiveError(_))
            | Err(Error::TagError(_)) => {}
            Err(e) => return Err(e),
        }

        for &password in passwords {
            // Privacy mode still answers this command, so silence here means
            // a box with no figure, not a wrong password.
            let random = match self.get_random_number() {
                Ok(random) => random,
                Err(Error::Timeout) => return Ok(None),
                Err(e) => return Err(e),
            };
            match self.set_password(password, random) {
                Ok(()) => return self.inventory(),
                // A wrong password gets no answer and leaves the tag deaf
                // until it loses power. Reset the field before the next
                // password, and after the last one, so the tag works again.
                Err(Error::Timeout) => self.reset_tags()?,
                Err(e) => return Err(e),
            }
        }
        Ok(None)
    }

    /// Reads one four-byte block of tag memory.
    ///
    /// `[flags, 0x20, block]`, three bytes, non-addressed — see `FLAGS`. The
    /// reply is `flags(1) + 4 data bytes`.
    ///
    /// The four bytes are returned exactly as the tag sent them. Unlike the
    /// UID in `inventory`, block data is not reversed.
    pub fn read_block(&mut self, block: u8) -> Result<[u8; 4], Error<E>> {
        let request = [FLAGS, CMD_READ_SINGLE_BLOCK, block];
        let mut buf = [0u8; MAX_RESPONSE];
        let n = self.transceive(&request, &mut buf)?;
        if n == 0 {
            return Err(Error::Timeout);
        }
        tag_error(&buf[..n])?;
        // flags(1) + 4 data bytes.
        if n < 5 {
            return Err(Error::BadResponse);
        }
        let mut data = [0u8; 4];
        data.copy_from_slice(&buf[1..5]);
        Ok(data)
    }

    /// Fills `out` from consecutive blocks starting at `first`, four bytes
    /// at a time.
    ///
    /// One `read_block` per block, rather than READ MULTIPLE BLOCKS (`0x23`):
    /// eight blocks in one reply would be `1 + 32 = 33` bytes, one more than
    /// `MAX_RESPONSE`.
    ///
    /// `out.len()` must be a multiple of four; otherwise it is rejected
    /// before any bus traffic.
    ///
    /// Used to read the 32-byte token teddyCloud forwards as
    /// `Authorization: BD <64 hex>`, which is the tag's memory. Measured on a
    /// real Tonie:
    ///
    /// - **Which blocks hold it:** the whole user memory, blocks 0 to 7
    ///   (`first = 0`, 32-byte `out`). Block 8 and beyond do not answer.
    /// - **Privacy must be unlocked first.** A locked figure does not answer
    ///   READ SINGLE BLOCK at all. Call `inventory_unlocked` first, or every
    ///   block returns `Timeout`.
    ///
    /// **No answer is ambiguous.** A locked tag, a missing tag and a block past
    /// the end all return `Error::Timeout`: this tag does not send the ISO
    /// 15693-3 §7.4 "block not available" error. So a missing or locked tag
    /// looks like one with no memory.
    pub fn read_memory(&mut self, first: u8, out: &mut [u8]) -> Result<(), Error<E>> {
        // Checked before any bus traffic, like `transceive`'s length check.
        if !out.len().is_multiple_of(4) {
            return Err(Error::BadLength);
        }
        for (i, chunk) in out.chunks_mut(4).enumerate() {
            let block = first.wrapping_add(i as u8);
            chunk.copy_from_slice(&self.read_block(block)?);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
