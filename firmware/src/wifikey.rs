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

use critical_section::Mutex as CsMutex;
use esp_bootloader_esp_idf::partitions::{self, FlashRegion, PARTITION_TABLE_MAX_LEN};
use teddiebox_wifikey::record::{parse, RecordError, Stored, RECORD};

use crate::flash;

/// The partition's label in `partitions.csv`.
const LABEL: &str = "wifi";

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
