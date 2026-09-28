//! Reads the version out of a downloaded image, and gates activation on it.
//!
//! `decide` compares the manifest with the *running* build's version. This
//! compares the manifest with the downloaded image's own version. If they
//! differ, the box would install the image, reboot, see the manifest still
//! differs, and install it again forever. This is the last check before
//! `otadata` is switched.

use crate::{Manifest, OtaError};

/// An ESP-IDF application image opens with a 24-byte image header and an
/// 8-byte segment header, so the application descriptor starts here.
/// (esp-bootloader-esp-idf-0.6.0/src/lib.rs:136-147)
const DESCRIPTOR_OFFSET: usize = 0x20;

/// `magic_word: u32`, the descriptor's first field, little-endian.
const MAGIC_OFFSET: usize = DESCRIPTOR_OFFSET;

/// `ESP_APP_DESC_MAGIC_WORD` (esp-bootloader-esp-idf-0.6.0/src/lib.rs:322).
const MAGIC_WORD: u32 = 0xABCD_5432;

/// `version: [c_char; 32]` sits after `magic_word: u32`, `secure_version:
/// u32` and `reserv1: [u32; 2]` — 16 bytes into the descriptor.
const VERSION_OFFSET: usize = DESCRIPTOR_OFFSET + 16;
const VERSION_LEN: usize = 32;

/// One past the last byte this function ever reads: image offset 0x50.
const DESCRIPTOR_END: usize = VERSION_OFFSET + VERSION_LEN;

/// Reads the version the downloaded image itself reports, straight out of
/// its ESP-IDF application descriptor.
///
/// Fails closed: too few bytes, or a magic word that is not exactly
/// `0xABCD5432` at offset 0x20, means the descriptor is not where it is
/// expected, and `NotAnImage` is returned rather than a guess.
pub fn image_version(image_head: &[u8]) -> Result<&str, OtaError> {
    if image_head.len() < DESCRIPTOR_END {
        return Err(OtaError::NotAnImage);
    }
    let Some(magic) = image_head[MAGIC_OFFSET..].first_chunk::<4>() else {
        return Err(OtaError::NotAnImage);
    };
    let magic = u32::from_le_bytes(*magic);
    if magic != MAGIC_WORD {
        return Err(OtaError::NotAnImage);
    }
    let field = &image_head[VERSION_OFFSET..DESCRIPTOR_END];
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    core::str::from_utf8(&field[..end]).map_err(|_| OtaError::NotText)
}

/// Whether a downloaded image may be activated.
///
/// The last check before `otadata` is switched. `Ok(())` only when the
/// image's own version matches the manifest's.
pub fn may_activate(image_head: &[u8], manifest: &Manifest) -> Result<(), OtaError> {
    let version = image_version(image_head)?;
    if version == manifest.version.as_str() {
        Ok(())
    } else {
        Err(OtaError::VersionMismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_MANIFEST;

    extern crate std;
    use std::format;

    const DIGEST: &str = "3f786850e387550fdab836ed7e6dc881de23001b000000000000000000000000";

    fn manifest(version: &str) -> Manifest {
        let s = format!("version = {version}\nsha256 = {DIGEST}\nlength = 1\nimage = a.bin\n");
        Manifest::parse_read(s.as_bytes(), MAX_MANIFEST).unwrap()
    }

    /// A valid 0x50-byte image head: 32 bytes of image and segment header
    /// (not read), the magic word (0xABCD5432, little-endian), zeroed
    /// secure_version and reserv1, then the version `"0e469de"`, NUL-padded to
    /// 32 bytes. Written by hand, not with the code's offsets.
    const WELL_FORMED: [u8; 0x50] = [
        // 0x00-0x1F: image header + segment header, unread by image_version.
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, // 0x20-0x23: magic_word = 0xABCD5432, little-endian.
        0x32, 0x54, 0xCD, 0xAB, // 0x24-0x27: secure_version.
        0, 0, 0, 0, // 0x28-0x2F: reserv1.
        0, 0, 0, 0, 0, 0, 0, 0,
        // 0x30-0x4F: version = "0e469de" then NUL padding to 32 bytes.
        b'0', b'e', b'4', b'6', b'9', b'd', b'e', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
    ];

    #[test]
    fn a_well_formed_descriptor_reports_its_version() {
        assert_eq!(image_version(&WELL_FORMED).unwrap(), "0e469de");
    }

    #[test]
    fn the_version_field_truncates_at_the_first_nul() {
        // Like WELL_FORMED, with a non-NUL byte inside the padding, to check
        // the version ends at the *first* NUL.
        let mut head = WELL_FORMED;
        head[0x30 + 10] = b'X'; // well past "0e469de\0", inside the padding
        assert_eq!(image_version(&head).unwrap(), "0e469de");
    }

    #[test]
    fn a_head_shorter_than_the_descriptor_is_not_an_image() {
        let short = &WELL_FORMED[..0x4F];
        assert_eq!(image_version(short), Err(OtaError::NotAnImage));
    }

    #[test]
    fn a_wrong_magic_word_is_not_an_image() {
        let mut head = WELL_FORMED;
        head[0x20] = 0x00;
        head[0x21] = 0x00;
        head[0x22] = 0x00;
        head[0x23] = 0x00;
        assert_eq!(image_version(&head), Err(OtaError::NotAnImage));
    }

    #[test]
    fn a_full_32_byte_version_with_no_nul_returns_all_32_bytes() {
        // 0x30-0x4F filled with 'a' and no NUL: a full field needs no NUL.
        #[rustfmt::skip]
        let head: [u8; 0x50] = [
            // 0x00-0x1F: image header + segment header, unread.
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            // 0x20-0x23: magic_word.
            0x32, 0x54, 0xCD, 0xAB,
            // 0x24-0x2F: secure_version (4) + reserv1 (8).
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            // 0x30-0x4F: version, 32 bytes, no NUL anywhere in the field.
            b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a',
            b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a', b'a',
        ];
        let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(expected.len(), 32);
        assert_eq!(image_version(&head).unwrap(), expected);
    }

    #[test]
    fn may_activate_accepts_a_matching_version() {
        let m = manifest("0e469de");
        assert_eq!(may_activate(&WELL_FORMED, &m), Ok(()));
    }

    /// A manifest version with a trailing comment (which the manifest parser
    /// does not strip) does not match the image's version. Without this check
    /// the box would reflash forever.
    #[test]
    fn may_activate_refuses_the_manifest_image_mismatch_that_causes_the_reflash_loop() {
        let m = manifest("v1 # published today");
        let mut head = WELL_FORMED;
        // Overwrite the version field with "v1", NUL-padded.
        for b in head[0x30..0x50].iter_mut() {
            *b = 0;
        }
        head[0x30] = b'v';
        head[0x31] = b'1';
        assert_eq!(may_activate(&head, &m), Err(OtaError::VersionMismatch));
    }
}
