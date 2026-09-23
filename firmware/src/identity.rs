//! The box's own certificate and key, read from the `cert` partition.
//!
//! **Why not the card.** The SD card is the one surface of this box that comes
//! out and goes into other machines; a private key does not belong there. Flash
//! stays with the board, is written once per box by `just identity`, and is
//! untouched by a firmware flash because an app write never reaches a data
//! partition.
//!
//! **Read at boot, not at mount.** The card is mounted lazily — often first by
//! a network command — so an identity read from it happened at a time nothing
//! chose. This runs before the card is involved at all.
use esp_bootloader_esp_idf::partitions::{self, PARTITION_TABLE_MAX_LEN};
use teddiebox_identity::{parse_header, IdentityError, HEADER};

use crate::ota;
use crate::tls;

/// The format's cap and the buffers it lands in must be the same number.
///
/// A length the laptop accepted and the box could not hold would fail after
/// provisioning looked successful, which is the worst moment to find out.
const _: () = assert!(
    teddiebox_identity::MAX_BODY == tls::CERT_BYTES,
    "teddiebox-identity::MAX_BODY and tls::CERT_BYTES must agree"
);

/// The partition's label in `partitions.csv`.
///
/// Matched by label rather than by subtype: an unnamed data partition carries
/// `Undefined`, which any future one would match too.
const LABEL: &str = "cert";

/// Reads the identity out of flash and publishes it. `true` if it did.
///
/// Every failure is reported and none is fatal: a box with no identity plays
/// everything on its card and cannot fetch, which is the same shape as a box
/// with no CA.
pub fn load() -> bool {
    let mut flash = ota::flash();
    let mut table_buffer = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = match partitions::read_partition_table(&mut flash, &mut table_buffer) {
        Ok(table) => table,
        Err(trouble) => {
            esp_println::println!(
                "teddiebox: identity cannot read the partition table — {trouble:?}"
            );
            return false;
        }
    };

    let Some(entry) = table.iter().find(|entry| entry.label_as_str() == LABEL) else {
        esp_println::println!(
            "teddiebox: identity no `{LABEL}` partition — this box was flashed with an older \
             partition table; `just flash` writes the current one"
        );
        return false;
    };

    let capacity = entry.len() as usize;
    let mut region = entry.as_flash_region(&mut flash);

    let mut header = [0u8; HEADER];
    if let Err(trouble) = region.read(0, &mut header) {
        esp_println::println!(
            "teddiebox: identity cannot read the `{LABEL}` partition — {trouble:?}"
        );
        return false;
    }

    let held = match parse_header(&header, capacity) {
        Ok(held) => held,
        Err(IdentityError::Blank) => {
            esp_println::println!(
                "teddiebox: identity none — nothing has been written to `{LABEL}`; \
                 run `just identity`"
            );
            return false;
        }
        Err(trouble) => {
            esp_println::println!(
                "teddiebox: identity unusable — `{LABEL}` holds something this firmware \
                 does not recognise ({trouble:?}). If `just identity` has never been run \
                 on this box, that is what this means: the partition still holds whatever \
                 was there before this firmware — the common case on a box built from a \
                 stock Toniebox. Run `just identity`."
            );
            return false;
        }
    };

    let mut certificate = [0u8; tls::CERT_BYTES];
    let mut key = [0u8; tls::CERT_BYTES];
    if let Err(trouble) = region.read(
        held.certificate_offset() as u32,
        &mut certificate[..held.certificate_len],
    ) {
        esp_println::println!("teddiebox: identity certificate would not read — {trouble:?}");
        return false;
    }
    if let Err(trouble) = region.read(held.key_offset() as u32, &mut key[..held.key_len]) {
        esp_println::println!("teddiebox: identity key would not read — {trouble:?}");
        return false;
    }

    // The certificate's length and the key's length. Never the key.
    if tls::set_identity(&certificate[..held.certificate_len], &key[..held.key_len]) {
        esp_println::println!(
            "teddiebox: identity {} byte certificate, {} byte key, from flash",
            held.certificate_len,
            held.key_len
        );
        true
    } else {
        esp_println::println!("teddiebox: identity already set");
        false
    }
}
