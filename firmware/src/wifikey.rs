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
use teddiebox_core::checksum::Crc32;
use teddiebox_wifikey::psk::{Derivation, Psk};
use teddiebox_wifikey::record::{parse, render, RecordError, Stored, RECORD};

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

/// Whether a join this boot succeeded with the passphrase itself.
///
/// The derivation waits for it, so that a passphrase the router refuses is
/// never kept — otherwise a wrong one on the card would rewrite a flash
/// sector on every boot.
static PASSPHRASE_JOINED: AtomicBool = AtomicBool::new(false);

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

/// Says a join this boot succeeded with the passphrase, so it is worth keeping.
pub fn passphrase_joined() {
    PASSPHRASE_JOINED.store(true, Ordering::Relaxed);
}

/// Which credentials a derivation is for, without keeping the passphrase.
fn fingerprint(config: &Config) -> u32 {
    let mut crc = Crc32::new();
    crc.update(config.ssid.as_bytes());
    crc.update(&[0]);
    crc.update(config.password.as_bytes());
    crc.finish()
}

/// A derivation part-way through, and what it is for.
struct InProgress {
    derivation: Derivation,
    credentials: u32,
    passes: u32,
    started: Instant,
}

/// Derives the key a slice at a time, owned by the net task.
pub struct Deriver {
    current: Option<InProgress>,
}

impl Deriver {
    pub const fn new() -> Self {
        Self { current: None }
    }

    /// Runs one slice, if there is a proved passphrase with no key and
    /// nothing is playing.
    ///
    /// Playing pauses it rather than restarting it: the slices done so far
    /// are kept, and the flash write at the end never lands under a story.
    pub fn pass(&mut self, credentials: impl FnOnce() -> Option<Config>, playing: bool) {
        if playing || !PASSPHRASE_JOINED.load(Ordering::Relaxed) {
            return;
        }
        let Some(config) = credentials() else {
            return self.stop();
        };
        if key_for(&config.ssid, &config.password).is_some() {
            return self.stop();
        }
        // Changed since the join that proved them: the new ones are unproved.
        let credentials = fingerprint(&config);
        if self
            .current
            .as_ref()
            .is_some_and(|job| job.credentials != credentials)
        {
            return self.stop();
        }
        let job = self.current.get_or_insert_with(|| InProgress {
            derivation: Derivation::new(config.ssid.as_bytes(), config.password.as_bytes()),
            credentials,
            passes: 0,
            started: Instant::now(),
        });
        job.passes += 1;
        let Some(psk) = job.derivation.step(DERIVE_SLICE) else {
            return;
        };
        esp_println::println!(
            "teddiebox: wifikey derived in {} ms over {} passes",
            job.started.elapsed().as_millis(),
            job.passes
        );
        store(&config.ssid, &config.password, &psk);
        self.stop();
    }

    /// Drops any derivation and waits for the next passphrase join.
    fn stop(&mut self) {
        self.current = None;
        PASSPHRASE_JOINED.store(false, Ordering::Relaxed);
    }
}
