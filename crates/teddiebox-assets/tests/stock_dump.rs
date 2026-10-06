//! The reader against a real stock box.
//!
//! Skipped unless asked for, because it needs a full flash dump of a box:
//!
//!     TEDDIEBOX_STOCK_DUMP=/path/to/dump.bin cargo test -p teddiebox-assets -- --ignored
//!
//! The dump holds the box's private key, so this checks the framing of what it
//! reads and never prints it.

use teddiebox_assets::{Assets, ReadAt};

/// Where stock puts the `assets` partition, and how big it is.
const ASSETS_AT: usize = 0xf000;
const ASSETS_LEN: usize = 0x160000;

struct Dump(Vec<u8>);

impl ReadAt for Dump {
    type Error = ();

    fn read_at(&mut self, offset: u32, out: &mut [u8]) -> Result<(), ()> {
        let from = offset as usize;
        out.copy_from_slice(self.0.get(from..from + out.len()).ok_or(())?);
        Ok(())
    }
}

/// A DER SEQUENCE whose own length accounts for the whole of `bytes`.
fn is_one_der_sequence(bytes: &[u8]) -> bool {
    matches!(bytes, [0x30, 0x82, high, low, rest @ ..]
        if usize::from(*high) << 8 | usize::from(*low) == rest.len())
}

#[test]
#[ignore = "needs TEDDIEBOX_STOCK_DUMP"]
fn the_certificate_and_key_of_a_stock_box_are_read_whole() {
    // Given
    let path = std::env::var("TEDDIEBOX_STOCK_DUMP").expect("TEDDIEBOX_STOCK_DUMP is not set");
    let dump = std::fs::read(path).unwrap();
    let partition = dump[ASSETS_AT..ASSETS_AT + ASSETS_LEN].to_vec();
    let mut assets = Assets::open(Dump(partition), ASSETS_LEN as u32).unwrap();
    let mut certificate = [0u8; 1536];
    let mut key = [0u8; 1536];

    // When
    let certificate_len = assets
        .read_file("CERT/client.der", &mut certificate)
        .unwrap();
    let key_len = assets.read_file("CERT/private.der", &mut key).unwrap();

    // Then
    assert!(is_one_der_sequence(&certificate[..certificate_len]));
    assert!(is_one_der_sequence(&key[..key_len]));
}
