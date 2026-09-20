//! The TRF7962A reader, and the tags on the plate.
//!
//! Bench step 10. The driver is `trf7962a`, host-tested against recorded bus
//! transactions; what is here is the pins, the bus and the bench commands.
//!
//! **A locked tag looks exactly like a broken reader.** Tonie figures sit in
//! ICODE SLIX privacy mode and do not answer inventory at all until they are
//! unlocked, so silence proves nothing on its own. That is why the reader's
//! configuration registers are read back at start-up: a link that answers
//! turns "no tag" into a statement about the plate rather than about the
//! wiring.

use embassy_time::Duration;
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use teddiebox_console::MAX_MEMORY_BLOCKS;
use trf7962a::{Trf7962a, INIT_SEQUENCE};

/// The reader takes up to 2 Mbit/s (SLOS757C §5.12). Half that is plenty for
/// register pokes and a twelve-byte FIFO, and leaves margin on a bench wire.
const BUS_RATE_KHZ: u32 = 1_000;

/// How many times the link is configured and read back before giving up.
///
/// **Measured on this board: it answers on the second attempt**, every boot.
/// The first one lands while the rail this shares with the card has only just
/// been raised. Twenty is a second, which is ample, and bounded rather than
/// endless because a genuinely miswired reader must still reach the message
/// that says so rather than hanging the task that would print it.
const LINK_ATTEMPTS: u8 = 20;
/// How long between those attempts. Twenty of these is a second, which is far
/// longer than any settling this part is documented to need and still short
/// enough that a real fault is reported while somebody is still watching.
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
    /// The storage rail must already be on: gate 47 feeds the reader as well
    /// as the card, and powering it is the console loop's business.
    pub async fn open(
        spi: Spi<'static, Blocking>,
        cs: Output<'static>,
        irq: Input<'static>,
        delay: Delay,
    ) -> Result<Self, &'static str> {
        let device =
            ExclusiveDevice::new(spi, cs, delay).map_err(|_| "chip select would not drive")?;
        let mut trf = Trf7962a::new(device, delay, irq);

        // Configure, then read back what was written, and keep trying until
        // the two agree. This is the reader's equivalent of the codec's
        // power-flags register: it separates "asked wrongly" from "not
        // listening", and it is the only check here that does not depend on a
        // tag being present.
        //
        // **Retried because the answer changes with time.** The reader shares
        // gate 47 with the card, and a box that polls the plate from boot
        // opens it as soon as that rail is raised — where every register
        // reads back 0x00 and the readback reports "suspect SPI" against a
        // part that is merely still coming up. The same reader answers
        // perfectly a few seconds later. A fixed settling delay would only be
        // a guess at a number nobody has measured; this waits for the thing
        // that actually matters, and stops as soon as it is true.
        let mut agreed = false;
        let mut attempts = 0;
        while attempts < LINK_ATTEMPTS && !agreed {
            attempts += 1;
            if attempts > 1 {
                embassy_time::Timer::after(Duration::from_millis(u64::from(LINK_RETRY_MS))).await;
            }
            trf.init_iso15693()
                .map_err(|_| "the reader would not configure")?;
            agreed = INIT_SEQUENCE
                .iter()
                .all(|&(register, expected)| trf.read_register(register) == Ok(expected));
            if !agreed && attempts % 10 == 0 {
                esp_println::println!(
                    "teddiebox: nfc still silent after {} ms",
                    u32::from(attempts) * LINK_RETRY_MS
                );
            }
        }

        // Printed once the outcome is settled, so a box that takes three
        // attempts does not fill the console with the two that failed.
        for &(register, expected) in INIT_SEQUENCE {
            match trf.read_register(register) {
                Ok(actual) if actual == expected => {
                    esp_println::println!("teddiebox: nfc reg {register:#04x} = {actual:#04x}")
                }
                Ok(actual) => esp_println::println!(
                    "teddiebox: nfc reg {register:#04x} reads {actual:#04x}, wrote {expected:#04x}"
                ),
                Err(_) => {
                    esp_println::println!("teddiebox: nfc reg {register:#04x} unreadable")
                }
            }
        }

        if agreed {
            esp_println::println!("teddiebox: nfc reader answers, field on ({attempts} attempts)");
        } else {
            // Not fatal: the bench should still be allowed to try a tag, and
            // seeing both results is more useful than refusing to continue.
            esp_println::println!(
                "teddiebox: nfc link is not answering as written after {attempts} attempts \
                 — suspect SPI"
            );
        }

        Ok(Self { trf })
    }

    /// Bench step 10a: whatever is on the plate, if it answers unlocked.
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
    /// state"). That turns the reader into a field detector, which is the only
    /// way to ask whether the antenna is connected without involving a tag.
    pub fn set_field(&mut self, on: bool) {
        // Not simply clearing rf_on. SLOS757C Table 6-2 defines bit 5 as
        // "transmitter on, receivers on", so clearing it alone silences the
        // receiver too and the measurement reads zero whatever the antenna is
        // doing. Bit 1, `rec_on`, exists for precisely this case: "receiver
        // activated for external field measurement — forces enabling of
        // receiver and TX oscillator".
        const REC_ON: u8 = 0x02;
        // Bit 0 selects the supply range, and turning the transmitter off is
        // no reason to change it. Taken from the driver's own word rather
        // than restated, so the two cannot disagree about which rail this
        // board has.
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
    /// Silence has three quite different causes — no tag in the field, a tag
    /// of the wrong family, or a reply the driver could not parse — and they
    /// are worth telling apart before anyone moves an antenna. The interrupt
    /// status register is the one that knows: it latches whether a reception
    /// even started.
    ///
    /// Reading it clears it, so this runs once, immediately after the attempt.
    fn diagnose(&mut self) {
        // The interrupt status first, and read the way SPI mode requires
        // (address 0x6C plus a dummy byte, SLOS757C §6.12.6). Read as an
        // ordinary register it never clears, so a stale or empty value here
        // says nothing about whether the reader transmitted — and "the reader
        // never transmits" is exactly the conclusion this bench has been
        // drawing from it.
        match self.trf.read_irq_status() {
            Ok(value) => esp_println::println!("teddiebox: nfc   irq status {value:#04x} (0x6C)"),
            Err(_) => esp_println::println!("teddiebox: nfc   irq status unreadable"),
        }

        // The line, separately from the register. They disagree in the one
        // case worth naming: an interrupt the reader latched and the wiring
        // never delivered, which is a wrong GPIO rather than a dead reader.
        match self.trf.irq_asserted() {
            Ok(true) => esp_println::println!("teddiebox: nfc   irq line high"),
            Ok(false) => esp_println::println!("teddiebox: nfc   irq line low"),
            Err(_) => esp_println::println!("teddiebox: nfc   irq line unreadable"),
        }

        // 0x0F is the RSSI register (SLOS757C §6.14.1.3.3); the driver has no
        // name for it because nothing in the protocol needs it, but at a bench
        // it says whether there is any energy coming back. 0x1D and 0x1E are
        // the transmit length the driver just wrote: read back, they say
        // whether the transmit setup reached the part at all.
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

    /// Bench step 10b: unlock a Tonie's privacy mode, then read it.
    ///
    /// GET RANDOM NUMBER is asked first, on its own. A SLIX in privacy mode
    /// refuses inventory but *does* answer this — that is what makes the
    /// unlock possible at all — so its answer separates the two silences that
    /// otherwise look identical: a locked tag sitting on the plate, and no tag
    /// in the field at all. Without it, a wrong password and an empty plate
    /// report the same thing.
    ///
    /// The random number it spends is not the one the unlock uses; the driver
    /// fetches its own, immediately before masking the password with it.
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
            // A SLIX that refuses the password says so, rather than going
            // quiet: ISO 15693-3 §7.4, an error response carrying a code.
            Err(trf7962a::Error::TagError(code)) => {
                esp_println::println!("teddiebox: nfc tag refused the password (error {code:#04x})")
            }
            Err(trf7962a::Error::ReceiveError(flags)) => report_receive_error(flags),
            Err(_) => esp_println::println!("teddiebox: nfc unlock failed"),
        }
    }

    /// Whatever is on the plate right now, unlocked if it needs to be —
    /// quietly.
    ///
    /// The poll loop's instrument: `unlock` prints for a person reading a
    /// console, and a reading taken several times a second has no person
    /// reading it. Delegates to the same `inventory_unlocked` and the same
    /// password list `unlock` builds, so the two paths cannot silently
    /// disagree about which passwords a poll is willing to try.
    /// The most interrupt polls any answered exchange has needed, and what
    /// that is in microseconds at the driver's poll interval.
    ///
    /// The number the reply window should be sized against. It is read out
    /// when polling stops rather than printed as it changes, because a figure
    /// on the plate is polled several times a second and the interesting value
    /// is the worst one across a whole run.
    pub fn slowest_reply(&self) -> (u32, u32) {
        let polls = self.trf.slowest_reply_polls();
        (polls, polls * trf7962a::IRQ_POLL_INTERVAL_US)
    }

    /// Whether anything is on the plate, without unlocking it.
    ///
    /// The one question a Tonie in privacy mode answers (SL2S5002 §1.3), and
    /// the reason the poller can afford to run at all: an empty plate costs one
    /// unanswered exchange here instead of two, and a locked figure is noticed
    /// without the password exchange that identifying it would need.
    ///
    /// A bus fault reads as "nothing there", which is the same answer the
    /// poller would reach anyway and keeps this off the error path of a loop
    /// that runs several times a second.
    pub fn tag_present(&mut self) -> bool {
        self.trf.tag_present().unwrap_or(false)
    }

    /// The UID of a tag that is already out of privacy mode, quietly.
    ///
    /// Once a figure has been unlocked it stays unlocked until its field is
    /// cycled, so for the whole time it sits on the plate this answers on the
    /// first try with no password exchange at all. Re-reading the UID rather
    /// than remembering it is what lets one figure being swapped for another
    /// be noticed.
    pub fn identify(&mut self) -> Option<[u8; 8]> {
        self.trf.inventory().ok()?
    }

    pub fn inventory_unlocked(&mut self, password: u32) -> Option<[u8; 8]> {
        self.trf.inventory_unlocked(&passwords(password)).ok()?
    }

    /// Bench instrument: put SET PASSWORD on the air whatever the tag's state.
    ///
    /// `unlock` cannot reach it on a tag that is already out of privacy mode,
    /// because the inventory it tries first answers and it stops there — and
    /// a SLIX stays out of privacy mode until something puts it back, which
    /// this driver has no command to do. That leaves the eight-byte password
    /// exchange, the longest frame the reader sends and the only one a tag
    /// can refuse, unreachable at the bench.
    ///
    /// The registers are read out afterwards whatever happens, because the
    /// question this exists to answer is what state a refused exchange leaves
    /// the reader in.
    pub fn force_unlock(&mut self, password: u32) {
        // The two exchanges are run separately rather than through
        // `unlock_privacy`, because silence from each means something quite
        // different — a tag that will not give a random number is not
        // answering at all, while one that gives a random number and then
        // ignores the password is answering selectively — and the bundled
        // call reports both as the same timeout.
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

        // Nothing is printed between the two exchanges. A console line is
        // milliseconds at 115200, and putting one here is a delay disguised
        // as a diagnostic — the exact variable under test.
        let outcome = self.trf.set_password(password, random);
        esp_println::println!("teddiebox: nfc   GET RANDOM NUMBER -> {random:#06x}");
        match outcome {
            Ok(()) => esp_println::println!("teddiebox: nfc   SET PASSWORD -> accepted"),
            Err(trf7962a::Error::TagError(code)) => {
                esp_println::println!("teddiebox: nfc   SET PASSWORD -> refused ({code:#04x})")
            }
            Err(trf7962a::Error::ReceiveError(flags)) => report_receive_error(flags),
            Err(trf7962a::Error::Timeout) => {
                // Silence means the tag does not hold this password, and it
                // now ignores everything until its field is cycled. Reset it
                // here so the next attempt can be typed straight away —
                // needing `rb` to un-stick a tag was only ever a side effect
                // of rebooting dropping the reader's rail.
                esp_println::println!("teddiebox: nfc   SET PASSWORD -> silent, resetting the tag");
                if self.trf.reset_tags().is_err() {
                    esp_println::println!("teddiebox: nfc   could not cycle the field");
                }
            }
            Err(_) => esp_println::println!("teddiebox: nfc   SET PASSWORD -> failed"),
        }
        self.diagnose();
    }

    /// Bench: put the tag back into privacy mode.
    ///
    /// A figure arrives locked and the stock firmware re-locks it after
    /// reading, so this is what restores a bench tag to a realistic state.
    /// It is also the only command here that makes a tag harder to read, so
    /// it reports the UID it is about to lock away.
    pub fn lock(&mut self, password: u32) {
        match self.trf.enable_privacy(password) {
            Ok(()) => {
                esp_println::println!("teddiebox: nfc tag is in privacy mode again");
                // Proof rather than assertion: a locked tag stops answering
                // inventory, so the same command that reads it also confirms
                // the lock took.
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

    /// Bench instrument: read a tag's memory out, one block at a time.
    ///
    /// This is what settles where the teddyCloud auth token lives. teddyCloud
    /// relays the box's `Authorization: BD <64 hex>` upstream verbatim and
    /// never validates it, and revvox's protocol analysis calls that value the
    /// memory content of the tag — so the reader already on this board can
    /// read it. Measured on a figure on 2026-09-03: the token is the whole
    /// user memory, blocks 0 to 7, and block 8 onward does not answer.
    ///
    /// **Deliberately does not unlock first**, which is what let this settle
    /// the second question: a figure in privacy mode is silent to every block,
    /// exactly as it is to inventory, so `pw` and `slix` are a precondition
    /// for reading the token rather than an optional step. Keeping the unlock
    /// out of here is also what makes the two states comparable at all.
    ///
    /// Blocks are read singly rather than through `read_memory` because that
    /// stops at the first failure and does not say which block failed — and
    /// which block first refuses is the measurement wanted here.
    ///
    /// The bytes this prints are a credential. They belong on a bench console
    /// and not in a bug report.
    /// Reads the tag's whole user memory, which is its cloud token.
    ///
    /// Eight blocks of four bytes: thirty-two, the length teddyCloud reads
    /// after `Authorization: BD `. All or nothing — a token assembled from the
    /// blocks that happened to answer would be silently wrong, and the server
    /// that rejects it is two network hops away from the cause.
    ///
    /// **Does not unlock.** A privacy-locked tag is silent to every block, so
    /// `pw` and `slix` come first, exactly as for `mem`.
    ///
    /// **What this returns is a credential.** It is deliberately not printed.
    pub fn read_token(&mut self) -> Option<[u8; 32]> {
        let mut token = [0u8; 32];
        for block in 0..8u8 {
            let data = self.trf.read_block(block).ok()?;
            token[block as usize * 4..][..4].copy_from_slice(&data);
        }
        Some(token)
    }

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
                // A tag that refuses says so rather than going quiet
                // (ISO 15693-3 §7.4), and reading past the last block is
                // exactly how that refusal is provoked. This is a result, not
                // a fault.
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

        // Only a run with no gap in it is printed whole. A dump assembled from
        // the blocks that happened to answer would read exactly like a
        // complete one, which is the mistake worth spending a branch on.
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
/// The Toniebox's own first, since that is what a figure holds. NXP's factory
/// default second, so a plain SLIX-L off the reel — the sort of tag used to
/// test this without risking a figure — reads too. A wrong password costs a
/// field reset rather than an error, so the fallback is cheap and only
/// happens on tags the first password does not fit.
fn passwords(tonie: u32) -> [u32; 2] {
    [tonie, trf7962a::slix::VENDOR_DEFAULT_PASSWORD]
}

/// Names the reader's own reason for rejecting a reception.
///
/// SLOS757G Table 6-29, B4 to B1. A reply that arrives and fails CRC is a
/// different problem from one the decoder could not frame at all, and at a
/// bench that difference decides whether to look at the antenna or at the
/// protocol settings.
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
/// ISO 15693 sends a UID least-significant byte first, while everything that
/// writes one down — teddyCloud, the moulding on a figure — shows it the other
/// way. Printing both saves guessing which one is being looked at.
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
