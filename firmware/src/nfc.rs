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

use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::time::Rate;
use trf7962a::{Trf7962a, INIT_SEQUENCE};

/// The reader takes up to 2 Mbit/s (SLOS757C §5.12). Half that is plenty for
/// register pokes and a twelve-byte FIFO, and leaves margin on a bench wire.
const BUS_RATE_KHZ: u32 = 1_000;

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
    pub fn open(
        spi: Spi<'static, Blocking>,
        cs: Output<'static>,
        irq: Input<'static>,
        delay: Delay,
    ) -> Result<Self, &'static str> {
        let device =
            ExclusiveDevice::new(spi, cs, delay).map_err(|_| "chip select would not drive")?;
        let mut trf = Trf7962a::new(device, delay, irq);

        trf.init_iso15693()
            .map_err(|_| "the reader would not configure")?;

        // Read back what init just wrote. This is the reader's equivalent of
        // the codec's power-flags register: it separates "asked wrongly" from
        // "not listening", and it is the only check here that does not depend
        // on a tag being present.
        let mut agreed = true;
        for &(register, expected) in INIT_SEQUENCE {
            match trf.read_register(register) {
                Ok(actual) if actual == expected => {
                    esp_println::println!("teddiebox: nfc reg {register:#04x} = {actual:#04x}")
                }
                Ok(actual) => {
                    agreed = false;
                    esp_println::println!(
                        "teddiebox: nfc reg {register:#04x} reads {actual:#04x}, wrote {expected:#04x}"
                    );
                }
                Err(_) => {
                    agreed = false;
                    esp_println::println!("teddiebox: nfc reg {register:#04x} unreadable");
                }
            }
        }

        if !agreed {
            // Not fatal: the bench should still be allowed to try a tag, and
            // seeing both results is more useful than refusing to continue.
            esp_println::println!("teddiebox: nfc link is not answering as written — suspect SPI");
        } else {
            esp_println::println!("teddiebox: nfc reader answers, field on");
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
        // receiver and TX oscillator". Bit 0 is the supply selection and is
        // left as the driver set it.
        const LISTEN: u8 = 0x03; // rec_on + supply, transmitter off
        let value = if on {
            trf7962a::regs::CHIP_STATUS_RF_ON
        } else {
            LISTEN
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
    pub fn unlock(&mut self, password: u32) {
        match self.trf.get_random_number() {
            Ok(random) => esp_println::println!(
                "teddiebox: nfc tag answered GET RANDOM NUMBER ({random:#06x}) — present and SLIX"
            ),
            Err(_) => {
                esp_println::println!(
                    "teddiebox: nfc no answer to GET RANDOM NUMBER — nothing in the field"
                );
                self.diagnose();
                return;
            }
        }

        match self.trf.inventory_unlocked(password) {
            Ok(Some(uid)) => report_uid("unlocked tag", &uid),
            Ok(None) => esp_println::println!(
                "teddiebox: nfc still silent after unlock — wrong password, or no tag"
            ),
            Err(_) => esp_println::println!("teddiebox: nfc unlock failed"),
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
