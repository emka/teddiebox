//! The TRF7962A reader, and the tags on the plate.
//!
//! The driver is `trf7962a`, tested on the host. This file has the pins,
//! the bus, the console commands, and the task that owns the reader: it
//! polls the plate and runs the tag commands other tasks ask for.
//!
//! **A locked tag looks like a broken reader.** Tonie figures are in ICODE
//! SLIX privacy mode and do not answer inventory until unlocked. So the
//! reader's registers are read back at start-up: if they answer, "no tag"
//! is about the plate, not the wiring.

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use critical_section::Mutex as CsMutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use teddiebox_console::MAX_MEMORY_BLOCKS;
use teddiebox_core::plate::{Presence, Seen, TagEvent, ARRIVALS_TO_AGREE, MISSES_TO_LEAVE};
use teddiebox_core::TagUid;
use trf7962a::{Trf7962a, INIT_SEQUENCE};

use crate::{park_task, PARKED, REQUEST_NONE};

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

/// What the console has asked the NFC reader to do.
///
/// Separate from [`crate::REQUEST`] because the reader has its own SPI bus
/// and shares only the power rail, so it has no reason to queue behind a
/// track that is still playing.
pub(crate) static NFC_REQUEST: AtomicU8 = AtomicU8::new(REQUEST_NONE);
pub(crate) const NFC_INVENTORY: u8 = 1;
pub(crate) const NFC_UNLOCK: u8 = 2;
pub(crate) const NFC_FORCE_UNLOCK: u8 = 3;
pub(crate) const NFC_LOCK: u8 = 4;
pub(crate) const NFC_READ_MEMORY: u8 = 5;
pub(crate) const NFC_READ_TOKEN: u8 = 6;

/// The tag's memory, kept to spend on a download.
///
/// Kept in RAM only, lost at reset, and never printed, like the SLIX password
/// and the Wi-Fi passphrase: the cloud accepts it as proof that this box owns
/// the figure.
///
/// Written only by the console `token` command and read only by the console
/// `get` command. A figure placed on the plate sends its own token inside
/// [`crate::FETCH_REQUEST`], so a console `token` cannot be attached to it.
pub(crate) static TAG_TOKEN: CsMutex<RefCell<Option<[u8; 32]>>> = CsMutex::new(RefCell::new(None));

/// The SLIX privacy password baked in at build time, or zero for none.
///
/// From `TEDDIEBOX_SLIX_PASSWORD` in the build environment (`.envrc.local`
/// locally, a repository secret in CI), never from the repository itself.
/// `build.rs` declares it, so changing it triggers a rebuild.
///
/// **This puts a credential in the image**: anyone with a built binary can
/// read it. Accepted, because the alternative is typing it every session, and
/// a box without it cannot read any figure. A tag with the wrong or missing
/// password is *silent*, which looks just like an empty plate or a broken
/// antenna.
///
/// An unset or empty variable gives zero: nothing is unlocked until someone
/// types `pw`.
const BUILT_IN_PASSWORD: u32 = match option_env!("TEDDIEBOX_SLIX_PASSWORD") {
    None => 0,
    Some(text) => {
        if text.is_empty() {
            0
        } else {
            match teddiebox_console::hex::u32_from_hex(text.as_bytes()) {
                Some(value) => value,
                // Fail the build: on the box, a bad password only shows up
                // as a tag that never answers.
                None => panic!("TEDDIEBOX_SLIX_PASSWORD must be exactly eight hex digits"),
            }
        }
    }
};

/// The SLIX privacy password in force.
///
/// Starts as [`BUILT_IN_PASSWORD`] and can be replaced with the console `pw`
/// command, to work with other tags. RAM only; never written to the card.
pub(crate) static NFC_PASSWORD: AtomicU32 = AtomicU32::new(BUILT_IN_PASSWORD);

/// The block range a pending `mem` carries: first block in the high byte,
/// block count in the low one.
///
/// One word rather than two, so the reader task cannot see the first block of
/// one command with the count of the next.
pub(crate) static NFC_MEM_RANGE: AtomicU32 = AtomicU32::new(0);

/// Whether the reader polls the plate on its own. On at boot.
///
/// **On by default**, because a release image only accepts `dl` on the
/// console, so it could never switch polling on. `plate off` switches it off
/// for one session, to keep automatic tag unlocking out of a measurement.
///
/// Polling an empty plate causes about 5 audio DMA restarts in 70 s, and none
/// with a figure present. Lifting the figure pauses the story
/// (`Playback::on_tag_absent`), so nothing is playing while the plate is
/// empty, and the restarts cannot be heard.
pub(crate) static PLATE_POLLING: AtomicBool = AtomicBool::new(true);
/// Asks the reader task to print the slowest reply it has seen. Set when
/// polling is switched off, because that is when a run is over.
pub(crate) static PLATE_REPORT: AtomicBool = AtomicBool::new(false);

/// What is on the plate right now — the current state, not a queue of edges.
///
/// A `Signal` rather than a channel, because the media task can be busy for a
/// long time and a queue would need an arbitrary depth. Keeping only the
/// latest state cannot overflow; a figure placed and lifted within one media
/// pass is never seen, which is fine.
pub(crate) static PLATE_TAG: Signal<CriticalSectionRawMutex, Seen> = Signal::new();

/// Serves one console `nfc` command, if a request is pending.
async fn handle_console_request(reader: &mut Reader, request: u8) {
    match request {
        NFC_INVENTORY => {
            reader.inventory();

            // Checks the antenna is connected. With our own field off, the
            // RSSI register shows RF from outside, such as a phone, which
            // proves the coil reaches the chip. Nothing else can tell a
            // disconnected antenna from an empty plate.
            esp_println::println!(
                "teddiebox: nfc listening for an external field for 6 s — \
                 hold an NFC phone against the plate"
            );
            reader.set_field(false);
            let mut peak = 0u8;
            for _ in 0..60 {
                peak = peak.max(reader.rssi());
                Timer::after(Duration::from_millis(100)).await;
            }
            reader.set_field(true);
            if peak == 0 {
                esp_println::println!(
                    "teddiebox: nfc heard nothing at all — the antenna is not coupled"
                );
            } else {
                esp_println::println!(
                    "teddiebox: nfc external field peaked at {peak:#04x} — the antenna works"
                );
            }
        }
        NFC_UNLOCK => {
            let password = NFC_PASSWORD.load(Ordering::Relaxed);
            if password == 0 {
                esp_println::println!("teddiebox: nfc no password set — type `pw <8 hex>`");
            } else {
                reader.unlock(password);
            }
        }
        NFC_FORCE_UNLOCK => {
            let password = NFC_PASSWORD.load(Ordering::Relaxed);
            if password == 0 {
                esp_println::println!("teddiebox: nfc no password set — type `pw <8 hex>`");
            } else {
                reader.force_unlock(password);
            }
        }
        NFC_LOCK => {
            let password = NFC_PASSWORD.load(Ordering::Relaxed);
            if password == 0 {
                esp_println::println!("teddiebox: nfc no password set — type `pw <8 hex>`");
            } else {
                reader.lock(password);
            }
        }
        NFC_READ_MEMORY => {
            let range = NFC_MEM_RANGE.load(Ordering::Relaxed);
            reader.dump_memory((range >> 8) as u8, range as u8);
        }
        NFC_READ_TOKEN => match reader.read_token() {
            Some(token) => {
                critical_section::with(|cs| *TAG_TOKEN.borrow_ref_mut(cs) = Some(token));
                // The length, never the value.
                esp_println::println!(
                    "teddiebox: nfc token read, {} bytes — `get` will send it",
                    token.len()
                );
            }
            None => esp_println::println!(
                "teddiebox: nfc token unreadable — unlock first with `pw` then `slix`"
            ),
        },
        _ => {}
    }
}

/// Polls the plate if it is due, and tells the media task about any change.
///
/// **Switching polling on forgets what the plate held.** `Presence` reports
/// changes, not states, so a figure it already believes present is not
/// reported again. While polling was off the figure may have been swapped or
/// removed, so start fresh. Otherwise `plate on` with a figure already on the
/// plate reports nothing and the story never starts, even though the reader
/// works.
fn poll_plate(
    reader: &mut Reader,
    presence: &mut Presence,
    was_polling: &mut bool,
    next_poll: &mut Instant,
    misses: &mut u16,
    believed_present: &mut bool,
) {
    let polling = PLATE_POLLING.load(Ordering::Relaxed);
    if polling && !*was_polling {
        *presence = Presence::new(ARRIVALS_TO_AGREE, MISSES_TO_LEAVE);
        *believed_present = false;
        *misses = 0;
        *next_poll = Instant::now();
    }
    *was_polling = polling;

    if !polling || Instant::now() < *next_poll {
        return;
    }

    let password = NFC_PASSWORD.load(Ordering::Relaxed);

    // Which request is cheapest depends on what was there last time.
    // Always sending the full unlock caused 49 audio DMA restarts in 70 s
    // of playback, against 0 with polling off, because every unanswered
    // exchange blocks this task for the whole `IRQ_POLL_ATTEMPTS` window.
    let seen = if *believed_present {
        // An unlocked figure stays unlocked until its field is switched
        // off, so a plain inventory answers. It also re-reads the UID,
        // which notices a figure being swapped.
        reader.identify()
    } else if reader.tag_present() {
        // Something is there but not yet identified. Only this case
        // sends the password.
        reader.inventory_unlocked(password)
    } else {
        // The usual case: one unanswered exchange and nothing else.
        None
    };
    // Counts polls in a row that found nothing, and prints the run length
    // when a figure answers again. The radio can cause false departures
    // (23 in ten minutes were seen), and the run lengths show whether
    // allowing more misses would help.
    //
    // Based on `seen` only, not `believed_present`, which is the previous
    // poll's result and would stop the count at one.
    if seen.is_none() {
        *misses = misses.saturating_add(1);
    } else {
        if *misses > 0 {
            esp_println::println!("teddiebox: plate answered again after {misses} missed polls");
        }
        *misses = 0;
    }
    *believed_present = seen.is_some();

    let event = presence.feed(seen.map(TagUid));
    *next_poll = Instant::now() + Duration::from_millis(u64::from(presence.poll_again_in_ms()));
    let Some(event) = event else {
        return;
    };
    // Printed, so the plate's state is visible even when the reducer does
    // nothing about it.
    match event {
        TagEvent::Arrived(TagUid(uid)) => {
            esp_println::println!("teddiebox: plate tag arrived {:016X}", TagUid(uid).ruid())
        }
        TagEvent::Left => {
            esp_println::println!("teddiebox: plate tag left after {misses} missed polls")
        }
    }
    match event {
        TagEvent::Arrived(TagUid(uid)) => {
            // Read the token now: it is only readable while the figure is
            // on the plate and unlocked, and teddyCloud needs it to fetch
            // the story from the cloud.
            let token = reader.read_token();
            PLATE_TAG.signal(Seen::Figure { uid, token });
        }
        TagEvent::Left => PLATE_TAG.signal(Seen::Nothing),
    }
}

/// Brings the NFC reader up on first use, polls the plate, and serves the
/// console `nfc` commands.
///
/// Waits for a request rather than starting at boot: the reader shares the
/// storage rail, and switching that rail on is the console loop's decision.
#[embassy_executor::task]
pub(crate) async fn nfc_reader(
    spi: Spi<'static, esp_hal::Blocking>,
    cs: Output<'static>,
    irq: Input<'static>,
) {
    while NFC_REQUEST.load(Ordering::Relaxed) == REQUEST_NONE
        && !PLATE_POLLING.load(Ordering::Relaxed)
    {
        Timer::after(Duration::from_millis(100)).await;
    }

    // The console loop switched the rail on; let it settle, as for the card.
    Timer::after(Duration::from_millis(50)).await;

    let mut reader = match Reader::open(spi, cs, irq, esp_hal::delay::Delay::new()).await {
        Ok(reader) => reader,
        Err(reason) => {
            esp_println::println!("teddiebox: nfc failed — {reason}");
            return;
        }
    };

    let mut presence = Presence::new(ARRIVALS_TO_AGREE, MISSES_TO_LEAVE);
    // Whether polling was on in the last pass, so switching it on can start
    // fresh (see below).
    let mut was_polling = false;
    // When the plate is next read. A time rather than a count of passes,
    // because a pass can take much longer than 100 ms.
    let mut next_poll = Instant::now();
    // Consecutive polls that found nothing.
    let mut misses: u16 = 0;
    // Whether the last poll found a figure. Decides what the next poll asks
    // first; a wrong guess costs one extra exchange and is corrected on the
    // next poll.
    let mut believed_present = false;

    loop {
        // Nothing to do for the rest of this power-on: see `PARKED`.
        if PARKED.load(Ordering::Relaxed) {
            park_task().await;
        }

        handle_console_request(
            &mut reader,
            NFC_REQUEST.swap(REQUEST_NONE, Ordering::Relaxed),
        )
        .await;

        // Printed when polling stops, summarising the run by its slowest
        // reply.
        if PLATE_REPORT.swap(false, Ordering::Relaxed) {
            let (polls, micros) = reader.slowest_reply();
            esp_println::println!(
                "teddiebox: plate slowest reply {polls} polls (~{micros} us) \
                 of {} attempts allowed",
                trf7962a::IRQ_POLL_ATTEMPTS
            );
        }

        poll_plate(
            &mut reader,
            &mut presence,
            &mut was_polling,
            &mut next_poll,
            &mut misses,
            &mut believed_present,
        );

        // Console requests are still answered within 100 ms; the plate is
        // read when `presence` asked for it, which can be sooner.
        let console_due = Instant::now() + Duration::from_millis(100);
        Timer::at(if PLATE_POLLING.load(Ordering::Relaxed) {
            next_poll.min(console_due)
        } else {
            console_due
        })
        .await;
    }
}
