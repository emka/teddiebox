//! The box's own certificate and key, read from the `cert` partition.
//!
//! **Why not the SD card.** The card is removed and put into other computers;
//! a private key does not belong there. Flash stays in the box, is written
//! once per box by `just identity`, and is not touched by firmware updates,
//! which never write data partitions.
//!
//! **Read at boot**, before the card is used at all.
use esp_bootloader_esp_idf::partitions::{self, PARTITION_TABLE_MAX_LEN};
use teddiebox_identity::{parse_header, verify, IdentityError, HEADER};

use crate::flash;
use crate::tls;

/// The format's size limit and these buffers must match, or an identity the
/// laptop accepted could fail on the box.
const _: () = assert!(
    teddiebox_identity::MAX_BODY == tls::CERT_BYTES,
    "teddiebox-identity::MAX_BODY and tls::CERT_BYTES must agree"
);

/// The partition's label in `partitions.csv`.
///
/// Matched by label, not subtype: its subtype is `Undefined`, which a future
/// partition could share.
const LABEL: &str = "cert";

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
            "teddiebox: identity no `{LABEL}` partition — this box was flashed with an older \
             partition table; `just flash` writes the current one"
        );
        return;
    };

    let capacity = entry.len() as usize;
    let mut region = entry.as_flash_region(&mut flash);

    let mut header = [0u8; HEADER];
    if let Err(trouble) = region.read(0, &mut header) {
        esp_println::println!(
            "teddiebox: identity cannot read the `{LABEL}` partition — {trouble:?}"
        );
        return;
    }

    let held = match parse_header(&header, capacity) {
        Ok(held) => held,
        Err(IdentityError::Blank) => {
            esp_println::println!(
                "teddiebox: identity none — nothing has been written to `{LABEL}`; \
                 run `just identity`"
            );
            return;
        }
        Err(trouble) => {
            esp_println::println!(
                "teddiebox: identity unusable — `{LABEL}` holds something this firmware \
                 does not recognise ({trouble:?}). If `just identity` has never been run \
                 on this box, that is what this means: the partition still holds whatever \
                 was there before this firmware — the common case on a box built from a \
                 stock Toniebox. Run `just identity`."
            );
            return;
        }
    };

    let mut certificate = [0u8; tls::CERT_BYTES];
    let mut key = [0u8; tls::CERT_BYTES];
    if let Err(trouble) = region.read(
        held.certificate_offset() as u32,
        &mut certificate[..held.certificate_len],
    ) {
        esp_println::println!("teddiebox: identity certificate would not read — {trouble:?}");
        return;
    }
    if let Err(trouble) = region.read(held.key_offset() as u32, &mut key[..held.key_len]) {
        esp_println::println!("teddiebox: identity key would not read — {trouble:?}");
        return;
    }

    // Check before publishing: an interrupted write can leave a valid header
    // over bodies that never arrived, which would report success and then
    // fail every TLS handshake.
    if let Err(trouble) = verify(
        &held,
        &certificate[..held.certificate_len],
        &key[..held.key_len],
    ) {
        esp_println::println!(
            "teddiebox: identity unusable — {trouble:?}; the `cert` partition's contents do not \
             match the checksum written with them, which is what an interrupted \
             `just identity` leaves behind. Run it again."
        );
        return;
    }

    // The certificate's length and the key's length. Never the key.
    if tls::set_identity(&certificate[..held.certificate_len], &key[..held.key_len]) {
        esp_println::println!(
            "teddiebox: identity {} byte certificate, {} byte key, from flash",
            held.certificate_len,
            held.key_len
        );
    } else {
        esp_println::println!("teddiebox: identity already set");
    }
}
