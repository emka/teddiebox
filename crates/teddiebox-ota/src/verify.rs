//! The gate between a downloaded image and switching `otadata` to it.

use crate::{Manifest, OtaError};

/// Whether the image just written to the spare slot is the one the manifest
/// promised, checked before `otadata` is switched.
///
/// `length` is how many bytes arrived, `digest` their SHA-256, and
/// `image_head` at least the first 0x50 of them.
///
/// The digest only proves the bytes are the ones the manifest describes, not
/// that whoever wrote the manifest is trusted; see the trust model on
/// [`crate::split`]. The version check stops a genuine image of another
/// version from being reflashed on every boot.
pub fn verify(
    manifest: &Manifest,
    length: u32,
    digest: &[u8; 32],
    image_head: &[u8],
) -> Result<(), OtaError> {
    if length != manifest.length {
        return Err(OtaError::LengthMismatch);
    }
    if digest != &manifest.sha256 {
        return Err(OtaError::DigestMismatch);
    }
    crate::may_activate(image_head, manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_MANIFEST;

    extern crate std;
    use std::format;

    /// SHA-256 of the three bytes `abc`, from FIPS 180-2's first example.
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    /// An image head whose descriptor reports the version `0e469de`: headers
    /// at 0x00-0x1F, the magic word 0xABCD5432 at 0x20, zeroed fields to
    /// 0x2F, then the version NUL-padded to 0x50.
    #[rustfmt::skip]
    const HEAD: [u8; 0x50] = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0x32, 0x54, 0xCD, 0xAB, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        b'0', b'e', b'4', b'6', b'9', b'd', b'e', 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    fn manifest(version: &str) -> Manifest {
        let s = format!("version = {version}\nsha256 = {ABC}\nlength = 3\nimage = a.bin\n");
        Manifest::parse_read(s.as_bytes(), MAX_MANIFEST).unwrap()
    }

    fn digest(hex: &str) -> [u8; 32] {
        crate::digest::parse_hex32(hex).unwrap()
    }

    #[test]
    fn the_promised_image_may_be_activated() {
        assert_eq!(verify(&manifest("0e469de"), 3, &digest(ABC), &HEAD), Ok(()));
    }

    #[test]
    fn a_body_shorter_than_the_manifest_length_is_refused() {
        assert_eq!(
            verify(&manifest("0e469de"), 2, &digest(ABC), &HEAD),
            Err(OtaError::LengthMismatch)
        );
    }

    #[test]
    fn a_body_longer_than_the_manifest_length_is_refused() {
        assert_eq!(
            verify(&manifest("0e469de"), 4, &digest(ABC), &HEAD),
            Err(OtaError::LengthMismatch)
        );
    }

    #[test]
    fn a_digest_that_differs_in_one_bit_is_refused() {
        let mut wrong = digest(ABC);
        wrong[31] ^= 1;
        assert_eq!(
            verify(&manifest("0e469de"), 3, &wrong, &HEAD),
            Err(OtaError::DigestMismatch)
        );
    }

    #[test]
    fn a_genuine_image_of_another_version_is_refused() {
        assert_eq!(
            verify(&manifest("1234567"), 3, &digest(ABC), &HEAD),
            Err(OtaError::VersionMismatch)
        );
    }
}
