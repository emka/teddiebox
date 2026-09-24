//! The derived Wi-Fi key, kept in the `wifi` partition.
//!
//! A join handed the passphrase spends 1.75 s in the driver deriving this
//! key; handed the key, it takes ~70 ms (measured 2026-09-24). So the key is
//! kept across boots and derived again only when the credentials change —
//! see `teddiebox_wifikey::record` for how "change" is decided.
//!
//! Only a data partition survives `just flash`, which is why this is not in
//! RAM alone, and why it is not on the card: the card already holds the
//! passphrase this came from, and needs nothing more.
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
/// Each pass holds the executor for as long as its rounds take, and the plate
/// poller is what waits, so a slice stays under ~10 ms. Measured 2026-09-24:
/// 64 rounds took 20.0–20.5 ms (software SHA-1, ~0.32 ms a round); 24 take
/// 5.0–8.2 ms, and a whole derivation is 171 passes, ~22 s in the background,
/// once per change of credentials.
const DERIVE_SLICE: u32 = 24;

/// Whether a key derived now could be kept.
///
/// False until [`load`] has found and read the partition, and again after a
/// write fails: a box flashed over the air never receives the new table, and
/// without this it would derive a key after every passphrase join — ~20 s of
/// slices — only to fail to write it.
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
/// Every failure is reported and none is fatal: without a key, a join hands
/// the driver the passphrase, exactly as before there was a key to keep.
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
/// Erases the sector first, which holds a critical section for ~300 ms
/// (memory `teddiebox-flash-writes-need-critical-section`), so the caller
/// must not reach here while audio plays. Read back and parsed before it is
/// believed: a write that does not come back whole is not a key.
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
        region.erase(0, SECTOR)?;
        region.write(0, &record.0)?;
        let mut back = Aligned([0; RECORD]);
        region.read(0, &mut back.0)?;
        Ok::<_, partitions::Error>(back)
    }) else {
        return false;
    };
    let back = match written {
        Ok(back) => back,
        Err(trouble) => {
            esp_println::println!("teddiebox: wifikey could not write `{LABEL}` — {trouble:?}");
            return false;
        }
    };
    match parse(&back.0) {
        Ok(stored) => {
            esp_println::println!(
                "teddiebox: wifikey stored a key for {ssid} in {} ms",
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
/// For a key the access point refused. The record stays in flash until the
/// passphrase join that follows proves a new one worth keeping.
pub fn forget() {
    critical_section::with(|cs| *STORED.borrow_ref_mut(cs) = None);
}

/// One idle pass of the derivation, owned by the net task.
///
/// Runs a slice only for credentials a passphrase join proved, and never
/// while audio plays: its slices and the sector erase at the end would land
/// on the story. See `teddiebox_wifikey::schedule` for when a key is due.
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
