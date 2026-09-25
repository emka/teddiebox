//! The TRF7962A reader, and the tags on the plate.
//!
//! The driver is `trf7962a`, tested on the host. This file has the pins, the
//! bus and the console commands.
//!
//! **A locked tag looks like a broken reader.** Tonie figures are in ICODE
//! SLIX privacy mode and do not answer inventory until unlocked. So the
//! reader's registers are read back at start-up: if they answer, "no tag"
//! is about the plate, not the wiring.

use embassy_time::Duration;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use teddiebox_console::MAX_MEMORY_BLOCKS;
use trf7962a::{Trf7962a, INIT_SEQUENCE};

/// The reader takes up to 2 Mbit/s (SLOS757C §5.12). Half that is plenty for
/// register access and a twelve-byte FIFO.
const BUS_RATE_KHZ: u32 = 1_000;

/// How many times the link is configured and read back before giving up.
///
/// **On this board it answers on the second attempt**, every boot: the first
/// comes just after its power rail (shared with the card) is turned on.
/// Twenty attempts is one second. Limited, so a miswired reader is reported
/// instead of hanging the task.
const LINK_ATTEMPTS: u8 = 20;
/// Room for the readback of every register `INIT_SEQUENCE` writes.
const MAX_INIT_REGISTERS: usize = 4;
const _: () = assert!(
    INIT_SEQUENCE.len() <= MAX_INIT_REGISTERS,
    "INIT_SEQUENCE has outgrown the readback buffer, and zip would drop the rest in silence"
);

/// Time between those attempts.
const LINK_RETRY_MS: u32 = 50;

type Blocking = esp_hal::Blocking;
type Device = ExclusiveDevice<Spi<'static, Blocking>, Output<'static>, Delay>;

/// The bus configuration the reader is driven at.
pub fn bus_config() -> SpiConfig {
    SpiConfig::default().with_frequency(Rate::from_khz(BUS_RATE_KHZ))
}

pub struct Reader {
    trf: Trf7962a<Device, Delay, Input<'static>>,
}

impl Reader {
    /// Brings the reader up and proves the link before any tag is involved.
    ///
    /// The storage rail must already be on: gate 47 powers the reader and the
    /// card, and the console loop turns it on.
    pub async fn open(
        spi: Spi<'static, Blocking>,
        cs: Output<'static>,
        irq: Input<'static>,
        delay: Delay,
    ) -> Result<Self, &'static str> {
        let device =
            ExclusiveDevice::new(spi, cs, delay).map_err(|_| "chip select would not drive")?;
        let mut trf = Trf7962a::new(device, delay, irq);

        // Configure, read back what was written, and retry until they match.
        // This checks the link without needing a tag.
        //
        // **Retried** because right after its power rail comes on, the reader
        // reads back 0x00 from every register, and answers correctly a little
        // later. Retrying waits exactly as long as needed.
        let mut agreed = false;
        let mut attempts = 0;
        // What the last attempt read, printed below, so the console shows
        // exactly what the decision was based on.
        let mut seen = [None; MAX_INIT_REGISTERS];
        while attempts < LINK_ATTEMPTS && !agreed {
            attempts += 1;
            if attempts > 1 {
                embassy_time::Timer::after(Duration::from_millis(u64::from(LINK_RETRY_MS))).await;
            }
            trf.init_iso15693()
                .map_err(|_| "the reader would not configure")?;
            agreed = true;
            for (slot, &(register, expected)) in seen.iter_mut().zip(INIT_SEQUENCE) {
                let actual = trf.read_register(register).ok();
                *slot = actual;
                agreed &= actual == Some(expected);
            }
            if !agreed && attempts % 10 == 0 {
                esp_println::println!(
                    "teddiebox: nfc still silent after {} ms",
                    u32::from(attempts) * LINK_RETRY_MS
                );
            }
        }

        // Printed once at the end, not for every failed attempt.
        for (slot, &(register, expected)) in seen.iter().zip(INIT_SEQUENCE) {
            match slot {
                Some(actual) if *actual == expected => {
                    esp_println::println!("teddiebox: nfc reg {register:#04x} = {actual:#04x}")
                }
                Some(actual) => esp_println::println!(
                    "teddiebox: nfc reg {register:#04x} reads {actual:#04x}, wrote {expected:#04x}"
                ),
                None => esp_println::println!("teddiebox: nfc reg {register:#04x} unreadable"),
            }
        }

        if agreed {
            esp_println::println!("teddiebox: nfc reader answers, field on ({attempts} attempts)");
        } else {
            // Not fatal: trying a tag anyway can still give useful results.
            esp_println::println!(
                "teddiebox: nfc link is not answering as written after {attempts} attempts \
                 — suspect SPI"
            );
        }

        Ok(Self { trf })
    }

    /// Reads whatever is on the plate, if it answers without unlocking.
    pub fn inventory(&mut self) {
        match self.trf.inventory() {
            Ok(Some(uid)) => report_uid("tag", &uid),
            Ok(None) => {
                esp_println::println!("teddiebox: nfc no answer");
                self.diagnose();
            }
            Err(trf7962a::Error::ReceiveError(flags)) => {
                report_receive_error(flags);
                self.diagnose();
            }
            Err(_) => {
                esp_println::println!("teddiebox: nfc inventory failed");
                self.diagnose();
            }
        }
    }

    /// Turns the reader's own field off, or back on.
    ///
    /// With the field off, register 0x0F reports the RF amplitude *arriving*
    /// at the antenna (SLOS757C Table 6-18: "RF amplitude during RF-off
    /// state"). This can check whether the antenna is connected, without a
    /// tag.
    pub fn set_field(&mut self, on: bool) {
        // Not just clearing rf_on: bit 5 is "transmitter on, receivers on"
        // (SLOS757C Table 6-2), so clearing it also turns the receiver off.
        // Bit 1, `rec_on`, is for this case: "receiver activated for external
        // field measurement — forces enabling of receiver and TX oscillator".
        const REC_ON: u8 = 0x02;
        // Bit 0 selects the supply range and must not change. Taken from the
        // driver's constant.
        const SUPPLY: u8 = 0x01;
        let listen = REC_ON | (trf7962a::regs::CHIP_STATUS_RF_ON & SUPPLY);
        let value = if on {
            trf7962a::regs::CHIP_STATUS_RF_ON
        } else {
            listen
        };
        if self
            .trf
            .write_register(trf7962a::regs::CHIP_STATUS_CONTROL, value)
            .is_err()
        {
            esp_println::println!("teddiebox: nfc could not change the field");
        }
    }

    /// The RSSI register's signal bits, with the oscillator flag masked off.
    pub fn rssi(&mut self) -> u8 {
        const RSSI: u8 = 0x0F;
        match self.trf.read_register(RSSI) {
            Ok(value) => value & 0x3F,
            Err(_) => 0,
        }
    }

    /// Says whether the reader heard anything at all.
    ///
    /// No answer can mean no tag, a tag of the wrong type, or a reply the
    /// driver could not parse. The interrupt status register shows whether a
    /// reception even started.
    ///
    /// Reading it clears it, so this runs once, right after the attempt.
    fn diagnose(&mut self) {
        // The interrupt status first, read the way SPI mode requires (address
        // 0x6C plus a dummy byte, SLOS757C §6.12.6). Read as an ordinary
        // register, it never clears.
        match self.trf.read_irq_status() {
            Ok(value) => esp_println::println!("teddiebox: nfc   irq status {value:#04x} (0x6C)"),
            Err(_) => esp_println::println!("teddiebox: nfc   irq status unreadable"),
        }

        // The line, separately from the register: if they disagree, the
        // interrupt is not reaching the GPIO.
        match self.trf.irq_asserted() {
            Ok(true) => esp_println::println!("teddiebox: nfc   irq line high"),
            Ok(false) => esp_println::println!("teddiebox: nfc   irq line low"),
            Err(_) => esp_println::println!("teddiebox: nfc   irq line unreadable"),
        }

        // 0x0F is the RSSI register (SLOS757C §6.14.1.3.3): whether any signal
        // is coming back. 0x1D and 0x1E are the transmit length just written:
        // they show whether the transmit setup reached the chip.
        for (name, register) in [
            ("chip status 0x00", 0x00u8),
            ("iso control 0x01", 0x01),
            ("fifo status 0x1C", trf7962a::regs::FIFO_STATUS),
            ("tx length 0x1D", trf7962a::regs::TX_LENGTH_BYTE1),
            ("tx length 0x1E", trf7962a::regs::TX_LENGTH_BYTE2),
            ("rssi 0x0F", 0x0F),
        ] {
            match self.trf.read_register(register) {
                Ok(value) => esp_println::println!("teddiebox: nfc   {name} = {value:#04x}"),
                Err(_) => esp_println::println!("teddiebox: nfc   {name} unreadable"),
            }
        }
    }

    /// Unlocks a Tonie's privacy mode, then reads it.
    ///
    /// GET RANDOM NUMBER is sent first on its own. A locked SLIX ignores
    /// inventory but answers this, so it tells a locked tag from an empty
    /// plate.
    ///
    /// The unlock fetches its own random number; this one is not reused.
    pub fn unlock(&mut self, password: u32) {
        match self.trf.get_random_number() {
            Ok(random) => esp_println::println!(
                "teddiebox: nfc tag answered GET RANDOM NUMBER ({random:#06x}) — present and SLIX"
            ),
            Err(trf7962a::Error::ReceiveError(flags)) => {
                report_receive_error(flags);
                self.diagnose();
                return;
            }
            Err(_) => {
                esp_println::println!(
                    "teddiebox: nfc no answer to GET RANDOM NUMBER — nothing in the field"
                );
                self.diagnose();
                return;
            }
        }

        match self.trf.inventory_unlocked(&passwords(password)) {
            Ok(Some(uid)) => report_uid("unlocked tag", &uid),
            Ok(None) => esp_println::println!(
                "teddiebox: nfc still silent after unlock — wrong password, or still locked"
            ),
            // A SLIX can refuse with an error response carrying a code
            // (ISO 15693-3 §7.4).
            Err(trf7962a::Error::TagError(code)) => {
                esp_println::println!("teddiebox: nfc tag refused the password (error {code:#04x})")
            }
            Err(trf7962a::Error::ReceiveError(flags)) => report_receive_error(flags),
            Err(_) => esp_println::println!("teddiebox: nfc unlock failed"),
        }
    }

    /// The most interrupt polls any answered exchange has needed, and that
    /// time in microseconds.
    ///
    /// Used to size the reply window. Read out when polling stops, since the
    /// worst case over a whole run is what matters.
    pub fn slowest_reply(&self) -> (u32, u32) {
        let polls = self.trf.slowest_reply_polls();
        (polls, polls * trf7962a::IRQ_POLL_INTERVAL_US)
    }

    /// Whether anything is on the plate, without unlocking it.
    ///
    /// The only command a locked Tonie answers (SL2S5002 §1.3). An empty
    /// plate costs one unanswered exchange, and a locked figure is noticed
    /// without the password exchange.
    ///
    /// A bus error counts as "nothing there".
    pub fn tag_present(&mut self) -> bool {
        self.trf.tag_present().unwrap_or(false)
    }

    /// The UID of a tag that is already out of privacy mode, quietly.
    ///
    /// An unlocked figure stays unlocked until its field is turned off, so
    /// while it is on the plate this needs no password. Reading the UID each
    /// time (rather than remembering it) notices a figure being swapped.
    pub fn identify(&mut self) -> Option<[u8; 8]> {
        self.trf.inventory().ok()?
    }

    /// Whatever is on the plate, unlocked if needed, without printing.
    ///
    /// For the poll loop. Uses the same password list as `unlock`.
    pub fn inventory_unlocked(&mut self, password: u32) -> Option<[u8; 8]> {
        self.trf.inventory_unlocked(&passwords(password)).ok()?
    }

    /// Test command: sends SET PASSWORD whatever the tag's state.
    ///
    /// `unlock` never sends it to an already unlocked tag, because the
    /// inventory it tries first succeeds. This tests the eight-byte password
    /// exchange, the longest frame the reader sends.
    ///
    /// The registers are printed afterwards in every case, to show the
    /// reader's state.
    pub fn force_unlock(&mut self, password: u32) {
        // The two exchanges run separately, not through `unlock_privacy`,
        // because no answer means different things for each: no random
        // number means no tag, while no answer to the password means the tag
        // refused it.
        let random = match self.trf.get_random_number() {
            Ok(random) => random,
            Err(trf7962a::Error::ReceiveError(flags)) => {
                report_receive_error(flags);
                self.diagnose();
                return;
            }
            Err(_) => {
                esp_println::println!("teddiebox: nfc   GET RANDOM NUMBER -> silent");
                self.diagnose();
                return;
            }
        };

        // Nothing is printed between the two exchanges: a console line takes
        // milliseconds at 115200 baud, which would change the timing.
        let outcome = self.trf.set_password(password, random);
        esp_println::println!("teddiebox: nfc   GET RANDOM NUMBER -> {random:#06x}");
        match outcome {
            Ok(()) => esp_println::println!("teddiebox: nfc   SET PASSWORD -> accepted"),
            Err(trf7962a::Error::TagError(code)) => {
                esp_println::println!("teddiebox: nfc   SET PASSWORD -> refused ({code:#04x})")
            }
            Err(trf7962a::Error::ReceiveError(flags)) => report_receive_error(flags),
            Err(trf7962a::Error::Timeout) => {
                // No answer means a wrong password, and the tag now ignores
                // everything until its field is turned off. Reset it so the
                // next attempt works.
                esp_println::println!("teddiebox: nfc   SET PASSWORD -> silent, resetting the tag");
                if self.trf.reset_tags().is_err() {
                    esp_println::println!("teddiebox: nfc   could not cycle the field");
                }
            }
            Err(_) => esp_println::println!("teddiebox: nfc   SET PASSWORD -> failed"),
        }
        self.diagnose();
    }

    /// Test command: puts the tag back into privacy mode.
    ///
    /// Figures come locked and the stock firmware re-locks them after
    /// reading, so this restores a test tag to that state.
    pub fn lock(&mut self, password: u32) {
        match self.trf.enable_privacy(password) {
            Ok(()) => {
                esp_println::println!("teddiebox: nfc tag is in privacy mode again");
                // Check: a locked tag stops answering inventory.
                match self.trf.inventory() {
                    Ok(Some(uid)) => report_uid("still readable — lock did NOT take", &uid),
                    Ok(None) => {
                        esp_println::println!("teddiebox: nfc   silent to inventory — locked")
                    }
                    Err(_) => esp_println::println!("teddiebox: nfc   inventory failed"),
                }
            }
            Err(trf7962a::Error::TagError(code)) => {
                esp_println::println!("teddiebox: nfc tag refused to lock (error {code:#04x})")
            }
            Err(trf7962a::Error::Timeout) => {
                esp_println::println!("teddiebox: nfc no answer to the lock — wrong password?");
                if self.trf.reset_tags().is_err() {
                    esp_println::println!("teddiebox: nfc   could not cycle the field");
                }
            }
            Err(_) => esp_println::println!("teddiebox: nfc lock failed"),
        }
    }

    /// Reads the tag's whole user memory, which is its cloud token.
    ///
    /// Eight blocks of four bytes: 32, the length teddyCloud reads after
    /// `Authorization: BD `. All or nothing: a partial token would be wrong.
    ///
    /// **Does not unlock.** A locked tag does not answer, so use `pw` and
    /// `slix` first, as for `mem`.
    ///
    /// **The result is a credential** and is not printed.
    pub fn read_token(&mut self) -> Option<[u8; 32]> {
        let mut token = [0u8; 32];
        for block in 0..8u8 {
            let data = self.trf.read_block(block).ok()?;
            token[block as usize * 4..][..4].copy_from_slice(&data);
        }
        Some(token)
    }

    /// Test command: prints a tag's memory, one block at a time.
    ///
    /// The token teddyCloud forwards as `Authorization: BD <64 hex>` is the
    /// tag's memory: blocks 0 to 7 of a Tonie. Block 8 onward does not
    /// answer.
    ///
    /// **Does not unlock first**: a locked figure does not answer any block,
    /// so use `pw` and `slix` first.
    ///
    /// Reads blocks one by one, rather than with `read_memory`, to show which
    /// block fails first.
    ///
    /// The printed bytes are a credential; do not paste them into bug
    /// reports.
    pub fn dump_memory(&mut self, first: u8, count: u8) {
        let mut whole = [0u8; 4 * MAX_MEMORY_BLOCKS as usize];
        let mut read = 0usize;
        let mut complete = true;

        for index in 0..count {
            let block = first.wrapping_add(index);
            match self.trf.read_block(block) {
                Ok(data) => {
                    esp_println::println!(
                        "teddiebox: nfc mem {:02X} {:02X}{:02X}{:02X}{:02X}",
                        block,
                        data[0],
                        data[1],
                        data[2],
                        data[3]
                    );
                    whole[read..read + 4].copy_from_slice(&data);
                    read += 4;
                }
                // A tag may refuse with an error response (ISO 15693-3 §7.4),
                // for example past its last block. A result, not a fault.
                Err(trf7962a::Error::TagError(code)) => {
                    esp_println::println!(
                        "teddiebox: nfc mem {block:02X} refused (error {code:#04x})"
                    );
                    complete = false;
                }
                Err(trf7962a::Error::Timeout) => {
                    esp_println::println!("teddiebox: nfc mem {block:02X} no answer");
                    complete = false;
                }
                Err(trf7962a::Error::ReceiveError(flags)) => {
                    esp_println::println!("teddiebox: nfc mem {block:02X}:");
                    report_receive_error(flags);
                    complete = false;
                }
                Err(_) => {
                    esp_println::println!("teddiebox: nfc mem {block:02X} failed");
                    complete = false;
                }
            }
        }

        // Only print the whole run if no block failed; otherwise it would
        // look complete.
        if !complete {
            esp_println::println!(
                "teddiebox: nfc mem — run incomplete, so not printed whole; \
                 the first block to refuse is where the readable memory ends"
            );
            return;
        }

        esp_println::print!(
            "teddiebox: nfc mem {:02X}..{:02X} ",
            first,
            first.wrapping_add(count - 1)
        );
        for byte in &whole[..read] {
            esp_println::print!("{byte:02X}");
        }
        esp_println::println!(" ({read} bytes)");
    }
}

/// The passwords to try on a tag, in the order they are expected.
///
/// The Toniebox password first, since figures use it. NXP's factory default
/// second, so a new, blank SLIX-L (useful for testing) can be read too. A
/// wrong password only costs a field reset.
fn passwords(tonie: u32) -> [u32; 2] {
    [tonie, trf7962a::slix::VENDOR_DEFAULT_PASSWORD]
}

/// Names the reader's own reason for rejecting a reception.
///
/// SLOS757G Table 6-29, B4 to B1. A CRC error and a framing error have
/// different causes, so each is named.
fn report_receive_error(flags: u8) {
    esp_println::println!("teddiebox: nfc reader rejected the reply ({flags:#04x}):");
    for (bit, meaning) in [
        (0x10u8, "CRC error"),
        (0x08, "parity error"),
        (0x04, "byte framing or EOF error"),
        (0x02, "collision, or noise in the receiver"),
    ] {
        if flags & bit != 0 {
            esp_println::println!("teddiebox: nfc   {meaning}");
        }
    }
}

/// Prints a UID both ways round.
///
/// ISO 15693 sends a UID least-significant byte first; teddyCloud and the
/// figure itself show it the other way round.
fn report_uid(what: &str, uid: &[u8; 8]) {
    esp_println::println!(
        "teddiebox: nfc {what} UID {:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X} (as sent)",
        uid[0],
        uid[1],
        uid[2],
        uid[3],
        uid[4],
        uid[5],
        uid[6],
        uid[7]
    );
    esp_println::println!(
        "teddiebox: nfc {what} UID {:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}{:02X} (reversed)",
        uid[7],
        uid[6],
        uid[5],
        uid[4],
        uid[3],
        uid[2],
        uid[1],
        uid[0]
    );
}
