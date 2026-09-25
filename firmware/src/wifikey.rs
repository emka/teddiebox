//! The derived Wi-Fi key, kept in the `wifi` partition.
//!
//! Given the passphrase, a Wi-Fi join spends 1.75 s deriving this key; given
//! the key, it takes about 70 ms. So the key is kept across boots and derived
//! again only when the credentials change (see `teddiebox_wifikey::record`).
//!
//! Stored in a data partition, which survives `just flash`. Not on the card,
//! which already holds the passphrase.
use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use critical_section::Mutex as CsMutex;
use embassy_time::Instant;
use esp_bootloader_esp_idf::partitions::{self, FlashRegion, PARTITION_TABLE_MAX_LEN};
use teddiebox_config::Config;
use teddiebox_wifikey::psk::Psk;
use teddiebox_wifikey::record::{parse, render, RecordError, Stored, RECORD};
use teddiebox_wifikey::schedule::{Pass, Schedule};

use crate::flash;

/// The partition's label in `partitions.csv`.
const LABEL: &str = "wifi";

/// Bytes erased before a record is written: one flash sector.
const SECTOR: u32 = 0x1000;

/// Rounds of the derivation run per pass of the net task's idle loop.
///
/// Each pass blocks the executor, delaying the NFC reader, so a pass stays
/// under about 10 ms. Measured: 64 rounds took 20.0–20.5 ms (software SHA-1,
/// about 0.32 ms a round); 24 take 5.0–8.2 ms. A whole derivation is 171
/// passes, about 22 s in the background, once per change of credentials.
const DERIVE_SLICE: u32 = 24;

/// Whether a key derived now could be kept.
///
/// False until [`load`] has read the partition, and after a write fails. A
/// box updated over the air keeps its old partition table (which may lack
/// this partition), and without this would derive a key after every join,
/// about 20 s of work, only to fail to write it.
static WRITABLE: AtomicBool = AtomicBool::new(false);

/// What the partition held at boot. `None` once [`forget`] has been called.
static STORED: CsMutex<RefCell<Option<Stored>>> = CsMutex::new(RefCell::new(None));

/// A record's bytes, word-aligned in memory as well as in length, so
/// esp-storage never falls back to its 4 KB stack buffer.
#[repr(C, align(4))]
struct Aligned([u8; RECORD]);

/// Runs `f` on the `wifi` partition, or says why it cannot.
fn with_region<R>(f: impl FnOnce(&mut FlashRegion<'_, '_>) -> R) -> Option<R> {
    let mut flash = flash::flash();
    let mut table_buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(&mut flash, &mut table_buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!(
                "teddiebox: wifikey cannot read the partition table — {trouble:?}"
            );
            return None;
        }
    };
    let Some(entry) = table.iter().find(|entry| entry.label_as_str() == LABEL) else {
        esp_println::println!(
            "teddiebox: wifikey no `{LABEL}` partition — this box was flashed with an older \
             partition table; `just flash` writes the current one"
        );
        return None;
    };
    Some(f(&mut entry.as_flash_region(&mut flash)))
}

/// Reads the stored key, if there is one, for [`key_for`] to answer from.
///
/// Failures are reported but not fatal: without a key, a join uses the
/// passphrase.
pub fn load() {
    let Some(read) = with_region(|region| {
        let mut raw = Aligned([0; RECORD]);
        region.read(0, &mut raw.0).map(|()| raw)
    }) else {
        return;
    };
    let raw = match read {
        Ok(raw) => raw,
        Err(trouble) => {
            esp_println::println!("teddiebox: wifikey cannot read `{LABEL}` — {trouble:?}");
            return;
        }
    };
    WRITABLE.store(true, Ordering::Relaxed);
    match parse(&raw.0) {
        Ok(stored) => {
            // The SSID, never the key.
            esp_println::println!(
                "teddiebox: wifikey a key for {} from flash",
                core::str::from_utf8(stored.ssid()).unwrap_or("?")
            );
            critical_section::with(|cs| *STORED.borrow_ref_mut(cs) = Some(stored));
        }
        Err(RecordError::Blank) => esp_println::println!("teddiebox: wifikey none stored"),
        Err(trouble) => esp_println::println!(
            "teddiebox: wifikey none usable ({trouble:?}) — the next join derives one"
        ),
    }
}

/// Writes a key for these credentials and uses it from now on.
///
/// Erasing the sector holds a critical section for about 300 ms, so this
/// must not run while audio plays. The record is read back and parsed before
/// it is used.
///
/// A record identical to the one in flash is not written again (for example
/// a correct key that was dropped after one handshake timeout), to avoid
/// needless flash wear.
fn store(ssid: &str, passphrase: &str, psk: &Psk) -> bool {
    let record = match render(ssid.as_bytes(), passphrase.as_bytes(), psk) {
        Ok(record) => Aligned(record),
        Err(trouble) => {
            esp_println::println!("teddiebox: wifikey not stored — {trouble:?}");
            return false;
        }
    };
    let started = Instant::now();
    let Some(written) = with_region(|region| {
        let mut back = Aligned([0; RECORD]);
        region.read(0, &mut back.0)?;
        if back.0 == record.0 {
            return Ok((back, false));
        }
        region.erase(0, SECTOR)?;
        region.write(0, &record.0)?;
        region.read(0, &mut back.0)?;
        Ok::<_, partitions::Error>((back, true))
    }) else {
        return false;
    };
    let (back, erased) = match written {
        Ok(written) => written,
        Err(trouble) => {
            esp_println::println!("teddiebox: wifikey could not write `{LABEL}` — {trouble:?}");
            return false;
        }
    };
    match parse(&back.0) {
        Ok(stored) => {
            esp_println::println!(
                "teddiebox: wifikey {} a key for {ssid} in {} ms",
                if erased { "stored" } else { "already held" },
                started.elapsed().as_millis()
            );
            critical_section::with(|cs| *STORED.borrow_ref_mut(cs) = Some(stored));
            true
        }
        Err(trouble) => {
            esp_println::println!("teddiebox: wifikey wrote `{LABEL}` and read back {trouble:?}");
            false
        }
    }
}

/// The stored key as the driver takes it, if it came from these credentials.
pub fn key_for(ssid: &str, passphrase: &str) -> Option<[u8; 64]> {
    critical_section::with(|cs| {
        STORED
            .borrow_ref(cs)
            .as_ref()
            .and_then(|stored| stored.key_for(ssid.as_bytes(), passphrase.as_bytes()))
            .map(|psk| psk.hex())
    })
}

/// Stops using the stored key for the rest of this boot.
///
/// For a key the access point refused. The record stays in flash until a
/// successful passphrase join produces a new one.
pub fn forget() {
    critical_section::with(|cs| *STORED.borrow_ref_mut(cs) = None);
}

/// One idle pass of the derivation, owned by the net task.
///
/// Only runs for credentials a passphrase join has proven, and never while
/// audio plays, which the work and the final sector erase would disturb. See
/// `teddiebox_wifikey::schedule`.
pub fn pass(schedule: &mut Schedule, credentials: impl FnOnce() -> Option<Config>, playing: bool) {
    if playing || !WRITABLE.load(Ordering::Relaxed) {
        return;
    }
    let Some(config) = credentials() else {
        return;
    };
    let stored = key_for(&config.ssid, &config.password).is_some();
    let current = (config.ssid.as_bytes(), config.password.as_bytes());
    if let Pass::Done(psk) = schedule.pass(Some(current), stored, playing, DERIVE_SLICE) {
        esp_println::println!("teddiebox: wifikey derived a key for {}", config.ssid);
        if !store(&config.ssid, &config.password, &psk) {
            esp_println::println!("teddiebox: wifikey not deriving again this boot");
            WRITABLE.store(false, Ordering::Relaxed);
        }
    }
}
