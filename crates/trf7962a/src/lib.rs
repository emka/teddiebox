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
    /// `read_memory`'s output length was not a whole number of four-byte
    /// blocks. A partial block cannot be represented, so the request is
    /// refused outright rather than reading a truncated last block or
    /// silently rounding the length down.
    BadLength,
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

/// Non-addressed request flags for a plain ISO 15693-3 command.
///
/// Same value as `slix::FLAGS`, for the same reason: ISO 15693-3 §7.3.1 puts
/// the Address flag in bit 6, and setting it would require carrying the
/// tag's eight-byte UID, which a read by block number has no cause to know.
/// Re-expressed here, rather than imported, because READ SINGLE BLOCK is a
/// standard ISO 15693-3 command rather than one of NXP's custom ones, so it
/// has no business living in `slix`.
const FLAGS: u8 = slix::FLAGS;

/// READ SINGLE BLOCK: ISO 15693-3's command code for reading four bytes of
/// tag memory by block number.
const CMD_READ_SINGLE_BLOCK: u8 = 0x20;

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
/// Being too short makes a tag on the plate read as no tag — the one failure
/// that cannot be told apart from a wiring fault — so the window keeps real
/// headroom over what a reply needs.
pub const IRQ_POLL_INTERVAL_US: u32 = 200;
/// Poll count, giving a 10 ms window at the interval above.
///
/// **Measured, 2026-09-07:** the slowest reply a real Tonie produced across a
/// run of the plate poller was **20 polls, about 4 ms** — see
/// `slowest_reply_polls`, which exists to keep this number honest. 50 leaves
/// two and a half times that, and is still well above the 5–6 ms an exchange
/// is supposed to take.
///
/// It was 100 until that measurement. Halving it matters because an empty
/// plate pays the whole window on every poll and the reader task blocks while
/// it does: at 100 the poller cost 49 audible DMA restarts in 70 s of
/// playback. Re-measure before cutting further; one figure on one board over
/// one run is thin evidence for a tighter bound.
pub const IRQ_POLL_ATTEMPTS: u32 = 50;

/// Quiet time the air is left after a tag has answered, before the next
/// request may go out — ISO 15693-3 §9.1's t2, 4192 carrier periods at
/// 13.56 MHz.
///
/// A tag is not listening again the instant it has finished replying, and the
/// reader is much faster than it: two SPI reads and the next frame is already
/// on the air. Nothing in this driver enforced the gap, and nothing in the
/// TRF7962A does it for us, so a caller making two exchanges in a row — which
/// is exactly what a privacy unlock is — got silence from the second about
/// half the time. Silence is the one answer that cannot be told apart from an
/// empty plate.
///
/// Rounded up from 309.1 µs, because the delay is a floor and a spare
/// microsecond costs nothing against an exchange of several milliseconds.
pub const T2_QUIET_US: u32 = 320;

/// How long the field is held down to reset the tags standing in it.
///
/// A tag that has refused a password answers nothing at all until it has been
/// through a power-on reset — SL2S2602 §9.5.3.2, "it will not execute any
/// following command until a Power-On Reset (POR) (RF reset) is executed" —
/// and a passive tag's only supply is the reader's field. The reservoir it
/// runs on is small, so this only has to outlast a few microseconds of stored
/// charge; the margin is cheap because nothing resets a tag on a hot path.
pub const FIELD_OFF_MS: u32 = 5;

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
    /// The most interrupt polls any answered exchange has needed, so the reply
    /// window can be sized against what this board actually does rather than
    /// against arithmetic. See `slowest_reply_polls`.
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
    /// The reader needs several milliseconds to complete an exchange, so a
    /// driver without a clock and an interrupt line cannot tell "no tag" from
    /// "not finished yet" — it always reads too early and reports an empty
    /// plate.
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

    /// Power-cycles every tag in the field, by taking the field away.
    ///
    /// A tag that has been sent a password it does not accept stops answering
    /// everything — GET RANDOM NUMBER included — until its supply has been
    /// interrupted (SL2S2602 §9.5.3.2). Nothing the reader can *say* to it
    /// helps, because it has stopped listening; the only lever is the field
    /// it draws its power from.
    ///
    /// Measured on the board: a tag muted this way answers again after this,
    /// and rebooting the box worked before only because that drops the
    /// storage rail the reader is on.
    pub fn reset_tags(&mut self) -> Result<(), Error<E>> {
        self.write_register(regs::CHIP_STATUS_CONTROL, regs::CHIP_STATUS_RF_OFF)?;
        self.delay.delay_ms(FIELD_OFF_MS);
        self.write_register(regs::CHIP_STATUS_CONTROL, regs::CHIP_STATUS_RF_ON)?;
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
    /// Multiplied by `IRQ_POLL_INTERVAL_US` this is how long the slowest reply
    /// actually took on this board, which is the only honest input to sizing
    /// `IRQ_POLL_ATTEMPTS`. Unanswered exchanges are excluded on purpose: they
    /// tell you what the window is, not what a reply needs.
    ///
    /// A window cut below this figure makes a tag on the plate read as no tag,
    /// which cannot be told apart from a disconnected antenna — so leave real
    /// headroom above whatever the bench reports.
    pub fn slowest_reply_polls(&self) -> u32 {
        self.slowest_reply_polls
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
    /// The way back out is `unlock_privacy` with the same password
    /// (SL2S2602 §9.5.3.9), so this is reversible — but only by whoever knows
    /// the password, which is the entire point of it.
    pub fn enable_privacy(&mut self, password: u32) -> Result<(), Error<E>> {
        let random = self.get_random_number()?;
        let mut buf = [0u8; MAX_RESPONSE];
        let n = self.transceive(&slix::enable_privacy_request(password, random), &mut buf)?;
        if n == 0 {
            return Err(Error::Timeout);
        }
        tag_error(&buf[..n])
    }

    /// Unlocks, then inventories. This is the operation the firmware calls;
    /// a locked tag is invisible to `inventory` alone.
    ///
    /// `passwords` are tried in order, so the caller states its preference:
    /// a Toniebox figure and a tag still holding NXP's factory default are
    /// both readable, and which one is expected comes first.
    /// Whether anything is on the plate, without unlocking it.
    ///
    /// SL2S5002 §1.3: a label in privacy mode "will not respond to any command
    /// except the command GET RANDOM NUMBER, until it next receives the correct
    /// Privacy password". That makes this the only question a locked Tonie will
    /// answer, and the cheapest one there is — a tag replies in about 6 ms
    /// where silence costs the full `IRQ_POLL_ATTEMPTS` window.
    ///
    /// It says *something is there*, never *what*: the reply is a fresh random
    /// number, so two different figures are indistinguishable by it. Identity
    /// still needs `inventory_unlocked`, which is worth paying once when a
    /// figure arrives rather than on every poll of an empty plate.
    ///
    /// A tag that is **not** in privacy mode answers this too — it is the first
    /// half of the password exchange, not a privacy-only command — so a caller
    /// does not have to know which state the tag is in before asking.
    pub fn tag_present(&mut self) -> Result<bool, Error<E>> {
        match self.get_random_number() {
            Ok(_) => Ok(true),
            Err(Error::Timeout) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn inventory_unlocked(&mut self, passwords: &[u32]) -> Result<Option<[u8; 8]>, Error<E>> {
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

        for &password in passwords {
            // Asked first and on its own, because privacy mode leaves exactly
            // this command open. Silence here is an empty plate rather than a
            // wrong password, and trying the rest would cost a field reset
            // per password for a tag that is not there.
            let random = match self.get_random_number() {
                Ok(random) => random,
                Err(Error::Timeout) => return Ok(None),
                Err(e) => return Err(e),
            };
            match self.set_password(password, random) {
                Ok(()) => return self.inventory(),
                // A password the tag does not hold is answered with silence,
                // and leaves it deaf to everything until its supply has been
                // interrupted. The next password would be shouted at a tag
                // that stopped listening, so the field goes down first —
                // including after the last one, so the tag is left usable
                // rather than mute for whoever polls next.
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
    /// The four bytes are returned exactly as the tag sent them. Unlike
    /// `inventory`'s UID, which ISO 15693 transmits least-significant byte
    /// first and which this driver therefore reverses, block data is raw
    /// memory with no such convention — reversing it here, the way
    /// `inventory` reverses its eight bytes, would silently corrupt every
    /// block read this way.
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
    /// One `read_block` exchange per block, rather than READ MULTIPLE BLOCKS
    /// (`0x23`): eight blocks in a single reply would be `1 + 32 = 33`
    /// bytes, one over `MAX_RESPONSE`, while a single-block reply is five
    /// bytes and sits comfortably inside both that and the twelve-byte FIFO
    /// that already ruled out addressed frames (see the handover note on
    /// `WRITE PASSWORD`). That arithmetic, not a missed optimisation, is why
    /// this stays as repeated single reads.
    ///
    /// `out.len()` must be a multiple of four; a partial block cannot be
    /// represented, so a length that is not is rejected before any bus
    /// traffic, the same way `transceive` rejects an oversized request
    /// before sending it.
    ///
    /// This exists to fetch the 32-byte content-pass teddyCloud relays as
    /// `Authorization: BD <64 hex>` — revvox's protocol analysis calls it
    /// the tag's memory-content, read off the tag rather than derived. Both
    /// of the questions this doc used to leave open were measured on a real
    /// Tonie figure at the bench on 2026-09-03:
    ///
    /// - **Which blocks hold it.** The whole user memory, blocks 0 to 7, and
    ///   nothing else: `first = 0` with a 32-byte `out`. Block 8 and beyond
    ///   do not answer, in a single run that straddles the boundary, so the
    ///   32 bytes are the token exactly with nothing spare.
    /// - **Whether privacy must be unlocked first.** It must. A figure in
    ///   privacy mode is *silent* to READ SINGLE BLOCK for every block,
    ///   exactly as it is to inventory. Unlock with `inventory_unlocked`
    ///   before calling this, or it returns `Timeout` for all of it.
    ///
    /// **Silence is ambiguous here, and callers must not read it as "past the
    /// end".** A tag in privacy mode, a tag that is not there, and a block
    /// beyond the last one all arrive as `Error::Timeout` — this tag answers
    /// an out-of-range block with nothing rather than the ISO 15693-3 §7.4
    /// error response ("block not available", code `0x03`) that would tell
    /// them apart. Anything walking memory to discover its size will read an
    /// absent or locked tag as a zero-length one.
    pub fn read_memory(&mut self, first: u8, out: &mut [u8]) -> Result<(), Error<E>> {
        // Checked before any bus traffic, matching `transceive`'s own length
        // check: a rejected call must leave the reader exactly as it was
        // found.
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
