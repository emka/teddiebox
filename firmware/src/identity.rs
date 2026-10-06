//! The box's own certificate and key, read from stock's `assets` partition.
//!
//! **Why not the SD card.** The card is removed and put into other computers;
//! a private key does not belong there. Stock provisions each box with its own
//! pair in `CERT/client.der` and `CERT/private.der` inside `assets`, which
//! stays in the box, is never written here, and is not touched by firmware
//! updates, which never write data partitions.
//!
//! **Read at boot**, before the card is used at all.
use esp_bootloader_esp_idf::partitions::{self, PARTITION_TABLE_MAX_LEN};
use teddiebox_assets::{Assets, ReadAt};

use crate::flash;
use crate::tls;

/// The partition's label, which stock names it.
///
/// Matched by label, not subtype: `0x81` is a FAT volume in general, and a
/// second one would be ambiguous.
const LABEL: &str = "assets";

const CERTIFICATE: &str = "CERT/client.der";
const KEY: &str = "CERT/private.der";

/// An `assets` partition as the byte source the volume reader reads.
struct Region<'a, 'd>(partitions::FlashRegion<'a, 'd>);

impl ReadAt for Region<'_, '_> {
    type Error = partitions::Error;

    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<(), partitions::Error> {
        self.0.read(offset, out)
    }
}

/// Reads the identity out of flash and publishes it.
///
/// Failures are reported but not fatal: without an identity the box still
/// plays everything on its card, but cannot download.
pub fn load() {
    let mut flash = flash::flash();
    let mut table_buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(&mut flash, &mut table_buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!(
                "teddiebox: identity cannot read the partition table — {trouble:?}"
            );
            return;
        }
    };

    let Some(entry) = table.iter().find(|entry| entry.label_as_str() == LABEL) else {
        esp_println::println!(
            "teddiebox: identity no `{LABEL}` partition — this box was flashed with a \
             different partition table; `just flash` writes the current one"
        );
        return;
    };

    let len = entry.len();
    let mut assets = match Assets::open(Region(entry.as_flash_region(&mut flash)), len) {
        Ok(assets) => assets,
        Err(trouble) => {
            esp_println::println!(
                "teddiebox: identity cannot open the `{LABEL}` volume — {trouble:?}"
            );
            return;
        }
    };

    let mut certificate = [0u8; tls::CERT_BYTES];
    let mut key = [0u8; tls::CERT_BYTES];
    let certificate_len = match assets.read_file(CERTIFICATE, &mut certificate) {
        Ok(len) => len,
        Err(trouble) => {
            esp_println::println!("teddiebox: identity cannot read {CERTIFICATE} — {trouble:?}");
            return;
        }
    };
    let key_len = match assets.read_file(KEY, &mut key) {
        Ok(len) => len,
        Err(trouble) => {
            esp_println::println!("teddiebox: identity cannot read {KEY} — {trouble:?}");
            return;
        }
    };

    // The certificate's length and the key's length. Never the key.
    if tls::set_identity(&certificate[..certificate_len], &key[..key_len]) {
        esp_println::println!(
            "teddiebox: identity {certificate_len} byte certificate, {key_len} byte key, \
             from flash"
        );
    } else {
        esp_println::println!("teddiebox: identity already set");
    }
}
