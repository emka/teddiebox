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
            Ok(None) => esp_println::println!(
                "teddiebox: nfc no answer — an empty plate and a locked Tonie look the same here"
            ),
            Err(_) => esp_println::println!("teddiebox: nfc inventory failed"),
        }
    }

    /// Bench step 10b: unlock a Tonie's privacy mode, then read it.
    pub fn unlock(&mut self, password: u32) {
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
