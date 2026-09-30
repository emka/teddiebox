//! The `wifi` partition's one record: which credentials a key was derived
//! from, and the key.
//!
//! **The check value is a CRC-32 of the passphrase, on purpose.** Any fast
//! check lets someone with a flash dump test passphrase guesses quickly. That
//! is acceptable: the passphrase is in plain text on the SD card inside the
//! same box.

use teddiebox_core::checksum::Crc32;

use crate::psk::Psk;

/// Bytes one record takes: a multiple of four, as a flash write needs.
pub const RECORD: usize = 80;
const MAGIC: [u8; 4] = *b"TBWK";
const VERSION: u16 = 1;
const MAX_SSID: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordError {
    /// Erased flash: nothing was ever written.
    Blank,
    /// Not a record this firmware wrote, usually a never-erased partition.
    Unrecognised,
    /// A record from another version of this format.
    Version(u16),
    /// The checksum does not match: an interrupted write.
    Damaged,
    /// Longer than the 32 bytes 802.11 allows an SSID.
    SsidTooLong,
}

/// A record that parsed: the credentials it answers for, and its key.
pub struct Stored {
    ssid: [u8; MAX_SSID],
    ssid_len: usize,
    check: u32,
    psk: Psk,
}

impl Stored {
    pub fn ssid(&self) -> &[u8] {
        &self.ssid[..self.ssid_len]
    }

    /// The key, if it was derived from exactly these credentials.
    pub fn key_for(&self, ssid: &[u8], passphrase: &[u8]) -> Option<&Psk> {
        (self.ssid() == ssid && self.check == crc_of(passphrase)).then_some(&self.psk)
    }
}

fn crc_of(bytes: &[u8]) -> u32 {
    let mut crc = Crc32::new();
    crc.update(bytes);
    crc.finish()
}

/// The record for a key derived from `ssid` and `passphrase`.
pub fn render(ssid: &[u8], passphrase: &[u8], psk: &Psk) -> Result<[u8; RECORD], RecordError> {
    if ssid.len() > MAX_SSID {
        return Err(RecordError::SsidTooLong);
    }
    let mut raw = [0u8; RECORD];
    raw[0..4].copy_from_slice(&MAGIC);
    raw[4..6].copy_from_slice(&VERSION.to_le_bytes());
    raw[6] = ssid.len() as u8;
    raw[8..8 + ssid.len()].copy_from_slice(ssid);
    raw[40..44].copy_from_slice(&crc_of(passphrase).to_le_bytes());
    raw[44..76].copy_from_slice(psk.as_bytes());
    let crc = crc_of(&raw[..76]);
    raw[76..80].copy_from_slice(&crc.to_le_bytes());
    Ok(raw)
}

/// Reads a record back, refusing anything this firmware did not write whole.
pub fn parse(raw: &[u8; RECORD]) -> Result<Stored, RecordError> {
    if raw.iter().all(|&b| b == 0xff) {
        return Err(RecordError::Blank);
    }
    if raw[0..4] != MAGIC {
        return Err(RecordError::Unrecognised);
    }
    let version = u16::from_le_bytes([raw[4], raw[5]]);
    if version != VERSION {
        return Err(RecordError::Version(version));
    }
    if crc_of(&raw[..76]) != u32::from_le_bytes([raw[76], raw[77], raw[78], raw[79]]) {
        return Err(RecordError::Damaged);
    }
    // The checksum covers this too, but check before using it as a slice
    // bound.
    let ssid_len = usize::from(raw[6]);
    if ssid_len > MAX_SSID {
        return Err(RecordError::Damaged);
    }
    let mut ssid = [0; MAX_SSID];
    ssid.copy_from_slice(&raw[8..40]);
    let mut key = [0; 32];
    key.copy_from_slice(&raw[44..76]);
    Ok(Stored {
        ssid,
        ssid_len,
        check: u32::from_le_bytes([raw[40], raw[41], raw[42], raw[43]]),
        psk: Psk::from_bytes(key),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: Psk = Psk::from_bytes([0x11; 32]);

    /// A record for `ssid` and `passphrase` holding [`KEY`], as it reads back.
    fn stored(ssid: &[u8], passphrase: &[u8]) -> Stored {
        parse(&render(ssid, passphrase, &KEY).unwrap()).unwrap()
    }

    /// The record for network `AB`, passphrase `pw`, key `0x22` repeated.
    fn small_record() -> [u8; RECORD] {
        render(b"AB", b"pw", &Psk::from_bytes([0x22; 32])).unwrap()
    }

    #[test]
    fn a_rendered_record_gives_its_key_back_for_the_same_credentials() {
        // Given
        let record = stored(b"example-ssid", b"correct horse");

        // When
        let key = record.key_for(b"example-ssid", b"correct horse");

        // Then
        assert_eq!(key, Some(&KEY));
    }

    #[test]
    fn another_passphrase_does_not_match() {
        // Given
        let record = stored(b"example-ssid", b"correct horse");

        // When
        let key = record.key_for(b"example-ssid", b"correct horsf");

        // Then
        assert_eq!(key, None);
    }

    #[test]
    fn another_ssid_does_not_match() {
        // Given
        let record = stored(b"example-ssid", b"correct horse");

        // When
        let key = record.key_for(b"Blueberr", b"correct horse");

        // Then
        assert_eq!(key, None);
    }

    #[test]
    fn longest_ssid_round_trips() {
        // Given
        let record = stored(&[b'S'; 32], b"correct horse");

        // When
        let key = record.key_for(&[b'S'; 32], b"correct horse");

        // Then
        assert_eq!(key, Some(&KEY));
    }

    #[test]
    fn an_ssid_longer_than_wifi_allows_is_refused() {
        // Given
        let ssid = [b'S'; 33];

        // When
        let rendered = render(&ssid, b"x", &KEY);

        // Then
        assert_eq!(rendered, Err(RecordError::SsidTooLong));
    }

    #[test]
    fn the_layout_is_fixed() {
        // Given: network `AB`, passphrase `pw`, key `0x22` repeated

        // When
        let raw = small_record();

        // Then
        assert_eq!(&raw[0..8], &[b'T', b'B', b'W', b'K', 1, 0, 2, 0]);
        assert_eq!(&raw[8..10], b"AB");
        assert_eq!(&raw[10..40], &[0u8; 30]);
        assert_eq!(&raw[44..76], &[0x22; 32]);
    }

    /// Both CRCs are from Python's zlib.crc32, not this crate. A change to the
    /// CRC or its byte order would pass round-trip tests but invalidate every
    /// record already in a box's flash.
    #[test]
    fn the_check_is_the_passphrase_crc32_little_endian() {
        // Given: network `AB`, passphrase `pw`

        // When
        let raw = small_record();

        // Then
        assert_eq!(&raw[40..44], &[150, 143, 135, 160]);
    }

    /// See `the_check_is_the_passphrase_crc32_little_endian`.
    #[test]
    fn the_trailer_is_the_crc32_of_everything_before_it() {
        // Given: network `AB`, passphrase `pw`, key `0x22` repeated

        // When
        let raw = small_record();

        // Then
        assert_eq!(&raw[76..80], &[111, 142, 177, 102]);
    }

    #[test]
    fn erased_flash_is_blank() {
        // Given
        let erased = [0xff; RECORD];

        // When
        let parsed = parse(&erased);

        // Then
        assert_eq!(parsed.err(), Some(RecordError::Blank));
    }

    #[test]
    fn garbage_is_not_a_record() {
        // Given
        let mut garbage = [0u8; RECORD];
        for (i, b) in garbage.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37) ^ 0x5a;
        }

        // When
        let parsed = parse(&garbage);

        // Then
        assert_eq!(parsed.err(), Some(RecordError::Unrecognised));
    }

    #[test]
    fn a_flipped_bit_is_damage() {
        // Given
        let mut raw = render(b"example-ssid", b"correct horse", &KEY).unwrap();
        raw[50] ^= 0x01;

        // When
        let parsed = parse(&raw);

        // Then
        assert_eq!(parsed.err(), Some(RecordError::Damaged));
    }

    #[test]
    fn a_later_version_is_named() {
        // Given
        let mut raw = render(b"example-ssid", b"correct horse", &KEY).unwrap();
        raw[4] = 2;

        // When
        let parsed = parse(&raw);

        // Then
        assert_eq!(parsed.err(), Some(RecordError::Version(2)));
    }
}
